---
name: release-agent-hooks
description: Cut a new agent-hooks release — bump workspace version, tag, build macOS (arm64, x64, universal) and Linux (musl x64 + arm64) locally with scripts/release-local.sh, let CI build Windows, then verify the assets and publish the SDKs.
---

# Release agent-hooks

Split build. **macOS + Linux are built locally on this Mac** by `scripts/release-local.sh`; **Windows is built by CI** (`.github/workflows/release.yml`, runs on tag push). npm/PyPI are published by `sdk-publish.yml` once every asset exists. Tag format: `v*`.

```mermaid
flowchart LR
    BUMP["bump Cargo.toml"] --> TEST["cargo test"] --> COMMIT["commit + tag"] --> PUSH["push tag → CI builds Windows"] --> LOCAL["scripts/release-local.sh: mac x3 + linux x2 → upload"] --> VERIFY["verify 16 assets + install.sh"] --> SDK["sdk-publish.yml"]
```

## 0. Preflight

```bash
git status --porcelain            # MUST be clean; stash or commit first
git branch --show-current         # MUST be main
grep '^version' Cargo.toml        # current version, e.g. 0.5.6
```

Pick the next version (semver): patch = fix, minor = feature, major =
breaking. New version below is `X.Y.Z`.

### Harness verification check

Ask the user: *"Have any supported harnesses shipped new versions since the
last release, or were new harnesses added? Were they verified live, and
against which version?"* For each:

- Offer to run the `verify-harness` skill for it first.
- Verified live: set `verified_against` in its `adapter.json` to that version
  (clear `unverified` if it was set).
- Not verified: leave it as is and mention it in the release notes.

Then `scripts/gen-adapter-table.py` and include the README change in the
release commit. Do not invent versions; only record what the user confirms.

## 1. Bump + commit

Single source of truth: root `Cargo.toml` `[workspace.package] version`
(all crates inherit via `version.workspace = true`).

```bash
# edit Cargo.toml: version = "X.Y.Z"
cargo check --workspace --locked  # refreshes Cargo.lock, catches breakage
cargo test --workspace            # MUST pass before tagging
git add Cargo.toml Cargo.lock README.md crates/agent-hooks-adapters/builtin
git commit -m "release: agent-hooks X.Y.Z"
```

## 2. Tag and push (starts the Windows build)

```bash
git tag vX.Y.Z
git push origin main vX.Y.Z
```

Tag push triggers `release.yml`, which builds **only Windows** (x64 + arm64) and attaches the zips, `.sha256` files, `install.sh` and `install.ps1`. It creates the release if the local upload hasn't yet.

## 3. Build + upload macOS and Linux locally, always on this Mac

```bash
scripts/release-local.sh
```

Builds aarch64 + x86_64 macOS natively, the universal binary via `lipo`, and static musl Linux x64 + arm64 via `cargo zigbuild`; packages into `dist/` (gitignored), smoke-tests every binary from its package (versions, `lipo -archs`, Rosetta, ELF headers), then `gh release upload --clobber`. It refuses to run if the tag is not on origin, so it never mints a tag from the default branch. `--no-upload` builds and tests only. Toolchain, one time: `brew install zig`, `cargo install cargo-zigbuild`, `rustup target add` the four macOS/Linux targets (the script names any that are missing).

## 4. Verify the release

```bash
gh release view vX.Y.Z --json assets --jq '.assets[].name' | sort
```

Expected: 16 assets, names are the contract `install.sh` / `install.ps1`
and package wrappers rely on —

```text
agent-hooks-{aarch64,x86_64}-apple-darwin.tar.gz(.sha256)
agent-hooks-universal-apple-darwin.tar.gz(.sha256)
agent-hooks-{aarch64,x86_64}-unknown-linux-musl.tar.gz(.sha256)
agent-hooks-{aarch64,x86_64}-pc-windows-msvc.zip(.sha256)
install.sh, install.ps1
```

Then prove the installer end to end against the real release:

```bash
H=$(mktemp -d); AGENT_HOOKS_HOME=$H/ah HOME=$H AGENT_HOOKS_VERSION=vX.Y.Z \
  sh <(curl -fsSL https://github.com/builtby-win/agent-hooks/releases/download/vX.Y.Z/install.sh) --no-hooks
$H/ah/bin/agent-hooks --version   # expect X.Y.Z
```

Run these smoke tests from a scratch directory, not the repo root. `self-install` only writes user-level configs (isolated by `HOME`/`AGENT_HOOKS_HOME`), but plain `agent-hooks install` also writes cwd-relative project configs (`.github/hooks/hooks.json`, `.muse/hooks.json`, ...) and will rewrite this repo's with temp paths.

If a name is wrong or an asset is missing, fix it and re-run
`scripts/release-local.sh` (it uploads with `--clobber`) for macOS/Linux, or
`workflow_dispatch` the `release` workflow with the tag for Windows.

## 4. SDK channels

Once all 16 assets are on the release, run `gh workflow run sdk-publish.yml -f tag=vX.Y.Z` (the script prints it). The workflow downloads the six archives the packers read and publishes `builtby.win-agent-hooks` (+ six platform packages) and `builtby-win-agent-hooks` wheels. Each job only runs when the repo variable `NPM_PUBLISH` / `PYPI_PUBLISH` is `true` (needs the `NPM_TOKEN` secret and a PyPI trusted publisher for `sdk-publish.yml`).

The Go SDK lives in a subdirectory module, so `go get` versions come from a prefixed tag; push it with the release tag:

```bash
git tag sdk/go/vX.Y.Z && git push origin sdk/go/vX.Y.Z
```
