// managed by agent-hooks
// agent-hooks opencode plugin — managed by agent-hooks.
// Staged via the opencode builtin manifest (stage_dir ".opencode/plugins"
// as agent-hooks.ts). Do not hand-edit: reinstall with
// `agent-hooks install --harness opencode`.
// Remove with: `agent-hooks uninstall --harness opencode`.
//
// Design: observability + gate enforcement. Every opencode hook is forwarded
// to the `agent-hooks` binary (`agent-hooks hook opencode --event <Event>`)
// over stdin JSON. The binary runs daemonless local hooks, then forwards to
// the daemon (fast-allow on observe events, long wait on gates for a decider
// verdict) and prints the merged decision; deny (including ask fallback)
// throws to block, so fail-open allow keeps the agent unblocked on infra
// failure.
//
// Event mapping (opencode -> agent-hooks AgentLifecycle):
//   tool.execute.before  -> PreToolUse (gate: throw to deny)
//   tool.execute.after   -> PostToolUse (observe, fire-and-forget)
//   permission.ask       -> PermissionRequest (gate: output.status allow|deny|ask)
//   question.asked       -> PermissionRequest/Question (answered out of band
//                           via client.question.reply — see deliverQuestionReply)
//   chat.message         -> UserPromptSubmit (gate: throw to deny)
//   session.created      -> SessionStart
//   session.updated      -> UserPromptSubmit (observe)
//   session.deleted      -> SessionEnd
//   session.idle         -> Stop (observe)
// NOTE: session events arrive via the generic "event" hook; only known
// session.* names are forwarded, everything else is ignored.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

function resolveBinary(): string {
  const override = process.env.AGENT_HOOKS_BIN?.trim();
  if (override) return override;
  for (const candidate of [
    join(homedir(), ".cargo", "bin", "agent-hooks"),
    "/usr/local/bin/agent-hooks",
    "agent-hooks",
  ]) {
    if (candidate === "agent-hooks") return candidate;
    try {
      if (existsSync(candidate)) return candidate;
    } catch {
      // ignore, try next
    }
  }
  return "agent-hooks";
}

const BINARY = resolveBinary();

// Safety bound for gate hooks (tool.execute.before awaits the daemon
// verdict). Must out-wait the daemon's 300s prompt timeout so a held verdict
// (allow/deny/ask→deny) arrives instead of failing open here; the fast path
// resolves in milliseconds, so this only binds a hung daemon.
const GATE_TIMEOUT_MS = 310_000;

// opencode session event name -> agent event flag.
const SESSION_EVENT_MAP: Record<string, string> = {
  "session.created": "SessionStart",
  "session.updated": "UserPromptSubmit",
  "session.deleted": "SessionEnd",
  "session.idle": "Stop",
};

function getSessionId(payload: any): string | undefined {
  const sid =
    payload?.sessionID ??
    payload?.sessionId ??
    payload?.session_id ??
    payload?.session?.id ??
    payload?.id;
  return typeof sid === "string" && sid.trim() ? sid.trim() : undefined;
}

// Parent passthrough (field copy only — no lookup, no I/O). When the harness
// names the parent under a variant key, stamp the stable `parent_session_id`
// so the manifest extracts one key.
type Fields = { [key: string]: unknown };
function fields(value: unknown): Fields {
  return typeof value === "object" && value !== null ? (value as Fields) : {};
}
function getParentId(payload: unknown): string | undefined {
  const root = fields(payload);
  const nested = fields(root.session ?? fields(root.properties).session);
  const pid =
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
  return typeof pid === "string" && pid.trim() ? pid.trim() : undefined;
}
function withSessionId(payload: any): any {
  const sid = getSessionId(payload);
  const out: Fields = { ...(payload ?? {}) };
  if (
    sid &&
    payload?.sessionID !== sid &&
    payload?.sessionId !== sid &&
    payload?.session_id !== sid
  ) {
    out.session_id = sid;
  }
  if (out.parent_session_id == null) {
    const pid = getParentId(payload);
    if (pid) out.parent_session_id = pid;
  }
  return out;
}

// Fire-and-forget forward for observe hooks. Never rejects.
function emit(agentEvent: string, payload: any): void {
  let child;
  try {
    child = spawn(BINARY, ["hook", "opencode", "--event", agentEvent], {
      stdio: ["pipe", "ignore", "ignore"],
    });
  } catch {
    return; // binary missing — never block the agent
  }
  child.on("error", () => {});
  try {
    child.stdin.end(JSON.stringify(withSessionId(payload)));
  } catch {
    // ignore
  }
}

// Awaited forward for gate hooks. Resolves the daemon decision JSON;
// falls back to {"decision":"allow"} on any failure so the agent is
// never blocked by hook infra.
function query(agentEvent: string, payload: any): Promise<any> {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(BINARY, ["hook", "opencode", "--event", agentEvent], {
        stdio: ["pipe", "pipe", "ignore"],
      });
    } catch {
      resolve({ decision: "allow" });
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
      done({ decision: "allow" });
    }, GATE_TIMEOUT_MS);
    child.on("error", () => done({ decision: "allow" }));
    child.stdout?.on("data", (d: any) => {
      out += d.toString();
    });
    child.on("close", () => {
      try {
        done(JSON.parse(out));
      } catch {
        done({ decision: "allow" });
      }
    });
    try {
      child.stdin.end(JSON.stringify(withSessionId(payload)));
    } catch {
      done({ decision: "allow" });
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

export default async function agentHooks(plugin: any = {}) {
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
      const d = decision?.decision;
      output.status = d === "deny" ? "deny" : d === "ask" ? "ask" : "allow";

      // "Approve every later call like this" has no in-band spelling — the
      // status field only covers this one request — so a persistent grant
      // has to go back through the SDK. Best effort: if it does not land the
      // call is still allowed, just not remembered.
      if (decision?.reply === "allow_always") {
        await deliverPermissionReply(input, input, "always");
      }
      debug(`permission.ask -> ${output.status} (reply=${decision?.reply})`);
    },
    "chat.message": async (input: any, _output: any) => {
      const decision = await query("chat.message", input ?? {});
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
        const decision = await query("question.asked", props);
        await deliverQuestionReply(event, props, decision, v2);
        return;
      }

      if (!SESSION_EVENT_MAP[name]) return;
      emit(name, props);
    },
  };
}
