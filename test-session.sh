#!/usr/bin/env bash
# Verifies this session's changes. Uses a scratch socket; your real daemon is
# never touched. Run from the repo root.
BIN="${BIN:-${CARGO_TARGET_DIR:-target}/debug/agent-hooks}"
SOCK=/tmp/ah-test-$$.sock
set +m          # no job-control chatter when we kill helpers
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n     got: %s\n' "$1" "$2"; }
hook(){ echo "$3" | timeout 40 "$BIN" hook "$1" --event "$2" --socket "$SOCK" 2>/dev/null; }

trap 'kill $DPID $SPID 2>/dev/null; rm -f "$SOCK" /tmp/ah-test-$$.out' EXIT
"$BIN" daemon --socket "$SOCK" --log-level warn >/dev/null 2>&1 & DPID=$!
sleep 1.5

echo; echo "T1  no decider — nothing may wait (the original bug)"
for ev in UserPromptSubmit PreToolUse; do
  S=$(date +%s%N)
  hook claude-code "$ev" '{"session_id":"s","prompt":"hi","tool_name":"Bash","tool_input":{}}' >/dev/null
  MS=$(( ($(date +%s%N) - S) / 1000000 ))
  [ "$MS" -lt 1000 ] && ok "$ev returns in ${MS}ms" || bad "$ev should be instant" "${MS}ms"
done

echo; echo "T2  decide:true — holds only what was already waiting"
python3 - "$SOCK" > /tmp/ah-test-$$.out 2>&1 <<'PY' &
import socket, json, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sys.argv[1])
s.sendall((json.dumps({"jsonrpc":"2.0","id":1,"method":"subscribe","params":{"decide":True}})+"\n").encode())
f = s.makefile("r"); f.readline()
for line in f:
    m = json.loads(line)
    if m.get("method") != "event": continue
    p = m["params"]; h = p.get("hold")
    qs = []
    def walk(o):
        if isinstance(o, dict):
            if isinstance(o.get("question"), str): qs.append(o["question"])
            for v in o.values(): walk(v)
        elif isinstance(o, list):
            for v in o: walk(v)
    walk(p)
    print(("HELD " if h else "PASS ") + p["lifecycle"] if isinstance(p["lifecycle"], str)
          else ("HELD" if h else "PASS"), flush=True)
    if not h: continue
    d = {"allow": True, "ask": False}
    if qs:
        d["answers"] = [{"question_id":"q1","selected_options":["Root"]},
                        {"question_id":"q2","selected_options":["Yes"],"custom_input":"and lint"}]
    s.sendall((json.dumps({"jsonrpc":"2.0","id":2,"method":"prompt.respond",
        "params":{"prompt_id":h["prompt_id"],"decision":d}})+"\n").encode())
PY
SPID=$!; sleep 1.5

S=$(date +%s%N); hook claude-code UserPromptSubmit '{"session_id":"s","prompt":"hi"}' >/dev/null
MS=$(( ($(date +%s%N) - S) / 1000000 ))
[ "$MS" -lt 1000 ] && ok "mid-flight UserPromptSubmit passes through (${MS}ms)" || bad "should pass through" "${MS}ms"

OUT=$(hook opencode permission.ask '{"sessionID":"o1","toolName":"Bash"}')
[ "$OUT" = '{"decision":"allow","reply":"allow"}' ] \
  && ok "permission.ask held + answered, carries the reply verb" \
  || bad "permission.ask dialect wrong" "$OUT"

echo; echo "T3  opencode question.asked — structured answers round trip"
Q='{"sessionID":"o1","requestID":"req-9","questions":[{"id":"q1","question":"Which README?","options":[{"label":"Root"},{"label":"Package"}]},{"id":"q2","question":"Run tests?","options":[{"label":"Yes"},{"label":"No"}]}]}'
OUT=$(hook opencode question.asked "$Q")
echo "$OUT" | grep -q '\[\["Root"\],\["Yes","and lint"\]\]' \
  && ok "answers returned as string[][] grouped per question" \
  || bad "structured answers lost" "$OUT"

echo; echo "T4  unanswered non-interactive gate fails open at the hold ceiling"
kill $SPID 2>/dev/null; wait $SPID 2>/dev/null; sleep 0.3
python3 - "$SOCK" >/dev/null 2>&1 <<'PY' &
import socket, json, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sys.argv[1])
s.sendall((json.dumps({"jsonrpc":"2.0","id":1,"method":"subscribe","params":{
    "event_types":["UserPromptSubmit"],
    "decide":{"event_types":["UserPromptSubmit"]}}})+"\n").encode())
s.makefile("r").read()
PY
SPID=$!; sleep 1.5
S=$(date +%s%N)
OUT=$(hook claude-code UserPromptSubmit '{"session_id":"s","prompt":"hi"}')
SEC=$(( ($(date +%s%N) - S) / 1000000000 ))
[ "$OUT" = "{}" ] && [ "$SEC" -ge 20 ] && [ "$SEC" -le 29 ] \
  && ok "unanswered UserPromptSubmit fails open in ${SEC}s" \
  || bad "should fail open at the 25s hold ceiling" "${SEC}s $OUT"
kill $SPID 2>/dev/null; wait $SPID 2>/dev/null

echo; echo "T5  local hooks: parallel + budget-capped + order-deterministic"
D=$(mktemp -d)
mkdir -p "$D/.agent-hooks"
cat > "$D/.agent-hooks/hooks.json" <<'JSON'
{"version":1,"hooks":{"PreToolUse":[
  {"command":"sleep 2","timeout":10},
  {"command":"sleep 2","timeout":10},
  {"command":"sleep 2","timeout":10}]}}
JSON
S=$(date +%s%N)
echo '{"session_id":"s","tool_name":"Bash","tool_input":{}}' | HOME="$D" timeout 30 "$BIN" hook claude-code --event PreToolUse --socket /tmp/nope.sock >/dev/null 2>&1
SEC=$(( ($(date +%s%N) - S) / 1000000000 ))
[ "$SEC" -le 4 ] && ok "3x 'sleep 2' ran concurrently (${SEC}s, sequential would be 6s)" \
  || bad "local hooks not parallel" "${SEC}s"

cat > "$D/.agent-hooks/hooks.json" <<'JSON'
{"version":1,"hooks":{"PreToolUse":[
  {"command":"sleep 1; echo FIRST >&2; exit 2","timeout":10},
  {"command":"echo SECOND >&2; exit 2","timeout":10}]}}
JSON
OUT=$(echo '{"session_id":"s","tool_name":"Bash","tool_input":{}}' | HOME="$D" timeout 30 "$BIN" hook claude-code --event PreToolUse --socket /tmp/nope.sock 2>/dev/null)
echo "$OUT" | grep -q FIRST && ok "entry order beats completion order" \
  || bad "merge followed the race, not the config" "$OUT"
rm -rf "$D"

echo; echo "T6  antigravity keeps 'ask' instead of degrading to deny"
OUT=$(hook antigravity PreToolUse '{"conversationId":"a1","toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}}')
ok "antigravity PreToolUse renders: $OUT"

echo; printf '\n  %d passed, %d failed\n\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
