# Vibe Island support audit

Research snapshot: 2026-10-12. This compares the [Vibe Island site](https://vibeisland.app/) with agent-hooks' built-in manifests. A product's marketing claim is not proof of its hook API, response dialect, or two-way approval support. Recommendations below distinguish upstream documentation from unverified integration details.

## Coverage

agent-hooks has 19 built-ins after adding Muse, Droid, Qwen Code, Qoder, Kimi Code, and Mistral Vibe: 17 overlap with Vibe Island's advertised list; JCode and Muse are additional integrations. Hermes was already implemented but missing from the README. Vibe Island advertises 30 names, including both “Kimi Code” and “Kimi”; its linked Kimi page does not establish that these are separate integrations.

The five new adapters ship with `"unverified": true`: built from the upstream hook references linked below, covered by docs-derived unit tests, but never exercised against the live CLIs. `list-adapters` and `status` flag them; graduate one by capturing live fixtures and clearing the flag. Copilot's `permissionRequest`/`ask_user` paths are likewise docs-only additions on an otherwise live-tested adapter.

| Advertised agent | agent-hooks adapter | Remaining work or integration boundary |
| --- | --- | --- |
| Claude Code | `claude-code` | Existing lifecycle, permissions, and question lifting |
| Codex | `codex` | Existing lifecycle, permissions, compaction, and interruption |
| ZCode | Not built in | Hook configuration and response contract not verified |
| Gemini CLI | `gemini-cli` | Existing JSON hooks |
| Antigravity CLI | `antigravity` | Vibe Island describes observation of native permissions, not external approval |
| Cursor | `cursor` | Existing flat JSON hooks |
| Trae | Not built in | Vibe Island describes alerts and terminal click-through; hook API not verified |
| OpenCode | `opencode` | Existing plugin with permission/question replies; Vibe Island uses a different SSE/REST transport |
| MiMoCode | Not built in | Vibe Island claims approval/question support; upstream contract not verified |
| Factory Droid | `droid` (unverified) | Shipped from documented Claude-like dialect; standalone hook-file root `{event}` |
| Devin | Not built in | Separate CLI investigation; desktop Cascade is advertised as read-only |
| Qoder | `qoder` (unverified) | Shipped from documented Claude-like JSON hooks; elicitation kept observe-only |
| QoderWork | Not built in | Desktop notification-only monitoring; no two-way hook contract verified |
| Qwen Code | `qwen-code` (unverified) | Shipped from documented Claude-like hooks; timeouts kept < 1000 (ms quirk) |
| QwenWork | Not built in | Desktop notification-only monitoring; no hook contract verified |
| Grok Build | Not built in | Vibe Island explicitly describes observation, not approval/question answering |
| Kimi Code | `kimi-code` (unverified) | Shipped via new `toml_list_hooks` op; exit-2 blocking, permission events observe-only |
| DeepSeek / CodeWhale | Not built in | Vibe Island advertises monitoring and quotas; upstream hook API not verified |
| DeepSeek Harness (`dsh`) | Not built in | Third-party sources converge on Claude/Codex-bridge plugins via `cordis.patch.yml`; no official hook contract verified, and the engine has no YAML-patch installer op — research only, no adapter shipped |
| Mistral Vibe | `mistral-vibe` (unverified) | Shipped via new `toml_list_hooks` op with named rows and per-event hook types |
| GitHub Copilot | `copilot` | `permissionRequest` (behavior allow/deny) and `ask_user`/`AskUserQuestion` added docs-only; `preToolUse` now installs with the documented `permissionDecision` dialect |
| CodeBuddy | Not built in | Vibe Island describes permission alerts and terminal click-through; upstream contract not verified |
| WorkBuddy | Not built in | Vibe Island describes permission/question alerts; upstream contract not verified |
| Kiro | `kiro` | Existing CLI integration; built-in IDE AI is a separate surface |
| Hermes | `hermes` | Existing staged Python plugin |
| Amp | `amp` | Existing plugin integration |
| Pi Agent | `pi` | Existing extension integration |
| Oh My Pi | `omp` | Existing registered plugin integration |
| Gajae Code | Not built in | Vibe Island's beta advertises terminal presence/jump, not deeper lifecycle integration |
| Kimi | Ambiguous duplicate | Do not create a second adapter without evidence of a distinct product/contract |

## Recommended implementation order

Sections 1–3 are now shipped as `"unverified"` adapters (docs-derived, not live-confirmed). Capture native payloads and exercise decisions before clearing the flag. Sharing a JSON dialect does not justify aliasing an unrelated product or blindly inheriting every event from Claude Code — the new manifests are explicit per-product event lists, not `extends: "claude-code"` aliases.

### 1. Droid, Qwen Code, and Qoder

These are the strongest next adapters because upstream references describe concrete configuration and decision contracts.

| Agent | Official reference | Configuration | Important differences |
| --- | --- | --- | --- |
| Droid | [Factory hooks](https://docs.factory.com/harness/hooks.md) | `~/.factory/hooks.json`, `.factory/hooks.json`; standalone files keyed directly by event | Matcher groups resemble Claude, but installer root is `{event}`, not `hooks.{event}`. Tool names include `Execute`, `Read`, `Edit`, `Create`; supported events are a subset of Claude's. |
| Qwen Code | [Hooks](https://qwenlm.github.io/qwen-code-docs/en/users/features/hooks/) | `~/.qwen/settings.json`, `.qwen/settings.json`, `hooks.{event}` matcher groups | Claude-style permission decisions and tool aliases are documented. Additional events include `PostToolBatch`, `StopFailure`, prompt expansion, and todo/message events. Command timeouts at or above 1000 are interpreted as milliseconds; do not copy timeout values blindly. |
| Qoder | [CLI hooks](https://docs.qoder.com/cli/hooks) | `~/.qoder/settings.json`, project `.qoder/settings.json` and local settings | `hookSpecificOutput.hookEventName` is required. `PermissionRequest` uses allow/deny behavior, not ask. Elicitation has a different accept/decline/cancel dialect; do not pretend it is a permission gate. |

`extends: "claude-code"` is a useful starting point, not proof of compatibility. The current merge rules merge event maps rather than deleting inherited keys, so these adapters may be clearer as explicit manifests listing only accepted native events.

### 2. Modernize Copilot's existing adapter

GitHub's [hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference) and [CLI hook setup](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/use-hooks) document more than the current adapter exposes:

- `permissionRequest` supports an allow/deny behavior object; it should not remain unavailable once verified against a supported CLI version.
- Runtime `ask_user` exists, and PascalCase Claude-compatible reporting can call it `AskUserQuestion`. Capture its actual question/options payload and answer semantics before enabling `ask_tools`; a permission allow alone does not prove question answering works.
- Config discovery includes user hook directories and inline settings in addition to repo `.github/hooks/*.json`. Confirm the supported versions and installer paths before changing them.
- camelCase and PascalCase hooks have different field conventions. Keep these distinctions explicit instead of accepting an unverified superset.

### 3. Add array-of-tables TOML installation for Kimi Code and Mistral Vibe

Both have documented hook APIs, but the current `toml_hooks` installer is keyed by event (`hooks.{event}`), not a flat `[[hooks]]` list. This is a genuine installer capability gap.

- [Kimi Code hooks](https://moonshotai.github.io/kimi-code/en/customization/hooks): `~/.kimi-code/config.toml` (relocatable using `KIMI_CODE_HOME`), `[[hooks]]` entries with `event`, `matcher`, `command`, and `timeout`. Only `UserPromptSubmit`, `PreToolUse`, and `Stop` are blockable; permission events are observational. No native question-answer reply dialect was verified.
- [Mistral Vibe hooks](https://docs.mistral.ai/vibe/code/cli/hooks): user `~/.vibe/hooks.toml` and trusted project `.vibe/hooks.toml`, named `[[hooks]]` rows with `type: pre_tool|post_tool|post_agent`, `match`, `command`, `timeout`, and `strict`. Its output uses `decision: allow|deny` and snake_case `hook_specific_output`; `post_agent` denial can request another turn.

A reusable TOML list-entry install operation needs ownership-aware deduplication, status detection, and removal that preserves unrelated rows/comments. Do not point the existing JCode adapter at these files: the schemas differ.

### 4. Investigate DeepSeek Harness and remaining CLI products

[Vibe Island's DSH page](https://vibeisland.app/dsh/) claims permission approvals, question answers, and browser-tab jumping. Investigation (2026-10-13) found no official hook contract: third-party integrations converge on DSH consuming Claude-Code-shaped (and Codex-shaped) `hooks.json` through official `@deepseek-ai/dsh-hooks-claude-code` / `dsh-hooks-codex` bridge plugins registered in `cordis.patch.yml` profiles. That makes DSH support a YAML-patch installer op plus a Claude-shape hooks file — not another `adapter.json` — and it still needs verification against the official bridge packages and a live `dsh` install. Do not infer OpenCode-compatible SSE/REST endpoints from similar UI behavior, and do not ship a guessed DSH adapter.

For ZCode, MiMoCode, Trae, Devin CLI, Grok Build, CodeBuddy, WorkBuddy, and DeepSeek/CodeWhale, gather official hook/plugin contracts and real fixtures first. Keep QoderWork, QwenWork, and jump-only Gajae monitoring separate from hook-based agent support. Read-only integrations can still be valuable, but must explicitly advertise their limitations.

## Capability comparison and ownership

| Vibe Island capability | agent-hooks status | Appropriate next work |
| --- | --- | --- |
| Unified multi-agent status/activity | Lifecycle stream, session tracker, and subscriptions exist | Broaden verified adapters; do not build a second tracker |
| Zero-config setup | Declarative install/status/uninstall exist | Improve native adapter coverage and required trust/activation notes |
| Permission approval and question answers | Scoped daemon holds, verdicts, and question lifting exist | Validate dialect support per event; Copilot is a concrete gap |
| Plan preview and approval | Tool inputs/raw payloads and permission gates are available | Markdown rendering and plan-reading UI belong to the consuming app; add protocol fields only for a proven missing contract |
| Exact terminal/tab/pane jump | TTY, tmux, SSH context and terminal env are forwarded; `EventEnricher`/`extensions` exist | Terminal-specific activation/resolution belongs to the consuming app, not a macOS-only hook engine |
| Sound alerts and notch/floating UI | Events can drive them | Consumer-app features |
| Quotas, token/cost dashboards, resets | No normalized quota-provider API | Separate usage-provider integration, not guessed lifecycle data or hook credentials |
| SSH remote monitoring | Socket selection, origin labels, and SSH context exist | Deployment, SSH tunnel management, reconnection, and host UI belong to the consumer; these are not already a complete remote-management feature |
| Custom integrations | Declarative user/project adapter manifests exist, including gate dialects | Prefer the existing manifest system over a second incompatible bridge format |

Vibe Island's [custom-agent guide](https://vibeisland.app/docs/custom-agents-agent-guide/) is itself fire-and-forget and does not support `PermissionRequest` in that public custom interface. Its broader [SSH](https://vibeisland.app/ssh-remote/), [usage](https://vibeisland.app/usage/), and [custom jump](https://vibeisland.app/docs/custom-jump-rules/) features are useful consumer-app references, not evidence that every listed agent offers two-way hooks.

## Muse verification boundary

Muse was implemented separately using its [hook documentation](https://dev.meta.ai/docs/muse-code/extending#hooks), [settings documentation](https://dev.meta.ai/docs/muse-code/configuration), and the installed 1.4.6 CLI. Actual echo-provider runs exercised the installed hooks through the daemon and subscriber, with stable session identity and correct idle/running/closed transitions. A native prompt denial cancelled the run. A real shim/daemon-held tool approval echoed original arguments; native permission-denial rendering was also exercised.

The echo provider does not execute tools, so Muse's own tool-approval application was not live-tested. `PostToolBatch` and `StopFailure` are recognized in the installed binary but identified there as staged events; their live runtime emission remains unverified. The adapter does not claim native Muse question answering.
