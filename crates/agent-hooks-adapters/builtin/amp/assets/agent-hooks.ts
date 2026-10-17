// agent-hooks Amp plugin — managed by agent-hooks.
// Staged via the amp builtin manifest into ~/.config/amp/plugins/agent-hooks.ts
// (Amp auto-loads plugins from there with Bun; it has no hooks.json).
// Reference: https://ampcode.com/manual/plugin-api
// Remove with: `agent-hooks uninstall --harness amp`.
//
// Port of the legacy Back2Vibing b2v-amp.ts plugin. Every Amp event is
// forwarded to `agent-hooks hook amp --event <native event>` over stdin JSON.
//
// Event mapping (amp -> agent-hooks AgentLifecycle, see builtin/amp/adapter.json):
//   session.start -> SessionStart
//   agent.start   -> UserPromptSubmit (a prompt was submitted: busy)
//   tool.call     -> PreToolUse (gate: deny -> reject-and-continue)
//   tool.result   -> PostToolUse
//   agent.end     -> Stop (the visible turn finished; no later idle follows)

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

// Install-time settings: `agent-hooks install` replaces each quoted token with
// a JSON string literal ("" when unset). An unstaged copy still holds the raw
// token — treat as unset.
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
  const sep = process.platform === "win32" ? ";" : ":";
  for (const dir of (process.env.PATH ?? "").split(sep)) {
    const d = dir.trim();
    if (!d) continue;
    try {
      if (existsSync(join(d, "agent-hooks"))) return join(d, "agent-hooks");
    } catch {
      // ignore, try next
    }
  }
  for (const candidate of [join(homedir(), ".cargo", "bin", "agent-hooks"), "/usr/local/bin/agent-hooks"]) {
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

// Safety bound for the tool.call gate; the fast path resolves in ms.
const GATE_TIMEOUT_MS = 25_000;

export function hookArgs(event: string): string[] {
  const args = ["hook", "amp", "--event", event];
  if (SOCKET) args.push("--socket", SOCKET);
  if (CONTEXT_CMD) args.push("--context-cmd", CONTEXT_CMD);
  if (ORIGIN) args.push("--origin", ORIGIN);
  return args;
}

// Kill switch: AGENT_HOOKS_DISABLED=1 skips every spawn (the gate allows).
// B2V_DISABLED is honored too because Back2Vibing users already set it to
// silence the legacy b2v plugins this asset replaces.
export function hooksDisabled(env: NodeJS.ProcessEnv = process.env): boolean {
  const on = (v: string | undefined) => {
    const t = v?.trim().toLowerCase();
    return t === "1" || t === "true";
  };
  return on(env.AGENT_HOOKS_DISABLED) || on(env.B2V_DISABLED);
}

type Fields = { [key: string]: unknown };
function fields(value: unknown): Fields {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Fields) : {};
}
function str(...values: unknown[]): string | undefined {
  for (const v of values) if (typeof v === "string" && v.length > 0) return v;
  return undefined;
}

// Amp names the session by its thread (legacy b2v-amp.ts sessionIdFrom).
export function buildPayload(event: unknown, ctx?: unknown): Fields {
  const e = fields(event);
  const thread = fields(e.thread ?? fields(ctx).thread);
  const tool = e.tool;
  return {
    ...e,
    session_id: str(thread.id, e.session_id, e.sessionId, e.thread_id, e.threadId),
    ...(str(thread.title) ? { session_title: str(thread.title) } : {}),
    request_id: str(e.toolUseID, e.request_id, e.requestId, e.id),
    tool_name: str(e.tool_name, e.toolName, tool, fields(tool).name),
    ...(e.input !== undefined ? { tool_input: e.input } : {}),
  };
}

function emit(event: string, payload: Fields): void {
  if (hooksDisabled()) return;
  try {
    const child = spawn(BINARY, hookArgs(event), { stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", () => {});
    child.stdin?.on("error", () => {});
    child.stdin?.end(JSON.stringify(payload));
  } catch {
    // binary missing — never block the agent
  }
}

// Awaited forward for the gate. Any failure resolves {} (no verdict = allow).
function query(event: string, payload: Fields): Promise<Fields> {
  return new Promise((resolve) => {
    if (hooksDisabled()) return resolve({});
    let child;
    try {
      child = spawn(BINARY, hookArgs(event), { stdio: ["pipe", "pipe", "ignore"] });
    } catch {
      return resolve({});
    }
    let out = "";
    const timer = setTimeout(() => {
      try {
        child.kill();
      } catch {
        // ignore
      }
      resolve({});
    }, GATE_TIMEOUT_MS);
    child.on("error", () => {
      clearTimeout(timer);
      resolve({});
    });
    child.stdout?.on("data", (d: unknown) => {
      out += String(d);
    });
    child.on("close", () => {
      clearTimeout(timer);
      try {
        resolve(fields(JSON.parse(out.trim() || "{}")));
      } catch {
        resolve({});
      }
    });
    child.stdin?.on("error", () => {});
    child.stdin?.end(JSON.stringify(payload));
  });
}

// Verdict -> Amp tool.call action. Only an explicit deny rejects.
export function toolCallAction(decision: Fields): { action: "allow" } | { action: "reject-and-continue"; message: string } {
  if (decision.decision !== "deny") return { action: "allow" };
  const reason = typeof decision.reason === "string" && decision.reason.trim() ? decision.reason.trim() : "denied by agent-hooks";
  return { action: "reject-and-continue", message: reason };
}

export default function agentHooks(amp: any) {
  amp.on("session.start", (event: unknown, ctx: unknown) => {
    emit("session.start", buildPayload(event, ctx));
  });
  amp.on("agent.start", (event: unknown, ctx: unknown) => {
    emit("agent.start", buildPayload(event, ctx));
  });
  amp.on("tool.call", async (event: unknown, ctx: unknown) => {
    return toolCallAction(await query("tool.call", buildPayload(event, ctx)));
  });
  amp.on("tool.result", (event: unknown, ctx: unknown) => {
    emit("tool.result", buildPayload(event, ctx));
  });
  amp.on("agent.end", (event: unknown, ctx: unknown) => {
    emit("agent.end", buildPayload(event, ctx));
  });
}
