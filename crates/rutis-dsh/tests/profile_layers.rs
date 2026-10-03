//! Profile layers composed in Rust against dsh-app-boot's own
//! `loadProfileDirectory` + `readProfilePatches` + `composeEntries`.

mod support;

use std::path::{Path, PathBuf};

use rutis_dsh::profile::{load, ProfileContext};
use rutis_loader::apply_patches;
use serde_json::{json, Value};

struct Fixture {
    _dir: tempfile::TempDir,
    context: ProfileContext,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A profile with the web bundles, aimux, two broken bundles, a commented
/// user layer, a home layer and one overlay.
fn fixture(project: &Path, telemetry: Option<&str>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let profile = home.join("profiles/web");
    write(
        &profile.join("package.json"),
        &json!({
            "name": "dsh-profile-web",
            "private": true,
            "dsh": { "profile": { "bundles": [
                "@deepseek-ai/dsh-base",
                "@deepseek-ai/dsh-web-app",
                "@rutis/dsh-aimux",
                "fake-incompatible",
                "fake-not-a-bundle",
                "fake-missing"
            ] } }
        })
        .to_string(),
    );
    // Bundles only found through the profile's own node_modules.
    write(
        &profile.join("node_modules/fake-incompatible/package.json"),
        &json!({
            "name": "fake-incompatible", "version": "1.0.0",
            "peerDependencies": { "@deepseek-ai/dsh": "^9.0.0" },
            "dsh": { "bundle": { "patch": "p.yml" } }
        })
        .to_string(),
    );
    write(
        &profile.join("node_modules/fake-incompatible/p.yml"),
        "[]\n",
    );
    write(
        &profile.join("node_modules/fake-not-a-bundle/package.json"),
        &json!({ "name": "fake-not-a-bundle", "version": "1.0.0" }).to_string(),
    );
    write(
        &profile.join("cordis.patch.yml"),
        "# user layer\n\
         - id: timer\n  disabled: true\n\
         # a local plugin\n\
         - insert:\n    - id: local\n      name: ./plugins/local.js\n      config:\n        level: !!js Number(process.env.LEVEL ?? 2)\n\
         - id: web\n  name: wrong-name\n  disabled: true\n",
    );
    write(
        &home.join("cordis.patch.yml"),
        "- id: llm-aimux\n  config:\n    providers: {}\n",
    );
    let overlay = dir.path().join("overlay/extra.yml");
    write(
        &overlay,
        "- insert:\n    - id: extra\n      name: ../shared/extra.js\n- id: nope\n  disabled: true\n",
    );
    Fixture {
        context: ProfileContext {
            name: "web".into(),
            dir: profile,
            install_anchor: project.join("package.json"),
            home,
            overlays: vec![overlay],
            telemetry_disabled: telemetry.map(str::to_owned),
            runtime_version: None,
        },
        _dir: dir,
    }
}

fn node_compose(context: &ProfileContext) -> Value {
    let request = json!({
        "name": context.name,
        "dir": context.dir,
        "patchPath": context.user_layer_path(),
        "installAnchor": context.install_anchor,
        "home": context.home,
        "overlayFiles": context.overlays,
        "telemetryDisabledEnv": context.telemetry_disabled,
        "cwd": "/",
        "startedBundles": [],
    });
    support::node(&["compose"], [request.to_string().as_str()])
}

fn compare(project: PathBuf, telemetry: Option<&str>) {
    let fixture = fixture(&project, telemetry);
    let expected = node_compose(&fixture.context);
    let profile = load(&fixture.context).unwrap();
    let composed = apply_patches(&profile.layers);
    let theirs = expected["rows"].as_array().unwrap();
    for (i, (ours, theirs)) in composed.rows.iter().zip(theirs).enumerate() {
        assert_eq!(ours, theirs, "row {i} differs");
    }
    assert_eq!(
        composed.rows.len(),
        theirs.len(),
        "row count; ours end with {:?}",
        composed.rows.last()
    );
    let skipped: Vec<&str> = profile.skipped.iter().map(|s| s.package.as_str()).collect();
    assert_eq!(json!(skipped), expected["skipped"]);
    assert_eq!(
        composed.warnings.len() as u64,
        expected["warnings"].as_u64().unwrap()
    );
    assert!(composed.rows.len() > 30);
}

#[test]
fn composes_like_dsh_app_boot() {
    let Some(project) = support::project() else {
        eprintln!("skipped: the dsh npm project is not installed");
        return;
    };
    compare(project, None);
}

#[test]
fn telemetry_switch_like_dsh_app_boot() {
    let Some(project) = support::project() else {
        return;
    };
    compare(project.clone(), Some("1"));
    let fixture = fixture(&project, Some("1"));
    let profile = load(&fixture.context).unwrap();
    let rows = apply_patches(&profile.layers).flat;
    let telemetry = rows
        .iter()
        .find(|r| r.id.as_deref() == Some("session-telemetry-otel"))
        .unwrap();
    assert_eq!(telemetry.value["disabled"], json!(true));
}

#[test]
fn reports_why_bundles_were_skipped() {
    let Some(project) = support::project() else {
        return;
    };
    let fixture = fixture(&project, None);
    let profile = load(&fixture.context).unwrap();
    let reasons: Vec<(String, String)> = profile
        .skipped
        .iter()
        .map(|s| (s.package.clone(), s.reason.clone()))
        .collect();
    assert!(reasons[0].1.contains("incompatible"), "{reasons:?}");
    assert!(
        reasons[1].1.contains("declares no dsh.bundle"),
        "{reasons:?}"
    );
    assert!(reasons[2].1.contains("cannot resolve"), "{reasons:?}");
    // The user layer is editable at the version of its bytes.
    assert_eq!(profile.editable.layer, "user");
    assert_ne!(profile.editable.version.0, "absent");
}
