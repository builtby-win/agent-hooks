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
    if !wait_until_ready(&client).await {
        let _ = daemon.kill().await;
        anyhow::bail!("dev daemon did not become ready at {}", socket.display());
    }
    println!("  status: ✓ ready (ping ok)");

    if !args.no_install {
        crate::commands::install::run(InstallArgs {
            harness: args.harness,
            binary_path: Some(binary),
            socket: Some(socket.clone()),
            context_cmd: None,
            origin: None,
            adapter_version: None,
        })?;
        println!("  harness: ✓ installed hooks target the dev socket");
    }

    let sessions = client.list_sessions().await.unwrap_or_default();
    println!("  sessions: {} active", sessions.len());
    println!("  events: waiting for agent activity (Ctrl-C to stop)\n");

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
            _ = tokio::signal::ctrl_c() => {
                println!("\nagent-hooks dev: stopping daemon");
                break;
            }
            result = daemon.wait() => anyhow::bail!("dev daemon exited unexpectedly: {}", result?),
        }
    }

    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
    Ok(())
}

async fn wait_until_ready(client: &AgentHooksClient) -> bool {
    for _ in 0..50 {
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
