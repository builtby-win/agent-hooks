//! Plugin management for agent-hooks: discovery, loading, installation, and hook resolution.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use agent_hooks_protocol::plugin::{PluginHookDefinition, PluginManifest};
use agent_hooks_protocol::AgentLifecycle;

/// An installed and loaded plugin.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub id: String,
    pub manifest: PluginManifest,
    pub root_dir: PathBuf,
    pub is_project_local: bool,
    pub enabled: bool,
}

impl Plugin {
    /// Return the resolved absolute path for a relative command or script in this plugin.
    pub fn resolve_command(&self, cmd: &str) -> String {
        let trimmed = cmd.trim();
        // If it starts with an existing file in the plugin root, resolve it to an absolute path
        let candidate = self.root_dir.join(trimmed);
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
        // If it's a command like "node dist/index.js" or "sh run.sh", prefix cwd execution
        format!("(cd {:?} && {})", self.root_dir, trimmed)
    }
}

/// Discovers, validates, and manages plugins across global and project directories.
#[derive(Debug, Default)]
pub struct PluginManager {
    plugins: BTreeMap<String, Plugin>,
    disabled: HashSet<String>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set disabled plugin IDs.
    pub fn set_disabled(&mut self, disabled: impl IntoIterator<Item = String>) {
        self.disabled = disabled.into_iter().collect();
        for (id, p) in &mut self.plugins {
            p.enabled = !self.disabled.contains(id);
        }
    }

    /// Discover and load plugins from standard locations:
    /// 1. Global: `~/.agent-hooks/plugins`
    /// 2. Project: `.agent-hooks/plugins`
    pub fn discover(&mut self, global_dir: Option<&Path>, project_dir: Option<&Path>) {
        if let Some(dir) = global_dir {
            self.load_from_plugins_dir(dir, false);
        }
        if let Some(dir) = project_dir {
            self.load_from_plugins_dir(dir, true);
        }
    }

    /// Load all plugin subdirectories within a plugins directory.
    pub fn load_from_plugins_dir(&mut self, plugins_dir: &Path, is_project_local: bool) {
        let entries = match fs::read_dir(plugins_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(plugin) = Self::load_single_plugin(&path, is_project_local) {
                    let id = plugin.id.clone();
                    let enabled = !self.disabled.contains(&id);
                    let mut plugin = plugin;
                    plugin.enabled = enabled;
                    // Project local overrides global
                    self.plugins.insert(id, plugin);
                }
            }
        }
    }

    /// Load a single plugin from its directory.
    /// Supports standard `plugin.json`, Claude Code `.claude-plugin/plugin.json`,
    /// or `package.json`.
    pub fn load_single_plugin(dir: &Path, is_project_local: bool) -> Option<Plugin> {
        let manifest = Self::read_manifest(dir)?;
        let id = manifest.name.clone();
        Some(Plugin {
            id,
            manifest,
            root_dir: dir.to_path_buf(),
            is_project_local,
            enabled: true,
        })
    }

    /// Read and parse a plugin manifest from a directory.
    pub fn read_manifest(dir: &Path) -> Option<PluginManifest> {
        // 1. Standard plugin.json or agent-plugin.json
        for name in &["plugin.json", "agent-plugin.json"] {
            let path = dir.join(name);
            if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(mut m) = serde_json::from_str::<PluginManifest>(&content) {
                        Self::fill_external_hooks(dir, &mut m);
                        return Some(m);
                    }
                }
            }
        }

        // 2. Claude Code .claude-plugin/plugin.json
        let claude_manifest = dir.join(".claude-plugin").join("plugin.json");
        if claude_manifest.is_file() {
            if let Ok(content) = fs::read_to_string(&claude_manifest) {
                if let Ok(mut m) = serde_json::from_str::<PluginManifest>(&content) {
                    Self::fill_external_hooks(dir, &mut m);
                    return Some(m);
                }
            }
        }

        // 3. Fallback: package.json (e.g. for npm / OMP style plugins)
        let package_json = dir.join("package.json");
        if package_json.is_file() {
            if let Ok(content) = fs::read_to_string(&package_json) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("plugin");
                    let version = v.get("version").and_then(|ver| ver.as_str()).unwrap_or("0.1.0");
                    let desc = v.get("description").and_then(|d| d.as_str()).map(str::to_string);
                    let author = v.get("author").and_then(|a| a.as_str()).map(str::to_string);
                    let license = v.get("license").and_then(|l| l.as_str()).map(str::to_string);

                    let mut m = PluginManifest {
                        name: name.to_string(),
                        version: version.to_string(),
                        description: desc,
                        author,
                        license,
                        homepage: None,
                        repository: None,
                        hooks: BTreeMap::new(),
                        user_config: None,
                    };
                    Self::fill_external_hooks(dir, &mut m);
                    return Some(m);
                }
            }
        }

        None
    }

    /// If hooks are also defined in `hooks/hooks.json` or `.hooks.json`, merge them.
    fn fill_external_hooks(dir: &Path, manifest: &mut PluginManifest) {
        for rel in &["hooks/hooks.json", "hooks.json", ".hooks.json"] {
            let path = dir.join(rel);
            if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                        let root = parsed.get("hooks").unwrap_or(&parsed);
                        if let Some(map) = root.as_object() {
                            for (event, entries) in map {
                                if let Ok(hook_entries) = serde_json::from_value::<Vec<PluginHookDefinition>>(entries.clone()) {
                                    manifest.hooks.entry(event.clone()).or_default().extend(hook_entries);
                                }
                            }
                        }
                    }
                }
                break;
            }
        }
    }

    /// List all discovered plugins.
    pub fn list(&self) -> Vec<&Plugin> {
        self.plugins.values().collect()
    }

    /// Get a plugin by ID.
    pub fn get(&self, id: &str) -> Option<&Plugin> {
        self.plugins.get(id)
    }

    /// Return all active hook definitions across all enabled plugins.
    pub fn all_hooks(&self) -> Vec<(&Plugin, &str, &PluginHookDefinition)> {
        let mut result = Vec::new();
        for plugin in self.plugins.values() {
            if !plugin.enabled {
                continue;
            }
            for (event, entries) in &plugin.manifest.hooks {
                for entry in entries {
                    result.push((plugin, event.as_str(), entry));
                }
            }
        }
        result
    }

    /// Return all active hook definitions for a given lifecycle event across all enabled plugins.
    pub fn hooks_for(&self, lifecycle: &AgentLifecycle) -> Vec<(&Plugin, &PluginHookDefinition)> {
        let event_name = lifecycle.name();
        let mut result = Vec::new();

        for plugin in self.plugins.values() {
            if !plugin.enabled {
                continue;
            }
            if let Some(entries) = plugin.manifest.hooks.get(event_name) {
                for entry in entries {
                    result.push((plugin, entry));
                }
            }
        }

        result
    }

    /// Install a plugin by copying or symlinking a source directory into the target plugins folder.
    pub fn install_from_dir(src: &Path, dest_plugins_dir: &Path, link: bool) -> Result<Plugin, String> {
        if !src.is_dir() {
            return Err(format!("Source path is not a directory: {}", src.display()));
        }

        let manifest = Self::read_manifest(src)
            .ok_or_else(|| format!("No valid plugin manifest found in {}", src.display()))?;

        let plugin_id = manifest.name.clone();
        let target_dir = dest_plugins_dir.join(&plugin_id);

        if target_dir.exists() || fs::symlink_metadata(&target_dir).is_ok() {
            Self::remove_target_entry(&target_dir)?;
        }

        fs::create_dir_all(dest_plugins_dir)
            .map_err(|e| format!("Failed to create plugins directory {}: {e}", dest_plugins_dir.display()))?;

        if link {
            #[cfg(unix)]
            std::os::unix::fs::symlink(src, &target_dir)
                .map_err(|e| format!("Failed to symlink plugin: {e}"))?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_dir(src, &target_dir)
                .map_err(|e| format!("Failed to symlink plugin: {e}"))?;
        } else {
            fs::create_dir_all(&target_dir)
                .map_err(|e| format!("Failed to create plugin directory {}: {e}", target_dir.display()))?;
            copy_dir_recursive(src, &target_dir)
                .map_err(|e| format!("Failed to copy plugin files: {e}"))?;
        }

        let plugin = Plugin {
            id: plugin_id,
            manifest,
            root_dir: target_dir,
            is_project_local: false,
            enabled: true,
        };

        Ok(plugin)
    }

    /// Uninstall a plugin by ID from a plugins directory.
    pub fn uninstall(id: &str, plugins_dir: &Path) -> Result<bool, String> {
        let target = plugins_dir.join(id);
        if target.exists() || fs::symlink_metadata(&target).is_ok() {
            Self::remove_target_entry(&target)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn remove_target_entry(target: &Path) -> Result<(), String> {
        if let Ok(meta) = fs::symlink_metadata(target) {
            if meta.file_type().is_symlink() {
                #[cfg(unix)]
                {
                    fs::remove_file(target)
                        .map_err(|e| format!("Failed to remove symlink {}: {e}", target.display()))?;
                    return Ok(());
                }
                #[cfg(windows)]
                {
                    if target.is_dir() {
                        fs::remove_dir(target)
                            .map_err(|e| format!("Failed to remove symlink dir {}: {e}", target.display()))?;
                    } else {
                        fs::remove_file(target)
                            .map_err(|e| format!("Failed to remove symlink file {}: {e}", target.display()))?;
                    }
                    return Ok(());
                }
            }
        }
        if target.is_dir() {
            fs::remove_dir_all(target)
                .map_err(|e| format!("Failed to delete {}: {e}", target.display()))?;
        } else if target.is_file() {
            fs::remove_file(target)
                .map_err(|e| format!("Failed to delete file {}: {e}", target.display()))?;
        }
        Ok(())
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            // Skip .git and node_modules
            if entry.file_name() == ".git" || entry.file_name() == "node_modules" {
                continue;
            }
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_load_and_discover_plugins() {
        let tmp = tempdir().unwrap();
        let plugin_dir = tmp.path().join("my-guard");
        fs::create_dir_all(&plugin_dir).unwrap();

        let manifest_content = r#"{
            "name": "my-guard",
            "version": "1.0.0",
            "hooks": {
                "PreToolUse": [
                    { "command": "run.sh", "timeout": 3 }
                ]
            }
        }"#;
        fs::write(plugin_dir.join("plugin.json"), manifest_content).unwrap();

        let mut mgr = PluginManager::new();
        mgr.load_from_plugins_dir(tmp.path(), false);

        assert_eq!(mgr.list().len(), 1);
        let p = mgr.get("my-guard").unwrap();
        assert_eq!(p.manifest.name, "my-guard");
        assert!(p.enabled);

        let lifecycle = AgentLifecycle::PreToolUse {
            tool_name: "Bash".into(),
            input: serde_json::json!({}),
            prompt: None,
        };
        let hooks = mgr.hooks_for(&lifecycle);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].1.command, "run.sh");
    }

    #[test]
    fn test_claude_plugin_compatibility() {
        let tmp = tempdir().unwrap();
        let plugin_dir = tmp.path().join("claude-style");
        fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
        fs::create_dir_all(plugin_dir.join("hooks")).unwrap();

        fs::write(
            plugin_dir.join(".claude-plugin").join("plugin.json"),
            r#"{"name": "claude-style", "version": "0.2.0"}"#,
        )
        .unwrap();

        fs::write(
            plugin_dir.join("hooks").join("hooks.json"),
            r#"{"hooks": {"SessionStart": [{"command": "echo start"}]}}"#,
        )
        .unwrap();

        let mut mgr = PluginManager::new();
        mgr.load_from_plugins_dir(tmp.path(), false);

        let p = mgr.get("claude-style").unwrap();
        assert_eq!(p.manifest.version, "0.2.0");
        let session_start = AgentLifecycle::SessionStart {
            resume_command: None,
        };
        let hooks = mgr.hooks_for(&session_start);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].1.command, "echo start");
    }
}
