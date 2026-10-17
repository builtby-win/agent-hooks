// Behaviour tests for the staged plugin assets (pi, omp, opencode, amp).
// Run: bun test crates/agent-hooks-adapters/builtin/plugins.test.ts
//
// Each asset is loaded as a fresh module whose `agent-hooks` binary is a fake
// shell script that logs argv + stdin and answers gate queries from a verdict
// file, so tests observe exactly what the plugin would send to the daemon.
import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, truncateSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const ASSETS = {
  pi: join(import.meta.dir, "pi/assets/pi/index.ts"),
  omp: join(import.meta.dir, "omp/assets/oh-my-pi/index.ts"),
  opencode: join(import.meta.dir, "opencode/assets/index.ts"),
  amp: join(import.meta.dir, "amp/assets/agent-hooks.ts"),
} as const;

type Call = { args: string[]; input: Record<string, unknown> };

let dir = "";
let logFile = "";
let verdictFile = "";
let fakeBin = "";
let loadSeq = 0;

beforeAll(() => {
  dir = realpathSync(mkdtempSync(join(tmpdir(), "ah-plugins-")));
  logFile = join(dir, "calls.log");
  verdictFile = join(dir, "verdict.json");
  fakeBin = join(dir, "agent-hooks");
  // One JSON line per call: {"args":[...],"input":<stdin>}. Gate queries read
  // their verdict from the verdict file (absent => no output).
  writeFileSync(
    fakeBin,
    `#!/bin/sh
[ "$1" = "--version" ] && exit 0
input=$(cat)
args=""
for a in "$@"; do args="$args\\"$a\\","; done
printf '{"args":[%s],"input":%s}\\n' "\${args%,}" "\${input:-null}" >> "${logFile}"
[ -f "${verdictFile}" ] && cat "${verdictFile}"
exit 0
`,
  );
  chmodSync(fakeBin, 0o755);
  process.env.AGENT_HOOKS_BIN = fakeBin;
});

afterAll(() => {
  rmSync(dir, { recursive: true, force: true });
});

function calls(): Call[] {
  if (!existsSync(logFile)) return [];
  return readFileSync(logFile, "utf8")
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l) as Call);
}

function reset(verdict?: unknown): void {
  rmSync(logFile, { force: true });
  if (verdict === undefined) rmSync(verdictFile, { force: true });
  else writeFileSync(verdictFile, JSON.stringify(verdict));
}

async function settle(expected: number, timeoutMs = 3000): Promise<Call[]> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const c = calls();
    if (c.length >= expected) {
      // Give stragglers a beat so "no extra call" assertions are meaningful.
      await Bun.sleep(80);
      return calls();
    }
    await Bun.sleep(20);
  }
  return calls();
}

function events(c: Call[]): string[] {
  return c.map((x) => x.args[x.args.indexOf("--event") + 1] ?? "");
}

// Load a private copy so module state (per-session sets) is isolated and the
// install-time tokens can be substituted exactly like the Rust stager does.
async function load(asset: keyof typeof ASSETS, stage?: { bin?: string; socket?: string; ctx?: string }): Promise<any> {
  let src = readFileSync(ASSETS[asset], "utf8");
  if (stage) {
    src = src
      .replace('"__AGENT_HOOKS_BIN__"', JSON.stringify(stage.bin ?? ""))
      .replace('"__AGENT_HOOKS_SOCKET__"', JSON.stringify(stage.socket ?? ""))
      .replace('"__AGENT_HOOKS_CONTEXT_CMD__"', JSON.stringify(stage.ctx ?? ""));
  }
  const out = join(dir, `mod-${asset}-${loadSeq++}.ts`);
  writeFileSync(out, src);
  const mod = await import(out);
  // opencode keeps its helpers off the function exports (V1 hosts call those).
  if (mod.__test) drains.push(mod.__test.drain);
  return mod.__test ? { ...mod, ...mod.__test } : mod;
}

// opencode serializes its observe emits; let each test's queue finish so its
// tail cannot land in the next test's call log.
const drains: Array<() => Promise<void>> = [];
afterEach(async () => {
  await Promise.all(drains.splice(0).map((drain) => drain()));
});

type Handler = (payload: unknown, ctx: unknown) => unknown;
function fakePi(): { api: any; on: Map<string, Handler> } {
  const on = new Map<string, Handler>();
  return { api: { on: (name: string, h: Handler) => on.set(name, h) }, on };
}

class SessionManager {
  constructor(
    private readonly id: string,
    private readonly file?: string,
    private readonly name?: string,
  ) {}
  getSessionId() {
    return this.id;
  }
  // Uses `this`: a detached call throws (the nestedParentId bug).
  getSessionFile() {
    return this.file;
  }
  getSessionName() {
    return this.name;
  }
}

for (const harness of ["pi", "omp"] as const) {
  describe(`${harness} asset`, () => {
    test("install placeholders: unstaged copy spawns plain hook argv", async () => {
      const mod = await load(harness);
      expect(mod.hookArgs("Stop")).toEqual(["hook", harness, "--event", "Stop"]);
    });

    test("install placeholders: staged bin/socket/context-cmd reach every spawn", async () => {
      reset();
      // AGENT_HOOKS_BIN points elsewhere: the staged INSTALL_BIN must win.
      const prev = process.env.AGENT_HOOKS_BIN;
      process.env.AGENT_HOOKS_BIN = "/nonexistent/agent-hooks";
      const mod = await load(harness, { bin: fakeBin, socket: "/tmp/ah.sock", ctx: "/opt/b2v" });
      process.env.AGENT_HOOKS_BIN = prev;
      expect(mod.hookArgs("Stop")).toEqual([
        "hook", harness, "--event", "Stop", "--socket", "/tmp/ah.sock", "--context-cmd", "/opt/b2v",
      ]);
      const { api, on } = fakePi();
      mod.default(api);
      on.get("before_agent_start")?.({ prompt: "hi" }, { sessionManager: new SessionManager("s-staged") });
      const c = await settle(1);
      expect(c[0]?.args).toEqual([
        "hook", harness, "--event", "UserPromptSubmit", "--socket", "/tmp/ah.sock", "--context-cmd", "/opt/b2v",
      ]);
    });

    test("kill switch: AGENT_HOOKS_DISABLED and legacy B2V_DISABLED", async () => {
      const mod = await load(harness);
      expect(mod.hooksDisabled({})).toBe(false);
      expect(mod.hooksDisabled({ AGENT_HOOKS_DISABLED: "1" })).toBe(true);
      // b2v-pi.ts isBack2VibingDisabled: B2V_DISABLED === 'true'
      expect(mod.hooksDisabled({ B2V_DISABLED: "true" })).toBe(true);
      expect(mod.hooksDisabled({ B2V_DISABLED: "0" })).toBe(false);
      reset();
      process.env.AGENT_HOOKS_DISABLED = "1";
      try {
        const { api, on } = fakePi();
        mod.default(api);
        on.get("before_agent_start")?.({}, { sessionManager: new SessionManager("s-off") });
        // Gates fail open without spawning.
        expect(await on.get("tool_call")?.({ toolName: "bash" }, {})).toBeUndefined();
        expect(await settle(1, 300)).toEqual([]);
      } finally {
        delete process.env.AGENT_HOOKS_DISABLED;
      }
    });

    describe("context injection", () => {
      const mgr = (id: string) => ({ sessionManager: new SessionManager(id) });

      test("session and prompt context ride a hidden message, once per change", async () => {
        reset({ additional_context: "User is new to Docker." });
        const mod = await load(harness);
        const { api, on } = fakePi();
        mod.default(api);
        await on.get("session_start")?.({}, mgr("s-ctx"));
        const first: any = await on.get("before_agent_start")?.({ prompt: "hi" }, mgr("s-ctx"));
        expect(first.message).toEqual({
          customType: "agent-hooks-context",
          content: "<agent-hooks-context>\nUser is new to Docker.\n\nUser is new to Docker.\n</agent-hooks-context>",
          display: false,
        });
        // Unchanged context is not appended to the transcript again.
        expect(await on.get("before_agent_start")?.({ prompt: "again" }, mgr("s-ctx"))).toBeUndefined();
        // A new session starts clean and is handed its context again.
        await on.get("session_switch")?.({}, mgr("s-ctx"));
        expect((await on.get("before_agent_start")?.({ prompt: "x" }, mgr("s-ctx")) as any).message).toBeDefined();
      });

      test("no context, deny, or a dead binary inject nothing", async () => {
        const mod = await load(harness);
        const { api, on } = fakePi();
        mod.default(api);
        for (const verdict of [undefined, { decision: "allow" }, { decision: "deny", reason: "no" }]) {
          reset(verdict);
          await on.get("session_start")?.({}, mgr("s-none"));
          expect(await on.get("before_agent_start")?.({ prompt: "hi" }, mgr("s-none"))).toBeUndefined();
        }
      });

      test("UserPromptSubmit is sent once per run, from before_agent_start", async () => {
        reset();
        const mod = await load(harness);
        const { api, on } = fakePi();
        mod.default(api);
        await on.get("before_agent_start")?.({ prompt: "hello" }, mgr("s-once"));
        on.get("agent_start")?.({}, mgr("s-once"));
        const c = await settle(1);
        expect(events(c)).toEqual(["UserPromptSubmit"]);
        // The prompt text now reaches the shim (agent_start never carried it).
        expect(c[0]?.input.prompt).toBe("hello");
      });

      test("contextOf only reads additional_context strings", async () => {
        const mod = await load(harness);
        expect(mod.contextOf({ additional_context: "x" })).toBe("x");
        expect(mod.contextOf({ additional_context: "  " })).toBeUndefined();
        expect(mod.contextOf({ decision: "allow" })).toBeUndefined();
        expect(mod.contextOf(null)).toBeUndefined();
      });
    });

    test("nestedParentId calls getSessionFile with its manager bound", async () => {
      const mod = await load(harness);
      const parentFile = join(dir, `${harness}-parent.jsonl`);
      writeFileSync(parentFile, `${JSON.stringify({ type: "session", id: "parent-1" })}\n`);
      const childFile = join(dir, `${harness}-child.jsonl`);
      writeFileSync(childFile, `${JSON.stringify({ type: "session", id: "child-1", parentSession: parentFile })}\n`);
      // Regression: the detached call threw on `this` and dropped the parent.
      expect(mod.nestedParentId({ sessionManager: new SessionManager("child-1", childFile) })).toBe("parent-1");
      // Negative: no lineage marker, no nesting dir -> no parent.
      const loneFile = join(dir, `${harness}-lone.jsonl`);
      writeFileSync(loneFile, `${JSON.stringify({ type: "session", id: "lone" })}\n`);
      expect(mod.nestedParentId({ sessionManager: new SessionManager("lone", loneFile) })).toBeUndefined();
    });

    // Memory invariant: lineage comes from the header only. A 3 GiB sparse
    // session file would make a whole-file read throw (and allocate GBs); the
    // bounded head read still resolves the parent.
    test("lineage read is bounded to the session header", async () => {
      const mod = await load(harness);
      const parentFile = join(dir, `${harness}-big-parent.jsonl`);
      writeFileSync(parentFile, `${JSON.stringify({ type: "session", id: "big-parent" })}\n`);
      const childFile = join(dir, `${harness}-big-child.jsonl`);
      writeFileSync(childFile, `${JSON.stringify({ type: "session", id: "big-child", parentSession: parentFile })}\n`);
      truncateSync(childFile, 3 * 1024 ** 3);
      expect(mod.nestedParentId({ sessionManager: new SessionManager("big-child", childFile) })).toBe("big-parent");
      rmSync(childFile);
    });

    test("nested subagent dir resolves parent (b2v-pi.ts subagentSessionInfo)", async () => {
      const mod = await load(harness);
      const root = join(dir, `${harness}-nest`, "sessions");
      mkdirSync(join(root, "parent-x"), { recursive: true });
      writeFileSync(join(root, "parent-x.jsonl"), `${JSON.stringify({ type: "session", id: "pid-x" })}\n`);
      const child = join(root, "parent-x", "0001_child.jsonl");
      writeFileSync(child, `${JSON.stringify({ type: "session", id: "cid-x" })}\n`);
      expect(mod.nestedParentId({ sessionManager: new SessionManager("cid-x", child) })).toBe("pid-x");
    });

    test("session title from getSessionName rides every event", async () => {
      const mod = await load(harness);
      const ctx = { sessionManager: new SessionManager("s-title", undefined, "  Fix login  ") };
      expect(JSON.parse(mod.buildInput({}, ctx)).session_title).toBe("Fix login");
      // Blank / missing names are not pushed.
      expect(JSON.parse(mod.buildInput({}, { sessionManager: new SessionManager("s", undefined, " ") })).session_title).toBeUndefined();
      expect(JSON.parse(mod.buildInput({}, {})).session_title).toBeUndefined();
    });

    test("session_switch / session_branch re-send SessionStart (b2v-pi.ts onSessionChanged)", async () => {
      reset();
      const mod = await load(harness);
      const { api, on } = fakePi();
      mod.default(api);
      on.get("session_switch")?.({ reason: "resume" }, { sessionManager: new SessionManager("s-switch") });
      on.get("session_branch")?.({}, { sessionManager: new SessionManager("s-branch") });
      const c = await settle(2);
      expect(events(c)).toEqual(["SessionStart", "SessionStart"]);
      expect(c.map((x) => x.input.session_id).sort()).toEqual(["s-branch", "s-switch"]);
    });

    test("plan approval parks the run: no Stop until the next turn", async () => {
      reset();
      const mod = await load(harness);
      const { api, on } = fakePi();
      mod.default(api);
      const ctx = { sessionManager: new SessionManager(`s-plan-${harness}`) };
      const terminal = harness === "pi" ? "agent_settled" : "agent_end";
      // b2v-pi.test.ts: write to xd://propose + "Plan ready for approval."
      on.get("tool_result")?.(
        {
          toolName: "write",
          input: { path: "xd://propose" },
          content: [{ type: "text", text: "Plan ready for approval." }],
        },
        ctx,
      );
      let c = await settle(2);
      // Concurrent fire-and-forget spawns: log order is not guaranteed.
      expect(events(c).sort()).toEqual(["PostToolUse", "plan_approval_requested"]);
      const plan = c.find((x) => x.args.includes("plan_approval_requested"))?.input;
      expect(plan?.notification_type).toBe("question");
      expect(plan?.session_id).toBe(`s-plan-${harness}`);
      reset();
      on.get(terminal)?.({}, ctx);
      expect(await settle(1, 300)).toEqual([]);
      // User answered: next turn starts, and that run's end is a real Stop.
      on.get("turn_start")?.({ turnIndex: 1 }, ctx);
      on.get(terminal)?.({}, ctx);
      c = await settle(2);
      expect(events(c).sort()).toEqual(["Stop", "TurnStart"]);
    });

    test("plan approval detection: legacy resolve/apply yes, ordinary write no", async () => {
      const mod = await load(harness);
      const ready = [{ type: "text", text: "Plan ready for approval." }];
      expect(mod.isPlanApprovalResult({ toolName: "resolve", input: { action: "apply" }, content: ready })).toBe(true);
      expect(mod.isPlanApprovalResult({ toolName: "write", input: { path: "/tmp/plan.md" }, content: ready })).toBe(false);
      expect(mod.isPlanApprovalResult({ toolName: "write", input: { path: "xd://propose" }, content: [] })).toBe(false);
    });

    test("allow_always is remembered per session and tool", async () => {
      const mod = await load(harness);
      const { api, on } = fakePi();
      mod.default(api);
      const ctx = { sessionManager: new SessionManager(`s-always-${harness}`) };
      reset({ decision: "allow", reply: "allow_always" });
      expect(await on.get("tool_call")?.({ toolName: "bash", input: { command: "ls" } }, ctx)).toBeUndefined();
      // Every later query would deny; the remembered tool must not ask again.
      reset({ decision: "deny", reason: "nope" });
      expect(await on.get("tool_call")?.({ toolName: "bash", input: { command: "ls" } }, ctx)).toBeUndefined();
      // Still observed (busy signal), just never held.
      expect(events(await settle(1))).toEqual(["PreToolUse"]);
      // Negative: another tool, and another session, still gate.
      expect(await on.get("tool_call")?.({ toolName: "edit" }, ctx)).toEqual({ block: true, reason: "nope" });
      const other = { sessionManager: new SessionManager(`s-other-${harness}`) };
      expect(await on.get("tool_call")?.({ toolName: "bash" }, other)).toEqual({ block: true, reason: "nope" });
      reset();
    });

    test("plain allow is not remembered", async () => {
      const mod = await load(harness);
      const { api, on } = fakePi();
      mod.default(api);
      const ctx = { sessionManager: new SessionManager(`s-once-${harness}`) };
      reset({ decision: "allow", reply: "allow" });
      await on.get("tool_call")?.({ toolName: "bash" }, ctx);
      reset({ decision: "deny", reason: "second" });
      expect(await on.get("tool_call")?.({ toolName: "bash" }, ctx)).toEqual({ block: true, reason: "second" });
      reset();
    });

    test("ask verdict answer groups map onto questions", async () => {
      const mod = await load(harness);
      const questions = [
        { id: "q1", question: "Pick", options: [{ label: "A" }, { label: "B" }] },
        { id: "q2", question: "Many", multi: true, options: [{ label: "X" }, { label: "Y" }] },
      ];
      expect(mod.verdictAnswers({ answers: [["A"], ["X", "Y"]] }, questions)).toEqual([
        { id: "q1", selectedOptions: ["A"] },
        { id: "q2", selectedOptions: ["X", "Y"] },
      ]);
      expect(mod.verdictAnswers({ answers: [["free text"], ["X"]] }, questions)).toEqual([
        { id: "q1", selectedOptions: [], customInput: "free text" },
        { id: "q2", selectedOptions: ["X"] },
      ]);
      // Single-select with two picks is rejected -> native fallback.
      expect(mod.verdictAnswers({ answers: [["A", "B"], ["X"]] }, questions)).toBeUndefined();
      // Protocol PromptAnswer objects still parse.
      expect(
        mod.verdictAnswers(
          { answers: [{ question_id: "q1", selected_options: ["B"] }, { question_id: "q2", selected_options: ["Y"] }] },
          questions,
        ),
      ).toEqual([
        { id: "q1", selectedOptions: ["B"] },
        { id: "q2", selectedOptions: ["Y"] },
      ]);
    });
  });
}

describe("omp asset", () => {
  // b2v-pi.ts isTerminalAgentEnd (e9043f2d7): willContinue hops are not the end.
  test("agent_end with willContinue is not a Stop", async () => {
    reset();
    const mod = await load("omp");
    const { api, on } = fakePi();
    mod.default(api);
    const ctx = { sessionManager: new SessionManager("s-cont") };
    on.get("agent_end")?.({ willContinue: true }, ctx);
    expect(await settle(1, 300)).toEqual([]);
    on.get("agent_end")?.({ willContinue: false }, ctx);
    on.get("agent_end")?.({}, ctx);
    expect(events(await settle(2))).toEqual(["Stop", "Stop"]);
  });
});

describe("pi asset", () => {
  test("agent_end is not forwarded; agent_settled is the Stop", async () => {
    reset();
    const mod = await load("pi");
    const { api, on } = fakePi();
    mod.default(api);
    expect(on.has("agent_end")).toBe(false);
    on.get("agent_settled")?.({}, { sessionManager: new SessionManager("s-settle") });
    expect(events(await settle(1))).toEqual(["Stop"]);
  });

  test("pi and omp share one body below the marker", () => {
    const marker = "// ---- shared pi/omp body";
    const body = (p: string) => {
      const s = readFileSync(p, "utf8");
      return s.slice(s.indexOf(marker));
    };
    expect(body(ASSETS.pi)).toBe(body(ASSETS.omp));
  });
});

describe("opencode asset", () => {
  test("install placeholders reach hook argv", async () => {
    const mod = await load("opencode", { bin: fakeBin, socket: "/tmp/ah.sock", ctx: "/opt/b2v" });
    expect(mod.hookArgs("session.idle")).toEqual([
      "hook", "opencode", "--event", "session.idle", "--socket", "/tmp/ah.sock", "--context-cmd", "/opt/b2v",
    ]);
    expect((await load("opencode")).hookArgs("session.idle")).toEqual(["hook", "opencode", "--event", "session.idle"]);
  });

  test("OpenCode forwards a per-event directory as cwd", async () => {
    reset();
    const mod = await load("opencode");
    const hooks = await mod.AgentHooksPlugin({ directory: "/workspace/project" });
    await hooks["tool.execute.before"]({ sessionID: "s1", directory: "/cell/project", toolName: "Bash" }, { args: {} });
    const [call] = await settle(1);
    expect(call.input.cwd).toBe("/cell/project");
  });

  test("OpenCode does not substitute the server plugin directory when event cwd is absent", async () => {
    reset();
    const mod = await load("opencode");
    await mod.AgentHooksPlugin({ directory: "/stale/server/project" });
    const out = mod.withSessionId({ sessionID: "s1" });
    expect(out.cwd).toBeUndefined();
  });

  test("OpenCode reuses a directory learned from that session's lifecycle event", async () => {
    const mod = await load("opencode");
    mod.forwardSessionEvent("session.created", { sessionID: "s1", directory: "/cell/project" });
    expect(mod.withSessionId({ sessionID: "s1" }).cwd).toBe("/cell/project");
  });

  test("kill switch", async () => {
    const mod = await load("opencode");
    expect(mod.hooksDisabled({ B2V_DISABLED: "1" })).toBe(true);
    expect(mod.hooksDisabled({ AGENT_HOOKS_DISABLED: "true" })).toBe(true);
    expect(mod.hooksDisabled({})).toBe(false);
  });

  // b2v-hooks.ts getLifecycleSignal: busy/retry -> busy, idle -> idle, unknown -> no-op.
  test("session.status busy/retry/idle mapping", async () => {
    const mod = await load("opencode");
    expect(mod.forwardSessionEvent("session.status", { sessionID: "a", status: { type: "busy" } }, 0)).toEqual(["session.busy"]);
    expect(mod.forwardSessionEvent("session.status", { sessionID: "b", status: "retry" }, 0)).toEqual(["session.busy"]);
    expect(mod.forwardSessionEvent("session.status", { sessionID: "c", status: { type: "idle" } }, 0)).toEqual(["session.idle"]);
    expect(mod.forwardSessionEvent("session.status", { sessionID: "d", status: { type: "compacting" } }, 0)).toEqual([]);
    // Noise events never forward.
    expect(mod.forwardSessionEvent("message.updated", { sessionID: "d" }, 0)).toEqual([]);
  });

  test("idle pair is sent once; busy right after idle is quarantined", async () => {
    const mod = await load("opencode");
    expect(mod.forwardSessionEvent("session.status", { sessionID: "s", status: { type: "busy" } }, 1000)).toEqual(["session.busy"]);
    expect(mod.forwardSessionEvent("session.status", { sessionID: "s", status: { type: "idle" } }, 2000)).toEqual(["session.idle"]);
    // session.idle right after session.status idle: same idle, not a second Stop.
    expect(mod.forwardSessionEvent("session.idle", { sessionID: "s" }, 2001)).toEqual([]);
    // b2v-hooks.ts BUSY_IDLE_REQUEST_GUARD_MS: weak busy within 3s is noise.
    expect(mod.forwardSessionEvent("session.status", { sessionID: "s", status: { type: "busy" } }, 4000)).toEqual([]);
    expect(mod.forwardSessionEvent("session.status", { sessionID: "s", status: { type: "busy" } }, 5500)).toEqual(["session.busy"]);
    expect(mod.forwardSessionEvent("session.idle", { sessionID: "s" }, 9000)).toEqual(["session.idle"]);
  });

  test("child sessions carry parent_session_id on later events", async () => {
    reset();
    const mod = await load("opencode");
    mod.forwardSessionEvent("session.created", { info: { id: "ses_child", parentID: "ses_parent" } });
    const out = mod.withSessionId({ sessionID: "ses_child", tool: "bash" });
    expect(out.parent_session_id).toBe("ses_parent");
    // Message ids are not sessions (b2v-hooks.ts isOpencodeMessageId).
    mod.forwardSessionEvent("session.created", { info: { id: "ses_top", parentID: "msg_123" } });
    expect(mod.withSessionId({ sessionID: "ses_top" }).parent_session_id).toBeUndefined();
    // session.created itself names the child by info.id.
    const c = await settle(2);
    const created = c.find((x) => x.input.session_id === "ses_child");
    expect(created?.input.parent_session_id).toBe("ses_parent");
    // Forgotten on delete.
    mod.forwardSessionEvent("session.deleted", { info: { id: "ses_child" } });
    expect(mod.withSessionId({ sessionID: "ses_child" }).parent_session_id).toBeUndefined();
    await settle(3);
  });

  test("title: placeholder ignored, changes pushed as session.title for known sessions", async () => {
    const mod = await load("opencode");
    expect(mod.forwardSessionEvent("session.created", { info: { id: "t1", title: "New session - 2026-09-01T10:00:00.000Z" } })).toEqual(["session.created"]);
    expect(mod.withSessionId({ sessionID: "t1" }).session_title).toBeUndefined();
    expect(mod.forwardSessionEvent("session.updated", { info: { id: "t1", title: "Refactor auth" } })).toEqual(["session.title"]);
    expect(mod.withSessionId({ sessionID: "t1" }).session_title).toBe("Refactor auth");
    // Same title again: nothing to push.
    expect(mod.forwardSessionEvent("session.updated", { info: { id: "t1", title: "Refactor auth" } })).toEqual([]);
    // Unknown session: remember the title but do not create it early.
    expect(mod.forwardSessionEvent("session.updated", { info: { id: "t2", title: "Other" } })).toEqual([]);
    expect(mod.withSessionId({ sessionID: "t2" }).session_title).toBe("Other");
  });

  // b2v-hooks.ts mapV2EventToV1 (V2 dropped session.status/idle).
  test("V2 execution events reconstruct busy/idle", async () => {
    const mod = await load("opencode");
    expect(mod.mapV2EventToV1({ type: "session.execution.started", data: { sessionID: "v" } })).toEqual([
      { type: "session.status", properties: { sessionID: "v", status: { type: "busy" } } },
    ]);
    for (const t of ["succeeded", "failed", "interrupted"]) {
      expect(mod.mapV2EventToV1({ type: `session.execution.${t}`, data: { sessionID: "v" } })).toEqual([
        { type: "session.status", properties: { sessionID: "v", status: { type: "idle" } } },
        { type: "session.idle", properties: { sessionID: "v" } },
      ]);
    }
    expect(mod.mapV2EventToV1({ type: "session.execution.started", data: {} })).toEqual([]);
    const [created] = mod.mapV2EventToV1({ type: "session.created", data: { sessionID: "k", parentID: "p", title: "T" } });
    expect(created.properties.info).toEqual({ id: "k", parentID: "p", title: "T" });
    const [form] = mod.mapV2EventToV1({
      type: "form.created",
      data: { form: { id: "f1", sessionID: "k", fields: [{ title: "Pick", type: "multiselect", options: [{ label: "A" }] }] } },
    });
    expect(form.type).toBe("question.asked");
    expect(form.properties.questions[0]).toMatchObject({ question: "Pick", multiple: true });
    // Global forms are not session prompts.
    expect(mod.mapV2EventToV1({ type: "form.created", data: { form: { id: "f", sessionID: "global" } } })).toEqual([]);
  });

  test("V2 setup drives the V1 event path; V1 export stays a function", async () => {
    reset();
    const mod = await load("opencode");
    expect(typeof mod.AgentHooksPlugin).toBe("function");
    expect(mod.default.id).toBe("agent-hooks");
    // Registration-only host: no subscribe, no-op.
    expect(typeof (await mod.default.setup({}))).toBe("function");
    async function* stream() {
      yield { type: "session.execution.started", data: { sessionID: "v2s" } };
      yield { type: "session.execution.succeeded", data: { sessionID: "v2s" } };
    }
    const stop = await mod.default.setup({ event: { subscribe: () => stream() }, location: { directory: dir } });
    const c = await settle(2);
    stop();
    expect(events(c)).toEqual(["session.busy", "session.idle"]);
    expect(c.every((x) => x.input.session_id === "v2s" || x.input.sessionID === "v2s")).toBe(true);
  });
});

describe("amp asset", () => {
  test("placeholders, payload shape and gate", async () => {
    const mod = await load("amp", { bin: fakeBin, socket: "/tmp/ah.sock" });
    expect(mod.hookArgs("agent.end")).toEqual(["hook", "amp", "--event", "agent.end", "--socket", "/tmp/ah.sock"]);
    // b2v-amp.ts sessionIdFrom: thread id first, then ctx thread.
    expect(mod.buildPayload({ thread: { id: "T-1" }, toolUseID: "tu", tool: "Bash", input: { cmd: "ls" } })).toMatchObject({
      session_id: "T-1",
      request_id: "tu",
      tool_name: "Bash",
      tool_input: { cmd: "ls" },
    });
    expect(mod.buildPayload({}, { thread: { id: "T-ctx" } }).session_id).toBe("T-ctx");
    expect(mod.toolCallAction({})).toEqual({ action: "allow" });
    expect(mod.toolCallAction({ decision: "allow" })).toEqual({ action: "allow" });
    expect(mod.toolCallAction({ decision: "deny", reason: "no" })).toEqual({ action: "reject-and-continue", message: "no" });
  });

  test("events forward with native names; deny rejects the tool", async () => {
    const mod = await load("amp");
    const on = new Map<string, Handler>();
    mod.default({ on: (n: string, h: Handler) => on.set(n, h) });
    expect([...on.keys()].sort()).toEqual(["agent.end", "agent.start", "session.start", "tool.call", "tool.result"]);
    reset({ decision: "deny", reason: "blocked" });
    expect(await on.get("tool.call")?.({ thread: { id: "T-2" }, tool: "Bash" }, {})).toEqual({
      action: "reject-and-continue",
      message: "blocked",
    });
    reset();
    on.get("agent.end")?.({ thread: { id: "T-2" } }, {});
    const c = await settle(1);
    expect(events(c)).toEqual(["agent.end"]);
    expect(c[0]?.input.session_id).toBe("T-2");
    // No verdict (infra failure) allows.
    expect(await on.get("tool.call")?.({ thread: { id: "T-2" } }, {})).toEqual({ action: "allow" });
  });
});
