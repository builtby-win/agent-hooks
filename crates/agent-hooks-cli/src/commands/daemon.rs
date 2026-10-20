use crate::cli::DaemonArgs;
use agent_hooks_core::daemon::AgentHooksDaemon;
use agent_hooks_core::HistoryRecorder;
use std::path::PathBuf;
use std::sync::Arc;

pub fn default_socket_path() -> PathBuf {
    PathBuf::from(agent_hooks_protocol::default_socket_path())
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    init_tracing(&args.log_level);

    let socket_path = args.socket.unwrap_or_else(default_socket_path);

    tracing::info!(socket = %socket_path.display(), "starting agent-hooks daemon");

    let mut daemon = AgentHooksDaemon::new();
    if args.history {
        // Explicit opt-in. The store must already exist: fail before serving
        // rather than silently running without capture.
        let dir = agent_hooks_history::resolve_history_dir(args.history_dir.as_deref());
        let recorder = HistoryRecorder::open(&dir).map_err(|e| match e {
            agent_hooks_history::HistoryError::Disabled(path) => anyhow::anyhow!(
                "history store not found at {}; run `agent-hooks history init` first",
                path.display()
            ),
            other => anyhow::anyhow!("cannot open history store at {}: {other}", dir.display()),
        })?;
        tracing::info!(dir = %dir.display(), "forwarded-event history capture enabled");
        daemon = daemon.with_history(Arc::new(recorder));
    }
    let history = args.history;
    daemon.run(Some(socket_path)).await.map_err(|e| {
        let msg = e.to_string();
        if history && msg.contains("already running") {
            // One daemon owns the socket for every agent. If it was started
            // without --history it keeps serving and records nothing.
            anyhow::anyhow!(
                "{msg}\nforwarded-event history is only active if that daemon was started \
                 with --history; otherwise stop it and start this one"
            )
        } else {
            anyhow::anyhow!(msg)
        }
    })?;
    Ok(())
}

fn init_tracing(level: &str) {
    let filter = match level.to_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "info" => "info",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .try_init();
}
