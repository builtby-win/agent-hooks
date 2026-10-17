# Agent Hooks IPC Socket Protocol Specification

The `agent-hooks` daemon exposes a high-performance, cross-platform Local IPC socket speaking newline-delimited JSON-RPC 2.0 (NDJSON).

Any application written in any language (Rust, TypeScript/JavaScript, Python, Go, Swift, C#, etc.) can connect directly to the socket to receive real-time normalized agent lifecycle events, query active sessions, and approve/deny permission prompts.

---

## 1. Transport & Endpoints

### Default Socket Endpoints
- **macOS / Linux / Unix:** `/tmp/agent-hooks.sock` (Unix Domain Socket)
- **Windows:** `\\.\pipe\agent-hooks` (Named Pipe)

### Environment Variable Override
Setting `AGENT_HOOKS_SOCKET` overrides the default socket path on any platform:
```bash
export AGENT_HOOKS_SOCKET="/tmp/custom-agent-hooks.sock"
```

### Framing
- **Format:** Newline-delimited JSON (`\n` separated UTF-8 strings).
- **Encoding:** Standard UTF-8 JSON objects.
- Each request or notification must be a single line terminated by `\n`.
- Each response or broadcast event from the daemon is a single line terminated by `\n`.

---

## 2. JSON-RPC 2.0 API Methods

### `subscribe`
Subscribes the active connection to receive real-time event broadcasts (`AgentEvent` notifications).

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "subscribe",
  "params": {
    "harnesses": ["claude-code", "codex"],
    "session_ids": [],
    "event_types": ["PreToolUse", "PermissionRequest", "Stop"]
  }
}
```
*Note:* Pass empty arrays or `null` params to receive all events from all harnesses.

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "subscribed": true
  }
}
```

---

### `list_sessions`
Returns a list of all known agent sessions with their normalized status and event statistics.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "list_sessions"
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": [
    {
      "session_id": "ses_claude_12345",
      "harness": "claude-code",
      "harness_version": "0.5.2",
      "status": "WaitingForInput",
      "event_count": 14,
      "last_event_at_ms": 1724750400000,
      "cwd": "/Users/example/project"
    }
  ]
}
```

---

### `get_session`
Fetches full details for a single session ID.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "get_session",
  "params": {
    "session_id": "ses_claude_12345"
  }
}
```

---

### `prompt.respond` / `respond_prompt`
Resolves a pending human-in-the-loop permission request (e.g. tool execution approval, bash command confirmation).

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "prompt.respond",
  "params": {
    "prompt_id": "prompt_98765",
    "decision": {
      "allow": true
    }
  }
}
```

**Decision wire shape** (`PromptDecision`): `{"allow": bool, "custom_input"?: string, "answers"?: Answer[], "ask"?: bool}` where each answer is `{"question_id": string, "selected_options": string[], "custom_input"?: string}`.
- **Allow:** `{"allow": true}`
- **Deny with reason:** `{"allow": false, "custom_input": "Command not permitted"}`
- **Ask (escalate to a human):** `{"allow": false, "ask": true, "custom_input": "Allow rm -rf?"}`
- **Answer questions:** `{"allow": true, "answers": [{"question_id": "favorite_color", "selected_options": ["Blue"]}]}` — e.g. yes/no: `{"allow": true, "answers": [{"question_id": "confirm", "selected_options": ["yes"]}]}`

**prompt_id rule:** answer with the broadcast event's `event_id`, except `PermissionRequest` which keeps `prompt.prompt_id`.

**Deciders must subscribe with `"decide": true`** (e.g. `"params": {"decide": true}`) — without it the daemon fast-allows gate events instead of holding them for your verdict. **Unanswered holds deny after 300s** (fail-safe timeout), so answer promptly or don't subscribe with `decide: true`.

### Local hooks: exit codes and `when`/`respond` config

Daemonless local hooks (`~/.agent-hooks.json`, `./.agent-hooks/hooks.json`) run shell commands with the normalized event JSON on stdin and use process exit codes:
- exit `0` → allow.
- exit `2` → deny; trimmed stderr becomes the reason.
- exit `3` → ask; trimmed stderr becomes the question for the decider.
- any other exit, spawn failure, or timeout → fail-open allow with a stderr warning.

Entries can also match and answer without spawning, via `when` + `respond`:
```json
{
  "version": 1,
  "hooks": {
    "PermissionRequest": [
      {
        "when": { "tool": "Bash" },
        "respond": {
          "decision": "allow",
          "answers": [{ "question_id": "favorite_color", "selected_options": ["Blue"] }]
        }
      }
    ],
    "PreToolUse": [
      {
        "when": { "prompt_matches": "rm -rf" },
        "respond": { "decision": "deny", "reason": "rm -rf is never auto-approved" }
      }
    ]
  }
}
```
`when.tool` is an exact case-insensitive tool-name match (`PreToolUse.tool_name` or `PermissionRequest.prompt.tool_name`); `when.prompt_matches` is a case-insensitive substring over the prompt description plus all question texts (the `PreToolUse` tool-input JSON as fallback). A matched `respond` answers directly (`allow` / `deny` / `ask`, plus optional `reason` and `answers`) without spawning `command`.

---

### `ping`
Heartbeat check.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "ping"
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "status": "ok",
    "version": "0.1.0"
  }
}
```

---

## 3. Server-Sent Event Notifications

When a client is subscribed, the daemon sends real-time JSON-RPC notifications (`method: "event"`) whenever a hook fires:

```json
{
  "jsonrpc": "2.0",
  "method": "event",
  "params": {
    "schema_version": 1,
    "harness": "claude-code",
    "harness_version": "0.5.2",
    "session_id": "ses_claude_12345",
    "parent_session_id": null,
    "timestamp_ms": 1724750400123,
    "lifecycle": {
      "type": "PreToolUse",
      "tool_name": "Bash",
      "tool_input": {
        "command": "cargo test"
      },
      "tool_use_id": "toolu_01Abc"
    },
    "context": {
      "cwd": "/Users/example/project",
      "git_worktree": "/Users/example/project",
      "tty": "/dev/ttys004",
      "pid": 45123,
      "is_ssh": false
    }
  }
}
```

### Agent Lifecycle Types
- `SessionStart`: Agent session initialized
- `SessionEnd`: Agent session closed
- `UserPromptSubmit`: User submitted input/prompt
- `PreToolUse`: Agent requested tool execution (e.g. bash, edit, read)
- `PostToolUse`: Tool execution succeeded
- `PostToolUseFailure`: Tool execution failed
- `PermissionRequest`: Agent requires human confirmation (contains `prompt: PromptRequest`)
- `Notification`: Progress status or message from agent
- `SubagentStart` / `SubagentStop`: Spawned subagent lifecycle
- `TurnStart` / `TurnEnd`: Busy signals for a single agent turn (one LLM response + tool calls, many per run). Both derive `Running`, never terminal — use for activity indicators, not completion.
- `Stop`: Agent run settled and waiting for input
- `PreCompact` / `PostCompact`: Context compaction boundaries; session stays `Running`
- `Interrupt`: User interruption marker
- `PreLLMCall` / `PostLLMCall`: Model-call boundaries; session stays `Running`, not turn completion
- `PostToolBatch`: Committed tool-batch marker; ordered native calls remain in `raw_event`
- `ToolUseStart`: Tool execution started after hooks/approval; native details remain in `raw_event`
- `StopFailure`: Final model failure for a turn; `data.error` carries the extracted error and status is `Failed`, not `Closed`

### Optional hook display text

`AgentEvent.flavor_text` is an optional string extracted from meaningful native hook text, trimmed and bounded to 300 Unicode characters. It is omitted when no useful text is available. Per-hook adapter sources take precedence over adapter-wide sources. Structured text blocks are supported; arbitrary JSON and non-text blocks are not displayed. No transcript or chat-store lookup is performed for flavor text.

The field survives subscriber filtering even though `raw_event` is removed. It is display-only: do not derive status, permission decisions, or routing from it. Older events without the field deserialize normally, and existing lifecycle-specific text fields retain their contracts.

### Extensions & passthrough_env (consumer enrichment)

`agent-hooks` is intentionally **harness-agnostic**: it normalizes lifecycle + `NormalizedStatus` and captures only the minimal `EventContext` (cwd, tty, pid, shallow tmux/ssh env). Precise terminal/mux targets (`bundle_id`, `terminal_tab_id`, exact `tmux pane_id/window_id/client_pid`, `cmux/herdr/wezterm/kitty` state, SSH-forward socket) are **consumer implementation details**, not core protocol.

Two fields support this split without forking the repo:

* `RawHookInput.passthrough_env: Map<string,string>` — opaque snapshot of selected env vars at shim time (`TERM_PROGRAM`, `KITTY_WINDOW_ID`, `WEZTERM_PANE`, `GHOSTTY_*`, `CMUX_*`, `HERDR_*`, `ZELLIJ_*`, `ITERM_SESSION_ID`, `TMUX*`, `SSH_*`, …). The shim never interprets them; it forwards them so the consumer can resolve targets lazily without paying for a spawn in the `<2ms` hot path.
* `AgentEvent.extensions: Map<string, Value>` — empty by default. When `agent-hooks-core` is embedded as a library (e.g. Back2Vibing linking `agent-hooks-core`), the consumer registers an `EventEnricher` (`AgentHooksDaemon::with_enricher(enricher)`) that populates `extensions` synchronously after `adapter.parse` and before `tracker.track` + `broadcast`. Keys are namespaced by the consumer (convention: `b2v.terminal_target`, `b2v.mux_target`, `b2v.ssh_forward`). The daemon never reads them.

**Out-of-process daemon** (standalone `agent-hooks daemon` binary): `extensions` stays empty. Consumers subscribe via `subscribe` and do **client-side enrichment** from `event.context` + `event.passthrough_env` + `event.raw_event` (e.g. Back2Vibing's `terminal_capture::resolve` / `tmux_context::capture`). This keeps the repo pure and lets Back2Vibing own all window/tmux/SSH logic.

```rust
// In-process embedding (Back2Vibing Rust)
use agent_hooks_core::{AgentHooksDaemon, EventEnricher};
use agent_hooks_protocol::{AgentEvent, RawHookInput};
use std::sync::Arc;

struct B2vEnricher;
impl EventEnricher for B2vEnricher {
    fn enrich(&self, event: &mut AgentEvent, raw: &RawHookInput) {
        // Resolve using existing B2V helpers, never in agent-hooks
        if let Some(target) = b2v_terminal_target::resolve(&event.context, &raw.passthrough_env) {
            event.extensions.insert("b2v.terminal_target".into(), serde_json::to_value(target).unwrap());
        }
        if let Some(mux) = b2v_mux_target::capture(&raw.passthrough_env) {
            event.extensions.insert("b2v.mux_target".into(), serde_json::to_value(mux).unwrap());
        }
    }
}

let daemon = AgentHooksDaemon::with_enricher(Arc::new(B2vEnricher));
```

Client-side (TypeScript) enrichment after `subscribe` is equivalent — see `event.passthrough_env` + `event.context` and populate your own store; no daemon change needed.


## 4. Reference Implementations

### A. TypeScript / Node.js (Zero Dependencies)

```typescript
import * as net from 'net';
import * as readline from 'readline';

const socketPath = process.env.AGENT_HOOKS_SOCKET || (
  process.platform === 'win32' ? '\\\\.\\pipe\\agent-hooks' : '/tmp/agent-hooks.sock'
);

const client = net.createConnection(socketPath, () => {
  console.log('Connected to agent-hooks daemon');

  // Subscribe to all events
  const subReq = {
    jsonrpc: '2.0',
    id: 1,
    method: 'subscribe',
    params: {}
  };
  client.write(JSON.stringify(subReq) + '\n');
});

const rl = readline.createInterface({ input: client });

rl.on('line', (line) => {
  if (!line.trim()) return;
  const msg = JSON.parse(line);

  // Handle event notification
  if (msg.method === 'event') {
    const event = msg.params;
    console.log(`[${event.harness}] ${event.lifecycle.type} (Session: ${event.session_id})`);

    // Handle interactive permission request
    if (event.lifecycle.type === 'PermissionRequest') {
      const prompt = event.lifecycle.prompt;
      console.log(`Permission requested: ${prompt.message || prompt.kind}`);

      // Respond allow (prompt_id: broadcast event_id, except PermissionRequest keeps prompt.prompt_id)
      const response = {
        jsonrpc: '2.0',
        id: Date.now(),
        method: 'prompt.respond',
        params: {
          prompt_id: prompt.prompt_id,
          decision: { allow: true }
        }
      };
      client.write(JSON.stringify(response) + '\n');
    }
  }
});

client.on('error', (err) => {
  console.error('Socket error:', err.message);
});
```

---

### B. Python (Asyncio / Zero Dependencies)

```python
import asyncio
import json
import os
import sys

SOCKET_PATH = os.environ.get(
    "AGENT_HOOKS_SOCKET",
    r"\\.\pipe\agent-hooks" if sys.platform == "win32" else "/tmp/agent-hooks.sock"
)

async def main():
    if sys.platform == "win32":
        # Windows named pipe connection via open
        print(f"Connecting to Windows named pipe: {SOCKET_PATH}")
        # Note: on Windows, use win32file or asyncio pipe connection
        return

    reader, writer = await asyncio.open_unix_connection(SOCKET_PATH)
    print(f"Connected to agent-hooks daemon at {SOCKET_PATH}")

    # Subscribe to events
    sub_request = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "subscribe",
        "params": {}
    }
    writer.write((json.dumps(sub_request) + "\n").encode())
    await writer.drain()

    while True:
        line = await reader.readline()
        if not line:
            break
        msg = json.loads(line.decode().strip())

        if msg.get("method") == "event":
            event = msg["params"]
            lifecycle = event["lifecycle"]
            print(f"[{event['harness']}] {lifecycle['type']} (Session: {event['session_id']})")

            if lifecycle["type"] == "PermissionRequest":
                prompt_id = lifecycle["prompt"]["id"]
                # Respond allow
                resp = {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "prompt.respond",
                    "params": {
                        "prompt_id": prompt_id,
                        "decision": {"behavior": "allow"}
                    }
                }
                writer.write((json.dumps(resp) + "\n").encode())
                await writer.drain()

if __name__ == "__main__":
    asyncio.run(main())
```

---

### C. Go (Standard Library)

```go
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"os"
)

func main() {
	socketPath := os.Getenv("AGENT_HOOKS_SOCKET")
	if socketPath == "" {
		socketPath = "/tmp/agent-hooks.sock"
	}

	conn, err := net.Dial("unix", socketPath)
	if err != nil {
		fmt.Printf("Failed to connect: %v\n", err)
		return
	}
	defer conn.Close()

	// Send subscribe request
	sub := map[string]interface{}{
		"jsonrpc": "2.0",
		"id":      1,
		"method":  "subscribe",
		"params":  map[string]interface{}{},
	}
	data, _ := json.Marshal(sub)
	conn.Write(append(data, '\n'))

	scanner := bufio.NewScanner(conn)
	for scanner.Scan() {
		line := scanner.Bytes()
		var msg map[string]interface{}
		if err := json.Unmarshal(line, &msg); err != nil {
			continue
		}

		if msg["method"] == "event" {
			params := msg["params"].(map[string]interface{})
			lifecycle := params["lifecycle"].(map[string]interface{})
			fmt.Printf("[%v] %v (Session: %v)\n", params["harness"], lifecycle["type"], params["session_id"])
		}
	}
}
```
