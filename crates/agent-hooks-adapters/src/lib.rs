//! # agent-hooks-adapters
//!
//! Harness adapters for normalizing raw hook inputs into agent events.
//! Supports Claude Code, Codex, Gemini, OpenCode, Pi, Copilot, Cursor, Kiro, JCode, Amp, Hermes.
//!
//! All adapters are declarative: each harness is described by an
//! [`AdapterManifest`] (see `builtin/<id>/adapter.json`) and served at
//! runtime by [`DeclarativeAdapter`] via [`AdapterRegistry`].

pub mod declarative;
pub mod error;
pub mod manifest;
pub mod registry;
pub mod traits;

// Generated builtin manifests (build.rs)
pub mod builtin_manifests {
    include!(concat!(env!("OUT_DIR"), "/builtin_manifests.rs"));
}

/// Content of a builtin staged asset, resolved by harness id + manifest key.
///
/// `stage_assets` ops reference sources as `"embedded": "<key>"` where
/// `<key>` is a path under `builtin/<harness>/assets/`. `harness` is the
/// manifest `id` (== its `builtin/` folder name), so user/project-tier
/// manifests — which have no embedded sources — simply never resolve.
pub fn embedded_asset(harness: &str, key: &str) -> Option<&'static str> {
    builtin_manifests::BUILTIN_ASSETS
        .iter()
        .find(|(h, k, _)| *h == harness && *k == key)
        .map(|(_, _, content)| *content)
}

#[cfg(test)]
mod builtin_tests {
    use super::*;

    #[test]
    fn builtin_dir_names_match_manifest_ids() {
        for (dir, raw) in builtin_manifests::BUILTIN_MANIFESTS {
            let m =
                AdapterManifest::parse(raw).unwrap_or_else(|e| panic!("{dir}/adapter.json: {e}"));
            assert_eq!(
                m.id, *dir,
                "builtin/{dir}/ folder name must equal manifest id"
            );
        }
    }

    #[test]
    fn embedded_assets_resolve_by_harness_and_key() {
        let idx = embedded_asset("pi", "pi/index.ts").expect("pi stages drop-in extension");
        assert!(!idx.is_empty());
        assert!(embedded_asset("pi", "pi/package.json").is_some());
        // Key belongs to another harness's assets dir.
        assert!(embedded_asset("pi", "index.ts").is_none());
        // omp is a separate harness with its own staged plugin assets.
        let omp_idx = embedded_asset("omp", "oh-my-pi/index.ts").expect("omp stages plugin entry");
        assert!(!omp_idx.is_empty());
        assert!(embedded_asset("omp", "oh-my-pi/package.json").is_some());
        // External-tier manifests have no embedded sources.
        assert!(embedded_asset("acme-cli", "index.ts").is_none());
    }
}

// Re-export primary types
pub use declarative::DeclarativeAdapter;
pub use error::AdapterError;
pub use manifest::{AdapterManifest, ResponseMode};
pub use registry::AdapterRegistry;
pub use traits::{HarnessAdapter, HookDefinition, HookEventTarget, HookLocation};

use std::sync::Arc;

/// All adapters as `Arc` (legacy `all_adapters()` returned `Vec<Arc<dyn HarnessAdapter>>`).
pub fn all_adapters() -> Vec<Arc<dyn HarnessAdapter>> {
    AdapterRegistry::all().into_iter().map(Arc::from).collect()
}

/// Resolve an adapter by hint (legacy).
pub fn adapter_for_hint(hint: &str) -> Option<Arc<dyn HarnessAdapter>> {
    AdapterRegistry::get(hint).map(Arc::from)
}
