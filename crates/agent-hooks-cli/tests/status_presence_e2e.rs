//! `status` reports an agent as present from its own home directory, even when
//! agent-hooks has installed nothing for it yet (its hooks file or plugin dir
//! is created by `install`, so it cannot be the presence signal).

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

fn exists(home: &std::path::Path, cwd: &std::path::Path, id: &str) -> bool {
    let out = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_agent-hooks")))
        .args(["status", "--json", "--socket"])
        .arg(home.join("none.sock"))
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cwd)
        .output()
        .expect("run status");
    let report: Value = serde_json::from_slice(&out.stdout).expect("status stdout is JSON");
    report["installed_hooks"]
        .as_array()
        .expect("installed_hooks")
        .iter()
        .find(|h| h["id"] == id)
        .unwrap_or_else(|| panic!("no {id} entry"))["exists"]
        .as_bool()
        .expect("exists")
}

#[test]
fn agent_home_dirs_count_as_present_without_installed_hooks() {
    let home = tempfile::tempdir().unwrap();
    // Not the repo: some adapters also list cwd-relative project configs.
    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    std::fs::create_dir(home.path().join(".hermes")).unwrap();

    assert!(exists(home.path(), cwd.path(), "codex"));
    assert!(exists(home.path(), cwd.path(), "hermes"));
    // Nothing in this HOME hints at Oh My Pi (no ~/.omp, none installed).
    assert!(!exists(home.path(), cwd.path(), "omp") || which_omp());
}

/// `omp` on a shared bin dir (`/usr/local/bin`, Homebrew) is searched whatever HOME is.
fn which_omp() -> bool {
    ["/opt/homebrew/bin/omp", "/usr/local/bin/omp"]
        .iter()
        .any(|p| std::path::Path::new(p).is_file())
}
