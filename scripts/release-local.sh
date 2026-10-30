#!/usr/bin/env bash
# Build and upload the macOS + Linux release assets from this Mac.
#
#   scripts/release-local.sh [--tag vX.Y.Z] [--no-upload]
#
# Builds, always, in this order:
#   aarch64-apple-darwin, x86_64-apple-darwin      native cargo
#   universal-apple-darwin                         lipo of the two above
#   x86_64/aarch64-unknown-linux-musl              cargo-zigbuild (static, any distro)
# Windows is built by the `release` workflow when the tag is pushed.
#
# Output: dist/agent-hooks-<target>.tar.gz + .sha256 (gitignored), each holding
# agent-hooks-<target>/{agent-hooks,README.md,LICENSE-*}. These names are the
# contract install.sh, install.ps1 and the npm/PyPI packers rely on.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=agent-hooks
PACKAGE=agent-hooks-cli
MAC_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
LINUX_TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
UNIVERSAL=universal-apple-darwin

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="v$version"
upload=1
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) tag="$2"; shift 2 ;;
    --no-upload) upload=0; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
want="${tag#v}"
[ "$want" = "$version" ] || { echo "tag $tag does not match Cargo.toml version $version" >&2; exit 1; }

[ "$(uname -s)" = Darwin ] || { echo "run this on macOS (lipo + native macOS builds)" >&2; exit 1; }
for tool in cargo cargo-zigbuild zig lipo shasum file; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool (brew install zig; cargo install cargo-zigbuild)" >&2; exit 1; }
done
installed=$(rustup target list --installed)
for t in "${MAC_TARGETS[@]}" "${LINUX_TARGETS[@]}"; do
  grep -qx "$t" <<<"$installed" || { echo "missing rust target: rustup target add $t" >&2; exit 1; }
done
if [ "$upload" = 1 ]; then
  command -v gh >/dev/null || { echo "missing tool: gh" >&2; exit 1; }
  # Never let `gh release create` mint the tag from the default branch.
  git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null \
    || { echo "tag $tag is not on origin; push it first (the push also starts the Windows build)" >&2; exit 1; }
fi

target_dir=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

rm -rf dist
mkdir dist

for t in "${MAC_TARGETS[@]}"; do
  cargo build --release --locked -p "$PACKAGE" --target "$t"
done
for t in "${LINUX_TARGETS[@]}"; do
  cargo zigbuild --release --locked -p "$PACKAGE" --target "$t"
done

# Universal binary: one file a Mac app can drop into Contents/Helpers.
mkdir -p "$target_dir/$UNIVERSAL/release"
lipo -create -output "$target_dir/$UNIVERSAL/release/$BIN" \
  "$target_dir/aarch64-apple-darwin/release/$BIN" \
  "$target_dir/x86_64-apple-darwin/release/$BIN"

package() {
  local t="$1" stage="dist/$BIN-$1"
  mkdir "$stage"
  cp "$target_dir/$t/release/$BIN" "$stage/"
  cp README.md LICENSE-MIT LICENSE-APACHE "$stage/"
  tar -czf "$stage.tar.gz" -C dist "$BIN-$t"
  (cd dist && shasum -a 256 "$BIN-$t.tar.gz" > "$BIN-$t.tar.gz.sha256")
}
for t in "${MAC_TARGETS[@]}" "$UNIVERSAL" "${LINUX_TARGETS[@]}"; do package "$t"; done

# Smoke test: every binary, from the packaged copy.
fail() { echo "smoke test failed: $*" >&2; exit 1; }
arm="dist/$BIN-aarch64-apple-darwin/$BIN"
[ "$("$arm" --version)" = "$BIN $want" ] || fail "arm64 --version != $BIN $want"
"$arm" list-adapters --json >/dev/null || fail "arm64 list-adapters"
uni="dist/$BIN-$UNIVERSAL/$BIN"
[ "$("$uni" --version)" = "$BIN $want" ] || fail "universal --version != $BIN $want"
archs=$(lipo -archs "$uni")
[[ "$archs" == *arm64* && "$archs" == *x86_64* ]] || fail "universal archs: $archs"
if arch -x86_64 true 2>/dev/null; then
  [ "$(arch -x86_64 "dist/$BIN-x86_64-apple-darwin/$BIN" --version)" = "$BIN $want" ] || fail "x86_64 --version under Rosetta"
else
  file "dist/$BIN-x86_64-apple-darwin/$BIN" | grep -q x86_64 || fail "x86_64 Mach-O header"
fi
file "dist/$BIN-x86_64-unknown-linux-musl/$BIN" | grep -q 'ELF 64-bit.*x86-64' || fail "linux x64 ELF"
file "dist/$BIN-aarch64-unknown-linux-musl/$BIN" | grep -q 'ELF 64-bit.*ARM aarch64' || fail "linux arm64 ELF"
echo "built and smoke-tested:"; ls dist/*.tar.gz

[ "$upload" = 1 ] || { echo "--no-upload: nothing pushed"; exit 0; }

gh release view "$tag" >/dev/null 2>&1 || gh release create "$tag" --generate-notes
gh release upload "$tag" --clobber dist/*.tar.gz dist/*.tar.gz.sha256

expected=(
  "$BIN-aarch64-apple-darwin.tar.gz" "$BIN-x86_64-apple-darwin.tar.gz" "$BIN-$UNIVERSAL.tar.gz"
  "$BIN-x86_64-unknown-linux-musl.tar.gz" "$BIN-aarch64-unknown-linux-musl.tar.gz"
  "$BIN-x86_64-pc-windows-msvc.zip" "$BIN-aarch64-pc-windows-msvc.zip"
)
have=$(gh release view "$tag" --json assets --jq '.assets[].name')
missing=0
for a in "${expected[@]}"; do
  grep -qx "$a" <<<"$have" || { echo "not on release yet: $a"; missing=1; }
done
if [ "$missing" = 1 ]; then
  echo "Windows assets come from the release workflow (gh run watch). Re-run this check with: gh release view $tag --json assets --jq '.assets[].name'"
else
  echo "all platform assets present. Publish the SDKs: gh workflow run sdk-publish.yml -f tag=$tag"
fi
