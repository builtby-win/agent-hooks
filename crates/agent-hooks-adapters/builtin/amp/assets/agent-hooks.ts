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
import { randomUUID } from "node:crypto";
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

// Bound held gates to the daemon's 300 second human-response window.
const GATE_TIMEOUT_MS = 290_000;

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
  const { promise, resolve } = Promise.withResolvers<Fields>();
  if (hooksDisabled()) {
    resolve({});
    return promise;
  }
  let child;
  try {
    child = spawn(BINARY, hookArgs(event), { stdio: ["pipe", "pipe", "ignore"] });
  } catch {
    resolve({});
    return promise;
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
  return promise;
}

// Verdict -> Amp tool.call action. Only an explicit deny rejects.
export function toolCallAction(decision: Fields): { action: "allow" } | { action: "reject-and-continue"; message: string } {
  if (decision.decision !== "deny") return { action: "allow" };
  const reason = typeof decision.reason === "string" && decision.reason.trim() ? decision.reason.trim() : "denied by agent-hooks";
  return { action: "reject-and-continue", message: reason };
}
type AskQuestion = {
  id: string;
  question: string;
  header?: string;
  options: AskOption[];
  multiple?: boolean;
  custom?: boolean;
  recommended?: number;
};
type AskAnswer = { question_id: string; selected_options: string[]; custom_input?: string };
type Ui = {
  confirm(options: { title: string; message: string; confirmButtonText: string }): Promise<boolean>;
  select(options: { title: string; message: string; options: string[]; allowOther: boolean }): Promise<string | undefined>;
  input(options: { title: string; helpText: string; submitButtonText: string }): Promise<string | undefined>;
};
type PluginContext = { ui: Ui; thread?: { id?: string } };
type ToolDefinition = {
  name: string;
  title: string;
  description: string;
  inputSchema: Fields;
  execute(input: unknown, ctx: PluginContext): Promise<string>;
};
type AmpPlugin = {
  on(event: string, handler: (event: unknown, ctx: PluginContext) => unknown): void;
  registerTool(tool: ToolDefinition): void;
};

function normalizeQuestions(value: unknown): AskQuestion[] | undefined {
  const input = fields(value);
  if (!Array.isArray(input.questions) || input.questions.length === 0) return undefined;
  const questions: AskQuestion[] = [];
  for (const value of input.questions) {
    const q = fields(value);
    if (typeof q.id !== "string" || typeof q.question !== "string" || !Array.isArray(q.options)) return undefined;
    const options: AskOption[] = [];
    for (const value of q.options) {
      const o = fields(value);
      if (typeof o.label !== "string") return undefined;
      options.push({
        label: o.label,
        ...(typeof o.description === "string" ? { description: o.description } : {}),
        ...(typeof o.preview === "string" ? { preview: o.preview } : {}),
      });
    }
    questions.push({
      id: q.id,
      question: q.question,
      options,
      ...(typeof q.header === "string" ? { header: q.header } : {}),
      multiple: q.multiple === true,
      custom: q.custom === true,
      ...(typeof q.recommended === "number" ? { recommended: q.recommended } : {}),
    });
  }
  return questions;
}

export function canonicalAnswers(value: unknown, questions: AskQuestion[]): AskAnswer[] | undefined {
  const decision = fields(value);
  if (!Array.isArray(decision.answers) || decision.answers.length === 0) return undefined;
  if (decision.decision !== "allow") throw new Error("Agent-hooks returned answers without an allow verdict");
  if (decision.answers.length !== questions.length) throw new Error("Agent-hooks returned incomplete question answers");
  const answers: AskAnswer[] = [];
  for (let i = 0; i < questions.length; i++) {
    const q = questions[i];
    const a = fields(decision.answers[i]);
    const custom = a.custom_input === null || a.custom_input === "" ? undefined : a.custom_input;
    if (!q || a.question_id !== q.id || !Array.isArray(a.selected_options) ||
      a.selected_options.some((o) => typeof o !== "string" || !q.options.some((option) => option.label === o)) ||
      new Set(a.selected_options).size !== a.selected_options.length ||
      (custom !== undefined && (typeof custom !== "string" || q.custom !== true)) ||
      a.selected_options.length + (typeof custom === "string" ? 1 : 0) === 0 ||
      (q.multiple !== true && a.selected_options.length + (typeof custom === "string" ? 1 : 0) > 1)) {
      throw new Error("Agent-hooks returned invalid question answers");
    }
    answers.push({
      question_id: q.id,
      selected_options: a.selected_options as string[],
      ...(typeof custom === "string" ? { custom_input: custom } : {}),
    });
  }
  return answers;
}

async function nativeAnswers(questions: AskQuestion[], ui: Ui): Promise<AskAnswer[]> {
  const answers: AskAnswer[] = [];
  for (const q of questions) {
    const selected: string[] = [];
    let customInput: string | undefined;
    if (q.multiple && q.options.length) {
      let remaining = q.options.map((o) => o.label);
      let doneLabel = "Done (finish selections)";
      while (remaining.includes(doneLabel)) doneLabel += " ";
      while (remaining.length) {
        const choice = await ui.select({
          title: q.header ?? "Answer question",
          message: `${q.question}\nSelected: ${selected.join(", ") || "(none)"}\nChoose an option or finish.`,
          options: [...remaining, ...(selected.length ? [doneLabel] : [])],
          allowOther: q.custom === true,
        });
        if (choice === undefined) throw new Error("Question cancelled");
        if (choice === doneLabel) break;
        if (remaining.includes(choice)) {
          selected.push(choice);
          remaining = remaining.filter((label) => label !== choice);
        } else if (q.custom) {
          customInput = choice;
          break;
        }
      }
    } else if (q.options.length) {
      const choice = await ui.select({
        title: q.header ?? "Answer question",
        message: q.question,
        options: q.options.map((o) => o.label),
        allowOther: q.custom === true,
      });
      if (choice === undefined) throw new Error("Question cancelled");
      if (q.options.some((o) => o.label === choice)) selected.push(choice);
      else if (q.custom) customInput = choice;
      else throw new Error("Question returned an invalid option");
    } else if (q.custom) {
      const input = await ui.input({
        title: q.header ?? "Answer question",
        helpText: q.question,
        submitButtonText: "Submit",
      });
      if (input === undefined) throw new Error("Question cancelled");
      if (input.trim()) customInput = input;
    }
    if (!selected.length && !customInput) throw new Error("Question cancelled");
    answers.push({ question_id: q.id, selected_options: selected, ...(customInput ? { custom_input: customInput } : {}) });
  }
  return answers;
}

function registerAskTool(amp: AmpPlugin): void {
  amp.registerTool({
    name: "agent_hooks_ask",
    title: "Ask",
    description: "Ask the user one or more structured questions and wait for their answers.",
    inputSchema: {
      type: "object",
      properties: {
        questions: {
          type: "array",
          minItems: 1,
          items: {
            type: "object",
            properties: {
              id: { type: "string" },
              question: { type: "string" },
              header: { type: "string" },
              options: { type: "array", items: { type: "object", properties: {
                label: { type: "string" }, description: { type: "string" }, preview: { type: "string" },
              }, required: ["label"] } },
              multiple: { type: "boolean" },
              custom: { type: "boolean" },
              recommended: { type: "integer", minimum: 0 },
            },
            required: ["id", "question", "options"],
          },
        },
      },
      required: ["questions"],
    },
    async execute(input: unknown, ctx: PluginContext): Promise<string> {
      const questions = normalizeQuestions(input);
      if (!questions) throw new Error("Invalid agent_hooks_ask questions");
      const p = buildPayload(input, ctx);
      let decision: Fields = {};
      try {
        decision = await query("question", {
          ...p,
          questions,
          tool_name: "agent_hooks_ask",
          session_id: str(p.session_id, ctx.thread?.id),
          request_id: str(p.request_id) ?? randomUUID(),
        });
      } catch {
        decision = {};
      }
      if (decision.decision === "deny") throw new Error(str(decision.reason) ?? "Question denied");
      if (Array.isArray(decision.answers) && decision.answers.length > 0) {
        const answers = canonicalAnswers(decision, questions);
        if (!answers) throw new Error("Agent-hooks returned no usable question answers");
        return JSON.stringify(answers);
      }
      return JSON.stringify(await nativeAnswers(questions, ctx.ui));
    },
  });
}

export default function agentHooks(amp: AmpPlugin) {
  registerAskTool(amp);
  amp.on("session.start", (event: unknown, ctx: PluginContext) => {
    emit("session.start", buildPayload(event, ctx));
  });
  amp.on("agent.start", (event: unknown, ctx: PluginContext) => {
    emit("agent.start", buildPayload(event, ctx));
  });
  amp.on("tool.call", async (event: unknown, ctx: PluginContext) => {
    const payload = buildPayload(event, ctx);
    if (payload.tool_name === "agent_hooks_ask") return { action: "allow" };
    const decision = await query("tool.call", payload);
    if (decision.decision === "ask") {
      try {
        const confirmed = await ctx.ui.confirm({
          title: `Allow ${str(payload.tool_name) ?? "tool"}?`,
          message: str(decision.reason) ?? "Agent-hooks requests permission to continue.",
          confirmButtonText: "Allow",
        });
        return confirmed
          ? { action: "allow" }
          : { action: "reject-and-continue", message: str(decision.reason) ?? "Permission denied" };
      } catch {
        return { action: "reject-and-continue", message: str(decision.reason) ?? "Permission confirmation failed" };
      }
    }
    return toolCallAction(decision);
  });
  amp.on("tool.result", (event: unknown, ctx: PluginContext) => {
    emit("tool.result", buildPayload(event, ctx));
  });
  amp.on("agent.end", (event: unknown, ctx: PluginContext) => {
    emit("agent.end", buildPayload(event, ctx));
  });
}
