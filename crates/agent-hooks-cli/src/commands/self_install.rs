//! `agent-hooks self-install`: copy the running binary to the shared,
//! stable location (`~/.agent-hooks/bin/agent-hooks`) and register hooks
//! against that path. Idempotent; the newest version wins, so several host
//! apps can each call it on startup without clobbering one another.

use crate::cli::{InstallArgs, SelfInstallArgs};
use anyhow::{bail, Context};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = if cfg!(windows) {
    "agent-hooks.exe"
} else {
    "agent-hooks"
};

/// Shared install dir: `$AGENT_HOOKS_HOME/bin`, else `~/.agent-hooks/bin`.
pub fn shared_bin_dir() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("AGENT_HOOKS_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join("bin"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("cannot resolve home directory (HOME/USERPROFILE unset)")?;
    Ok(PathBuf::from(home).join(".agent-hooks").join("bin"))
}

/// Parse `X.Y.Z[-pre]` into a comparable tuple; pre-release suffixes are ignored.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Version reported by an existing binary (`agent-hooks --version` → "agent-hooks X.Y.Z").
fn installed_version(path: &Path) -> Option<(u64, u64, u64)> {
    let out = Command::new(path).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_version(text.split_whitespace().last()?)
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Copy `src` over `dest` atomically (tmp + rename). On Windows a running
/// exe cannot be overwritten but can be renamed away first.
fn replace_binary(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let dir = dest.parent().context("destination has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(".{BIN}.{}.tmp", std::process::id()));
    std::fs::copy(src, &tmp).with_context(|| format!("copy to {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    if dest.exists() {
        let old = dest.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dest, &old).context("move running binary aside")?;
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("install {}", dest.display()));
    }
    Ok(())
}

pub fn run(args: SelfInstallArgs) -> anyhow::Result<()> {
    let current = std::env::current_exe().context("resolve current executable")?;
    let ours = parse_version(env!("CARGO_PKG_VERSION")).context("own version is not X.Y.Z")?;
    let dest = shared_bin_dir()?.join(BIN);

    let min = args
        .min_version
        .as_deref()
        .map(|m| parse_version(m).with_context(|| format!("invalid --min-version '{m}'")))
        .transpose()?;
    if let Some(min) = min {
        if ours < min && !args.dry_run {
            bail!(
                "this binary is {}.{}.{} but --min-version requires {}.{}.{}",
                ours.0,
                ours.1,
                ours.2,
                min.0,
                min.1,
                min.2
            );
        }
    }

    let existing = if dest.exists() {
        installed_version(&dest)
    } else {
        None
    };
    let action = if same_file(&current, &dest) {
        "current"
    } else {
        match existing {
            Some(v) if v >= ours => "kept",
            Some(_) => "upgraded",
            None => "installed",
        }
    };

    if !args.dry_run && matches!(action, "installed" | "upgraded") {
        replace_binary(&current, &dest)?;
    }

    // Hooks always point at the shared path, even when an equal/newer copy was kept.
    let harnesses = if !args.no_hooks && !args.dry_run {
        crate::commands::install::run_collect(InstallArgs {
            harness: args.harness.clone(),
            binary_path: Some(dest.clone()),
            socket: args.socket.clone(),
            context_cmd: None,
            origin: None,
            adapter_version: None,
        })?
    } else {
        Vec::new()
    };

    let version = existing
        .filter(|_| action == "kept")
        .map(|v| format!("{}.{}.{}", v.0, v.1, v.2))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    if args.json {
        println!(
            "{}",
            json!({
                "path": dest,
                "action": action,
                "version": version,
                "harnesses": harnesses,
                "on_path": std::env::var_os("PATH")
                    .map(|p| std::env::split_paths(&p).any(|d| Some(d.as_path()) == dest.parent()))
                    .unwrap_or(false),
            })
        );
    } else {
        eprintln!("agent-hooks {version}: {action} at {}", dest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn versions_compare_numerically() {
        assert!(parse_version("0.12.0") > parse_version("0.9.10"));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }
}
