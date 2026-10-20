// managed by agent-hooks
// agent-hooks opencode plugin — managed by agent-hooks.
// Staged via the opencode builtin manifest (stage_dir "~/.config/opencode/plugins"
// as agent-hooks.ts). Do not hand-edit: reinstall with
// `agent-hooks install --harness opencode`.
// Remove with: `agent-hooks uninstall --harness opencode`.
//
// Design: observability + gate enforcement. Every opencode hook is forwarded
// to the `agent-hooks` binary (`agent-hooks hook opencode --event <Event>`)
// over stdin JSON. The binary runs daemonless local hooks, then forwards to
// the daemon (fast-allow on observe events, long wait on gates for a decider
// verdict) and prints the merged decision; deny (including ask fallback)
// throws to block. No verdict ({}) on infra failure keeps the agent
// unblocked and leaves opencode's own permission handling in charge.
//
// Event mapping (opencode -> agent-hooks AgentLifecycle):
//   tool.execute.before  -> PreToolUse (gate: throw to deny)
//   tool.execute.after   -> PostToolUse (observe, fire-and-forget)
//   permission.ask       -> PermissionRequest (gate: output.status allow|deny|ask)
//   question.asked       -> PermissionRequest/Question (answered out of band
//                           via client.question.reply — see deliverQuestionReply)
//   chat.message         -> UserPromptSubmit (gate: throw to deny)
//   session.created      -> SessionStart
//   session.updated      -> not forwarded (noisy mutation hook, not a user prompt;
//                           chat.message is the true prompt). Its title change is
//                           pushed as session.title -> TurnEnd (status-neutral noise).
//   session.deleted      -> SessionEnd
//   session.idle         -> Stop (observe)
//   session.status       -> busy/retry: session.busy -> TurnStart (observe: the
//                           poll asserts Running, it is not a prompt submit);
//                           idle: session.idle -> Stop. Other status types ignored
//                           (legacy b2v-hooks.ts getLifecycleSignal).
// NOTE: session events arrive via the generic "event" hook; only known
// session.* names are forwarded, everything else is ignored.
//
// Subagents: a child session names its parent only on session.created /
// session.updated (`info.parentID`). The parent is remembered per child so
// every later event of that child carries `parent_session_id`.
//
// Exports: the named `AgentHooksPlugin` is the V1 entry (V1 hosts call every
// exported function); the default `{ id, setup }` is the V2 entry, which
// registers the prompt-admission hook and reads completed assistant text from
// session context when execution succeeds.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

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

// Install-time settings: `agent-hooks install` replaces each quoted token with
// a JSON string literal ("" when unset). An unstaged copy still holds the raw
// token — treat as unset.
const INSTALL_BIN = "";
const INSTALL_SOCKET = "";
const INSTALL_CONTEXT_CMD = "";
const INSTALL_ORIGIN = "";

function installSetting(value: string): string {
  const v = value.trim();
  return v.startsWith("__AGENT_HOOKS_") ? "" : v;
}

const BINARY = installSetting(INSTALL_BIN) || resolveBinary();
const SOCKET = installSetting(INSTALL_SOCKET);
const CONTEXT_CMD = installSetting(INSTALL_CONTEXT_CMD);
// Install-time origin label (e.g. the ssh target this host was set up from);
// forwarded so a consumer with several remotes knows which one sent the event.
const ORIGIN = installSetting(INSTALL_ORIGIN);

// argv for `agent-hooks hook opencode --event <E>` plus the install-time
// daemon socket / context command, so every spawn reaches the same daemon.
function hookArgs(agentEvent: string): string[] {
  const args = ["hook", "opencode", "--event", agentEvent];
  if (SOCKET) args.push("--socket", SOCKET);
  if (CONTEXT_CMD) args.push("--context-cmd", CONTEXT_CMD);
  if (ORIGIN) args.push("--origin", ORIGIN);
  return args;
}

// Kill switch: AGENT_HOOKS_DISABLED=1 skips every spawn (gates fail open).
// B2V_DISABLED is honored too because Back2Vibing users already set it to
// silence the legacy b2v-hooks.ts plugin this asset replaces (isB2vDisabled
// accepted "true"/"1").
function hooksDisabled(env: NodeJS.ProcessEnv = process.env): boolean {
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

// Safety bound for gate hooks (tool.execute.before awaits the daemon
// verdict). Must fire below host ~30s kill so fail-open allow is reachable;
// daemon hold is bounded shorter. The fast path resolves in milliseconds,
// so this only binds a hung daemon.
const GATE_TIMEOUT_MS = 25_000;
// Questions are answered out of band (deliverQuestionReply), so no host kill
// applies: outlast the shim (its GATE_WAIT is 330s) so its verdict always lands first. Cutting this short
// closes the daemon hold and a later panel answer is dropped.
const QUESTION_TIMEOUT_MS = 335_000;

// opencode session event name -> agent event flag.
const SESSION_EVENT_MAP: Record<string, string> = {
  "session.created": "SessionStart",
  "session.deleted": "SessionEnd",
  "session.idle": "Stop",
};

// session.created/updated carry the id only as `info.id` (no sessionID);
// message-ish ids (`msg_`, `evt_`, `tool_`, `run_`) are never sessions
// (legacy b2v-hooks.ts extractSessionId).
const NON_SESSION_ID = /^(msg|evt|tool|run)[_-]/;
function getSessionId(payload: any): string | undefined {
  const direct =
    payload?.sessionID ??
    payload?.sessionId ??
    payload?.session_id ??
    payload?.session?.id ??
    payload?.info?.sessionID;
  if (typeof direct === "string" && direct.trim()) return direct.trim();
  const fallback = payload?.info?.id ?? payload?.id;
  return typeof fallback === "string" && fallback.trim() && !NON_SESSION_ID.test(fallback.trim())
    ? fallback.trim()
    : undefined;
}

// Parent passthrough (field copy only — no lookup, no I/O). When the harness
// names the parent under a variant key, stamp the stable `parent_session_id`
// so the manifest extracts one key.
type Fields = { [key: string]: unknown };
function fields(value: unknown): Fields {
  return typeof value === "object" && value !== null ? (value as Fields) : {};
}
// opencode message ids (`msg_…`) show up under parent-ish keys on message
// events; they are not sessions (legacy b2v-hooks.ts isOpencodeMessageId).
function isMessageId(v: unknown): boolean {
  return typeof v === "string" && (v.startsWith("msg_") || v.startsWith("msg-"));
}
function getParentId(payload: unknown): string | undefined {
  const root = fields(payload);
  const nested = fields(root.session ?? fields(root.properties).session);
  const info = fields(root.info ?? fields(root.properties).info);
  const pid =
    info.parentID ??
    info.parentId ??
    info.parent_id ??
    root.parent_session_id ??
    root.parentSessionId ??
    root.parentSessionID ??
    root.parent_id ??
    root.parentId ??
    root.parentID ??
    root.parent_session_hash ??
    root.parentSessionHash ??
    nested.parent_id ??
    nested.parentId ??
    nested.parent_session_id ??
    nested.parentSessionId ??
    nested.parent_session_hash ??
    nested.parentSessionHash;
  return typeof pid === "string" && pid.trim() && !isMessageId(pid) ? pid.trim() : undefined;
}

// Per-session memory, bounded so a long-lived server with many sessions stays
// flat (oldest entries evicted first — Map iterates in insertion order).
const SESSION_MEMORY_MAX = 512;
function remember<V>(map: Map<string, V>, key: string, value: V): void {
  map.delete(key);
  map.set(key, value);
  while (map.size > SESSION_MEMORY_MAX) {
    const oldest = map.keys().next().value;
    if (oldest === undefined) break;
    map.delete(oldest);
  }
}
const childParents = new Map<string, string>();
const sessionTitles = new Map<string, string>();
const sessionDirectories = new Map<string, string>();
// Last lifecycle signal forwarded per session: dedupes the idle pair
// (session.status idle + session.idle) and arms the busy-after-idle guard.
const lastSignal = new Map<string, { signal: "busy" | "idle"; at: number }>();
// OpenCode's busy/retry session.status is a weak signal: subagent/event-burst
// noise right after an idle must not announce a new one. It maps to TurnStart,
// never UserPromptSubmit — a status poll is not a prompt submit, and
// `chat.message` is the only submission signal this harness has (legacy
// b2v-hooks.ts BUSY_IDLE_REQUEST_GUARD_MS).
const BUSY_AFTER_IDLE_GUARD_MS = 3000;

// OpenCode seeds every session with `New session - <ISO timestamp>`; storing
// it would clobber a real title (legacy OPENCODE_PLACEHOLDER_TITLE).
const PLACEHOLDER_TITLE = /^New session - \d{4}-\d{2}-\d{2}T/;

/** Record parent/title from a session.created/updated payload. True when the title changed. */
function trackSession(props: unknown): boolean {
  const sid = getSessionId(props);
  if (!sid) return false;
  const root = fields(props);
  const directory = root.directory ?? fields(root.info).directory ?? fields(root.session).directory;
  if (typeof directory === "string" && directory.trim()) {
    remember(sessionDirectories, sid, directory.trim());
  }
  const parent = getParentId(props);
  if (parent && parent !== sid) remember(childParents, sid, parent);
  const title = fields(fields(props).info).title;
  if (typeof title !== "string" || !title.trim() || PLACEHOLDER_TITLE.test(title)) return false;
  if (sessionTitles.get(sid) === title.trim()) return false;
  remember(sessionTitles, sid, title.trim());
  return true;
}

function forgetSession(sid: string): void {
  childParents.delete(sid);
  sessionTitles.delete(sid);
  sessionDirectories.delete(sid);
  lastSignal.delete(sid);
}

// session.status payloads spell the status as a string or an object with a
// type/name under several keys (legacy b2v-hooks.ts getSessionStatusType).
function sessionStatusType(props: unknown): string {
  const p = fields(props);
  const info = fields(p.info);
  const candidates = [p.status, fields(p.session).status, info.status];
  for (const c of candidates) {
    if (typeof c === "string" && c.trim()) return c.trim().toLowerCase();
    const o = fields(c);
    const t = o.type ?? o.name ?? o.id;
    if (typeof t === "string" && t.trim()) return t.trim().toLowerCase();
  }
  return "";
}

function withSessionId(payload: any): any {
  const sid = getSessionId(payload);
  const out: Fields = { ...(payload ?? {}) };
  // Use directory information tied to this event/session only. A plugin
  // instance's directory is server/location scoped and can belong to a
  // different session when the server hosts several projects.
  if (out.cwd == null) {
    const directory = out.directory ?? fields(out.info).directory ?? fields(out.session).directory;
    if (typeof directory === "string" && directory.trim()) out.cwd = directory;
  }
  if (out.cwd == null && sid) {
    const directory = sessionDirectories.get(sid);
    if (directory) out.cwd = directory;
  }
  if (
    sid &&
    payload?.sessionID !== sid &&
    payload?.sessionId !== sid &&
    payload?.session_id !== sid
  ) {
    out.session_id = sid;
  }
  if (out.parent_session_id == null) {
    const pid = getParentId(payload) ?? (sid ? childParents.get(sid) : undefined);
    if (pid) out.parent_session_id = pid;
  }
  if (out.session_title == null && sid) {
    const title = sessionTitles.get(sid);
    if (title) out.session_title = title;
  }
  return out;
}

// Fire-and-forget forward for observe hooks. Never rejects.
// Observe events go out one at a time: each is its own process, so two
// emitted milliseconds apart (busy then idle) could otherwise reach the daemon
// swapped and leave the session looking busy. Never awaited by callers; a
// hung child releases the chain after EMIT_ORDER_TIMEOUT_MS.
const EMIT_ORDER_TIMEOUT_MS = 2_000;
let emitChain: Promise<void> = Promise.resolve();

function emit(agentEvent: string, payload: any): void {
  if (hooksDisabled()) return;
  const input = JSON.stringify(withSessionId(payload));
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

// Awaited forward for gate hooks. Resolves the daemon decision JSON;
// falls back to {} (no verdict) on any failure, so opencode keeps its own
// handling — never blocked by hook infra, never silently approved by it.
function query(agentEvent: string, payload: any, timeoutMs: number = GATE_TIMEOUT_MS): Promise<any> {
  return new Promise((resolve) => {
    if (hooksDisabled()) {
      resolve({});
      return;
    }
    let child;
    try {
      child = spawn(BINARY, hookArgs(agentEvent), {
        stdio: ["pipe", "pipe", "ignore"],
      });
    } catch {
      resolve({});
      return;
    }
    let out = "";
    const done = (value: any) => {
      clearTimeout(timer);
      resolve(value);
    };
    const timer = setTimeout(() => {
      try {
        child.kill();
      } catch {
        // ignore
      }
      done({});
    }, timeoutMs);
    child.on("error", () => done({}));
    child.stdout?.on("data", (d: any) => {
      out += d.toString();
    });
    child.on("close", () => {
      try {
        done(JSON.parse(out));
      } catch {
        done({});
      }
    });
    try {
      child.stdin.end(JSON.stringify(withSessionId(payload)));
    } catch {
      done({});
    }
  });
}

function denyReason(decision: any): string | null {
  if (!decision || typeof decision !== "object") return null;
  if (decision.decision === "deny") {
    const reason = decision.reason;
    return typeof reason === "string" && reason.trim()
      ? reason.trim()
      : "denied by agent-hooks";
  }
  return null;
}

// Answering is a CALLBACK, not a return value: a verdict that only travelled
// back as this hook's result would be dropped on the floor.
//
// Delivery order mirrors b2v-hooks.ts (the proven reference):
//   1. the provided `client`'s own `permission`/`question` resources, when
//      present — same transport opencode itself uses, no URL needed;
//   2. an SDK client built on the provided `_client` transport, then a fresh
//      SDK client on `serverUrl` (root, then v2);
//   3. direct HTTP through the provided transport's fetch as a last resort.
// Raw `fetch` to `serverUrl` is deliberately NOT on the list: the plugin
// host serves no HTTP there (connection refused even from inside the
// plugin), so that path can only silently drop the verdict.
//
// Every failure path stays silent and non-fatal: a dropped answer must
// degrade to opencode's own prompt, never to a thrown error that blocks
// the agent.
let promptClient: any = null;
let promptDirectory: string | undefined;
let promptServerUrl: string | undefined;
let sdkClientPromise: Promise<any> | null = null;

function clientCapabilities(c: any): { permissionReply: boolean; questionReply: boolean; questionReject: boolean } {
  return {
    permissionReply: typeof c?.permission?.reply === "function",
    questionReply: typeof c?.question?.reply === "function",
    questionReject: typeof c?.question?.reject === "function",
  };
}

function providedFetch(): ((req: Request) => Promise<Response>) | null {
  try {
    const fn = promptClient?._client?.getConfig?.()?.fetch;
    if (typeof fn === "function") return (req: Request) => fn(req);
  } catch {
    // ignore — no usable transport
  }
  return null;
}

async function importSdk(spec: string): Promise<any | null> {
  try {
    return await import(/* @vite-ignore */ spec);
  } catch (err) {
    debug(`import ${spec} failed: ${err}`);
    return null;
  }
}

/// Build (once) an SDK client that answers prompt replies: adapt the provided
/// `_client` transport under a v2 `OpencodeClient` first, else a fresh root
/// client then a v2 client on `serverUrl`. Null when nothing is available.
function sdkPromptClient(): Promise<any | null> {
  if (!sdkClientPromise) {
    sdkClientPromise = (async () => {
      const transport = promptClient?._client ?? null;
      if (transport && typeof transport === "object") {
        const v2 = await importSdk("@opencode-ai/sdk/v2/client");
        if (typeof v2?.OpencodeClient === "function") {
          try {
            const adapted = new v2.OpencodeClient({ client: transport });
            const caps = clientCapabilities(adapted);
            debug(`adapted provided transport: permission_reply=${caps.permissionReply} question_reply=${caps.questionReply} question_reject=${caps.questionReject}`);
            if (caps.permissionReply || caps.questionReply || caps.questionReject) return adapted;
          } catch (err) {
            debug(`adapt provided transport failed: ${err}`);
          }
        }
      }
      if (!promptServerUrl) return null;
      const dir = promptDirectory ?? process.cwd();
      const root = await importSdk("@opencode-ai/sdk");
      if (typeof root?.createOpencodeClient === "function") {
        try {
          const c = root.createOpencodeClient({ baseUrl: promptServerUrl, directory: dir });
          const caps = clientCapabilities(c);
          debug(`root SDK client: permission_reply=${caps.permissionReply} question_reply=${caps.questionReply} question_reject=${caps.questionReject}`);
          if (caps.permissionReply || caps.questionReply || caps.questionReject) return c;
        } catch (err) {
          debug(`root SDK client failed: ${err}`);
        }
      }
      const v2mod = await importSdk("@opencode-ai/sdk/v2/client");
      if (typeof v2mod?.createOpencodeClient === "function") {
        try {
          const c = v2mod.createOpencodeClient({ baseUrl: promptServerUrl, directory: dir });
          const caps = clientCapabilities(c);
          debug(`v2 SDK client: permission_reply=${caps.permissionReply} question_reply=${caps.questionReply} question_reject=${caps.questionReject}`);
          if (caps.permissionReply || caps.questionReply || caps.questionReject) return c;
        } catch (err) {
          debug(`v2 SDK client failed: ${err}`);
        }
      }
      return null;
    })();
  }
  return sdkClientPromise;
}

/// POST through the provided transport's fetch (the only HTTP channel that
/// reaches the real server). False when there is no transport.
async function postViaTransport(path: string, body?: unknown, withDirectory = true): Promise<boolean> {
  const via = providedFetch();
  if (!via || !promptServerUrl) {
    debug(`no transport for POST ${path}`);
    return false;
  }
  const dir = promptDirectory ?? process.cwd();
  const url =
    `${promptServerUrl.replace(/\/$/, "")}${path}` +
    (withDirectory ? `?directory=${encodeURIComponent(dir)}` : "");
  try {
    const res = await via(
      new Request(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
      }),
    );
    debug(`POST ${path} -> ${res.status}`);
    return res.ok;
  } catch (err) {
    debug(`POST ${path} threw: ${err}`);
    return false;
  }
}

// Answer delivery is deliberately silent on failure — a dropped answer must
// fall back to opencode's own prompt, never throw. That makes "the client was
// never called" and "the call failed" look identical from outside, so set
// AGENT_HOOKS_DEBUG=1 to tell them apart.
const DEBUG = process.env.AGENT_HOOKS_DEBUG === "1";
function debug(msg: string): void {
  if (DEBUG) console.error(`[agent-hooks] ${msg}`);
}
// The request id opencode wants back is not reliably at the top level: it has
// appeared on the event, under `properties`, under `properties.info`, and
// nested in the `question`/`permission`/`request` objects depending on
// version. Search all of them — a miss here is indistinguishable from "no
// answer given", because both end as a silent return.
function findRequestId(event: any, props: any): string | undefined {
  const obj = (v: unknown): any =>
    typeof v === "object" && v !== null ? v : {};
  const info = obj(props?.info);
  const request = obj(event?.request ?? props?.request);
  const permission = obj(event?.permission ?? props?.permission);
  const question = obj(event?.question ?? props?.question);
  for (const candidate of [
    event?.requestID, event?.requestId, event?.request_id,
    props?.requestID, props?.requestId, props?.request_id, props?.id,
    info.requestID, info.requestId, info.request_id, info.id,
    request.requestID, request.requestId, request.request_id, request.id,
    permission.id,
    question.id,
    event?.id,
  ]) {
    if (typeof candidate === "string" && candidate.trim()) return candidate.trim();
  }
  return undefined;
}

function requestIds(event: any, props: any): Record<string, string> {
  const id = findRequestId(event, props);
  if (!id) return {};
  // The generated SDK spells the key `requestID` (capital ID) on every
  // question/permission method; send it and the common variants rather than
  // guess which one this build reads.
  return { id, requestID: id, request_id: id, requestId: id };
}

/// Reply through one SDK-style client object. True when the call resolves
/// without throwing — the call itself is the delivery confirmation.
async function replyViaClient(c: any, kind: "permission" | "question", action: "reply" | "reject", ids: Record<string, string>, body: Record<string, unknown>, label: string): Promise<boolean> {
  const fn = c?.[kind]?.[action];
  if (typeof fn !== "function") return false;
  try {
    await fn.call(c[kind], { ...ids, ...body, directory: promptDirectory ?? process.cwd() });
    debug(`${kind}.${action} delivered (${label})`);
    return true;
  } catch (err) {
    debug(`${kind}.${action} threw (${label}): ${err}`);
    return false;
  }
}

/// Persistent grants and rejections that the in-band status cannot express.
/// `reply` is opencode's vocabulary: "once" | "always" | "reject".
async function deliverPermissionReply(
  event: any,
  props: any,
  reply: "once" | "always" | "reject",
): Promise<void> {
  const ids = requestIds(event, props);
  if (!ids.id) {
    debug(`permission reply: no request id (reply=${reply})`);
    return;
  }
  try {
    debug(`permission reply ${reply} id=${ids.id}`);
    if (await replyViaClient(promptClient, "permission", "reply", ids, { reply }, "provided-client")) return;
    const sdk = await sdkPromptClient();
    if (sdk && (await replyViaClient(sdk, "permission", "reply", ids, { reply }, "sdk-client"))) return;
    // The in-band hook does not say which generation asked, so try the flat
    // route and fall back to the session-scoped one.
    let ok = await postViaTransport(
      `/permission/${encodeURIComponent(ids.id)}/reply`,
      { reply },
    );
    const sid = props?.sessionID ?? event?.sessionID;
    if (!ok && sid) {
      ok = await postViaTransport(
        `/api/session/${encodeURIComponent(sid)}/permission/${encodeURIComponent(ids.id)}/reply`,
        { reply },
        false,
      );
    }
    debug(ok ? "permission.reply delivered (transport)" : "permission.reply failed");
  } catch (err) {
    debug(`permission reply threw: ${err}`);
  }
}

/// Transport route for a prompt reply: flat `/question/<id>/reply` (legacy
/// b2v-hooks.ts) for V1, the session-scoped route for V2 (needs sessionID,
/// same shape deliverPermissionReply falls back to).
function replyPath(
  kind: "question" | "permission",
  action: "reply" | "reject",
  v2: boolean,
  sessionID: unknown,
  id: string,
): string | undefined {
  if (!v2) return `/${kind}/${encodeURIComponent(id)}/${action}`;
  if (typeof sessionID !== "string" || !sessionID) return undefined;
  return `/api/session/${encodeURIComponent(sessionID)}/${kind}/${encodeURIComponent(id)}/${action}`;
}

/// The V2 service advertises its URL and basic-auth password here; the V2
/// plugin context has no form API, so this HTTP route is the only way to
/// answer a form from the plugin.
function v2Service(): { url: string; password: string } | null {
  const stateHome = process.env.XDG_STATE_HOME || join(homedir(), ".local", "state");
  try {
    const info = fields(JSON.parse(readFileSync(join(stateHome, "opencode", "service.json"), "utf8")));
    const url = str(info.url);
    const password = typeof info.password === "string" ? info.password : "";
    return url ? { url, password } : null;
  } catch (err) {
    debug(`v2 service.json unreadable: ${err}`);
    return null;
  }
}

async function v2ServiceRequest(method: "POST" | "DELETE", path: string, body?: unknown): Promise<boolean> {
  const service = v2Service();
  if (!service) return false;
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (service.password) {
    headers.Authorization = `Basic ${Buffer.from(`opencode:${service.password}`).toString("base64")}`;
  }
  try {
    const res = await fetch(`${service.url.replace(/\/$/, "")}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    debug(`${method} ${path} -> ${res.status}`);
    return res.ok;
  } catch (err) {
    debug(`${method} ${path} threw: ${err}`);
    return false;
  }
}

/// V1 answer groups (labels, index-aligned with the questions) -> the V2 form
/// answer `{ [field.key]: value }`, mapping picked labels back to option values.
function formAnswer(formFields: Fields[], groups: unknown[]): Fields {
  const answer: Fields = {};
  formFields.forEach((field, index) => {
    const key = str(field.key);
    const group = groups[index];
    if (!key || !Array.isArray(group)) return;
    const options = Array.isArray(field.options) ? field.options.map((o: unknown) => fields(o)) : [];
    const values = group
      .filter((v): v is string => typeof v === "string" && v.trim() !== "")
      .map((picked) => {
        const option = options.find((o) => o.label === picked || o.value === picked);
        return option && typeof option.value === "string" ? option.value : picked;
      });
    if (values.length === 0) return;
    answer[key] = field.type === "multiselect" ? values : values[0];
  });
  return answer;
}

async function deliverFormReply(sessionID: string, formID: string, formFields: Fields[], decision: any): Promise<void> {
  const base = `/api/session/${encodeURIComponent(sessionID)}/form/${encodeURIComponent(formID)}`;
  const groups = Array.isArray(decision?.answers) ? decision.answers : [];
  const answer = formAnswer(formFields, groups);
  if (Object.keys(answer).length > 0) {
    debug(`form reply ${JSON.stringify(answer)} id=${formID}`);
    await v2ServiceRequest("POST", `${base}/reply`, { answer });
    return;
  }
  if (decision?.reply === "deny" || decision?.decision === "deny") {
    await v2ServiceRequest("DELETE", base);
  }
  // No answer and no rejection: leave it for opencode's own prompt.
}

async function deliverQuestionReply(
  event: any,
  props: any,
  decision: any,
  v2: boolean,
): Promise<void> {
  const ids = requestIds(event, props);
  if (!ids.id) {
    debug(
      `question.asked: no request id; event keys=${Object.keys(event ?? {}).join(",")} ` +
        `props keys=${Object.keys(props ?? {}).join(",")}`,
    );
    return;
  }
  const formFields = props?.formFields;
  if (Array.isArray(formFields) && typeof props?.sessionID === "string") {
    await deliverFormReply(props.sessionID, ids.id, formFields.map((f: unknown) => fields(f)), decision);
    return;
  }
  const answers = decision?.answers;
  const answered =
    Array.isArray(answers) &&
    answers.length > 0 &&
    answers.every((group: unknown) => Array.isArray(group));

  try {
    if (answered) {
      debug(`question reply ${JSON.stringify(answers)} id=${ids.id}`);
      if (await replyViaClient(promptClient, "question", "reply", ids, { answers }, "provided-client")) return;
      const sdk = await sdkPromptClient();
      if (sdk && (await replyViaClient(sdk, "question", "reply", ids, { answers }, "sdk-client"))) return;
      const path = replyPath("question", "reply", v2, props?.sessionID, ids.id);
      if (!path) {
        debug("question reply: v2 event without sessionID");
        return;
      }
      const ok = await postViaTransport(path, { answers }, !v2);
      debug(ok ? "question.reply delivered (transport)" : "question.reply failed");
      return;
    }
    debug(`no answers on verdict: ${JSON.stringify(decision)}`);
    if (decision?.reply === "deny" || decision?.decision === "deny") {
      if (await replyViaClient(promptClient, "question", "reject", ids, {}, "provided-client")) return;
      const sdk = await sdkPromptClient();
      if (sdk && (await replyViaClient(sdk, "question", "reject", ids, {}, "sdk-client"))) return;
      const path = replyPath("question", "reject", v2, props?.sessionID, ids.id);
      if (!path) {
        debug("question reject: v2 event without sessionID");
        return;
      }
      const ok = await postViaTransport(path, undefined, !v2);
      debug(ok ? "question.reject delivered (transport)" : "question.reject failed");
    }
    // No answer and no rejection: leave it for opencode's own prompt.
  } catch (err) {
    debug(`question reply threw: ${err}`);
  }
}

export async function AgentHooksPlugin(plugin: any = {}) {
  promptClient = plugin?.client ?? null;
  promptDirectory =
    typeof plugin?.directory === "string" ? plugin.directory : undefined;
  promptServerUrl = plugin?.serverUrl ? String(plugin.serverUrl) : undefined;
  sdkClientPromise = null;
  debug(
    `plugin init: client=${plugin?.client ? "present" : "null"} ` +
      `question=${plugin?.client?.question ? "yes" : "no"} serverUrl=${promptServerUrl ?? "none"}`,
  );
  return {
    "tool.execute.before": async (input: any, output: any) => {
      const decision = await query("tool.execute.before", { ...(input ?? {}), args: output?.args });
      const reason = denyReason(decision);
      if (reason) throw new Error(reason);
    },
    "tool.execute.after": async (input: any, output: any) => {
      emit("tool.execute.after", { ...(input ?? {}), output: output?.output, title: output?.title });
    },
    "permission.ask": async (input: any, output: any) => {
      const decision = await query("permission.ask", input ?? {});
      // opencode's status is "ask" | "deny" | "allow". Escalation is its own
      // outcome here: collapsing ask into allow silently approves the very
      // call a decider wanted a human to confirm.
      // No verdict ({} = deferred, or hook infra failed) leaves opencode's
      // own status untouched so its native permission prompt still runs.
      const d = decision?.decision;
      if (d === "deny" || d === "ask" || d === "allow") output.status = d;

      // "Approve every later call like this" has no in-band spelling — the
      // status field only covers this one request — so a persistent grant
      // has to go back through the SDK. Best effort: if it does not land the
      // call is still allowed, just not remembered.
      if (decision?.reply === "allow_always") {
        await deliverPermissionReply(input, input, "always");
      }
      debug(`permission.ask -> ${output.status} (reply=${decision?.reply})`);
    },
    "chat.message": async (input: unknown, output: unknown) => {
      const emitted = fields(output);
      const decision = await query("chat.message", {
        ...fields(input),
        message: emitted.message ?? fields(input).message,
        parts: emitted.parts ?? fields(input).parts,
      });
      const reason = denyReason(decision);
      if (reason) throw new Error(reason);
    },
    event: async ({ event }: any) => {
      const name =
        event?.type ?? event?.event ?? event?.name ?? event?.hook ?? null;
      if (typeof name !== "string") return;
      const props = event?.properties ?? event?.payload ?? event ?? {};

      // Questions are opencode's own channel, not a tool call: they arrive as
      // `question.asked` and are answered out of band through the SDK. The
      // hook return value cannot carry the answer, so we hold for the verdict
      // and then deliver it ourselves.
      if (name === "question.asked" || name === "question.v2.asked") {
        const v2 = name === "question.v2.asked";
        debug(`${name} received`);
        // Not awaited: holding for a human would stall every later event on
        // this stream (busy/idle, other sessions) until the answer lands.
        void query("question.asked", props, QUESTION_TIMEOUT_MS)
          .then((decision) => deliverQuestionReply(event, props, decision, v2))
          .catch((err) => debug(`question handling failed: ${err}`));
        return;
      }

      forwardSessionEvent(name, props);
    },
  };
}

// Session lifecycle forwarding for the generic `event` hook. Returns the
// agent-hooks event names emitted (for tests); never throws.
function forwardSessionEvent(name: string, props: any, now: number = Date.now()): string[] {
  const sent: string[] = [];
  const send = (event: string, payload: any) => {
    emit(event, payload);
    sent.push(event);
  };
  const sid = getSessionId(props);

  if (name === "session.created" || name === "session.updated") {
    // A title change on an already-announced session is pushed on its own:
    // session.updated fires ~30x per session and is otherwise dropped.
    const known = sid !== undefined && lastSignal.has(sid);
    const titleChanged = trackSession(props);
    if (name === "session.updated") {
      if (titleChanged && known) send("session.title", { sessionID: sid });
      return sent;
    }
  }

  let event = name;
  if (name === "session.status") {
    const status = sessionStatusType(props);
    if (status === "busy" || status === "retry") event = "session.busy";
    else if (status === "idle") event = "session.idle";
    else return sent; // unknown status payloads are a no-op
  } else if (!SESSION_EVENT_MAP[name]) {
    return sent;
  }

  if (sid) {
    const last = lastSignal.get(sid);
    if (event === "session.busy") {
      if (last?.signal === "idle" && now - last.at < BUSY_AFTER_IDLE_GUARD_MS) return sent;
      remember(lastSignal, sid, { signal: "busy", at: now });
    } else if (event === "session.idle") {
      // session.status idle and session.idle both announce the same idle.
      if (last?.signal === "idle") return sent;
      remember(lastSignal, sid, { signal: "idle", at: now });
    } else if (event === "session.created" && !last) {
      remember(lastSignal, sid, { signal: "busy", at: 0 });
    }
  }
  send(event, props);
  if (sid && name === "session.deleted") forgetSession(sid);
  return sent;
}

// ---- V2 host support (legacy b2v-hooks.ts mapV2EventToV1 / setupBack2VibingV2) ----

function str(...values: unknown[]): string {
  for (const v of values) if (typeof v === "string" && v.trim()) return v.trim();
  return "";
}

/** V2 hosts put the payload under `data`; V1 used `properties`. */
function v2Payload(event: Fields): Fields {
  return fields(event.data ?? event.properties);
}

function formFieldToQuestion(field: Fields): Fields | undefined {
  const title = str(field.title, field.key);
  if (!title) return undefined;
  const options = Array.isArray(field.options)
    ? field.options.map((o: unknown) => fields(o)).map((o) => ({ label: str(o.label), description: str(o.description) }))
    : [];
  return { question: title, header: title.slice(0, 30), options, ...(field.type === "multiselect" ? { multiple: true } : {}) };
}

/**
 * Map one V2 server event onto zero or more V1-shaped events. V2 dropped
 * busy/idle `session.status` / `session.idle` for `session.execution.*`, and
 * replaced `question.*` with `form.*`.
 * Reference: https://opencode.ai/v2/docs/build/plugins/migrate-v1
 */
function mapV2EventToV1(event: Fields): Fields[] {
  const type = typeof event.type === "string" ? event.type : "";
  const props = v2Payload(event);
  const sessionID = str(props.sessionID, props.session_id, props.sessionId);
  switch (type) {
    case "session.execution.started":
      return sessionID ? [{ type: "session.status", properties: { sessionID, status: { type: "busy" } } }] : [];
    case "session.execution.succeeded":
    case "session.execution.failed":
    case "session.execution.interrupted":
      return sessionID
        ? [
            { type: "session.status", properties: { sessionID, status: { type: "idle" } } },
            { type: "session.idle", properties: { sessionID } },
          ]
        : [];
    case "form.created": {
      const form = fields(props.form);
      const id = str(form.id);
      const sid = str(form.sessionID, form.session_id, form.sessionId);
      if (!id || !sid || sid === "global") return [];
      const formFields = Array.isArray(form.fields) ? form.fields.map((f: unknown) => fields(f)) : [];
      const asked = formFields
        .map((field) => ({ field, question: formFieldToQuestion(field) }))
        .filter((q): q is { field: Fields; question: Fields } => q.question !== undefined);
      // `formFields` rides along so the reply can be keyed back onto the form:
      // V2 answers `{ [field.key]: value }`, not V1's positional string[][].
      return [
        {
          type: "question.asked",
          properties: {
            id,
            sessionID: sid,
            questions: asked.map((q) => q.question),
            formFields: asked.map((q) => q.field),
          },
        },
      ];
    }
    case "session.created":
    case "session.deleted": {
      if (!sessionID) break;
      const info: Fields = { id: sessionID, ...fields(props.info) };
      const parentID = str(props.parentID, props.parentId, props.parent_id, info.parentID);
      if (parentID) info.parentID = parentID;
      if (typeof props.title === "string") info.title = props.title;
      return [{ type, properties: { ...props, sessionID, info } }];
    }
  }
  const data = fields(event.data);
  return [event.data === undefined ? event : { ...event, properties: { ...data, ...fields(event.properties) } }];
}

type V2PromptEvent = {
  sessionID?: string;
  prompt?: { text?: string };
};
type V2Context = {
  location?: { directory?: string };
  session?: {
    hook?: (
      name: "prompt",
      handler: (event: V2PromptEvent) => void,
    ) => Promise<{ dispose?: () => Promise<void> }>;
    context?: (input: { sessionID: string }) => Promise<unknown>;
  };
  event?: { subscribe?: (options?: { signal?: AbortSignal }) => AsyncIterable<unknown> };
};

function textParts(value: unknown): string | undefined {
  const root = fields(value);
  const parts = Array.isArray(value) ? value : Array.isArray(root.content) ? root.content : [];
  const texts: string[] = [];
  for (const part of parts) {
    const block = fields(part);
    if (block.type !== "text" && block.type !== "output_text") continue;
    if (typeof block.text === "string" && block.text.trim()) texts.push(block.text.trim());
  }
  return texts.length ? texts.join("\n") : undefined;
}

function latestAssistantText(value: unknown): string | undefined {
  if (!Array.isArray(value)) return undefined;
  for (let index = value.length - 1; index >= 0; index -= 1) {
    const row = fields(value[index]);
    const message = fields(row.info ?? row);
    if (message.type !== "assistant") continue;
    const text = textParts(message.content ?? row.parts);
    if (text) return text;
  }
  return undefined;
}
// Process-wide (globalThis) so every location instance and every hot-reloaded
// copy of this module share it. Bounded; events without an id pass through.
const SEEN_EVENT_IDS = Symbol.for("agent-hooks.opencode.seen-event-ids");
const SEEN_EVENT_IDS_MAX = 2048;
function firstSighting(id: unknown): boolean {
  if (typeof id !== "string" || !id) return true;
  const g = globalThis as { [SEEN_EVENT_IDS]?: Set<string> };
  const seen = (g[SEEN_EVENT_IDS] ??= new Set<string>());
  if (seen.has(id)) return false;
  seen.add(id);
  if (seen.size > SEEN_EVENT_IDS_MAX) seen.delete(seen.values().next().value as string);
  return true;
}

function ownsV2Event(event: Fields, dir: unknown): boolean {
  const eventDir = fields(event.location).directory;
  if (typeof eventDir === "string" && eventDir && typeof dir === "string" && dir) {
    return eventDir === dir;
  }
  return firstSighting(event.id);
}

/**
 * V2 `setup` entry. Registration-only / embedded-v2-on-v1 hosts omit
 * `event.subscribe`; skip so the V1 export owns hooks and nothing double-fires.
 */
async function setupV2(ctx: V2Context): Promise<() => void> {
  const dir = ctx.location?.directory;
  const hooks = await AgentHooksPlugin({ directory: typeof dir === "string" && dir.trim() ? dir : process.cwd() });
  const promptRegistration =
    typeof ctx.session?.hook === "function"
      ? await ctx.session.hook("prompt", (event) => {
          const sessionID = str(event.sessionID);
          const text = event.prompt?.text;
          if (!sessionID || typeof text !== "string" || !text.trim()) return;
          if (typeof dir === "string" && dir.trim()) remember(sessionDirectories, sessionID, dir.trim());
          emit("chat.message", { sessionID, message: text, parts: [{ type: "text", text }] });
        })
      : undefined;
  const controller = new AbortController();
  const subscribe = ctx?.event?.subscribe;
  if (typeof subscribe !== "function") {
    return () => {
      controller.abort();
      void promptRegistration?.dispose?.();
    };
  }
  void (async () => {
    try {
      for await (const raw of subscribe.call(ctx.event, { signal: controller.signal })) {
        if (typeof raw !== "object" || raw === null) continue;
        // The service loads one instance per location but streams every
        // location's events to each. A located event belongs to the instance
        // loaded there (its directory and reply context); unlocated ones
        // (session.execution.*) forward once, by the event `id` every
        // instance shares.
        if (!ownsV2Event(fields(raw), dir)) continue;
        const rawEvent = fields(raw);
        const payload = v2Payload(rawEvent);
        const sid = getSessionId(payload);
        const eventDirectory = fields(rawEvent.location).directory;
        if (sid && typeof eventDirectory === "string" && eventDirectory.trim()) {
          remember(sessionDirectories, sid, eventDirectory.trim());
        }
        for (const mapped of mapV2EventToV1(raw as Fields)) {
          try {
            const mappedProps = fields(mapped.properties);
            const mappedSid = getSessionId(mappedProps);
            let forwarded = mapped;
            const idleEvent =
              mapped.type === "session.status" && sessionStatusType(mappedProps) === "idle";
            if (
              idleEvent &&
              rawEvent.type === "session.execution.succeeded" &&
              mappedSid &&
              ctx.session?.context
            ) {
              try {
                const summary = latestAssistantText(
                  await ctx.session.context({ sessionID: mappedSid }),
                );
                if (summary) {
                  forwarded = { ...mapped, properties: { ...mappedProps, summary } };
                }
              } catch (err) {
                debug(`session context lookup failed: ${err}`);
              }
            }
            const directory =
              (typeof eventDirectory === "string" && eventDirectory.trim()
                ? eventDirectory.trim()
                : undefined) ?? (mappedSid ? sessionDirectories.get(mappedSid) : undefined);
            await hooks.event({
              event:
                directory && mappedProps.directory == null && mappedProps.cwd == null
                  ? { ...forwarded, properties: { ...fields(forwarded.properties), directory } }
                  : forwarded,
            });
          } catch (err) {
            debug(`v2 event handler failed: ${err}`);
          }
        }
      }
    } catch (err) {
      if (!controller.signal.aborted) debug(`v2 event stream ended: ${err}`);
    }
  })();
  return () => {
    controller.abort();
    void promptRegistration?.dispose?.();
  };
}

// Dual V1+V2 export (legacy b2v-hooks.ts): V2 reads `id` + `setup()`; V1 uses
// the named `AgentHooksPlugin`. No `server()` here so hosts that walk named
// exports and call default.server do not double-register.
export default {
  id: "agent-hooks",
  setup: setupV2,
};

// V1 hosts call every exported *function* as a plugin (with the plugin input),
// so helpers must not be function exports. Tests reach them through this
// object, which V1 hosts skip like the `default` object.
export const __test = {
  // Resolves once every queued observe emit has finished.
  drain: () => emitChain,
  hookArgs,
  hooksDisabled,
  getSessionId,
  getParentId,
  trackSession,
  sessionStatusType,
  withSessionId,
  formAnswer,
  forwardSessionEvent,
  mapV2EventToV1,
};
