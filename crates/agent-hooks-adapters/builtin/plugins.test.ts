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

    test("plan approval parks the run: no Stop until the user starts the next run", async () => {
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
          content: [{ type: "text", text: "Plan ready for review." }],
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
      // omp opens and aborts one more turn while the approval dialog shows;
      // its busy signals must not flip waiting back to running, and every
      // terminal event of the parked run is held.
      on.get("turn_start")?.({ turnIndex: 1 }, ctx);
      on.get("turn_end")?.({ turnIndex: 1, message: { role: "assistant", content: [] } }, ctx);
      on.get(terminal)?.({}, ctx);
      on.get(terminal)?.({}, ctx);
      expect(await settle(1, 500)).toEqual([]);
      // User answered: the next run starts, and that run's end is a real Stop.
      reset();
      await on.get("before_agent_start")?.({ prompt: "Plan approved." }, ctx);
      on.get(terminal)?.({}, ctx);
      c = await settle(2);
      expect(events(c).sort()).toEqual(["Stop", "UserPromptSubmit"]);
    });

    test("plan approval detection: successful xd://propose write, legacy resolve/apply by text", async () => {
      const mod = await load(harness);
      const propose = { path: "xd://propose" };
      const ready = [{ type: "text", text: "Plan ready for approval." }];
      // The proposal is the write itself; result wording is irrelevant.
      expect(mod.isPlanApprovalResult({ toolName: "write", input: propose, content: [] })).toBe(true);
      expect(mod.isPlanApprovalResult({ toolName: "write", input: propose, content: [{ type: "text", text: "anything" }] })).toBe(true);
      expect(mod.isPlanApprovalResult({ toolName: "write", input: propose, isError: true, content: [] })).toBe(false);
      expect(mod.isPlanApprovalResult({ toolName: "write", input: { path: "/tmp/plan.md" }, content: ready })).toBe(false);
      // Legacy resolve/apply is not plan-specific, so it still needs the text.
      expect(mod.isPlanApprovalResult({ toolName: "resolve", input: { action: "apply" }, content: ready })).toBe(true);
      expect(mod.isPlanApprovalResult({ toolName: "resolve", input: { action: "apply" }, content: [{ type: "text", text: "Applied 2 edits." }] })).toBe(false);
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

    test("ask verdict PromptAnswer[] maps onto questions without label guessing", async () => {
      const mod = await load(harness);
      const questions = [
        { id: "q1", question: "Pick", custom: true, options: [{ label: "A" }, { label: "B" }] },
        { id: "q2", question: "Many", multi: true, custom: true, options: [{ label: "X" }, { label: "Y" }] },
      ];
      const allow = (answers: unknown) => ({ decision: "allow", reply: "allow", answers });
      expect(
        mod.verdictAnswers(
          allow([{ question_id: "q1", selected_options: ["B"] }, { question_id: "q2", selected_options: ["X", "Y"] }]),
          questions,
        ),
      ).toEqual([
        { id: "q1", selectedOptions: ["B"] },
        { id: "q2", selectedOptions: ["X", "Y"] },
      ]);
      // Custom text that collides with an option label stays custom: it is
      // carried in custom_input, never re-derived from the label set.
      expect(
        mod.verdictAnswers(
          allow([
            { question_id: "q1", selected_options: [], custom_input: "A" },
            { question_id: "q2", selected_options: ["X"], custom_input: "Y" },
          ]),
          questions,
        ),
      ).toEqual([
        { id: "q1", selectedOptions: [], customInput: "A" },
        { id: "q2", selectedOptions: ["X"], customInput: "Y" },
      ]);
      // Long free text is preserved verbatim, edge whitespace included.
      const long = ` ${"x".repeat(20_000)}\nend `;
      expect(
        mod.verdictAnswers(
          allow([{ question_id: "q1", selected_options: [], custom_input: long }, { question_id: "q2", selected_options: ["X"] }]),
          questions,
        )?.[0]?.customInput,
      ).toBe(long);
      // Legacy string[][] groups are no longer a contract -> native fallback.
      expect(mod.verdictAnswers(allow([["A"], ["X", "Y"]]), questions)).toBeUndefined();
      // camelCase is not a contract either: canonical snake_case only.
      expect(
        mod.verdictAnswers(
          allow([{ id: "q1", selectedOptions: ["A"] }, { id: "q2", selectedOptions: ["X"] }]),
          questions,
        ),
      ).toBeUndefined();
      // Out-of-order answers (id mismatch) are rejected, not re-indexed.
      expect(
        mod.verdictAnswers(
          allow([{ question_id: "q2", selected_options: ["X"] }, { question_id: "q1", selected_options: ["A"] }]),
          questions,
        ),
      ).toBeUndefined();
      // Single-select with two picks is rejected -> native fallback.
      expect(
        mod.verdictAnswers(
          allow([{ question_id: "q1", selected_options: ["A", "B"] }, { question_id: "q2", selected_options: ["X"] }]),
          questions,
        ),
      ).toBeUndefined();
      expect(
        mod.verdictAnswers(
          allow([
            { question_id: "q1", selected_options: ["A"], custom_input: "" },
            { question_id: "q2", selected_options: [], custom_input: " \t " },
          ]),
          questions,
        ),
      ).toEqual([
        { id: "q1", selectedOptions: ["A"] },
        { id: "q2", selectedOptions: [], customInput: " \t " },
      ]);
      const whitespaceQuestions = [
        { id: "q1", question: "Pick", options: [{ label: "  " }] },
        { id: "q2", question: "Pick custom", custom: true, options: [] },
      ];
      expect(
        mod.verdictAnswers(
          allow([
            { question_id: "q1", selected_options: ["  "] },
            { question_id: "q2", selected_options: [], custom_input: " \n " },
          ]),
          whitespaceQuestions,
        ),
      ).toEqual([
        { id: "q1", selectedOptions: ["  "] },
        { id: "q2", selectedOptions: [], customInput: " \n " },
      ]);
      expect(
        mod.verdictAnswers(
          allow([
            { question_id: "q1", selected_options: ["A"], custom_input: "extra" },
            { question_id: "q2", selected_options: ["X"] },
          ]),
          questions,
        ),
      ).toBeUndefined();
      expect(
        mod.verdictAnswers(
          allow([
            { question_id: "q1", selected_options: [], custom_input: "extra" },
            { question_id: "q2", selected_options: ["X"] },
          ]),
          [{ ...questions[0], custom: false }, questions[1]],
        ),
      ).toBeUndefined();
      expect(
        mod.verdictAnswers(
          allow([
            { question_id: "q1", selected_options: [], custom_input: "legacy custom answer" },
            { question_id: "q2", selected_options: ["X"] },
          ]),
          [
            { id: "q1", question: "Legacy ask", options: [] },
            questions[1],
          ],
        ),
      ).toEqual([
        { id: "q1", selectedOptions: [], customInput: "legacy custom answer" },
        { id: "q2", selectedOptions: ["X"] },
      ]);
      // A deny that still carries answers never counts as an answer.
      expect(
        mod.verdictAnswers(
          { decision: "deny", reason: "no", answers: [{ question_id: "q1", selected_options: ["A"] }, { question_id: "q2", selected_options: ["X"] }] },
          questions,
        ),
      ).toBeUndefined();
    });

    test("Stop carries only the assistant's text blocks as last_assistant_message", async () => {
      const terminal = harness === "pi" ? "agent_settled" : "session_stop";
      const reply = [
        { type: "thinking", thinking: "hmm" },
        { type: "text", text: "All done." },
        { type: "toolCall", name: "read" },
      ];
      reset();
      const mod = await load(harness);
      const { api, on } = fakePi();
      mod.default(api);
      const session = (n: number) => ({ sessionManager: new SessionManager(`s-text-${harness}-${n}`) });
      // A whole message object (omp session_stop) and a plain string both
      // reduce to the text.
      on.get(terminal)?.({ last_assistant_message: { role: "assistant", content: reply } }, session(1));
      on.get(terminal)?.({ last_assistant_message: "All done." }, session(2));
      // Terminal payload without the reply falls back to the last turn's text.
      on.get("turn_end")?.({ message: { role: "assistant", content: reply }, toolResults: [] }, session(3));
      on.get(terminal)?.({}, session(3));
      const stops = (await settle(4)).filter((c) => c.args.includes("Stop"));
      expect(stops.map((c) => c.input.last_assistant_message).sort()).toEqual(["All done.", "All done.", "All done."]);
    });

    test("assistantText skips thinking and tool calls", async () => {
      const mod = await load(harness);
      expect(mod.assistantText([{ type: "thinking", thinking: "x" }, { type: "toolCall" }])).toBe("");
      expect(mod.assistantText({ role: "assistant", content: [{ type: "text", text: "a " }, { type: "text", text: "b" }] })).toBe("a\nb");
      const out = JSON.parse(
        mod.buildInput({ messages: [{ role: "assistant", content: [{ type: "text", text: "hi" }] }, { role: "assistant", content: [{ type: "toolCall" }] }] }, {}),
      );
      expect(out.last_assistant_message).toBe("hi");
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
    expect(events(await settle(1))).toEqual(["Stop"]);
  });

  test("agent_end and session_stop of one run emit a single Stop; a new turn re-arms", async () => {
    reset();
    const mod = await load("omp");
    const { api, on } = fakePi();
    mod.default(api);
    const ctx = { sessionManager: new SessionManager("s-dedupe") };
    on.get("agent_end")?.({ messages: [{ role: "assistant", content: [{ type: "text", text: "one" }] }] }, ctx);
    on.get("session_stop")?.({ last_assistant_message: { role: "assistant", content: [{ type: "text", text: "one" }] } }, ctx);
    expect(events(await settle(2, 400))).toEqual(["Stop"]);
    on.get("turn_start")?.({}, ctx);
    on.get("session_stop")?.({ last_assistant_message: "two" }, ctx);
    const stops = (await settle(3)).filter((c) => c.args.includes("Stop"));
    expect(stops.map((c) => c.input.last_assistant_message)).toEqual(["one", "two"]);
  });

  test("a Stop without a reply does not suppress the later Stop that has it", async () => {
    reset();
    const mod = await load("omp");
    const { api, on } = fakePi();
    mod.default(api);
    const ctx = { sessionManager: new SessionManager("s-late-reply") };
    on.get("agent_end")?.({ messages: [{ role: "assistant", content: [{ type: "toolCall" }] }] }, ctx);
    on.get("session_stop")?.({ last_assistant_message: { role: "assistant", content: [{ type: "text", text: "late" }] } }, ctx);
    on.get("agent_end")?.({ messages: [] }, ctx);
    const stops = (await settle(2)).filter((c) => c.args.includes("Stop"));
    expect(stops.map((c) => c.input.last_assistant_message ?? "").sort()).toEqual(["", "late"]);
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

  test("V2 prompt hooks and completed context become history events", async () => {
    reset();
    const mod = await load("opencode");
    let promptHandler:
      | ((event: { sessionID?: string; prompt?: { text?: string } }) => void)
      | undefined;
    async function* stream() {
      yield { type: "session.created", data: { sessionID: "v2-history" } };
      yield { type: "session.execution.succeeded", data: { sessionID: "v2-history" } };
    }
    const stop = await mod.default.setup({
      location: { directory: "/workspace/history" },
      session: {
        hook: async (_name: string, handler: (event: { sessionID?: string; prompt?: { text?: string } }) => void) => {
          promptHandler = handler;
          return { dispose: async () => {} };
        },
        context: async () => [
          { type: "user", content: [{ type: "text", text: "Explain this change" }] },
          {
            type: "assistant",
            content: [
              { type: "text", text: "It fixes history capture." },
              { type: "tool", text: "do not store tool output" },
            ],
          },
        ],
      },
      event: { subscribe: () => stream() },
    });
    promptHandler?.({ sessionID: "v2-history", prompt: { text: "Explain this change" } });
    const c = await settle(3);
    stop();
    expect(events(c)).toContain("chat.message");
    expect(c.find((call) => events([call])[0] === "chat.message")?.input.message).toBe(
      "Explain this change",
    );
    const idle = c.find((call) => events([call])[0] === "session.idle");
    expect(idle?.input.summary).toBe("It fixes history capture.");
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
  test("workspace root is forwarded as cwd", async () => {
    const mod = await load("amp");
    const amp = {
      system: { workspaceRoot: { toString: () => "file:///Users/dev/project" } },
      helpers: { filePathFromURI: (uri: unknown) => String(uri).replace(/^file:\/\//, "") },
    };
    // ctx.system workspace root wins via amp helpers.
    expect(
      mod.buildPayload({ thread: { id: "T-1" } }, { system: { workspaceRoot: "file:///Users/dev/project" } }, amp).cwd,
    ).toBe("/Users/dev/project");
    // amp-level fallback when ctx carries no system.
    expect(mod.resolveCwd({}, amp)).toBe("/Users/dev/project");
    // event-level cwd is authoritative and never dropped.
    expect(mod.buildPayload({ thread: { id: "T-1" }, cwd: "/evt/dir" }, {}, amp).cwd).toBe("/evt/dir");
    // no workspace anywhere => no cwd key (shim falls back to process cwd).
    expect("cwd" in mod.buildPayload({ thread: { id: "T-1" } })).toBe(false);
  });

  test("canonical answer parsing follows Rust custom-input and option-label semantics", async () => {
    const mod = await load("amp");
    const questions = [
      { id: "q1", question: "Choose", options: [{ label: "  " }, { label: "A" }], custom: true },
      { id: "q2", question: "Custom", options: [], custom: true },
    ];
    expect(
      mod.canonicalAnswers(
        {
          decision: "allow",
          answers: [
            { question_id: "q1", selected_options: ["  "], custom_input: "" },
            { question_id: "q2", selected_options: [], custom_input: " \t " },
          ],
        },
        questions,
      ),
    ).toEqual([
      { question_id: "q1", selected_options: ["  "] },
      { question_id: "q2", selected_options: [], custom_input: " \t " },
    ]);
    expect(
      mod.canonicalAnswers(
        { decision: "allow", answers: [{ question_id: "q1", selected_options: ["A"], custom_input: null }] },
        [questions[0]],
      ),
    ).toEqual([{ question_id: "q1", selected_options: ["A"] }]);
    expect(() =>
      mod.canonicalAnswers(
        { decision: "allow", answers: [{ question_id: "q1", selected_options: ["A"], custom_input: "extra" }] },
        [{ ...questions[0], custom: false }],
      ),
    ).toThrow();
  });

  test("question tool forwards structured answers and rejects denial or cancellation", async () => {
    const mod = await load("amp");
    const on = new Map<string, Handler>();
    let askTool: { execute: (input: unknown, ctx: unknown) => Promise<string> } | undefined;
    mod.default({
      on: (name: string, handler: Handler) => on.set(name, handler),
      registerTool: (tool: unknown) => { askTool = tool as typeof askTool; },
    });
    expect(askTool).toBeDefined();
    const questions = [{
      id: "q1",
      question: "Choose",
      options: [{ label: "A", description: "First" }, { label: "B" }],
      multiple: true,
      custom: true,
    }];
    const ctx = {
      thread: { id: "T-ask" },
      ui: {
        confirm: async () => true,
        select: async () => undefined,
        input: async () => undefined,
      },
    };
    reset({ decision: "allow", answers: [{ question_id: "q1", selected_options: ["A", "B"] }] });
    expect(JSON.parse(await askTool?.execute({ questions }, ctx) ?? "[]")).toEqual([
      { question_id: "q1", selected_options: ["A", "B"] },
    ]);
    const questionCalls = await settle(1);
    expect(events(questionCalls)).toEqual(["question"]);
    expect(questionCalls[0]?.input).toMatchObject({
      questions,
      tool_name: "agent_hooks_ask",
      session_id: "T-ask",
    });

    reset({ decision: "deny", reason: "blocked" });
    await expect(askTool?.execute({ questions }, ctx)).rejects.toThrow("blocked");
    reset();
    await expect(askTool?.execute({ questions }, ctx)).rejects.toThrow("cancelled");
  });

  test("tool gate denies and confirms permission asks", async () => {
    const mod = await load("amp");
    const on = new Map<string, Handler>();
    mod.default({
      on: (name: string, handler: Handler) => on.set(name, handler),
      registerTool: () => {},
    });
    reset({ decision: "deny", reason: "blocked" });
    expect(await on.get("tool.call")?.({ thread: { id: "T-2" }, tool: "Bash" }, { ui: { confirm: async () => true } })).toEqual({
      action: "reject-and-continue",
      message: "blocked",
    });
    reset({ decision: "ask", reason: "Needs approval" });
    expect(await on.get("tool.call")?.({ thread: { id: "T-2" }, tool: "Bash" }, { ui: { confirm: async () => true } })).toEqual({
      action: "allow",
    });
    reset({ decision: "ask", reason: "Needs approval" });
    expect(await on.get("tool.call")?.({ thread: { id: "T-2" }, tool: "Bash" }, { ui: { confirm: async () => false } })).toEqual({
      action: "reject-and-continue",
      message: "Needs approval",
    });
    reset();
    on.get("agent.end")?.({ thread: { id: "T-2" } }, {});
    const c = await settle(1);
    expect(events(c)).toEqual(["agent.end"]);
    expect(c[0]?.input.session_id).toBe("T-2");
    // No verdict (infra failure) allows.
    expect(await on.get("tool.call")?.({ thread: { id: "T-2" } }, { ui: { confirm: async () => true } })).toEqual({ action: "allow" });
  });
});
