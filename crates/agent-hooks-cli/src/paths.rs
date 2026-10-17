//! Shared path-expansion helpers for manifest op paths and hook commands.
//!
//! Single definition site for `expand_tilde` / `expand_env_vars` /
//! `expand_op_path`, consumed by the install / status / uninstall commands.

use std::path::PathBuf;

/// Expand a leading `~/` (or bare `~`) via `HOME`, falling back to
/// `USERPROFILE`. Anything else passes through untouched.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(stripped) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(stripped);
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            return PathBuf::from(home).join(stripped);
        }
    } else if p == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(p)
}

/// Expand `$VAR` / `${VAR}` references, leaving unknown vars literal.
pub fn expand_env_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&nc) = chars.peek() {
            if nc.is_ascii_alphanumeric() || nc == '_' {
                name.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if braced {
            if chars.peek() == Some(&'}') {
                chars.next();
            } else {
                // Not a valid ${...} — emit literally.
                out.push('$');
                out.push('{');
                out.push_str(&name);
                continue;
            }
        }
        if name.is_empty() {
            out.push('$');
            continue;
        }
        match std::env::var(&name) {
            Ok(v) => out.push_str(&v),
            Err(_) => {
                out.push('$');
                if braced {
                    out.push('{');
                    out.push_str(&name);
                    out.push('}');
                } else {
                    out.push_str(&name);
                }
            }
        }
    }
    out
}

/// Expand an op path: `$XDG_DATA_HOME` (fallback `~/.local/share`), other
/// `$VAR`s, then `~`.
pub fn expand_op_path(raw: &str) -> PathBuf {
    let mut s = raw.to_string();
    if s.contains("$XDG_DATA_HOME") {
        let xdg = std::env::var("XDG_DATA_HOME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| expand_tilde("~/.local/share").display().to_string());
        s = s.replace("$XDG_DATA_HOME", &xdg);
    }
    expand_tilde(&expand_env_vars(&s))
}
