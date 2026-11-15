//! `install`/`uninstall` through the real binary under a release-style file
//! name: the installer writes whatever binary path it is given, so it must
//! recognise those rows as its own on the next install and on uninstall.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn run(binary: &Path, home: &Path, args: &[&str]) {
    let out = Command::new(binary)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("AGENT_HOOKS_SOCKET")
        .output()
        .expect("run agent-hooks");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn occurrences(home: &Path, needle: &str) -> usize {
    let settings = std::fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    settings.matches(needle).count()
}

#[test]
fn reinstall_replaces_and_uninstall_removes_rows_of_a_renamed_binary() {
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let binary = home.path().join("agent-hooks-aarch64-apple-darwin");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agent-hooks"), &binary).unwrap();
    let binary_arg = binary.to_str().unwrap();
    let install = |socket: &str| {
        run(
            &binary,
            home.path(),
            &[
                "install",
                "--harness",
                "claude",
                "--binary-path",
                binary_arg,
                "--socket",
                socket,
            ],
        )
    };

    install("/tmp/first.sock");
    let rows = occurrences(home.path(), "agent-hooks-aarch64-apple-darwin hook");
    assert!(rows > 0, "install registered hooks");

    // A changed command (new socket) replaces the old rows instead of
    // stacking a second set beside them.
    install("/tmp/second.sock");
    assert_eq!(
        occurrences(home.path(), "agent-hooks-aarch64-apple-darwin hook"),
        rows
    );
    assert_eq!(occurrences(home.path(), "/tmp/first.sock"), 0);

    run(&binary, home.path(), &["uninstall", "--harness", "claude"]);
    assert_eq!(
        occurrences(home.path(), "agent-hooks-aarch64-apple-darwin hook"),
        0
    );
}
