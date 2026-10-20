use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// agent-hooks — universal lifecycle hooks for AI coding agents
#[derive(Parser, Debug)]
#[command(
    name = "agent-hooks",
    version,
    about = "Universal agent harness hooks — observe, control, and automate AI coding agents",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Fast CLI hook shim (invoked by agent harness)
    Hook(HookArgs),

    /// Start the background daemon server
    Daemon(DaemonArgs),

    /// Start a local development daemon, install hooks, and stream events
    Dev(DevArgs),

    /// Install hooks into agent harnesses
    Install(InstallArgs),

    /// Remove hooks installed by agent-hooks
    Uninstall(UninstallArgs),

    /// Check daemon health, active sessions, and installed hooks
    Status(StatusArgs),

    /// Scaffold, validate, test, and manage declarative adapter manifests
    Adapter(AdapterArgs),

    /// Start an installed agent interactively with an initial prompt
    Start(AgentLaunchArgs),

    /// Run an installed agent non-interactively with a prompt
    Exec(AgentLaunchArgs),

    /// Print the models an installed agent's own CLI reports
    Models(ModelsArgs),

    /// List all supported harnesses, aliases, and hook file locations
    #[command(name = "list-adapters")]
    ListAdapters(ListAdaptersArgs),

    /// Validate or send canonical answers, or open an interactive decider
    Answer(AnswerArgs),

    /// Scaffold ~/.agent-hooks/hooks.json for daemonless local hooks
    Init(InitArgs),

    /// Manage agent-hooks plugins (install, uninstall, list, enable, disable, validate)
    Plugin(PluginArgs),

    /// Initialize and query the opt-in conversation history store (read-only queries)
    History(HistoryArgs),
}

/// Answer held prompts, or validate a normalized event without connecting to the daemon.
#[derive(Parser, Debug)]
pub struct AnswerArgs {
    /// Optional session id used to select a pending prompt
    pub session_id: Option<String>,

    /// Daemon socket path to connect to
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Non-interactive verdict: allow|deny[:reason]|ask[:question]|answer:<option-or-text>
    /// (`answer:` allows and answers every question: an option label match is
    /// selected, anything else is sent as free text)
    #[arg(long, value_name = "VERDICT")]
    pub auto: Option<String>,

    /// Plain stdin/stdout mode instead of the fullscreen TUI
    #[arg(long)]
    pub line: bool,

    /// Validate normalized AgentEvent from stdin and print the resolved decision (no daemon)
    #[arg(long = "return")]
    pub return_event: bool,

    /// Canonical PromptDecision JSON
    #[arg(long, value_name = "JSON")]
    pub decision: Option<String>,

    /// Respond to this exact pending prompt id
    #[arg(long, value_name = "ID")]
    pub prompt_id: Option<String>,

    /// Approve the request (answer flags also imply allow)
    #[arg(long, conflicts_with_all = ["decision", "deny", "auto", "line"])]
    pub allow: bool,

    /// Deny the request with a reason
    #[arg(long, value_name = "REASON", conflicts_with_all = ["decision", "allow", "select", "custom_input", "auto", "line"])]
    pub deny: Option<String>,

    /// Select an exact option label for a question; repeat for more answers
    #[arg(long, num_args = 2, value_names = ["QUESTION_ID", "OPTION_LABEL"], action = clap::ArgAction::Append, allow_hyphen_values = true, conflicts_with_all = ["decision", "deny", "auto", "line"])]
    pub select: Vec<String>,

    /// Supply exact custom input for a question that permits it; once per question
    #[arg(long, num_args = 2, value_names = ["QUESTION_ID", "TEXT"], action = clap::ArgAction::Append, allow_hyphen_values = true, conflicts_with_all = ["decision", "deny", "auto", "line"])]
    pub custom_input: Vec<String>,
}

/// Fast hook shim called by the harness for every lifecycle event.
///
/// Reads payload from stdin (preferred) or trailing argv, captures
/// TTY/CWD/TMUX/SSH context, runs daemonless local hooks from
/// ~/.agent-hooks/hooks.json, forwards a `RawHookInput` to the daemon
/// over the Unix socket, and prints the merged decision to stdout.
///
/// If the daemon is unreachable the local decision stands (allow when no
/// local hooks match), rendered in the harness dialect — the hook exits 0
/// immediately so agent execution is never blocked.
#[derive(Parser, Debug)]
pub struct HookArgs {
    /// Harness identifier (e.g. `claude`, `codex`, `gemini`, `cursor`)
    /// If the value looks like JSON (starts with '{' after trimming), it is
    /// treated as the raw payload and the harness is sniffed via registry
    /// detection (codex quirk). Use `--harness` to override sniffing.
    pub harness: String,

    /// Explicit harness identifier — when present, overrides sniffing and
    /// positional JSON detection. Example: `--harness codex`.
    #[arg(long = "harness", value_name = "HARNESS")]
    pub harness_override: Option<String>,

    /// Lifecycle event name (e.g. `PreToolUse`, `PostToolUse`, `Stop`)
    #[arg(long, value_name = "NAME")]
    pub event: Option<String>,

    /// Unique request ID for request-reply prompt cycles
    #[arg(long = "request-id", value_name = "ID")]
    pub request_id: Option<String>,

    /// Daemon socket path (default: /tmp/agent-hooks.sock)
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Pinned manifest version, e.g. v1
    #[arg(long = "adapter-version", value_name = "VERSION")]
    pub adapter_version: Option<String>,

    /// Capture raw hook payloads to fixtures dir (best-effort, ignored on write errors).
    /// Writes `<dir>/<harness>-<event>-<unixts>.json` = {"harness","event_hint","stdin"}.
    #[arg(long = "capture-fixtures", value_name = "DIR")]
    pub capture_fixtures: Option<PathBuf>,

    /// Consumer context command, run before the daemon forward as
    /// `<PATH> hook-context --harness <id> --event <name>` (cwd = hook cwd,
    /// stdin null, 1500ms timeout, override `AGENT_HOOKS_CONTEXT_TIMEOUT_MS`).
    /// A JSON-object stdout is merged key-wise into `AgentEvent.extensions`;
    /// any failure is a warning. Skipped when no daemon send will happen.
    #[arg(long = "context-cmd", value_name = "PATH")]
    pub context_cmd: Option<PathBuf>,

    /// Opaque origin label stamped at install time (e.g. the ssh target a
    /// remote host was set up from). Forwarded as the `agent_hooks.origin`
    /// extension so a consumer with several remotes can tell which one an
    /// event came from without any per-event lookup.
    #[arg(long, value_name = "LABEL")]
    pub origin: Option<String>,
    /// Optional inline payload (alternative to stdin). Remaining args are joined as payload.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub payload: Vec<String>,
}
#[derive(Parser, Debug)]
pub struct DaemonArgs {
    /// Unix socket path to listen on (default: /tmp/agent-hooks.sock)
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long = "log-level", value_name = "LEVEL", default_value = "info")]
    pub log_level: String,

    /// Record hook events forwarded from other machines (those installed with
    /// `--origin`) into the history store created by `agent-hooks history init`.
    /// Local hooks are recorded by their own shim. Off unless passed; fails if
    /// the store is missing.
    #[arg(long)]
    pub history: bool,

    /// History directory (default: $AGENT_HOOKS_HISTORY_DIR, else ~/.agent-hooks/history)
    #[arg(long = "history-dir", value_name = "PATH", requires = "history")]
    pub history_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub command: HistoryCommand,
}

#[derive(Subcommand, Debug)]
pub enum HistoryCommand {
    /// Create or migrate the history store (does not install hooks or start the daemon)
    Init(HistoryDirArgs),
    /// List captured sessions
    Sessions(HistorySessionsArgs),
    /// List one session's captured messages and tool markers
    Messages(HistoryMessagesArgs),
    /// Schema version, counts and last persisted time
    Status(HistoryDirArgs),
}

#[derive(Parser, Debug)]
pub struct HistoryDirArgs {
    /// History directory (default: $AGENT_HOOKS_HISTORY_DIR, else ~/.agent-hooks/history)
    #[arg(long = "history-dir", value_name = "PATH")]
    pub history_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct HistorySessionsArgs {
    /// Only this native session id (exact match)
    #[arg(long)]
    pub session: Option<String>,
    /// Literal substring of the session title, native id, or workspace (ASCII case-insensitive)
    #[arg(long)]
    pub search: Option<String>,
    /// Only this harness
    #[arg(long)]
    pub harness: Option<String>,
    /// Only this origin label (`local` when no `--origin` was installed)
    #[arg(long)]
    pub origin: Option<String>,
    /// Opaque cursor from a previous page's `next_cursor`
    #[arg(long)]
    pub after: Option<String>,
    /// Page size, 1..=1000 (default 100)
    #[arg(long)]
    pub limit: Option<u32>,
    /// History directory (default: $AGENT_HOOKS_HISTORY_DIR, else ~/.agent-hooks/history)
    #[arg(long = "history-dir", value_name = "PATH")]
    pub history_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct HistoryMessagesArgs {
    /// Native session id as reported by the harness
    #[arg(long)]
    pub session: String,
    /// Disambiguate when the same session id exists under several harnesses
    #[arg(long)]
    pub harness: Option<String>,
    /// Disambiguate when the same session id exists under several origins
    #[arg(long)]
    pub origin: Option<String>,
    /// Opaque cursor from a previous page's `next_cursor`
    #[arg(long)]
    pub after: Option<String>,
    /// Exclusive upper sequence bound for scroll-up paging: return the
    /// newest page below this sequence, oldest first. Cannot combine with
    /// `--after`; pass the first returned item's `seq` to load older rows.
    #[arg(long)]
    pub before: Option<i64>,
    /// Page size, 1..=1000 (default 100)
    #[arg(long)]
    pub limit: Option<u32>,
    /// Only these entry kinds (comma-separated or repeated); default all. A
    /// chat view passes `user,assistant` to skip tool bodies.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub kind: Vec<HistoryKindArg>,
    /// History directory (default: $AGENT_HOOKS_HISTORY_DIR, else ~/.agent-hooks/history)
    #[arg(long = "history-dir", value_name = "PATH")]
    pub history_dir: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum HistoryKindArg {
    User,
    Assistant,
    Tool,
}

#[derive(Parser, Debug)]
pub struct DevArgs {
    /// Harness to install for (`all` or a specific name like `claude`, `codex`)
    #[arg(long, value_name = "HARNESS", default_value = "all")]
    pub harness: String,

    /// Unix socket path for the dev daemon and installed hooks
    /// (default: isolated per-user, per-directory socket, see dev_socket_path)
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Do not modify harness configuration; only run the daemon and event log
    #[arg(long)]
    pub no_install: bool,

    /// Log level for the child daemon
    #[arg(long = "log-level", value_name = "LEVEL", default_value = "info")]
    pub log_level: String,
}

#[derive(Parser, Debug)]
pub struct InstallArgs {
    /// Harness to install for (`all` or a specific name like `claude`, `codex`)
    #[arg(long, value_name = "HARNESS", default_value = "all")]
    pub harness: String,

    /// Path to the `agent-hooks` binary to embed in hook commands and
    /// plugin assets (`--bin` is an alias)
    #[arg(long = "binary-path", visible_alias = "bin", value_name = "PATH")]
    pub binary_path: Option<PathBuf>,

    /// Socket path to embed in installed hook commands
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Context command to embed in installed hook commands and plugin
    /// assets (see `hook --context-cmd`)
    #[arg(long = "context-cmd", value_name = "PATH")]
    pub context_cmd: Option<PathBuf>,

    /// Origin label to embed in installed hook commands and plugin assets
    /// (see `hook --origin`)
    #[arg(long, value_name = "LABEL")]
    pub origin: Option<String>,

    /// Pinned manifest version, e.g. v1
    #[arg(long = "adapter-version", value_name = "VERSION")]
    pub adapter_version: Option<String>,
}
#[derive(Parser, Debug)]
pub struct UninstallArgs {
    /// Harness to uninstall from (`all` or a specific name)
    #[arg(long, value_name = "HARNESS", default_value = "all")]
    pub harness: String,
}

#[derive(Parser, Debug)]
pub struct StatusArgs {
    /// Daemon socket path to check
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Output machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Parser, Debug)]
pub struct AgentLaunchArgs {
    /// Harness id or alias (see `list-adapters`)
    #[arg(long)]
    pub harness: String,

    /// Model to use; must be one the agent reports (list with `agent-hooks models`)
    #[arg(long)]
    pub model: Option<String>,

    /// Pass --model through without checking it against the agent's model list
    #[arg(long, requires = "model")]
    pub unchecked: bool,

    /// Start with the harness's YOLO/auto-approve launch arguments, when recorded by its adapter
    #[arg(long)]
    pub yolo: bool,

    /// Print the resolved argv as JSON instead of running it
    #[arg(long)]
    pub dry_run: bool,

    /// Prompt, passed to the agent as a single argument
    pub prompt: String,

    /// Native harness arguments, passed unchanged after `--`
    #[arg(last = true)]
    pub native_args: Vec<String>,
}

#[derive(Parser, Debug)]
pub struct ModelsArgs {
    /// Harness id or alias (see `list-adapters`)
    #[arg(long)]
    pub harness: String,

    /// Output normalized model ids as machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Parser, Debug)]
pub struct ListAdaptersArgs {
    /// Output machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

/// Manage declarative adapter manifests (`adapter.json` + fixtures).
#[derive(Parser, Debug)]
pub struct AdapterArgs {
    #[command(subcommand)]
    pub command: AdapterCommands,
}

#[derive(Subcommand, Debug)]
pub enum AdapterCommands {
    /// Scaffold a new adapter manifest (`<id>.json`) plus a `fixtures/` dir
    New(AdapterNewArgs),
    /// Validate a manifest file or builtin id (schema + structural checks)
    Validate(AdapterTargetArgs),
    /// Replay `fixtures/` through `parse_event` + `format_response`
    Test(AdapterTargetArgs),
    /// Install an external manifest into the user tier (+ provenance sidecar)
    Add(AdapterAddArgs),
    /// List builtin + user + project manifests
    List(AdapterListArgs),
}

#[derive(Parser, Debug)]
pub struct AdapterNewArgs {
    /// Adapter id, e.g. `myagent` (lowercase letters, digits, `-`/`_`)
    pub id: String,
}

#[derive(Parser, Debug)]
pub struct AdapterTargetArgs {
    /// Manifest path on disk, or a builtin adapter id
    pub target: String,
}

#[derive(Parser, Debug)]
pub struct AdapterAddArgs {
    /// Manifest source: file path, git URL, or https tarball URL
    pub source: String,

    /// Name to install as (defaults to the file stem / manifest id)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

#[derive(Parser, Debug)]
pub struct AdapterListArgs {
    /// Output machine-readable JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Parser, Debug)]
pub struct InitArgs {
    /// Write ./.agent-hooks/hooks.json (project tier) instead of ~/.agent-hooks/hooks.json
    #[arg(long)]
    pub project: bool,
}

/// Manage agent-hooks plugins (install, uninstall, list, enable, disable, validate)
#[derive(Parser, Debug)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(Subcommand, Debug)]
pub enum PluginCommand {
    /// List installed plugins
    List(PluginListArgs),
    /// Install a plugin from a directory or repository
    Install(PluginInstallArgs),
    /// Uninstall an installed plugin
    Uninstall(PluginUninstallArgs),
    /// Enable an installed plugin
    Enable(PluginEnableArgs),
    /// Disable an installed plugin
    Disable(PluginDisableArgs),
    /// Validate a plugin manifest and entrypoints
    Validate(PluginValidateArgs),
}

#[derive(Parser, Debug)]
pub struct PluginListArgs {
    /// Emit machine-readable JSON
    #[arg(long)]
    pub json: bool,

    /// Only list project-local plugins
    #[arg(long)]
    pub project: bool,

    /// Only list global plugins
    #[arg(long)]
    pub global: bool,

    /// Show detailed info (hook definitions, paths)
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Parser, Debug)]
pub struct PluginInstallArgs {
    /// Source directory containing plugin.json, .claude-plugin/plugin.json, or package.json
    pub source: PathBuf,

    /// Install into project directory (.agent-hooks/plugins) instead of global (~/.agent-hooks/plugins)
    #[arg(long, short = 'p')]
    pub project: bool,

    /// Symlink the plugin instead of copying (useful for development)
    #[arg(long, short = 'l')]
    pub link: bool,

    /// Force overwrite if plugin already exists
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Emit JSON result
    #[arg(long)]
    pub json: bool,
}

#[derive(Parser, Debug)]
pub struct PluginUninstallArgs {
    /// Name / ID of the plugin to uninstall
    pub name: String,

    /// Uninstall from project directory (.agent-hooks/plugins)
    #[arg(long, short = 'p')]
    pub project: bool,

    /// Emit JSON result
    #[arg(long)]
    pub json: bool,
}

#[derive(Parser, Debug)]
pub struct PluginEnableArgs {
    /// Name / ID of the plugin to enable
    pub name: String,

    /// Enable in project config instead of user config
    #[arg(long, short = 'p')]
    pub project: bool,
}

#[derive(Parser, Debug)]
pub struct PluginDisableArgs {
    /// Name / ID of the plugin to disable
    pub name: String,

    /// Disable in project config instead of user config
    #[arg(long, short = 'p')]
    pub project: bool,
}

#[derive(Parser, Debug)]
pub struct PluginValidateArgs {
    /// Directory of the plugin to validate
    pub path: PathBuf,

    /// Emit JSON result
    #[arg(long)]
    pub json: bool,
}
