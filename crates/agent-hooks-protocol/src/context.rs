use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Environment and TTY provenance captured at the moment the hook fired.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventContext {
    /// Working directory where the agent is running
    #[serde(default)]
    pub cwd: Option<PathBuf>,

    /// Git worktree root if inside a git repository
    #[serde(default)]
    pub git_worktree: Option<PathBuf>,

    /// Terminal TTY path (e.g. "/dev/ttys004" or "/dev/pts/1")
    #[serde(default)]
    pub tty: Option<String>,

    /// Process ID of the hook caller / agent
    #[serde(default)]
    pub pid: Option<u32>,

    /// Parent Process ID
    #[serde(default)]
    pub ppid: Option<u32>,

    /// Tmux Pane ID (e.g. "%2")
    #[serde(default)]
    pub tmux_pane_id: Option<String>,

    /// Tmux Session Name
    #[serde(default)]
    pub tmux_session_name: Option<String>,

    /// Tmux Window Index
    #[serde(default)]
    pub tmux_window_index: Option<u32>,

    /// Tmux Pane Index
    #[serde(default)]
    pub tmux_pane_index: Option<u32>,

    /// Tmux client PID
    #[serde(default)]
    pub tmux_client_pid: Option<u32>,

    /// Whether this hook originated from an SSH remote session
    #[serde(default)]
    pub is_ssh: bool,

    /// SSH Connection identifier if remote
    #[serde(default)]
    pub ssh_connection: Option<String>,

    /// SSH Client TTY
    #[serde(default)]
    pub ssh_tty: Option<String>,
}
