//! `daemon --exit-on-stdin-eof`: a supervisor holding the daemon's stdin pipe gets a clean
//! shutdown (socket removed) when it drops the pipe, however many connections came before.
#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(socket: &Path) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_agent-hooks"))
        .args(["daemon", "--exit-on-stdin-eof", "--socket"])
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "daemon never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

fn wait_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn closing_stdin_stops_the_daemon_after_many_connections() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let mut daemon = spawn(&socket);

    // More than tokio's 512-thread blocking pool: a watcher per accepted connection would
    // have wedged the daemon long before this.
    for _ in 0..600 {
        drop(UnixStream::connect(&socket).unwrap());
        // Stay under the listen backlog; this is about the accept loop, not a flood.
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(UnixStream::connect(&socket).is_ok(), "daemon stopped serving");

    drop(daemon.stdin.take());
    wait_exit(&mut daemon);
    assert!(!socket.exists(), "graceful shutdown must remove the socket");
}

#[test]
fn sigterm_still_stops_it_while_stdin_is_open() {
    let dir = tempfile::TempDir::new().unwrap();
    let socket = dir.path().join("d.sock");
    let mut daemon = spawn(&socket);

    let status = Command::new("kill")
        .arg(daemon.id().to_string())
        .status()
        .unwrap();
    assert!(status.success());
    wait_exit(&mut daemon);
    assert!(!socket.exists(), "graceful shutdown must remove the socket");
    drop(daemon.stdin.take());
}
