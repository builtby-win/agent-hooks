# agent-hooks Hermes plugin — managed by agent-hooks.
# Staged via the hermes builtin manifest into ~/.hermes/plugins/agent-hooks/
# (Hermes has no hooks.json; plugins register hooks from register(ctx)).
# Reference: https://hermes-agent.nousresearch.com/docs/user-guide/features/hooks
# Remove with: `agent-hooks uninstall --harness hermes`.
#
# Port of the legacy Back2Vibing b2v_hooks.py plugin. Each hook is forwarded
# to `agent-hooks hook hermes --event <hook name>` with a small JSON payload.
# Only pre_tool_call is awaited (Hermes lets it block the tool); every other
# hook is fire-and-forget. Mapping lives in builtin/hermes/adapter.json.

import json
import os
import shutil
import subprocess

# Install-time settings: `agent-hooks install` replaces each quoted token with
# a JSON string literal ("" when unset). An unstaged copy still holds the raw
# token — treat as unset.
INSTALL_BIN = "__AGENT_HOOKS_BIN__"
INSTALL_SOCKET = "__AGENT_HOOKS_SOCKET__"
INSTALL_CONTEXT_CMD = "__AGENT_HOOKS_CONTEXT_CMD__"
INSTALL_ORIGIN = "__AGENT_HOOKS_ORIGIN__"

GATE_TIMEOUT_S = 25


def _install_setting(value):
    value = (value or "").strip()
    return "" if value.startswith("__AGENT_HOOKS_") else value


def _resolve_binary():
    override = os.environ.get("AGENT_HOOKS_BIN", "").strip()
    if override:
        return override
    found = shutil.which("agent-hooks")
    if found:
        return found
    for candidate in (
        os.path.expanduser("~/.cargo/bin/agent-hooks"),
        "/usr/local/bin/agent-hooks",
    ):
        if os.path.exists(candidate):
            return candidate
    return "agent-hooks"


BINARY = _install_setting(INSTALL_BIN) or _resolve_binary()
SOCKET = _install_setting(INSTALL_SOCKET)
CONTEXT_CMD = _install_setting(INSTALL_CONTEXT_CMD)
# Install-time origin label, forwarded so a consumer can tell remotes apart.
ORIGIN = _install_setting(INSTALL_ORIGIN)


def hook_args(event):
    args = [BINARY, "hook", "hermes", "--event", event]
    if SOCKET:
        args += ["--socket", SOCKET]
    if CONTEXT_CMD:
        args += ["--context-cmd", CONTEXT_CMD]
    if ORIGIN:
        args += ["--origin", ORIGIN]
    return args


def hooks_disabled(env=None):
    """AGENT_HOOKS_DISABLED=1 skips every spawn. B2V_DISABLED is honored too
    because Back2Vibing users already set it to silence the legacy plugin."""
    env = os.environ if env is None else env

    def on(name):
        return (env.get(name) or "").strip().lower() in ("1", "true")

    return on("AGENT_HOOKS_DISABLED") or on("B2V_DISABLED")


# Scalar kwargs worth forwarding. Hermes also passes large objects
# (conversation_history, tool_call_history): never serialize those.
_KEYS = (
    "session_id",
    "parent_session_id",
    "turn_id",
    "task_id",
    "tool_name",
    "tool_call_id",
    "args",
    "status",
    "error_type",
    "error_message",
    "command",
    "description",
    "surface",
    "choice",
    "completed",
    "interrupted",
    "turn_exit_reason",
    "is_first_turn",
    "model",
    "platform",
)


def build_payload(event, kwargs):
    payload = {"event": event}
    for key in _KEYS:
        value = kwargs.get(key)
        if value is None:
            continue
        try:
            json.dumps(value)
        except (TypeError, ValueError):
            continue
        payload[key] = value
    if "args" in payload:
        payload["tool_input"] = payload.pop("args")
    if event in ("subagent_start", "subagent_stop"):
        # These fire on the PARENT session and carry no session_id of their
        # own: the parent is the session, the child is the subagent.
        payload.pop("parent_session_id", None)
        payload["session_id"] = kwargs.get("parent_session_id")
        payload["subagent_id"] = kwargs.get("child_session_id") or kwargs.get("child_role")
        summary = kwargs.get("child_summary") or kwargs.get("child_goal")
        if isinstance(summary, str):
            payload["summary"] = summary
    if event == "pre_approval_request":
        # Approval ids key the prompt; the command text is what is asked.
        payload["request_id"] = kwargs.get("tool_call_id") or kwargs.get("pattern_key")
        if not payload.get("session_id") and kwargs.get("session_key"):
            payload["session_id"] = kwargs.get("session_key")
    return payload


def _emit(event, kwargs):
    if hooks_disabled():
        return
    try:
        child = subprocess.Popen(
            hook_args(event),
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        child.stdin.write(json.dumps(build_payload(event, kwargs)).encode())
        child.stdin.close()
    except Exception:
        pass  # never break the agent over hook infra


def _query(event, kwargs):
    if hooks_disabled():
        return {}
    try:
        done = subprocess.run(
            hook_args(event),
            input=json.dumps(build_payload(event, kwargs)),
            text=True,
            capture_output=True,
            timeout=GATE_TIMEOUT_S,
            check=False,
        )
        verdict = json.loads(done.stdout.strip() or "{}")
        return verdict if isinstance(verdict, dict) else {}
    except Exception:
        return {}


def tool_call_action(verdict):
    """Verdict -> Hermes pre_tool_call return. Only an explicit deny blocks."""
    if verdict.get("decision") != "deny":
        return None
    reason = verdict.get("reason")
    if not isinstance(reason, str) or not reason.strip():
        reason = "denied by agent-hooks"
    return {"action": "block", "message": reason.strip()}


def pre_tool_call(**kwargs):
    return tool_call_action(_query("pre_tool_call", kwargs))


def _observer(event):
    def hook(**kwargs):
        _emit(event, kwargs)

    hook.__name__ = event
    return hook


OBSERVED = (
    "on_session_start",
    "pre_llm_call",
    "post_tool_call",
    "pre_approval_request",
    "post_approval_response",
    "subagent_start",
    "subagent_stop",
    "on_session_end",
)


def register(ctx):
    """Register hooks with Hermes. Older Hermes may reject newer hook names."""
    try:
        ctx.register_hook("pre_tool_call", pre_tool_call)
    except Exception:
        pass
    for event in OBSERVED:
        try:
            ctx.register_hook(event, _observer(event))
        except Exception:
            pass
