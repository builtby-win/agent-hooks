use std::path::{Path, PathBuf};

/// Generate the embedded-builtin tables from `builtin/{id}/` folders:
///
/// - `BUILTIN_MANIFESTS: &[(&str, &str)]` — `(dir_name, adapter.json content)`
/// - `BUILTIN_ASSETS: &[(&str, &str, &str)]` — `(dir_name, asset_key, content)`
///   where `asset_key` is the path under that harness's `assets/` dir, the
///   same key a manifest's `stage_assets` op references via `"embedded"`.
///
/// Adding a harness (or a staged asset) is pure file work: create
/// `builtin/{id}/adapter.json`, drop files under `builtin/{id}/assets/`, and
/// reference them from the manifest. No Rust edits.
fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("builtin_manifests.rs");
    let builtin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("builtin");
    println!("cargo:rerun-if-changed=builtin");

    let mut dirs: Vec<PathBuf> = list_dirs(&builtin_dir);
    dirs.sort();

    let mut manifests: Vec<String> = Vec::new();
    let mut assets: Vec<String> = Vec::new();
    for dir in dirs {
        let id = dir.file_name().unwrap().to_string_lossy().to_string();
        let manifest = dir.join("adapter.json");
        if !manifest.is_file() {
            continue;
        }
        manifests.push(format!(
            "    ({id:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/builtin/{id}/adapter.json\"))),"
        ));
        let asset_root = dir.join("assets");
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&asset_root, Path::new(""), &mut files);
        files.sort();
        for rel in files {
            let key = rel.to_string_lossy().replace('\\', "/");
            assets.push(format!(
                "    ({id:?}, {key:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/builtin/{id}/assets/{key}\"))),"
            ));
        }
    }

    let content = format!(
        "pub static BUILTIN_MANIFESTS: &[(&str, &str)] = &[\n{}\n];\n\npub static BUILTIN_ASSETS: &[(&str, &str, &str)] = &[\n{}\n];\n",
        manifests.join("\n"),
        assets.join("\n")
    );
    std::fs::write(&dest, content).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}

/// Immediate subdirectories of `dir` (skips non-dirs and dot entries).
fn list_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !is_hidden(p))
        .collect()
}

/// Files under `root` recursively, as `(root-relative path)` pairs appended to
/// `out`. Skips dot entries (`.DS_Store`, …) so they never get embedded.
fn collect_files(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    for entry in read.filter_map(|e| e.ok()) {
        let path = entry.path();
        if is_hidden(&path) {
            continue;
        }
        let child = rel.join(entry.file_name());
        if path.is_dir() {
            collect_files(root, &child, out);
        } else if path.is_file() {
            out.push(child);
        }
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with('.'))
}
