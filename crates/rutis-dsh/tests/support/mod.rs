#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The npm project with dsh, when installed.
pub fn project() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("dsh");
    dir.join("node_modules/@deepseek-ai/dsh-app-boot")
        .exists()
        .then_some(dir)
}

/// Every `cordis.patch.yml` of the installed dsh bundles, plus aimux's.
pub fn bundle_patch_files() -> Option<Vec<PathBuf>> {
    let project = project()?;
    let scope = project.join("node_modules/@deepseek-ai");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&scope)
        .ok()?
        .filter_map(|entry| {
            let file = entry.ok()?.path().join("cordis.patch.yml");
            file.exists().then_some(file)
        })
        .collect();
    files.push(project.join("aimux/aimux.patch.yml"));
    files.sort();
    Some(files)
}

/// Run `tests/support/dsh.mjs` and parse its JSON output.
pub fn node<'a>(args: &[&str], rest: impl IntoIterator<Item = &'a str>) -> Value {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/dsh.mjs");
    let output = Command::new("node")
        .arg(script)
        .args(args)
        .args(rest)
        .output()
        .expect("node");
    assert!(
        output.status.success(),
        "dsh.mjs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("dsh.mjs output")
}
