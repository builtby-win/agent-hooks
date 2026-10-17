use crate::cli::DaemonArgs;
use agent_hooks_core::daemon::AgentHooksDaemon;
use std::path::PathBuf;

pub fn default_socket_path() -> PathBuf {
    PathBuf::from(agent_hooks_protocol::default_socket_path())
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    init_tracing(&args.log_level);

    let socket_path = args.socket.unwrap_or_else(default_socket_path);

    tracing::info!(socket = %socket_path.display(), "starting agent-hooks daemon");

    let daemon = AgentHooksDaemon::new();
    daemon
        .run(Some(socket_path))
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
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
