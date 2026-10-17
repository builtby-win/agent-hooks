//! End-to-end tests for `agent-hooks plugin` CLI commands.

use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn agent_hooks_bin() -> std::path::PathBuf {
    let mut bin = std::env::current_exe().unwrap();
    bin.pop(); // deps
    if bin.ends_with("deps") {
        bin.pop();
    }
    bin.join("agent-hooks")
}

#[test]
fn plugin_lifecycle_e2e() {
    let bin = agent_hooks_bin();
    let tmp = tempdir().unwrap();

    let plugin_src = tmp.path().join("my-test-plugin");
    fs::create_dir_all(&plugin_src).unwrap();

    let manifest = r#"{
        "name": "my-test-plugin",
        "version": "1.0.0",
        "description": "A test plugin",
        "hooks": {
            "PreToolUse": [
                { "command": "run.sh", "timeout": 5 }
            ]
        }
    }"#;
    fs::write(plugin_src.join("plugin.json"), manifest).unwrap();
    fs::write(plugin_src.join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();

    // 1. Validate
    let out = Command::new(&bin)
        .args(["plugin", "validate", plugin_src.to_str().unwrap()])
        .output()
        .expect("validate command");
    assert!(out.status.success(), "validate should succeed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Plugin validation passed"));

    // 2. Install to project dir
    let project_dir = tmp.path().join("fake-project");
    fs::create_dir_all(&project_dir).unwrap();

    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "install", plugin_src.to_str().unwrap(), "--project"])
        .output()
        .expect("install command");
    assert!(out.status.success(), "install should succeed: {}", String::from_utf8_lossy(&out.stderr));

    // 3. List
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "list", "--project", "--json"])
        .output()
        .expect("list command");
    assert!(out.status.success());
    let list_json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json list");
    assert_eq!(list_json.as_array().unwrap().len(), 1);
    assert_eq!(list_json[0]["id"], "my-test-plugin");
    assert_eq!(list_json[0]["enabled"], true);

    // 4. Disable
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "disable", "my-test-plugin", "--project"])
        .output()
        .expect("disable command");
    assert!(out.status.success());

    // Check list shows disabled
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "list", "--project", "--json"])
        .output()
        .expect("list command");
    let list_json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json list");
    assert_eq!(list_json[0]["enabled"], false);

    // 5. Enable
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "enable", "my-test-plugin", "--project"])
        .output()
        .expect("enable command");
    assert!(out.status.success());

    // 6. Uninstall
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "uninstall", "my-test-plugin", "--project"])
        .output()
        .expect("uninstall command");
    assert!(out.status.success());

    // Verify list is empty
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "list", "--project", "--json"])
        .output()
        .expect("list command");
    let list_json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json list");
    assert_eq!(list_json.as_array().unwrap().len(), 0);
}

#[test]
fn plugin_hook_execution_e2e() {
    use std::io::Write;

    let bin = agent_hooks_bin();
    let tmp = tempdir().unwrap();

    let plugin_src = tmp.path().join("veto-plugin");
    fs::create_dir_all(&plugin_src).unwrap();

    let manifest = r#"{
        "name": "veto-plugin",
        "version": "1.0.0",
        "hooks": {
            "PreToolUse": [
                {
                    "command": "sh block.sh",
                    "when": {
                        "tool": "Bash"
                    }
                }
            ]
        }
    }"#;
    fs::write(plugin_src.join("plugin.json"), manifest).unwrap();
    fs::write(
        plugin_src.join("block.sh"),
        "#!/bin/sh\necho 'Blocked by veto-plugin' >&2\nexit 2\n",
    )
    .unwrap();

    let project_dir = tmp.path().join("my-project");
    fs::create_dir_all(&project_dir).unwrap();

    // Install plugin into project
    let out = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["plugin", "install", plugin_src.to_str().unwrap(), "--project"])
        .output()
        .expect("install");
    assert!(out.status.success());

    // Run agent-hooks hook claude --event PreToolUse
    let payload = r#"{"session_id":"s1","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let mut child = Command::new(&bin)
        .current_dir(&project_dir)
        .args(["hook", "claude-code", "--event", "PreToolUse"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hook");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();

    // Claude Code outputs JSON on stdout with permissionDecision: deny
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(r#""permissionDecision":"deny""#) || stdout.contains(r#""permissionDecision": "deny""#));
    assert!(stdout.contains("Blocked by veto-plugin"));
}

