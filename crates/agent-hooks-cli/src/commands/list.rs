use crate::cli::ListAdaptersArgs;
use agent_hooks_adapters::manifest::{AdapterManifest, InstallOp};

fn install_paths(manifest: &AdapterManifest) -> Vec<String> {
    let Some(vs) = manifest.versions.get(&manifest.default_version) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for op in &vs.install {
        match op {
            InstallOp::JsonHooks { paths, .. }
            | InstallOp::TomlHooks { paths, .. }
            | InstallOp::TomlListHooks { paths, .. }
            | InstallOp::JsonEdit { paths, .. } => {
                for p in paths {
                    if !out.contains(p) {
                        out.push(p.clone());
                    }
                }
            }
            InstallOp::StageAssets { stage_dir, .. } => {
                if !out.contains(stage_dir) {
                    out.push(stage_dir.clone());
                }
            }
            InstallOp::RunCommand { .. } => {}
        }
    }
    out
}

pub fn run(args: ListAdaptersArgs) -> anyhow::Result<()> {
    let mut manifests = AdapterManifest::load_tiered();
    manifests.sort_by(|a, b| a.id.cmp(&b.id));

    if args.json {
        let list: Vec<serde_json::Value> = manifests
            .iter()
            .map(|m| {
                let vs = m.versions.get(&m.default_version);
                let support = vs.map(|v| v.answer_support());
                serde_json::json!({
                    "id": m.id,
                    "display_name": m.display_name,
                    "aliases": m.aliases,
                    "default_version": m.default_version,
                    "unverified": m.unverified,
                    "verified_against": vs.and_then(|v| v.verified_against.clone()),
                    "agent_cli": vs.and_then(|v| v.agent_cli.clone()),
                    "answer": support.map(|s| serde_json::json!({
                        "permissions": s.permissions,
                        "questions": s.questions,
                        "questions_native": s.questions_native,
                        "escalate": s.escalate,
                    })).unwrap_or(serde_json::Value::Null),
                    "resume_status": if vs
                        .and_then(|v| v.agent_cli.as_ref())
                        .and_then(|agent_cli| agent_cli.resume.as_ref())
                        .is_some()
                    {
                        "supported"
                    } else {
                        "unknown"
                    },
                    "config_paths": install_paths(m),
                    "events": vs.map(|v| v.events.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                    "unavailable": vs.map(|v| v.unavailable.clone()).unwrap_or_default(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(());
    }

    println!("Supported harnesses (adapters):\n");
    println!(
        "Answer flows: permissions = allow/deny a held gate, questions = structured answers reach the agent, escalate = bounce back to the human.\n"
    );
    // Header
    println!(
        "{:<16} {:<28} {:<34} {:<28} Description",
        "ID", "Aliases", "Config", "Answer"
    );
    println!("{}", "-".repeat(120));
    for m in &manifests {
        let aliases = m.aliases.join(", ");
        let paths = install_paths(m);
        let first = paths.first().cloned().unwrap_or_default();
        let answer = m
            .versions
            .get(&m.default_version)
            .map(|v| v.answer_support().label())
            .unwrap_or_else(|| "—".to_string());
        // Primary row
        let name = if m.unverified {
            format!("{} [unverified]", m.display_name)
        } else {
            m.display_name.clone()
        };
        println!(
            "{:<16} {:<28} {:<34} {:<28} {}",
            m.id, aliases, first, answer, name
        );
        // Additional config paths indented
        for extra in paths.iter().skip(1) {
            println!("{:<16} {:<28} {:<34}", "", "", extra);
        }
    }

    println!("\nUsage:");
    println!(
        "  agent-hooks install --harness all              # install for all detected harnesses"
    );
    println!("  agent-hooks install --harness claude-code      # install for a specific harness");
    println!("  agent-hooks install --binary-path /usr/local/bin/agent-hooks");
    println!(
        "\nHook lifecycle events: {}",
        super::user_hooks::KNOWN_LIFECYCLES.join(", ")
    );
    println!("\nIPC: Unix socket /tmp/agent-hooks.sock (Windows: \\\\.\\pipe\\agent-hooks)");
    println!("Protocol: JSON-RPC 2.0 / NDJSON line stream");

    Ok(())
}
