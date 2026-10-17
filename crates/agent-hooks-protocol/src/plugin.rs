//! Plugin manifest types for the universal agent-hooks plugin architecture.
//!
//! Compatible with the universal plugin manifest (`plugin.json`), Claude Code
//! plugin manifests (`.claude-plugin/plugin.json` + `hooks/hooks.json`), and
//! Oh My Pi (`omp`) plugin packages.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

fn default_version() -> String {
    "0.1.0".to_string()
}

/// A plugin manifest declaring metadata, hooks, and configuration schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,

    #[serde(default = "default_version")]
    pub version: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,

    /// Hooks declared directly in the manifest:
    /// `{ "PreToolUse": [ { "command": "bin/check.sh", "timeout": 5 } ] }`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, Vec<PluginHookDefinition>>,

    /// User configuration schema/definitions.
    #[serde(
        default,
        rename = "userConfig",
        alias = "user_config",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_config: Option<BTreeMap<String, PluginConfigProperty>>,
}

/// A hook entry defined within a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHookDefinition {
    /// Shell command or executable entrypoint relative to the plugin root.
    #[serde(default)]
    pub command: String,

    /// Execution timeout in seconds (fail-open if exceeded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    /// Match filter for when this hook should fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<PluginHookMatch>,
}

/// Match conditions for a plugin hook entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginHookMatch {
    /// Tool name to match (e.g. "Bash", "Edit"). Case-insensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,

    /// Substring match over the prompt or tool input.
    #[serde(
        default,
        rename = "promptMatches",
        alias = "prompt_matches",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_matches: Option<String>,
}

/// Declarative schema property for plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginConfigProperty {
    #[serde(default, rename = "type")]
    pub property_type: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_plugin_manifest() {
        let json = r#"{ "name": "block-rm" }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "block-rm");
        assert_eq!(manifest.version, "0.1.0");
        assert!(manifest.hooks.is_empty());
    }

    #[test]
    fn parse_full_plugin_manifest() {
        let json = r#"{
            "name": "security-guard",
            "version": "1.2.0",
            "description": "Block dangerous commands",
            "author": "builtby.win",
            "license": "MIT",
            "hooks": {
                "PreToolUse": [
                    {
                        "command": "bin/check.sh",
                        "timeout": 5,
                        "when": {
                            "tool": "Bash",
                            "promptMatches": "rm -rf"
                        }
                    }
                ]
            },
            "userConfig": {
                "strict": {
                    "type": "boolean",
                    "default": true
                }
            }
        }"#;
        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.name, "security-guard");
        assert_eq!(manifest.version, "1.2.0");
        let pre_hooks = manifest.hooks.get("PreToolUse").unwrap();
        assert_eq!(pre_hooks.len(), 1);
        assert_eq!(pre_hooks[0].command, "bin/check.sh");
        assert_eq!(pre_hooks[0].timeout, Some(5));
        let when = pre_hooks[0].when.as_ref().unwrap();
        assert_eq!(when.tool.as_deref(), Some("Bash"));
        assert_eq!(when.prompt_matches.as_deref(), Some("rm -rf"));
        assert!(manifest.user_config.unwrap().contains_key("strict"));
    }
}
