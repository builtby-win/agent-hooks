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
    "event_types": ["PreToolUse", "PermissionRequest", "Stop"],
    "decide": true
  }
}
```
*Note:* Pass empty arrays or `null` params to receive all events from all harnesses. Include `decide: true` when this connection is acting as a decider and should hold eligible gate events; omitting it is observe-only/fast-allow behavior.

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

### `prompt.list` / `list_prompts`
Returns the daemon's current pending-prompt backlog, oldest first. The answer CLI uses this to select a pending prompt; `session_id` is a selector, not an offline rendezvous.

**Request:**
```json
{"jsonrpc": "2.0", "id": 3, "method": "prompt.list", "params": null}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "prompts": [{
      "prompt_id": "prompt_98765",
      "session_id": "ses_claude_12345",
      "key": "Bash",
      "question": "Run this command?",
      "waiters": 1,
      "parked": 0,
      "age_ms": 1200,
      "prompt": {
        "prompt_id": "prompt_98765",
        "kind": "question",
        "description": "Run this command?",
        "questions": [{
          "id": "confirm",
          "question": "Continue?",
          "options": [{"label": "Yes"}, {"label": "No"}],
          "multiple": false,
          "custom": false
        }]
      }
    }]
  }
}
```
The `prompt` payload may be absent for holds without question metadata; `question` is optional. Validate question answers against `prompt.questions` from the selected pending entry.

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

**Decision wire shape** (`PromptDecision`, unknown fields rejected): `{"allow": bool, "custom_input"?: string, "answers"?: Answer[], "ask"?: bool, "defer"?: bool, "allow_always"?: bool, "suggestion"?: number, "additional_context"?: string}` where each answer is `{"question_id": string, "selected_options": string[], "custom_input"?: string}`. This is also the exact schema for local config `respond` and exit-0 command stdout.
- **Allow:** `{"allow": true}`
- **Deny with reason:** `{"allow": false, "custom_input": "Command not permitted"}`
- **Ask (escalate to a human):** `{"allow": false, "ask": true, "custom_input": "Allow rm -rf?"}`
- **Answer questions:** `{"allow": true, "answers": [{"question_id": "favorite_color", "selected_options": ["Blue"]}]}` — e.g. yes/no: `{"allow": true, "answers": [{"question_id": "confirm", "selected_options": ["yes"]}]}`

**prompt_id rule:** use the authoritative `event.hold.prompt_id` from an answerable event, or the `prompt_id` returned by `prompt.list`. Never reconstruct it from `event_id` or lifecycle prompt fields.

Nonempty answers must cover every question exactly once, matched by exact `question_id` and reordered into request order. Unknown/duplicate IDs, invalid labels, disallowed custom input and multiple picks on single-select questions are rejected. Answers require `allow: true`; allow/ask/defer are mutually exclusive, `allow_always` requires allow, and `suggestion` requires allow_always. Empty answers preserve permission approval/native question handling. Answer text is never trimmed or truncated. Invalid RPC decisions return an error without consuming the pending prompt, allowing a corrected retry.

**Deciders must subscribe with `"decide": true`** to hold interactive events the harness already blocked on. To intercept mid-flight tools, use an explicit scope, for example `"decide": {"event_types": ["PreToolUse"], "tools": ["Bash"]}`; explicit scopes introduce a wait for matching events unless `interactive_only: true`. Without a matching decider, native harness behavior continues without a new wait. A streamed event is actionable only when `event.answerable` is `true`; use its `event.hold.prompt_id`. **Unanswered holds deny after 300s** (fail-safe timeout).

### Local hooks: exit codes and `when`/`respond` config

Daemonless local hooks (`~/.agent-hooks/hooks.json`, `<project>/.agent-hooks/hooks.json`) run shell commands with the normalized event JSON on stdin and use process exit codes:
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
        "when": { "tool": "ask" },
        "respond": {
          "allow": true,
          "answers": [{ "question_id": "favorite_color", "selected_options": ["Blue"] }]
        }
      }
    ],
    "PreToolUse": [
      {
        "when": { "prompt_matches": "rm -rf" },
        "respond": { "allow": false, "custom_input": "rm -rf is never auto-approved" }
      }
    ]
  }
}
```
`when.tool` is an exact case-insensitive tool-name match (`PreToolUse.tool_name` or `PermissionRequest.prompt.tool_name`); `when.prompt_matches` is a case-insensitive substring over the prompt description plus all question texts (the `PreToolUse` tool-input JSON as fallback). A matched `respond` answers directly with a canonical `PromptDecision`, without spawning `command` or requiring a daemon. A plain allow is an explicit approval, not a defer.

For structured app responses, `agent-hooks answer --return --decision JSON` can run as the local hook command: it reads the normalized `AgentEvent` from stdin, validates the canonical `PromptDecision` against the event's questions, and on success writes only canonical decision JSON to stdout and exits `0`. Validation/input errors produce an actionable stderr diagnostic, no stdout, and exit `2` (deny); this must not be treated as a generic nonzero infrastructure error, which is fail-open. The command is fully offline and does not connect to or start the daemon. Session IDs alone cannot rendezvous with offline hook invocations; there is no asynchronous offline queue.

As an additive convenience, the same utility accepts `--allow`, `--deny REASON`, `--select QUESTION_ID OPTION_LABEL`, or `--custom-input QUESTION_ID TEXT` instead of `--decision JSON`. Answer flags imply allow; repeat `--select` to answer multiple questions or select multiple options, and use `--custom-input` at most once per question that permits it. Multi-select questions may combine selections and custom text. Question IDs and option labels are exact, and text is preserved without trimming. All forms use the same canonical validation and produce the same decision JSON; conflicting verdicts and mixing simple flags with `--decision` are rejected. These flags add no new IDs or wire-protocol requirements.

To answer an already-held daemon prompt, `agent-hooks answer SESSION_ID --select color Blue` selects exactly one pending prompt for that session, while `agent-hooks answer --prompt-id ID --deny "Protected branch"` targets that exact prompt. The existing `--decision JSON` interface remains available with either selector, and a supplied session ID may additionally constrain a prompt ID. The CLI queries `prompt.list`, validates against the pending entry's `prompt.questions`, and sends `prompt.respond` with its authoritative `prompt_id`. A missing or ambiguous match, invalid decision, or RPC failure produces a stderr diagnostic and a nonzero exit status; success prints only the accepted canonical decision JSON. This one-shot command does not create a persistent decider subscription or make future gates wait. An app/decider must already be subscribed when events occur. Simple flags and `--decision` require `--return` or a target and conflict with `--auto`/`--line`. Existing interactive and automatic test modes are unchanged.

For single-select questions in the answer TUI, entering custom text replaces the selected option, and selecting an option clears custom input. Multi-select questions can combine options and custom text. A custom-only question cannot submit empty text, `custom: false` disables its editor, and entered text is preserved exactly without trimming.

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
    "answerable": true,
    "hold": {
      "prompt_id": "prompt_98765",
      "deadline_ms": 1724750700123,
      "verdicts": ["allow", "deny"],
      "interactive": false
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
    params: { decide: true }
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

    // Answer only held, answerable events using the authoritative hold ID.
    if (event.answerable === true) {
      const promptId = event.hold.prompt_id;
      const response = {
        jsonrpc: '2.0',
        id: Date.now(),
        method: 'prompt.respond',
        params: {
          prompt_id: promptId,
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
        "params": {"decide": True}
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

            if event.get("answerable") is True:
                prompt_id = event["hold"]["prompt_id"]
                # Respond only to this held event, with its authoritative ID.
                resp = {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "prompt.respond",
                    "params": {
                        "prompt_id": prompt_id,
                        "decision": {"allow": True}
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
