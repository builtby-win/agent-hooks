#!/bin/sh
# agent-hooks installer / bootstrap: detects OS/arch, downloads the matching
# release asset, verifies its sha256, installs to ~/.agent-hooks/bin, and
# registers hooks. Idempotent, so host apps can run it on every launch.
#
#   curl -fsSL https://github.com/builtby-win/agent-hooks/releases/latest/download/install.sh | sh
#
# Embedding it from an app (no download if a new-enough copy is already there):
#
#   curl -fsSL .../install.sh | sh -s -- --min-version 0.13.0 --json
#   # last stdout line: {"path":"…/agent-hooks","action":"kept","version":"…","harnesses":[…],"on_path":false}
#
# Env:   AGENT_HOOKS_VERSION=v0.12.0   pin a release (default: latest)
#        AGENT_HOOKS_HOME=...          install root (default: ~/.agent-hooks)
#        AGENT_HOOKS_BASE_URL=...      asset mirror (overrides GitHub Releases)
# Flags: --no-hooks                    place the binary only
#        --harness <id|all>            harness to register (default: all)
#        --min-version X.Y.Z           reuse the installed copy when it is at least this
#                                      version; otherwise download and install
#        --json                        print the self-install result as the last stdout line
set -eu

REPO="builtby-win/agent-hooks"
HARNESS="all"
HOOKS=1
MIN=""
JSON=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-hooks) HOOKS=0 ;;
    --harness) shift; HARNESS="${1:?--harness needs a value}" ;;
    --min-version) shift; MIN="${1:?--min-version needs a value}" ;;
    --json) JSON="--json" ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

# Args forwarded to `agent-hooks self-install`.
set -- --harness "$HARNESS"
[ "$HOOKS" = 1 ] || set -- --no-hooks
[ -z "$MIN" ] || set -- "$@" --min-version "$MIN"
[ -z "$JSON" ] || set -- "$@" "$JSON"

# Fast path: a new-enough copy is already installed, so skip the download and
# just (re)register hooks against it.
if [ -n "$MIN" ]; then
  existing="${AGENT_HOOKS_HOME:-$HOME/.agent-hooks}/bin/agent-hooks"
  if [ -x "$existing" ]; then
    have="$("$existing" --version 2>/dev/null | awk '{print $NF}')"
    if [ -n "$have" ] && awk -v a="$have" -v b="$MIN" 'BEGIN {
      split(a, x, "[.-]"); split(b, y, "[.-]");
      for (i = 1; i <= 3; i++) { if (x[i] + 0 > y[i] + 0) exit 0; if (x[i] + 0 < y[i] + 0) exit 1 }
      exit 0 }'; then
      exec "$existing" self-install "$@"
    fi
  fi
fi

err() { echo "agent-hooks install: $*" >&2; exit 1; }

command -v uname >/dev/null 2>&1 || err "missing required tool: uname"
command -v tar >/dev/null 2>&1 || err "missing required tool: tar"
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "need curl or wget"
fi

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux)  os="unknown-linux-musl" ;;  # static; works on glibc and musl distros
  *) err "unsupported OS: $(uname -s) (Windows: use install.ps1)" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64)
    arch="x86_64"
    # Rosetta shell on Apple Silicon reports x86_64; prefer the native build.
    if [ "$os" = "apple-darwin" ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ]; then
      arch="aarch64"
    fi
    ;;
  *) err "unsupported architecture: $(uname -m)" ;;
esac
target="${arch}-${os}"
asset="agent-hooks-${target}.tar.gz"

if [ -n "${AGENT_HOOKS_BASE_URL:-}" ]; then
  base="$AGENT_HOOKS_BASE_URL"
elif [ -n "${AGENT_HOOKS_VERSION:-}" ]; then
  base="https://github.com/${REPO}/releases/download/${AGENT_HOOKS_VERSION}"
else
  base="https://github.com/${REPO}/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "agent-hooks: downloading ${asset}" >&2
fetch "${base}/${asset}" "$tmp/$asset" || err "download failed: ${base}/${asset}"
fetch "${base}/${asset}.sha256" "$tmp/$asset.sha256" || err "checksum download failed"

expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
else
  err "need sha256sum or shasum to verify the download"
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || err "checksum mismatch for ${asset}"

tar -xzf "$tmp/$asset" -C "$tmp"
bin="$tmp/agent-hooks-${target}/agent-hooks"
[ -x "$bin" ] || err "archive did not contain agent-hooks"

# The binary owns placement (newest wins, atomic) and hook registration.
"$bin" self-install "$@"

dest="${AGENT_HOOKS_HOME:-$HOME/.agent-hooks}/bin"
case ":$PATH:" in
  *":$dest:"*) ;;
  *) echo "agent-hooks: add to PATH:  export PATH=\"$dest:\$PATH\"" >&2 ;;
esac
