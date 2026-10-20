---
name: verify-harness
description: End-to-end live verification of an agent-hooks adapter. Opens a herdr or tmux pane, starts the real harness CLI, sends a prompt, triggers a tool permission and a question, answers them through agent-hooks, asserts the event log and the harness screen at every step, quits the agent and checks it unregisters. Then runs the offline × daemon answer matrix (id-matched/reordered answers, free text, invalid-answer rejects, timeout inheritance, per-adapter channel fidelity). Ends by asking the user before recording verified_against.
---

# Verify a harness live (end to end)

Run after `onboard-harness`, when a harness ships a new version, or from the
release checklist. It only reports; `unverified` / `verified_against` change
only after the user confirms (§5).

```mermaid
flowchart LR
    S["panes: daemon · decider · harness"] --> A["start + prompt"] --> B["tool permission allow/deny"] --> C["question + answer"] --> D["daemon down"] --> E["quit → SessionEnd"] --> R["report → ask → record"]
```

## 0. Setup

Backend: `herdr` when `HERDR_ENV=1` (herdr CLI), else `tmux`; force with
`MUX=tmux|herdr`. Source the helper, which gives one API over both
(`mux_new_pane mux_run mux_text mux_key mux_read mux_wait mux_close mux_cleanup`):

```bash
. skills/verify-harness/mux.sh
```

tmux runs on its **own server** (`tmux -L ahv`); it never touches the user's
default tmux server. Never run `tmux kill-server` without `-L ahv`, and never
close herdr panes you did not create (record the ids `mux_new_pane` returns).

```bash
cargo build -p agent-hooks-cli
AH=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/agent-hooks
ID=<adapter id>
WORK=$(mktemp -d); SOCK=$WORK/ah.sock; LOG=$WORK/events.log; DEC=$WORK/decider.log
echo "hello from verify" > "$WORK/notes.txt"      # file the agent will read
$AH adapter validate crates/agent-hooks-adapters/builtin/$ID/adapter.json
```

Never reuse `/tmp/agent-hooks.sock`. **Do not install into the user's real
harness config.** It is usually already hooked by another consumer (e.g. a
desktop app on `/tmp/agent-hooks-dev.sock`), and rewriting it would hook every
other running session of that harness and let your decider hold or deny *their*
tool calls. Check first (`grep agent-hooks ~/.claude/settings.json`; `$AH status`).
Install into the temp project only: run `install` from `$WORK` with `HOME`
pointed at a scratch dir (`~` paths land in `$WORK/home`, project paths in
`$WORK`), and start the daemon with `dev --no-install`. Start the harness with
the real `HOME` so its login works. The user's existing global hooks will still
fire for the test session; that is expected noise.
If registry tests misbehave after adding adapters: `touch
crates/agent-hooks-adapters/build.rs` (stale generated manifests in a shared
cargo target dir).

Assertion helper (events are the source of truth, the screen is the second check):

```bash
expect() { # expect "<regex>" [timeout_s]  -> grep the event log
  local i=0; while [ $i -lt "${2:-30}" ]; do grep -Eq -- "$1" "$LOG" && { echo "PASS $1"; return 0; }; sleep 1; i=$((i+1)); done
  echo "FAIL $1"; return 1; }
```

## 1. Panes

```bash
P_DAEMON=$(mux_new_pane "$WORK");  P_DECIDER=$(mux_new_pane "$WORK");  P_AGENT=$(mux_new_pane "$WORK")
mkdir -p "$WORK/home"
(cd "$WORK" && HOME="$WORK/home" $AH install --harness $ID --socket $SOCK --binary-path $AH)
# install writes only the first (~) path; copy it to the project path the harness will load
# (Claude Code: mkdir -p $WORK/.claude && cp $WORK/home/.claude/settings.json $WORK/.claude/settings.json)
mux_run $P_DAEMON "$AH dev --no-install --socket $SOCK 2>&1 | tee $LOG"
mux_wait $P_DAEMON "ready (ping ok)" 30
```

`dev --no-install` starts the daemon and prints
`[event] <Lifecycle> harness= session= status= cwd=` lines (that is what
`expect` greps). Start the decider per scenario (§3) in `$P_DECIDER`; stop it
with `mux_key $P_DECIDER ctrl-c` before starting the next verdict.

**Manual permission mode is required.** Start the harness with its default
approval mode: no bypass/yolo/auto-accept/skip-permissions flags, no
"accept edits" or "always allow" state, and no pre-approved tools in its
settings. In an auto-approving mode the harness never raises a permission
request, so steps 4-5 would pass or fail vacuously. Check the screen before
step 1 (mode indicator, or run one write and see that the harness asks). If it
cannot run in manual mode, mark steps 4-5 SKIP.
Claude Code: the status line names the mode (`bypass permissions on`, `auto mode
on`, `accept edits on`, `manual mode on`); `shift+tab` cycles them, so press it
until it reads `manual mode`. In a narrow split the line is truncated: toggle
`herdr pane zoom $P_AGENT` before reading, and toggle back. Accept the project
trust dialog for the scratch dir (`down`, `enter`).

Start the harness in `$P_AGENT`, inside `$WORK`, after install (restart it if its
docs say hooks load at startup):

- **herdr, supported kind** (`herdr agent` lists kinds): `herdr agent start verify --kind <kind> --pane $P_AGENT` then drive with
  `herdr agent prompt verify "…" --wait --timeout 120000`. `herdr agent get verify`
  reports `idle | working | blocked | done | unknown`: use it as a second status
  check (`blocked` = herdr recognised an approval or question dialog).
- **anything else (or tmux)**: `mux_run $P_AGENT "cd $WORK && <cli>"`, then `mux_text` + `mux_key enter` for prompts,
  `mux_wait` for screen text.

## 2. Scenarios, with assertions at each step

Run in order; record PASS/FAIL/SKIP and the evidence line for each.

| # | Step | Drive | Assert (events) | Assert (screen / state) |
| :-- | :-- | :-- | :-- | :-- |
| 1 | Start | open the CLI | `SessionStart` for `harness=$ID` | prompt visible; herdr: `idle` |
| 2 | Prompt | decider `--auto allow`; send "Reply with the word PONG" | `UserPromptSubmit`, then `Stop` | screen contains `PONG`; herdr `working` → `idle` |
| 3 | Tool, allowed | "Read notes.txt and repeat it" | `PreToolUse` then `PostToolUse`, same session id | screen shows the file text |
| 4 | Permission allow | "Create a.txt containing x" | `PermissionRequest` (or gating `PreToolUse`) held then released; decider log shows `auto → allow` | `$WORK/a.txt` exists |
| 5 | Permission deny | restart decider with `--auto deny:blocked by verify`; "Create b.txt" | same event, decider log `auto → deny` | `b.txt` does NOT exist; harness shows the deny reason |
| 6 | Question asked | decider `--auto answer:blue`; "Use your ask-question tool to ask which colour: red or blue, then print only the colour" | `PermissionRequest` with `q[...]` in the decider log (questions lifted into `prompt.questions`) | herdr: `blocked` while held |
| 7 | Question answered | (same) | decider log `auto answers → [{"question_id":…,"selected_options":["blue"]}]` | **agent's reply contains `blue`**: the answer reached the model |
| 7b | Free-text answer | decider `--auto answer:purple` (not one of the offered options); same ask-question prompt | decider log shows `custom_input":"purple"` with empty `selected_options` | **agent's reply contains `purple`**, or the harness visibly rejected free text (record which) |
| 8 | Stop/continue | **SKIP for now**: the decider sees `Stop` (it drives `status=Idle`, asserted in steps 2-5) but `answer` does not hold it for a verdict, because `is_gate` omits `Stop`, so it cannot send "continue". Once it can, and only if the manifest has `continue` capability: decider `--auto deny:keep going` at `Stop` | second turn starts | agent keeps working instead of stopping |
| 9 | Daemon down | stop pane A's daemon (`mux_key $P_DAEMON ctrl-c`), send "Reply PONG2" | (no events) | agent still answers `PONG2`: never blocked |
| 10 | Restart daemon, quit | restart `dev` (same `$SOCK`), then quit the CLI (`/exit`, `ctrl-d`, or `ctrl-c ctrl-c` per harness) | `SessionEnd` for that session | process gone; herdr: pane back to a shell, agent name released |

Notes:
- For #6 the harness must expose a question tool (`ask_tools` in the manifest,
  e.g. Claude Code `AskUserQuestion`). SKIP with a reason if it has none.
  `answer:<text>` selects an option whose label matches `<text>` (case-insensitive)
  and otherwise sends `<text>` as free text. Needs a decider build that has it
  (`$AH answer --help` shows `answer:`).
- Assert on the model's reply line, not just any screen match: the typed prompt
  also contains the word you are looking for (Claude Code replies start with `⏺ `;
  a count of 1 means only your own prompt matched).
- The decider may log `respond failed … Prompt not found` for a `PreToolUse`
  hold (seen on Claude Code Read and Write). Harmless when the `PermissionRequest`
  for the same call is answered and the tool behaves correctly; note it, don't fail on it.
- `custom: true` is **global**: `declarative.rs` sets it on every lifted
  question for every adapter; no manifest field controls it. So whether a
  harness accepts free text is only learned here (7b). On a reject or silent
  drop, report it as a finding: the fix is a per-manifest opt-out (e.g.
  `ask_tools` flag that sets `custom: false`) and not a verified pass. Copilot's
  `ask_user` is the known unverified case.
- #10 is the unregister check: the session ends and a consumer can drop it. If
  the harness has no session-end hook (manifest has no `SessionEnd` mapping or
  `unavailable` lists it), mark "no unregister hook" as an expected SKIP, not a failure.
- A hang with no `[event]` line means the hook never fired: wrong config path,
  harness not restarted, or an unaccepted project-trust prompt. Capture it with
  `mux_read $P_AGENT 80` and record it as a finding (maybe a `notes.post_install` fix).
- Optionally add `--capture-fixtures $WORK/fixtures` to the installed hook
  command (see `onboard-harness` §2) so real payloads become tests.
- Gate holds time out to deny after 300s; if pane C stalls, check the decider pane first.

## 3. Decider cheat sheet

```bash
mux_run $P_DECIDER "$AH answer --socket $SOCK --auto allow 2>&1 | tee -a $DEC"
# swap verdict: mux_key $P_DECIDER ctrl-c, then one of:
#   --auto deny:blocked by verify
#   --auto answer:blue
#   --auto ask:need a human            (escalates; harness shows its native dialog)
```

## 4. Teardown (always, even on failure)

```bash
(cd "$WORK" && HOME="$WORK/home" $AH uninstall --harness $ID)   # scratch config only; rm -rf below removes it anyway
mux_key $P_AGENT ctrl-c; mux_close $P_AGENT; mux_close $P_DECIDER; mux_close $P_DAEMON
mux_cleanup           # tmux: kills only the -L ahv server
rm -rf "$WORK"
```

## 5. Report, then ask

Print the table (§2 steps and §6 matrix rows: step, PASS/FAIL/SKIP, evidence). Then ask the user with
AskUserQuestion; don't assume:

1. "Record this as verified?": verified against `<cli> --version` · verified
   with gaps (list skipped/failed steps in the manifest `notes`) · not verified.
2. Only on a yes: set `verified_against` on the version spec, clear
   `unverified` if set, run `scripts/gen-adapter-table.py`, and commit the
   manifest and README together.

A FAIL is an adapter bug, not "unverified": fix the manifest or fixtures and
re-run the failed steps.

## 6. Answer matrix (offline × daemon)

Run after §2, before §4–§5; rows go into the §5 table. One decision language
everywhere (canonical `PromptDecision` — config `respond`, command stdout,
daemon `prompt.respond`); `resolve_questions` matches by `question_id` only,
reorders into request order, requires every question exactly once, validates
exact option labels, keeps text verbatim. Invalid answers fail closed:
daemon RPC errors and the hold stays pending (retryable); offline and adapter
rendering deny with a reason.

Setup (no harness needed for O/F rows — the real shim, no daemon):

```bash
MX=$WORK/matrix; mkdir -p $MX
Q='{"sessionID":"mx1","requestID":"req-mx1","questions":[{"id":"q0","question":"Pick a color","options":[{"label":"Red"},{"label":"Blue"}]},{"id":"q1","question":"Name it","options":[{"label":"my-app"}]}]}'
qhook() { printf '%s' "$Q" | HOME=$MX timeout 30 $AH hook opencode --event question.asked --socket $MX/none.sock; }
# per row: write $MX/.agent-hooks/hooks.json (mkdir -p first), run qhook, assert stdout JSON (stderr carries the daemon-unreachable note + deny reasons)
```

| # | Case | Drive | Assert |
| :-- | :-- | :-- | :-- |
| O1 | canned allow | `respond: {"allow": true}` on `PermissionRequest` | allow dialect, no answers (plain allows used to be discarded) |
| O2 | reversed order | answers `[q1, q0]` | output ordered `q0, q1` (never positional) |
| O3 | label collision | `q1` `custom_input: "my-app"` (= its option label) | stays `custom_input`, not re-derived into a pick |
| O4 | ghost id | `answers: [{question_id: "ghost", …}]` | deny, reason `missing answer for "q0"` |
| O5 | bad label | `q0` picks `["Mango"]` | deny, reason `"Mango" is not an option (valid: ["Red","Blue"])` |
| O6 | double pick, single-select | `q0` picks `["Red","Blue"]` | deny, reason `single-select: give exactly one…` |
| O7 | verbatim text | 5k-char padded `custom_input` | byte-identical round trip (no trim/truncate; old cap was 2000) |
| O8 | timeout inheritance | no-`timeout` `sleep 30` entry; `hook codex --event SessionEnd` (3s window) | `exceeded 1s, killed`; wall ~1s, not 10s. Explicit `timeout` still respected, capped by the window |
| O9 | stdout verdict | command prints a decision, exit 0 | rendered verdict (needs an `allow` key; stale `{decision:…}` shapes are warned + ignored) |
| O10 | exit codes | exit 2 with stderr / exit 3 | deny with the stderr reason / ask (beats stdout) |
| O11 | oversize stdout | >1 MiB, exit 0 | deny with size error, never parsed |
| O12 | mailbox human | §6d below: script holds, human pane drops the verdict file | rendered dialect; answer inside the entry timeout or the kill discards it (fail-open) |

O1–O7 mirror unit tests (`protocol resolve_questions`, `commands::user_hooks`,
`manifest_opencode`); O8 mirrors `omitted_timeout_inherits_the_harness_window`.
Extend those when adding rows — the skill rows are the live proof, not the
regression net.

Daemon rows (second daemon `SOCK2=$WORK/d.sock`, started per §1; decider in a
pane: `mux_run $P_DECIDER "python3 $MX/decider.py $SOCK2 $MX"`; invoke the
shim under test with `--socket $SOCK2` so the gate forwards to the daemon.
The invoking shim staying blocked while a hold is pending is the
retryability signal — drop each verdict file promptly (the shim's daemon wait
is bounded; if it releases with `{}`, you were too slow — re-fire):

| # | Case | Drive | Assert |
| :-- | :-- | :-- | :-- |
| D1 | bad label rejected, retryable | verdict `q0: ["Mango"]` → decider log shows RPC error `-32000 Invalid decision…`; shim still blocked | corrected `q0: ["Blue"]` → shim releases with ordered answers |
| D2 | reversed answers | verdict `[q1, q0]` | delivered in request order |
| D3 | ghost id | verdict `ghost` → RPC error, still blocked | corrected retry releases |
| D4 | custom verbatim | padded mixed-case token | token and case exact at the consumer |

```python
# $MX/decider.py: subscribe decide:true, log holds, answer from $MX/next.json
import asyncio, json, sys
SOCK, BOX = sys.argv[1], sys.argv[2]
def find(o, k):
    if isinstance(o, dict):
        if k in o: return o[k]
        for v in o.values():
            r = find(v, k)
            if r is not None: return r
    elif isinstance(o, list):
        for v in o:
            r = find(v, k)
            if r is not None: return r
async def main():
    r, w = await asyncio.open_unix_connection(SOCK)
    seq, pending = 0, None
    async def send(method, params):
        nonlocal seq; seq += 1
        w.write((json.dumps({"jsonrpc": "2.0", "id": seq, "method": method, "params": params}) + "\n").encode()); await w.drain()
    await send("subscribe", {"decide": True})
    import os
    while True:
        try: line = await asyncio.wait_for(r.readline(), 0.5)
        except asyncio.TimeoutError: line = b""
        if line:
            try: msg = json.loads(line)
            except Exception: continue
            pid = find(msg, "prompt_id")
            if pid and "result" not in msg and "error" not in msg:
                pending = pid; print("HOLD", pid, json.dumps(find(msg, "questions") or "?")[:200], flush=True)
            else: print("RPC", str(msg)[:200], flush=True)
        vf = os.path.join(BOX, "next.json")
        if pending and os.path.exists(vf):
            await send("prompt.respond", {"prompt_id": pending, "decision": json.load(open(vf))})
            os.remove(vf)
asyncio.run(main())
```

Channel fidelity (expected loss per adapter — assert the shape, not equality
beyond it; `adapter test` replays the repo fixtures for the same paths):

| adapter | channel | fidelity |
| :-- | :-- | :-- |
| omp, pi | `{answers_structured}` | lossless: ids, picks, free text |
| opencode | `{answer_groups}` (`string[][]`) | request-ordered; loses which entry is free text |
| claude-code, muse, qwen-code | `updatedInput` map keyed by question text | multi-pick comma-joined; duplicate question text → deny |
| codex | deny block reason | prose only |
| all | `list --json` answer block | `questions_native` per manifest (native vs prose) |

Live harness rows (only where the harness actually fires hooks — §2 covers
allow/deny; this is the question path):

| # | Case | Drive | Assert |
| :-- | :-- | :-- | :-- |
| L1 | native question → decider → model | ask-question prompt; answer via decider/TUI | model continues with the exact answer |
| L2 | no-fire diagnosis | nothing in hooks/log after a question + tool call | record harness version + which events never arrive (opencode v2.0.28: V2 `setup()` never registers `tool.execute.before`/`permission.ask`, and no `form.created` reaches the plugin — offline gates never fire there) |

### 6d. Mailbox human-in-the-loop (offline, two panes)

The script cannot use the TUI (`answer` is daemon-only: no daemon, no backlog,
nothing to address). Instead it parks the verdict in a file the human pane
completes. Set the entry `timeout` past the harness window you need (see O8).

```bash
# hook command (ask-human.sh pattern): stdin event -> $MX/mailbox/req-<id>.json
# (questions with real ids/options), poll resp-<id>.json, cat it to stdout.
# human pane ($P_HUMAN): show each req without a resp, read the pick from
# /dev/tty (heredoc stdin is the script — `input()` reads EOF there), write
# {"allow": true, "answers": [{question_id, selected_options|custom_input}]}.
mux_run $P_HUMAN "$MX/human.sh"
# fire the hook (live harness or qhook with a holding command), then type the
# pick into the human pane:
mux_text $P_HUMAN "Blue"; mux_key $P_HUMAN enter
# assert: shim stdout is the rendered dialect with the picked answer, exit 0
```

Timeout kill is fail-open and discards late verdicts: a human answer must land
inside the entry timeout. If it cannot, that setup needs the daemon (300s
holds, retryable rejects) — say so instead of stretching offline past the
harness kill.
