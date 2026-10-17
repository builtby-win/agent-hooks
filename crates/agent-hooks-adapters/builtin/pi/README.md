# agent-hooks for Pi

Source of truth for the `pi` extension. Installs as a drop-in file — no npm publish required.

## Install (local path, recommended)

From the repo root:

```bash
# 1. Build / install the Rust shim the plugin shells out to
cargo install --path crates/agent-hooks-cli

# 2. Stage the extension into pi's auto-discovery dir
agent-hooks install --harness pi   # stages ~/.pi/agent/extensions/agent-hooks.ts
# or manually: cp ./crates/agent-hooks-adapters/builtin/pi/assets/pi/index.ts ~/.pi/agent/extensions/agent-hooks.ts

# 3. Verify + reload session
omp plugin list
# then restart pi or run /reload inside it
```

Or via the Rust installer (does steps 2–3 for you):

```bash
agent-hooks install --harness pi
# aliases: pi-agent, pi_agent
```

## What it does

`agent-hooks.ts` subscribes to pi lifecycle events and fire-and-forgets each one to
`agent-hooks hook pi --event <AgentEvent>` over stdin JSON:

| pi event | forwarded as |
|---|---|
| `session_start` | `SessionStart` |
| `agent_start` | `UserPromptSubmit` (`before_agent_start` intentionally not forwarded — see `index.ts`) |
| `turn_start` / `turn_end` | `TurnStart` / `TurnEnd` (busy signals: assert `Running`, never terminal) |
| `tool_call` | `PreToolUse` |
| `tool_result` | `PostToolUse` |
| `tool_approval_requested` | `PermissionRequest` |
| `agent_settled` | `Stop` (`agent_end` intentionally not forwarded: a run may still auto-retry/continue) |
| `session_shutdown` | `SessionEnd` |

The shim forwards to the daemon (`/tmp/agent-hooks.sock`, 500ms timeout) and
always exits 0 with `{"decision":"allow"}` fallback — the agent is never blocked.
Policy decisions stay daemon-side (`prompt.respond` / sessions API).

## Configuration

| Env | Purpose | Default |
|---|---|---|
| `AGENT_HOOKS_BIN` | Override path to `agent-hooks` binary | `~/.cargo/bin/agent-hooks` → `/usr/local/bin/agent-hooks` → `agent-hooks` on `PATH` |

## Uninstall

```bash
agent-hooks uninstall --harness pi
# or directly: rm ~/.pi/agent/extensions/agent-hooks.ts
```

## Files

```
assets/pi/
  package.json  # name + pi.extensions = ["./index.ts"] (pi package installs)
  index.ts      # ExtensionAPI entry (default-export factory)

README.md (this file) sits at builtin/pi/ so it is not embedded into the binary.
