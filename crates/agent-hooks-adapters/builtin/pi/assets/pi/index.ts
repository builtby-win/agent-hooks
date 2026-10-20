// agent-hooks pi extension — managed source of truth.
// Source: crates/agent-hooks-adapters/builtin/pi/assets/pi — staged by
// `agent-hooks install --harness pi` into ~/.pi/agent/extensions/agent-hooks.ts.
// Remove with: `agent-hooks uninstall --harness pi`.
//
// Design: observability everywhere, enforcement on gates. Every pi event is
// forwarded to the `agent-hooks` binary (`agent-hooks hook pi --event <Event>`)
// over stdin JSON. Observe events are fire-and-forget; gate events
// (`tool_call`, `tool_approval_requested`) await the binary verdict and return
// { block: true, reason } on deny (ask falls back to deny in the pi
// manifest). The binary runs daemonless local hooks, then forwards to the
// daemon (fast-allow on observe, long wait on gates for a decider verdict),
// so cross-agent ask works here too — the held verdict returns down the
// awaited call.
//
// Event mapping (pi -> agent-hooks AgentLifecycle):
//   session_start            -> SessionStart
//   session_switch/_branch   -> SessionStart (in-process /new, /resume, fork)
//   before_agent_start       -> UserPromptSubmit (awaited; may inject context)
//   turn_start               -> TurnStart (busy signal, asserts Running)
//   turn_end                 -> TurnEnd (busy signal, stays Running)
//   tool_call                -> PreToolUse
//   tool_result              -> PostToolUse (+ plan_approval_requested)
//   tool_approval_requested  -> PermissionRequest
//   agent_settled            -> Stop (settled: no auto-retry/follow-up left)
//   session_shutdown         -> SessionEnd
// NOTE: turns are busy signals, not lifecycle boundaries. A turn is one LLM
// response + tool calls, many per run: turn_start is not a prompt submit and
// turn_end is not a stop (both derive Running, never terminal). Only
// agent_settled -> Stop and session_shutdown -> SessionEnd terminate.
// before_agent_start carries UserPromptSubmit (not agent_start, which fires right
// after it every run and would double-emit). It is awaited so a hook's
// `additional_context` can be returned as a hidden message; session_start is
// awaited the same way and its context rides the first run. agent_end is
// intentionally not forwarded: per pi docs a run may still auto-retry,
// auto-compact, or continue with queued follow-ups. The Rust PiAdapter accepts
// agent names, so either side can evolve.

import { spawn, spawnSync } from "node:child_process";
import { closeSync, existsSync, openSync, readSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";

// Harness id passed to `agent-hooks hook <HARNESS>`.
const HARNESS = "pi";
// Native events that end a run (-> Stop). See the mapping above.
const TERMINAL_EVENTS: readonly string[] = ["agent_settled"];

// ---- shared pi/omp body: keep byte-identical with the omp asset below this line ----

// Install-time settings: `agent-hooks install` replaces each quoted token with
// a JSON string literal ("" when unset). An unstaged copy (e.g. loaded
// straight from the source tree) still holds the raw token — treat as unset.
const INSTALL_BIN = "__AGENT_HOOKS_BIN__";
const INSTALL_SOCKET = "__AGENT_HOOKS_SOCKET__";
const INSTALL_CONTEXT_CMD = "__AGENT_HOOKS_CONTEXT_CMD__";
const INSTALL_ORIGIN = "__AGENT_HOOKS_ORIGIN__";

function installSetting(value: string): string {
  const v = value.trim();
  return v.startsWith("__AGENT_HOOKS_") ? "" : v;
}

function resolveBinary(): string {
  const override = process.env.AGENT_HOOKS_BIN?.trim();
  if (override) return override;
  // PATH first so a fresh install on PATH shadows a stale ~/.cargo/bin build.
  try {
    const rawPath = process.env.PATH ?? "";
    const sep = process.platform === "win32" ? ";" : ":";
    for (const dir of rawPath.split(sep)) {
      const d = dir.trim();
      if (!d) continue;
      for (const name of process.platform === "win32" ? ["agent-hooks.exe", "agent-hooks"] : ["agent-hooks"]) {
        try {
          if (existsSync(join(d, name))) return join(d, name);
        } catch {
          // ignore, try next
        }
      }
    }
  } catch {
    // ignore, fall through to well-known locations
  }
  for (const candidate of [
    join(homedir(), ".cargo", "bin", "agent-hooks"),
    "/usr/local/bin/agent-hooks",
  ]) {
    try {
      if (existsSync(candidate)) return candidate;
    } catch {
      // ignore, try next
    }
  }
  return "agent-hooks";
}

const BINARY = installSetting(INSTALL_BIN) || resolveBinary();
const SOCKET = installSetting(INSTALL_SOCKET);
const CONTEXT_CMD = installSetting(INSTALL_CONTEXT_CMD);
// Install-time origin label (e.g. the ssh target this host was set up from);
// forwarded so a consumer with several remotes knows which one sent the event.
const ORIGIN = installSetting(INSTALL_ORIGIN);

// argv for `agent-hooks hook <harness> --event <E>` plus the install-time
// daemon socket / context command, so every spawn reaches the same daemon.
export function hookArgs(agentEvent: string): string[] {
  const args = ["hook", HARNESS, "--event", agentEvent];
  if (SOCKET) args.push("--socket", SOCKET);
  if (CONTEXT_CMD) args.push("--context-cmd", CONTEXT_CMD);
  if (ORIGIN) args.push("--origin", ORIGIN);
  return args;
}

// Kill switch: AGENT_HOOKS_DISABLED=1 skips every spawn (gates fail open).
// B2V_DISABLED is honored too because Back2Vibing users already set it to
// silence the legacy b2v plugins this asset replaces (b2v-pi.ts
// isBack2VibingDisabled / b2v-hooks.ts isB2vDisabled accepted "true"/"1").
export function hooksDisabled(env: NodeJS.ProcessEnv = process.env): boolean {
  const on = (v: string | undefined) => {
    const t = v?.trim().toLowerCase();
    return t === "1" || t === "true";
  };
  return on(env.AGENT_HOOKS_DISABLED) || on(env.B2V_DISABLED);
}

// Best-effort version handshake: surfaces stale-binary skew in logs, never blocks.
try {
  const probed = spawnSync(BINARY, ["--version"], { timeout: 2000, encoding: "utf8" });
  const ver = typeof probed?.stdout === "string" ? probed.stdout.trim() : String(probed?.stdout ?? "").trim();
  if (ver) {
    const embedded = (globalThis as { ADAPTER_VERSION?: unknown }).ADAPTER_VERSION;
    if (typeof embedded === "string" && embedded.trim() && !ver.includes(embedded.trim())) {
      console.error(`[agent-hooks] version mismatch: binary ${BINARY} reports "${ver}" but adapter expects "${embedded.trim()}"`);
    } else if (process.env.AGENT_HOOKS_DEBUG === "1") {
      console.error(`[agent-hooks] binary ${BINARY} version: ${ver}`);
    }
  }
} catch {
  // ignore — version probe must never block the agent
}

// Native event name -> agent event flag (observe-only; gates live in
// GATE_MAP, terminal events in TERMINAL_EVENTS).
const EVENT_MAP: Record<string, string> = {
  turn_start: "TurnStart",
  turn_end: "TurnEnd",
  tool_result: "PostToolUse",
  session_shutdown: "SessionEnd",
};

// Gate events: awaited — deny (or ask→deny) blocks the agent action.
const GATE_MAP: Record<string, string> = {
  tool_call: "PreToolUse",
  tool_approval_requested: "PermissionRequest",
};

// Session starts are awaited so a hook can contribute context. omp fires
// session_start once per process; /new, /resume, forks and handoffs swap the
// session in-process and only fire the others. Without a SessionStart the new
// session's first event carries no registration (legacy b2v-pi.ts
// onSessionChanged).
const SESSION_START_EVENTS: readonly string[] = ["session_start", "session_switch", "session_branch"];

// Context hooks must not hold up a prompt: bound the wait well under a gate's.
const CONTEXT_TIMEOUT_MS = 5_000;
const CONTEXT_MESSAGE_TYPE = "agent-hooks-context";

// Safety bound for gate hooks: must fire below host ~30s kill so fail-open
// allow is reachable; daemon hold is bounded shorter. The fast path
// resolves in milliseconds, so this only binds a hung daemon.
const GATE_TIMEOUT_MS = 25_000;

// pi/omp invoke handlers with (payload, context) objects; assert the index
// shape once here so field reads below stay total via unknown.
type Fields = { [key: string]: unknown };
function fields(value: unknown): Fields {
  return typeof value === "object" && value !== null ? (value as Fields) : {};
}

function getSessionId(payload: unknown, context: unknown): string | undefined {
  void payload;
  const ctx = fields(context);
  const manager: unknown = ctx.sessionManager;
  if (typeof manager === "object" && manager !== null) {
    if ("getSessionId" in manager) {
      const getId: unknown = manager.getSessionId;
      if (typeof getId === "function") {
        try {
          const v: unknown = Reflect.apply(getId, manager, []);
          if (typeof v === "string" && v.trim()) return v.trim();
        } catch {
          // ignore
        }
      }
    }
    if ("getSessionFile" in manager) {
      const getFile: unknown = manager.getSessionFile;
      if (typeof getFile === "function") {
        try {
          const file: unknown = Reflect.apply(getFile, manager, []);
          if (typeof file === "string" && file.trim()) {
            const fileId = readSessionIdFromFile(file.trim());
            if (fileId) return fileId;
          }
        } catch {
          // ignore — file fallback is best-effort
        }
      }
    }
  }
  const session = fields(ctx.session);
  const id: unknown =
    ctx.sessionId ?? ctx.session_id ?? ctx.sessionID ?? session.id;
  return typeof id === "string" && id.trim() ? id.trim() : undefined;
}

// Parent resolution: explicit payload keys first, then the session-file
// nesting forks use (`<ts>_<child-id>.json` under a `<parent-id>.jsonl`
// sibling dir). Without the file fallback every forked subagent registers
// as a top-level session. Best-effort, never throws.
function getParentId(p: Fields): string | undefined {
  const session = fields(p.session);
  const pid =
    p.parent_session_id ??
    p.parentSessionId ??
    p.parentSessionID ??
    p.parent_id ??
    p.parentId ??
    p.parentID ??
    p.parent_session_hash ??
    p.parentSessionHash ??
    session.parent_id ??
    session.parentId ??
    session.parent_session_id ??
    session.parentSessionId ??
    session.parent_session_hash ??
    session.parentSessionHash;
  return typeof pid === "string" && pid.trim() ? pid.trim() : undefined;
}

// The agent owns its title: `getSessionName` already resolves precedence (an
// auto-generated title never beats an explicit `/name`), so push it instead
// of making consumers reverse-engineer the session file (legacy b2v-pi.ts
// sessionTitle). Best-effort, never throws.
export function getSessionTitle(context: unknown): string | undefined {
  try {
    const manager: unknown = fields(context).sessionManager;
    if (typeof manager !== "object" || manager === null || !("getSessionName" in manager)) {
      return undefined;
    }
    const getName: unknown = manager.getSessionName;
    if (typeof getName !== "function") return undefined;
    const name: unknown = Reflect.apply(getName, manager, []);
    return typeof name === "string" && name.trim() ? name.trim() : undefined;
  } catch {
    return undefined;
  }
}

export function buildInput(payload: unknown, context: unknown): string {
  const p = fields(payload);
  const session = fields(p.session);
  const sid: unknown =
    p.session_id ??
    p.sessionId ??
    session.id ??
    p.sessionID ??
    p.id ??
    getSessionId(p, fields(context));
  const out: Fields = sid ? { ...p, session_id: sid } : { ...p };
  if (out.parent_session_id == null) {
    const parentId = getParentId(p) ?? nestedParentId(context);
    if (parentId) out.parent_session_id = parentId;
  }
  if (out.session_title == null) {
    const title = getSessionTitle(context);
    if (title) out.session_title = title;
  }
  // omp's session_stop hands over last_assistant_message as a whole message
  // object; the history store wants the chat text only.
  const lastText = assistantText(out.last_assistant_message);
  if (lastText) out.last_assistant_message = lastText;
  else delete out.last_assistant_message;
  if (out.last_assistant_message == null) {
    const reply = lastAssistantReply(p.messages);
    if (reply) out.last_assistant_message = reply;
  }
  return JSON.stringify(out);
}

// Chat text of an assistant message. Accepts a string, a message object, or
// its content: an array of typed blocks (thinking / text / toolCall) of which
// only the text blocks are chat.
export function assistantText(value: unknown): string {
  if (typeof value === "string") return value.trim();
  if (!Array.isArray(value)) {
    const content = fields(value).content;
    return content == null ? "" : assistantText(content);
  }
  return value
    .map((block) => {
      const b = fields(block);
      return b.type === "text" && typeof b.text === "string" ? b.text.trim() : "";
    })
    .filter(Boolean)
    .join("\n");
}

// Text of the newest assistant message in `messages` that has any.
function lastAssistantReply(messages: unknown): string {
  if (!Array.isArray(messages)) return "";
  for (let index = messages.length - 1; index >= 0; index--) {
    const message = fields(messages[index]);
    if (message.role !== "assistant") continue;
    const text = assistantText(message);
    if (text) return text;
  }
  return "";
}

// Explicit child marker set by launchers that fork outside the harness.
function explicitChildParent(): string | undefined {
  if (process.env.B2V_CHILD_SESSION !== "true") return undefined;
  const parent =
    process.env.B2V_PARENT_SESSION_HASH?.trim() ||
    process.env.AGENT_HOOKS_PARENT_SESSION_ID?.trim();
  return parent || undefined;
}

// Session files are append-only JSONL that grow for the whole session; the
// header is the first record. Read a bounded head, never the whole file.
const SESSION_HEAD_BYTES = 64 * 1024;
function readHeadLines(filePath: string): string[] {
  const fd = openSync(filePath, "r");
  try {
    const buf = Buffer.alloc(SESSION_HEAD_BYTES);
    const n = readSync(fd, buf, 0, SESSION_HEAD_BYTES, 0);
    const lines = buf.subarray(0, n).toString("utf8").split("\n");
    // A cut-off last line is partial JSON; drop it unless the file ended.
    if (n === SESSION_HEAD_BYTES) lines.pop();
    return lines;
  } finally {
    closeSync(fd);
  }
}

// First `{"type":"session","id":...}` record wins, mirroring b2v-pi.ts.
function readSessionIdFromFile(filePath: string): string | undefined {
  try {
    for (const line of readHeadLines(filePath)) {
      if (!line.trim()) continue;
      try {
        const parsed = JSON.parse(line) as { id?: unknown; type?: unknown };
        if (
          parsed.type === "session" &&
          typeof parsed.id === "string" &&
          parsed.id.trim()
        ) {
          return parsed.id.trim();
        }
        if (
          parsed.type === undefined &&
          typeof parsed.id === "string" &&
          parsed.id.trim()
        ) {
          return parsed.id.trim();
        }
      } catch {
        continue;
      }
    }
    return undefined;
  } catch {
    return undefined;
  }
}

// Lineage marker from the session header: `parentSession` is the source
// session id (fork) or session file path (branch into a new file). One read
// of the current file — no directory walk. Falls back to the nesting walk
// below when the header predates the marker.
function headerParentFromSessionFile(sessionFile: string): string | undefined {
  try {
    for (const line of readHeadLines(sessionFile)) {
      if (!line.trim()) continue;
      try {
        const parsed = JSON.parse(line) as {
          type?: unknown;
          parentSession?: unknown;
          previousSessionFiles?: unknown;
        };
        if (parsed.type !== "session") continue;
        const parent = parsed.parentSession;
        if (typeof parent === "string" && parent.trim()) {
          const marker = parent.trim();
          // Branch marker is a file path — resolve the id from that file.
          if (marker.endsWith(".jsonl") || marker.endsWith(".json") || marker.includes("/")) {
            return readSessionIdFromFile(marker);
          }
          return marker;
        }
        const previous = parsed.previousSessionFiles;
        if (Array.isArray(previous)) {
          for (const entry of previous) {
            if (typeof entry === "string" && entry.trim()) {
              const resolved = readSessionIdFromFile(entry.trim());
              if (resolved) return resolved;
            }
          }
        }
        return undefined;
      } catch {
        continue;
      }
    }
    return undefined;
  } catch {
    return undefined;
  }
}

// Walk up from a nested session file: `<parent-dir>/<child>.json` next to
// `<parent-dir>.jsonl` names the parent. Returns the parent session id.
function nestedParentFromSessionFile(sessionFile: string): string | undefined {
  try {
    let dir = dirname(sessionFile);
    const sessionRoot = dirname(dir);
    while (dir !== sessionRoot && dir !== dirname(dir)) {
      const parentFile = join(dirname(dir), `${basename(dir)}.jsonl`);
      if (existsSync(parentFile)) {
        return readSessionIdFromFile(parentFile);
      }
      dir = dirname(dir);
    }
    return undefined;
  } catch {
    return undefined;
  }
}

// Lineage never changes for a session file, and this runs on every event:
// resolve once per file (bounded; oldest evicted first).
const parentByFile = new Map<string, string | null>();
function parentForSessionFile(file: string): string | undefined {
  const cached = parentByFile.get(file);
  if (cached !== undefined) return cached ?? undefined;
  const parent = headerParentFromSessionFile(file) ?? nestedParentFromSessionFile(file);
  // A header not yet flushed resolves nothing: only cache real answers or a
  // file that already has content.
  if (parent !== undefined || readSessionIdFromFile(file) !== undefined) {
    parentByFile.set(file, parent ?? null);
    if (parentByFile.size > 256) {
      const oldest = parentByFile.keys().next().value;
      if (oldest !== undefined) parentByFile.delete(oldest);
    }
  }
  return parent;
}

export function nestedParentId(context: unknown): string | undefined {
  const explicit = explicitChildParent();
  if (explicit) return explicit;
  try {
    const manager: unknown = fields(context).sessionManager;
    if (typeof manager === "object" && manager !== null && "getSessionFile" in manager) {
      const getFile: unknown = (manager as { getSessionFile?: unknown }).getSessionFile;
      if (typeof getFile === "function") {
        try {
          // Bound call: a detached `getFile()` loses `this`, throws inside
          // the class method, and the catch below silently drops the parent.
          const file: unknown = Reflect.apply(getFile, manager, []);
          if (typeof file === "string" && file.trim()) return parentForSessionFile(file.trim());
        } catch {
          // ignore — file fallback is best-effort
        }
      }
    }
  } catch {
    // ignore — file fallback is best-effort
  }
  return undefined;
}

// Observe events go out one at a time: each is its own process, so two emitted
// milliseconds apart (plan_approval_requested right after PostToolUse) could
// otherwise reach the store swapped and leave the session looking busy. Never
// awaited by callers; a hung child releases the chain after EMIT_ORDER_TIMEOUT_MS.
const EMIT_ORDER_TIMEOUT_MS = 2_000;
let emitChain: Promise<void> = Promise.resolve();

function emit(agentEvent: string, payload: any, context: any): void {
  if (hooksDisabled()) return;
  const input = buildInput(payload, context);
  emitChain = emitChain.then(
    () =>
      new Promise<void>((done) => {
        let child;
        try {
          child = spawn(BINARY, hookArgs(agentEvent), {
            stdio: ["pipe", "ignore", "ignore"],
          });
        } catch {
          done(); // binary missing — never block the agent
          return;
        }
        const timer = setTimeout(done, EMIT_ORDER_TIMEOUT_MS);
        const finish = () => {
          clearTimeout(timer);
          done();
        };
        child.on("error", finish);
        child.on("close", finish);
        try {
          child.stdin.end(input);
        } catch {
          // ignore
        }
      }),
  );
}

// Awaited forward for gate hooks. Resolves the binary's verdict JSON;
// falls back to allow on any failure so the agent is never blocked by
// hook infra.
function query(agentEvent: string, payload: unknown, context: unknown, timeoutMs: number = GATE_TIMEOUT_MS): Promise<unknown> {
  const { promise, resolve } = Promise.withResolvers<unknown>();
  if (hooksDisabled()) {
    resolve({ decision: "allow" });
    return promise;
  }
  let child;
  try {
    child = spawn(BINARY, hookArgs(agentEvent), {
      stdio: ["pipe", "pipe", "ignore"],
    });
  } catch {
    resolve({ decision: "allow" });
    return promise;
  }
  let out = "";
  const timer = setTimeout(() => {
    try {
      child.kill();
    } catch {
      // ignore
    }
    resolve({ decision: "allow" });
  }, timeoutMs);
  child.on("error", () => {
    clearTimeout(timer);
    resolve({ decision: "allow" });
  });
  child.stdout?.on("data", (d: unknown) => {
    out += String(d);
  });
  child.on("close", () => {
    clearTimeout(timer);
    try {
      resolve(JSON.parse(out.trim() || '{"decision":"allow"}'));
    } catch {
      resolve({ decision: "allow" });
    }
  });
  try {
    child.stdin.end(buildInput(payload, context));
  } catch {
    // ignore
  }
  return promise;
}

function denyReason(decision: unknown): string | null {
  if (typeof decision !== "object" || decision === null || !("decision" in decision)) {
    return null;
  }
  if (decision.decision !== "deny") return null;
  if ("reason" in decision && typeof decision.reason === "string" && decision.reason.trim()) {
    return decision.reason.trim();
  }
  return "denied by agent-hooks";
}

// ---- Run-state helpers (ported from legacy b2v-pi.ts) ----

// `agent_end` with willContinue=true is a mid-run hop (auto-retry, queued
// follow-up), not the end of the run: forwarding it flapped the session to
// idle while it was still working (b2v-pi.ts isTerminalAgentEnd, e9043f2d7).
export function isTerminalAgentEnd(payload: unknown): boolean {
  return fields(payload).willContinue !== true;
}

// Current omp dispatches plan approval as a `write` to the `xd://propose`
// device URL; a successful one is the proposal, whatever its result text says.
// Older builds used a `resolve` call with action "apply", which is also used
// for non-plan edits, so only that branch falls back to the result text
// (b2v-pi.ts isPlanApprovalResult, f5c918940).
const PROPOSE_DEVICE_PATH = "xd://propose";
export function isPlanApprovalResult(payload: unknown): boolean {
  const p = fields(payload);
  if (p.isError === true) return false;
  const input = fields(p.input);
  const tool = p.toolName ?? p.tool_name;
  if (tool === "write") return (input.path ?? input.file_path) === PROPOSE_DEVICE_PATH;
  if (tool !== "resolve" || input.action !== "apply") return false;
  const content = p.content;
  if (!Array.isArray(content)) return false;
  return content.some((c: unknown) => {
    const part = fields(c);
    return part.type === "text" && typeof part.text === "string" && part.text.includes("Plan ready for approval.");
  });
}

// Per-session run state. A pending plan approval keeps the session waiting:
// the run's terminal event must not flip it to idle underneath the user
// (b2v-pi.ts agent_end handler). The hold lasts until the user starts the next
// run (before_agent_start): omp opens and aborts one more turn while it shows
// the approval dialog, so turn_start cannot release it.
const planApprovalPending = new Set<string>();
// Last non-empty assistant text of the run in progress, per session, taken
// from turn_end. The terminal event's own payload may not carry the reply.
const lastAssistantText = new Map<string, string>();
// Per session, whether the current run already emitted a Stop carrying the
// reply. omp fires both agent_end and session_stop per run; once one has
// delivered the reply the other is dropped, but a Stop without a reply never
// suppresses a later one that has it. The next turn_start re-arms the session.
const stopEmitted = new Map<string, boolean>();
// Tools the user approved with "allow always", per session: later calls skip
// the gate hold (still observed). Mirrors b2v-pi.ts sessionAllowed.
const allowedAlways = new Map<string, Set<string>>();

// Context a SessionStart hook produced, per session, delivered with the first
// run; and what each session was last handed, so an unchanged context is not
// appended to the transcript again on every prompt.
const sessionContext = new Map<string, string>();
const lastInjected = new Map<string, string>();

// `additional_context` from the shim's reply, if any (see the pi/omp
// `inject` templates). A deny or plain allow carries none.
export function contextOf(decision: unknown): string | undefined {
  const text = fields(decision).additional_context;
  return typeof text === "string" && text.trim() ? text : undefined;
}

function sessionKey(payload: unknown, context: unknown): string {
  const p = fields(payload);
  const sid = p.session_id ?? p.sessionId ?? getSessionId(p, context);
  return typeof sid === "string" ? sid : "";
}

function toolNameOf(payload: unknown): string | undefined {
  const p = fields(payload);
  const tool = p.toolName ?? p.tool_name;
  return typeof tool === "string" && tool.trim() ? tool.trim() : undefined;
}

export function isAllowedAlways(session: string, tool: string | undefined): boolean {
  return tool !== undefined && allowedAlways.get(session)?.has(tool) === true;
}

// Remember an allow_always verdict (`reply` rendered by the pi/omp manifests).
export function rememberAllowAlways(session: string, tool: string | undefined, decision: unknown): void {
  if (tool === undefined || fields(decision).reply !== "allow_always") return;
  let tools = allowedAlways.get(session);
  if (!tools) {
    tools = new Set();
    allowedAlways.set(session, tools);
  }
  tools.add(tool);
}

// ---- Ask bridge: answer `ask` tool calls in-band ----
// Port of b2v-pi.ts registerPiAskBridge. When the harness exposes
// registerTool, override `ask` so daemon verdicts (answers[]) resolve the
// tool call directly instead of round-tripping through a gate hold the
// harness would then re-ask natively.

type AskOption = {
  readonly label: string;
  readonly description?: string;
  readonly preview?: string;
};

type AskQuestion = {
  readonly id: string;
  readonly question: string;
  readonly header?: string;
  readonly options: readonly AskOption[];
  readonly multi?: boolean;
  readonly custom?: boolean;
  readonly recommended?: number;
};

type AskAnswer = {
  readonly id: string;
  readonly selectedOptions: readonly string[];
  readonly customInput?: string;
  readonly note?: string;
  readonly timedOut?: boolean;
};

type AskDialogResult =
  | { readonly kind: "chat" }
  | { readonly kind: "submit"; readonly results: readonly AskAnswer[] };

type AskContext = {
  readonly abort: () => void;
  readonly ui: {
    readonly askDialog?: (
      questions: readonly AskQuestion[],
      options?: { readonly signal?: AbortSignal },
    ) => Promise<AskDialogResult | undefined>;
  };
};

type AskToolResult = {
  readonly content: readonly { readonly type: "text"; readonly text: string }[];
  readonly details: unknown;
};

// Daemon holds cap at 300s; resolve inside it so the verdict lands in-band.
const ASK_TIMEOUT_MS = 290_000;

class AskCancelledError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AskCancelledError";
  }
}

function cancelAsk(ctx: AskContext, message: string): never {
  try {
    ctx.abort();
  } catch {
    // ignore
  }
  throw new AskCancelledError(message);
}

function canRegisterAskBridge(pi: any): boolean {
  if (typeof pi?.registerTool !== "function") return false;
  const z: unknown = pi?.zod?.z;
  if (typeof z !== "object" || z === null) return false;
  for (const m of ["object", "array", "string", "boolean", "number"]) {
    if (typeof Reflect.get(z, m) !== "function") return false;
  }
  return true;
}

// Verdict `answers` is the canonical protocol PromptAnswer[] (manifest
// `{answers_structured}`), snake_case only, already resolved to request order
// on the Rust side. Returns undefined when unusable, or when the verdict is
// not an allow (a deny never carries answers).
export function verdictAnswers(decision: unknown, questions: readonly AskQuestion[]): AskAnswer[] | undefined {
  if (typeof decision !== "object" || decision === null) return undefined;
  if (Reflect.get(decision, "decision") !== "allow") return undefined;
  const reply: unknown = Reflect.get(decision, "reply");
  if (reply !== undefined && reply !== "allow" && reply !== "allow_always") return undefined;
  const raw: unknown = Reflect.get(decision, "answers");
  if (!Array.isArray(raw) || raw.length !== questions.length) return undefined;
  const out: AskAnswer[] = [];
  for (let i = 0; i < questions.length; i++) {
    const q = questions[i];
    const a: unknown = raw[i];
    if (q === undefined || typeof a !== "object" || a === null) return undefined;
    const id: unknown = Reflect.get(a, "question_id");
    if (id !== q.id) return undefined;
    const sel: unknown = Reflect.get(a, "selected_options");
    if (!Array.isArray(sel) || sel.some((o: unknown) => typeof o !== "string")) return undefined;
    const allowed = new Set(q.options.map((o) => o.label));
    if (sel.some((o: unknown) => !allowed.has(o as string))) return undefined;
    if (new Set(sel).size !== sel.length) return undefined;
    const customRaw: unknown = Reflect.get(a, "custom_input");
    const custom = customRaw === null || customRaw === "" ? undefined : customRaw;
    if (custom !== undefined && (typeof custom !== "string" || q.custom === false)) return undefined;
    const customCount = typeof custom === "string" ? 1 : 0;
    if (sel.length + customCount === 0) return undefined;
    if (q.multi !== true && sel.length + customCount > 1) return undefined;
    out.push({
      id: q.id,
      selectedOptions: sel as string[],
      ...(typeof custom === "string" ? { customInput: custom } : {}),
    });
  }
  return out;
}

function validateNativeAnswers(results: readonly AskAnswer[], questions: readonly AskQuestion[]): AskAnswer[] | undefined {
  if (results.length !== questions.length) return undefined;
  const out: AskAnswer[] = [];
  for (let i = 0; i < questions.length; i++) {
    const q = questions[i];
    const r = results[i];
    if (q === undefined || r === undefined || r.id !== q.id) return undefined;
    if (!Array.isArray(r.selectedOptions) || r.selectedOptions.some((o) => typeof o !== "string" || !o.trim())) {
      return undefined;
    }
    const allowed = new Set(q.options.map((o) => o.label));
    if (r.selectedOptions.some((o) => !allowed.has(o))) return undefined;
    if (new Set(r.selectedOptions).size !== r.selectedOptions.length) return undefined;
    if (q.multi !== true && r.selectedOptions.length > 1) return undefined;
    if (r.customInput !== undefined && (typeof r.customInput !== "string" || !r.customInput.trim())) {
      return undefined;
    }
    out.push({
      id: q.id,
      selectedOptions: r.selectedOptions,
      ...(r.customInput !== undefined ? { customInput: r.customInput } : {}),
      ...(r.note !== undefined ? { note: r.note } : {}),
      ...(r.timedOut !== undefined ? { timedOut: r.timedOut } : {}),
    });
  }
  return out;
}

function formatAskResult(answer: AskAnswer, question: AskQuestion): string {
  const note = answer.note ? ` (note: ${answer.note})` : "";
  if (answer.customInput !== undefined) return `${answer.id}: "${answer.customInput}"${note}`;
  if (answer.selectedOptions.length > 0) {
    const suffix = `${answer.timedOut ? " (auto-selected after timeout)" : ""}${note}`;
    return question.multi === true
      ? `${answer.id}: [${answer.selectedOptions.join(", ")}]${suffix}`
      : `${answer.id}: ${answer.selectedOptions[0]}${suffix}`;
  }
  return `${answer.id}: (cancelled)${note}`;
}

function buildAskResult(questions: readonly AskQuestion[], answers: readonly AskAnswer[]): AskToolResult {
  const results = questions.map((question, index) => {
    const answer = answers[index];
    if (!answer) throw new AskCancelledError("Ask results did not match the requested questions");
    return {
      id: question.id,
      question: question.question,
      options: question.options.map((o) => o.label),
      multi: question.multi ?? false,
      selectedOptions: answer.selectedOptions,
      ...(answer.customInput !== undefined ? { customInput: answer.customInput } : {}),
      ...(answer.note !== undefined ? { note: answer.note } : {}),
      ...(answer.timedOut !== undefined ? { timedOut: answer.timedOut } : {}),
    };
  });
  if (results.length === 1) {
    const r = results[0];
    if (!r) throw new AskCancelledError("Ask result was missing");
    const selectedText = r.multi
      ? `User selected: ${r.selectedOptions.join(", ")}`
      : `User selected: ${r.selectedOptions[0]}`;
    const parts = [
      ...(r.selectedOptions.length > 0
        ? [r.timedOut ? `${selectedText} (auto-selected after timeout)` : selectedText]
        : []),
      ...(r.customInput !== undefined ? [`User provided custom input: ${r.customInput}`] : []),
      ...(r.note ? [`User added note: ${r.note}`] : []),
    ];
    return {
      content: [{ type: "text", text: parts.length > 0 ? parts.join("\n") : "User cancelled the selection" }],
      details: {
        question: r.question,
        options: r.options,
        multi: r.multi,
        selectedOptions: r.selectedOptions,
        ...(r.customInput !== undefined ? { customInput: r.customInput } : {}),
        ...(r.note !== undefined ? { note: r.note } : {}),
        ...(r.timedOut !== undefined ? { timedOut: r.timedOut } : {}),
      },
    };
  }
  return {
    content: [
      {
        type: "text",
        text: `User answers:\n${results.map((r, i) => formatAskResult(answers[i] ?? answers[0] as AskAnswer, questions[i] ?? questions[0] as AskQuestion)).join("\n")}`,
      },
    ],
    details: { results },
  };
}

async function runNativeAskFallback(
  questions: readonly AskQuestion[],
  signal: AbortSignal | undefined,
  ctx: AskContext,
): Promise<AskToolResult> {
  if (signal?.aborted) return cancelAsk(ctx, "Ask input was cancelled");
  const dialog = ctx.ui.askDialog;
  if (!dialog) return cancelAsk(ctx, "agent-hooks ask bridge failed and the native ask dialog is unavailable");
  const result = await dialog(questions, { signal });
  if (!result) return cancelAsk(ctx, "Ask tool was cancelled by the user");
  if (result.kind === "chat") {
    return {
      content: [
        {
          type: "text",
          text: `User chose to chat about this instead of answering.\n\nQuestions asked:\n${questions.map((q) => q.question).join("\n")}`,
        },
      ],
      details: { chatRedirect: true, questions: questions.map((q) => q.question) },
    };
  }
  const answers = validateNativeAnswers(result.results, questions);
  if (!answers) return cancelAsk(ctx, "Native ask dialog returned invalid answers");
  const first = answers[0];
  if (
    questions.length === 1 &&
    first?.timedOut !== true &&
    first?.selectedOptions.length === 0 &&
    first?.customInput === undefined
  ) {
    return cancelAsk(ctx, "Ask tool was cancelled by the user");
  }
  return buildAskResult(questions, answers);
}

function registerAskBridge(pi: any): boolean {
  if (!canRegisterAskBridge(pi)) return false;
  const z: any = pi.zod.z;
  const optional = (s: unknown) => {
    const m = typeof s === "object" && s !== null ? Reflect.get(s, "optional") : undefined;
    return typeof m === "function" ? Reflect.apply(m, s, []) : s;
  };
  const call = (s: unknown, name: string, args: readonly unknown[] = []) => {
    const m = typeof s === "object" && s !== null ? Reflect.get(s, name) : undefined;
    return typeof m === "function" ? Reflect.apply(m, s, args) : s;
  };
  let parameters: unknown;
  try {
    parameters = z.object({
      questions: call(
        z.array(
          z.object({
            id: z.string(),
            question: z.string(),
            header: optional(z.string()),
            options: z.array(z.object({ label: z.string(), description: optional(z.string()), preview: optional(z.string()) })),
            multi: optional(z.boolean()),
            custom: optional(z.boolean()),
            recommended: optional(call(call(z.number(), "int"), "nonnegative")),
          }),
        ),
        "min",
        [1],
      ),
    });
  } catch {
    return false;
  }
  try {
    pi.registerTool({
      name: "ask",
      label: "Ask",
      description: "Ask the user one or more structured questions and wait for their answers.",
      parameters,
      approval: "read",
      loadMode: "discoverable",
      execute: async (
        toolCallId: string,
        params: { questions: AskQuestion[] },
        signal: AbortSignal | undefined,
        _onUpdate: unknown,
        ctx: AskContext,
      ) => {
        const questions: readonly AskQuestion[] = params?.questions ?? [];
        if (signal?.aborted) return cancelAsk(ctx, "Ask input was cancelled");
        let decision: unknown;
        try {
          decision = await query(
            "PreToolUse",
            { tool_name: "ask", toolName: "ask", tool_input: { questions }, input: { questions }, toolCallId },
            ctx,
            ASK_TIMEOUT_MS,
          );
        } catch {
          decision = undefined;
        }
        if (signal?.aborted) return cancelAsk(ctx, "Ask input was cancelled");
        const parsed = verdictAnswers(decision, questions);
        if (parsed) return buildAskResult(questions, parsed);
        const reason = denyReason(decision);
        if (reason) return cancelAsk(ctx, reason);
        return runNativeAskFallback(questions, signal, ctx);
      },
    });
  } catch {
    return false;
  }
  return true;
}

export default function agentHooks(pi: any): void {
  let hasAskBridge = false;
  try {
    hasAskBridge = registerAskBridge(pi);
  } catch {
    hasAskBridge = false;
  }
  for (const [nativeEvent, agentEvent] of Object.entries(EVENT_MAP)) {
    try {
      pi.on(nativeEvent, (payload: any, context: any) => {
        const p = payload ?? {};
        const key = sessionKey(p, context);
        if (nativeEvent === "turn_start") stopEmitted.delete(key);
        if (nativeEvent === "turn_end") {
          const message = fields(p.message);
          const text = message.role === "assistant" ? assistantText(message) : "";
          if (text) lastAssistantText.set(key, text);
        }
        // While a plan awaits approval, the turn omp opens and aborts for the
        // dialog would flip the session from waiting back to running.
        if (planApprovalPending.has(key) && (nativeEvent === "turn_start" || nativeEvent === "turn_end")) return;
        emit(agentEvent, p, context);
        // A plan awaiting approval parks the session on the user: surface it
        // as a question-type notification (WaitingForInput).
        if (nativeEvent === "tool_result" && isPlanApprovalResult(p)) {
          planApprovalPending.add(sessionKey(p, context));
          emit(
            "plan_approval_requested",
            { reason: "plan_approval", notification_type: "question", message: "Plan ready for approval." },
            context,
          );
        }
      });
    } catch {
      // Older harness may not know a newer event name — skip it.
    }
  }
  for (const nativeEvent of SESSION_START_EVENTS) {
    try {
      pi.on(nativeEvent, async (payload: any, context: any) => {
        const p = payload ?? {};
        const key = sessionKey(p, context);
        const text = contextOf(await query("SessionStart", p, context, CONTEXT_TIMEOUT_MS));
        if (text) sessionContext.set(key, text);
        else sessionContext.delete(key);
        lastInjected.delete(key);
      });
    } catch {
      // Older harness may not know a newer event name — skip it.
    }
  }
  try {
    pi.on("before_agent_start", async (payload: any, context: any) => {
      const p = payload ?? {};
      const key = sessionKey(p, context);
      planApprovalPending.delete(key);
      const text = [
        sessionContext.get(key),
        contextOf(await query("UserPromptSubmit", p, context, CONTEXT_TIMEOUT_MS)),
      ]
        .filter(Boolean)
        .join("\n\n");
      if (!text || lastInjected.get(key) === text) return;
      lastInjected.set(key, text);
      // Tagged because omp's prompt treats XML tags as system-authored and
      // untagged message text as ordinary user content to be weighed.
      const content = `<${CONTEXT_MESSAGE_TYPE}>\n${text}\n</${CONTEXT_MESSAGE_TYPE}>`;
      return { message: { customType: CONTEXT_MESSAGE_TYPE, content, display: false } };
    });
  } catch {
    // Older harness without before_agent_start — no context, no prompt event.
  }
  for (const nativeEvent of TERMINAL_EVENTS) {
    try {
      pi.on(nativeEvent, (payload: any, context: any) => {
        const p = payload ?? {};
        if (!isTerminalAgentEnd(p)) return;
        const key = sessionKey(p, context);
        if (planApprovalPending.has(key)) return;
        if (stopEmitted.get(key)) return;
        const reply =
          assistantText(p.last_assistant_message) || lastAssistantReply(p.messages) || lastAssistantText.get(key) || "";
        lastAssistantText.delete(key);
        stopEmitted.set(key, reply !== "");
        emit("Stop", reply ? { ...p, last_assistant_message: reply } : p, context);
      });
    } catch {
      // Older harness may not know a newer event name — skip it.
    }
  }
  for (const [nativeEvent, agentEvent] of Object.entries(GATE_MAP)) {
    try {
      pi.on(nativeEvent, async (payload: unknown, context: unknown) => {
        const p = payload ?? {};
        const tool = toolNameOf(p);
        // The ask bridge owns `ask` tool calls; the gate must not re-handle them.
        if (hasAskBridge && tool?.toLowerCase() === "ask") return;
        const key = sessionKey(p, context);
        // Approved "always" earlier this session: observe, never hold again.
        if (isAllowedAlways(key, tool)) {
          emit(agentEvent, p, context);
          return;
        }
        const decision = await query(agentEvent, p, context);
        rememberAllowAlways(key, tool, decision);
        const reason = denyReason(decision);
        if (reason) return { block: true, reason };
        return;
      });
    } catch {
      // Older harness may not know a newer event name — skip it.
    }
  }
}
