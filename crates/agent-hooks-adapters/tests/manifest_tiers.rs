//! Tier discovery: embedded < user < project, flat + per-adapter folder
//! layouts, same-id shadowing, and `extends` across tiers.

use agent_hooks_adapters::manifest::AdapterManifest;
use std::fs;
use std::path::Path;

fn write_flat(tier: &Path, id: &str, display: &str) {
    let manifest = format!(
        r#"{{"id":"{id}","display_name":"{display}","default_version":"v1",
            "versions":{{"v1":{{"events":{{"stop":{{"lifecycle":"Stop","capability":"continue"}}}}}}}}}}"#
    );
    fs::create_dir_all(tier).unwrap();
    fs::write(tier.join(format!("{id}.json")), manifest).unwrap();
}

fn write_folder(tier: &Path, id: &str, display: &str) {
    let dir = tier.join(id);
    fs::create_dir_all(dir.join("fixtures")).unwrap();
    let manifest = format!(
        r#"{{"id":"{id}","display_name":"{display}","default_version":"v1",
            "versions":{{"v1":{{"events":{{"stop":{{"lifecycle":"Stop","capability":"continue"}}}}}}}}}}"#
    );
    fs::write(dir.join("adapter.json"), manifest).unwrap();
}

fn ids(manifests: &[AdapterManifest]) -> Vec<String> {
    manifests.iter().map(|m| m.id.clone()).collect()
}

#[test]
fn folder_and_flat_layouts_are_discovered() {
    let tmp = tempfile::tempdir().unwrap();
    let tier = tmp.path().join("adapters");
    write_folder(&tier, "acme-cli", "Acme Folder");
    write_flat(&tier, "flat-co", "Flat Co");

    let loaded = AdapterManifest::load_tiered_from(&[tier.clone()]);
    let ids = ids(&loaded);
    assert!(
        ids.contains(&"acme-cli".to_string()),
        "folder form: {ids:?}"
    );
    assert!(ids.contains(&"flat-co".to_string()), "flat form: {ids:?}");
}

#[test]
fn project_shadows_user_shadows_embedded() {
    let tmp = tempfile::tempdir().unwrap();
    let user = tmp.path().join("user");
    let project = tmp.path().join("project");

    // Same id as an embedded builtin ("pi"), overridden at both tiers.
    write_folder(&user, "pi", "User Pi Override");
    write_flat(&project, "pi", "Project Pi Override");
    // A user-tier-only adapter must survive.
    write_folder(&user, "solo", "Solo");

    // Precedence order, as documented for `load_tiered_from`.
    let loaded = AdapterManifest::load_tiered_from(&[project, user]);
    let ids = ids(&loaded);

    // Shadowed lower-tier copies are dropped, not merged.
    let pi_count = ids.iter().filter(|id| *id == "pi").count();
    assert_eq!(pi_count, 1, "exactly one 'pi' survives: {ids:?}");
    // Higher tier comes first: first-match lookup yields the project copy.
    let pi = loaded.iter().find(|m| m.id == "pi").unwrap();
    assert_eq!(pi.display_name, "Project Pi Override");
    // Embedded manifests still load and sit last.
    assert!(ids.contains(&"antigravity".to_string()));
    let pi_pos = ids.iter().position(|id| id == "pi").unwrap();
    let ag_pos = ids.iter().position(|id| id == "antigravity").unwrap();
    assert!(
        pi_pos < ag_pos,
        "tiered manifests precede embedded: {ids:?}"
    );
}

#[test]
fn meta_sidecars_and_broken_manifests_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let tier = tmp.path().join("adapters");
    fs::create_dir_all(&tier).unwrap();
    fs::write(
        tier.join("good.json"),
        r#"{"id":"good","display_name":"G","default_version":"v1","versions":{"v1":{}}}"#,
    )
    .unwrap();
    fs::write(tier.join("good.meta.json"), r#"{"source":"somewhere"}"#).unwrap();
    fs::write(tier.join("broken.json"), r#"{"id": 12345}"#).unwrap();

    let loaded = AdapterManifest::load_tiered_from(&[tier.clone()]);
    let ids = ids(&loaded);
    // Only the good tier manifest precedes embedded; sidecar and broken
    // files are skipped with a warning, never fatal.
    assert_eq!(ids.first().map(String::as_str), Some("good"), "{ids:?}");
    assert_eq!(
        ids.iter().filter(|id| *id == "good").count(),
        1,
        "no duplicate parse: {ids:?}"
    );
}

#[test]
fn extends_across_tiers_inherits_embedded_base() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("adapters");
    fs::create_dir_all(&project).unwrap();
    // Inherits everything from the embedded antigravity manifest, overrides
    // display_name only.
    fs::write(
        project.join("my-antigravity.json"),
        r#"{"id":"my-antigravity","display_name":"My Antigravity","extends":"antigravity","default_version":"v1"}"#,
    )
    .unwrap();

    let loaded = AdapterManifest::load_tiered_from(&[project.clone()]);
    let resolved = AdapterManifest::resolve_all(&loaded).unwrap();
    let mine = resolved.iter().find(|m| m.id == "my-antigravity").unwrap();
    assert_eq!(mine.display_name, "My Antigravity");
    // Inherited from the embedded base: the antigravity v1 event set.
    let v1 = mine.get_version("v1").expect("inherited v1");
    assert!(v1.events.contains_key("Stop"));
    assert!(v1.events.contains_key("PreToolUse"));
}
