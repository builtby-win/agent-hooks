# agent-hooks for Oh My Pi (`omp`)

Source of truth for the `omp` plugin. Installs as a **local-directory plugin** — no npm publish required.

## Install (local path, recommended)

From the repo root:

```bash
# 1. Build / install the Rust shim the plugin shells out to
cargo install --path crates/agent-hooks-cli

# 2. Link the plugin dir as an omp plugin (symlink, source edits apply immediately)
omp plugin install ./crates/agent-hooks-adapters/builtin/omp/assets/oh-my-pi
# equivalent: omp plugin link ./crates/agent-hooks-adapters/builtin/omp/assets/oh-my-pi

# 3. Verify + reload session
omp plugin list
# then restart omp or run /reload-plugins inside it
```

Or via the Rust installer (does steps 2–3 for you):

```bash
agent-hooks install --harness omp
# aliases: oh-my-pi, oh_my_pi
```

## What it does

`index.ts` subscribes to omp lifecycle events and fire-and-forgets each one to
`agent-hooks hook omp --event <AgentEvent>` over stdin JSON:

| omp event | forwarded as |
|---|---|
| `session_start` | `SessionStart` |
| `agent_start` | `UserPromptSubmit` (`before_agent_start` intentionally not forwarded — see `index.ts`) |
| `turn_start` / `turn_end` | `TurnStart` / `TurnEnd` (busy signals: assert `Running`, never terminal) |
| `tool_call` | `PreToolUse` |
| `tool_result` | `PostToolUse` |
| `tool_approval_requested` | `PermissionRequest` |
| `agent_end` / `session_stop` | `Stop` (omp v18.1.x renamed `agent_settled`; both forwarded, `session_stop` carries `session_id` + settle semantics) |
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
agent-hooks uninstall --harness omp
# or directly:
omp plugin uninstall agent-hooks
```

## Files

```
assets/oh-my-pi/
  package.json  # name + omp.extensions = ["./index.ts"]; required for `omp plugin install ./path`
  index.ts      # ExtensionAPI entry (default-export factory)

README.md (this file) sits at builtin/omp/ so it is not embedded into the binary.
