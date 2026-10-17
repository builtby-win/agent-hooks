# Source me: `. skills/verify-harness/mux.sh`
# One tiny API over tmux and herdr so the verify flow is backend-agnostic.
#   MUX=tmux|herdr   force a backend (default: herdr when HERDR_ENV=1, else tmux)
#   mux_new_pane [cwd]          print the new pane id (tmux: session "ahv" is created on first call)
#   mux_run  PANE "command"     type the command and press Enter
#   mux_text PANE "text"        type literal text, no Enter
#   mux_key  PANE key...        enter esc tab up down left right ctrl-c ctrl-d (or a single char)
#   mux_read PANE [lines]       print recent pane text
#   mux_wait PANE "substr" [s]  poll until substr appears (default 60s); returns 1 on timeout
#   mux_close PANE
#   mux_cleanup                 tear down ONLY what this helper created (the ahv tmux server; herdr panes are closed per-pane)

: "${MUX:=$([ "${HERDR_ENV:-}" = 1 ] && command -v herdr >/dev/null && echo herdr || echo tmux)}"
AHV_SESSION=${AHV_SESSION:-ahv}

# Dedicated tmux server (-L ahv): never touches the user's default tmux server or sessions.
_mux_t() { command tmux -L "${AHV_SOCKET:-ahv}" "$@"; }

_mux_key_name() {  # normalized key -> backend key name
  case "$MUX:$1" in
    tmux:enter) echo Enter;; tmux:esc) echo Escape;; tmux:tab) echo Tab;;
    tmux:up) echo Up;; tmux:down) echo Down;; tmux:left) echo Left;; tmux:right) echo Right;;
    tmux:ctrl-c) echo C-c;; tmux:ctrl-d) echo C-d;;
    herdr:ctrl-c) echo ctrl+c;; herdr:ctrl-d) echo ctrl+d;;
    *) echo "$1";;
  esac
}

mux_new_pane() {
  local cwd=${1:-$PWD}
  if [ "$MUX" = herdr ]; then
    herdr pane split --current --direction right --cwd "$cwd" --no-focus \
      | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["pane"]["pane_id"])'
  elif _mux_t has-session -t "$AHV_SESSION" 2>/dev/null; then
    _mux_t split-window -d -P -F '#{pane_id}' -t "$AHV_SESSION" -c "$cwd"
  else
    _mux_t new-session -d -s "$AHV_SESSION" -x 200 -y 50 -c "$cwd" -P -F '#{pane_id}'
  fi
}

mux_run() {
  if [ "$MUX" = herdr ]; then herdr pane run "$1" "$2" >/dev/null
  else _mux_t send-keys -t "$1" -l -- "$2"; _mux_t send-keys -t "$1" Enter; fi
}

mux_text() {
  if [ "$MUX" = herdr ]; then herdr pane send-text "$1" "$2" >/dev/null
  else _mux_t send-keys -t "$1" -l -- "$2"; fi
}

mux_key() {
  local pane=$1 k; shift
  for k in "$@"; do
    k=$(_mux_key_name "$k")
    if [ "$MUX" = herdr ]; then herdr pane send-keys "$pane" "$k" >/dev/null
    else _mux_t send-keys -t "$pane" "$k"; fi
  done
}

mux_read() {
  if [ "$MUX" = herdr ]; then herdr pane read "$1" --source recent-unwrapped --lines "${2:-120}"
  else _mux_t capture-pane -p -J -t "$1" -S "-${2:-120}"; fi
}

mux_wait() {
  local pane=$1 needle=$2 timeout=${3:-60} i=0
  while [ "$i" -lt "$timeout" ]; do
    mux_read "$pane" 200 2>/dev/null | grep -qF -- "$needle" && return 0
    sleep 1; i=$((i + 1))
  done
  return 1
}

mux_close() {
  if [ "$MUX" = herdr ]; then herdr pane close "$1" >/dev/null
  else _mux_t kill-pane -t "$1" 2>/dev/null; fi
}

mux_cleanup() {
  [ "$MUX" = tmux ] && _mux_t kill-server 2>/dev/null   # -L ahv server only
  return 0
}
