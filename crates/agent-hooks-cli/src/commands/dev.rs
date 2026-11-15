use crate::cli::{DevArgs, InstallArgs};
use agent_hooks_client::AgentHooksClient;
use futures::StreamExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

pub async fn run(args: DevArgs) -> anyhow::Result<()> {
    let socket = args
        .socket
        .unwrap_or_else(|| PathBuf::from(agent_hooks_protocol::dev_socket_path()));
    let binary = std::env::current_exe()?;

    println!("agent-hooks dev");
    println!("  socket: {}", socket.display());

    let mut daemon = tokio::process::Command::new(&binary)
        .args(["daemon", "--socket"])
        .arg(&socket)
        .args(["--log-level", &args.log_level])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    let client = AgentHooksClient::new(socket.to_string_lossy().into_owned());
    if !wait_until_ready(&client, &mut daemon).await {
        stop_daemon(&mut daemon, &socket).await;
        anyhow::bail!("dev daemon did not become ready at {}", socket.display());
    }
    println!("  status: ✓ ready (ping ok)");

    if !args.no_install {
        let installed = crate::commands::install::run(InstallArgs {
            harness: args.harness.clone(),
            binary_path: Some(binary),
            socket: vec![socket.clone()],
            context_cmd: None,
            origin: None,
            adapter_version: None,
        });
        if let Err(e) = installed {
            stop_daemon(&mut daemon, &socket).await;
            return Err(e);
        }
        println!("  harness: ✓ installed hooks target the dev socket");
    }

    let sessions = client.list_sessions().await.unwrap_or_default();
    println!("  sessions: {} active", sessions.len());
    println!("  events: waiting for agent activity (Ctrl-C to stop)\n");

    let mut outcome = Ok(());
    let mut events = client.subscribe(None);
    loop {
        tokio::select! {
            event = events.next() => match event {
                Some(event) => println!(
                    "[event] {} harness={} session={}{} status={:?} cwd={}",
                    lifecycle_name(&event), event.harness, short_id(&event.session_id),
                    event.parent_session_id.as_deref().map(|p| format!(" parent={}", short_id(p))).unwrap_or_default(),
                    event.status,
                    event.context.cwd.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".to_string())
                ),
                None => break,
            },
            _ = stop_requested() => {
                println!("\nagent-hooks dev: stopping daemon");
                break;
            }
            result = daemon.wait() => {
                outcome = match result {
                    Ok(status) => Err(anyhow::anyhow!("dev daemon exited unexpectedly: {status}")),
                    Err(e) => Err(e.into()),
                };
                break;
            }
        }
    }

    stop_daemon(&mut daemon, &socket).await;
    if !args.no_install {
        // The installer records no previous state, so the user's real hooks
        // cannot be restored automatically; `install` rewrites stale hooks.
        eprintln!(
            "agent-hooks dev: harness hooks still target {}; restore them with `agent-hooks install --harness {}`",
            socket.display(),
            args.harness
        );
    }
    outcome
}

/// Ctrl-C, or a plain `kill` of the wrapper: either way the daemon it started
/// must not outlive it.
async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// SIGTERM, a short grace period, then SIGKILL, then remove the socket file.
/// A daemon that already exited is left alone: it either never owned the path
/// (another daemon is live on it, or it is not a socket) or cleaned up itself.
async fn stop_daemon(daemon: &mut tokio::process::Child, socket: &std::path::Path) {
    if matches!(daemon.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = daemon.id() {
        let _ = tokio::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await;
    }
    if tokio::time::timeout(Duration::from_secs(3), daemon.wait())
        .await
        .is_err()
    {
        let _ = daemon.kill().await;
    }
    let _ = std::fs::remove_file(socket);
}

async fn wait_until_ready(client: &AgentHooksClient, daemon: &mut tokio::process::Child) -> bool {
    for _ in 0..50 {
        if matches!(daemon.try_wait(), Ok(Some(_))) {
            return false;
        }
        if matches!(client.ping().await, Ok(true)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn lifecycle_name(event: &agent_hooks_protocol::AgentEvent) -> &'static str {
    event.lifecycle.name()
}
