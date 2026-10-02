//! A built binary runs against a deployed copy of its npm project.
#![cfg(all(unix, dsh_installed))]

mod common;

use std::sync::Arc;

use common::{mounted, Scripted};
use rutis_dsh::agent;

#[tokio::test(flavor = "multi_thread")]
async fn mounts_load_from_the_npm_project_named_at_run_time() {
    let deployed = tempfile::tempdir().unwrap();
    let copy = deployed.path().join("dsh");
    // A deployment ships the npm project; its own files are copied and its
    // (large) installed dependencies linked.
    let project = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/dsh"));
    std::fs::create_dir(&copy).unwrap();
    for entry in std::fs::read_dir(project).unwrap() {
        let entry = entry.unwrap();
        let target = copy.join(entry.file_name());
        if entry.file_name() == "node_modules" {
            std::os::unix::fs::symlink(entry.path(), &target).unwrap();
        } else {
            let status = std::process::Command::new("cp")
                .arg("-R")
                .arg(entry.path())
                .arg(&target)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
    // Mark the copy, so the probe shows which project was loaded.
    let probe = copy.join("probes/llm-probe.ts");
    let source = std::fs::read_to_string(&probe).unwrap();
    let marked = source.replace(
        "    return chunks\n",
        "    chunks.push('\"deployed copy\"')\n    return chunks\n",
    );
    assert_ne!(source, marked);
    std::fs::write(&probe, marked).unwrap();

    // This test binary holds only this test: the variable reaches no other mount.
    std::env::set_var(rutis_interop::ROOT_VARIABLE, &copy);
    let ctx = mounted(Arc::new(Scripted::default())).await;
    let chunks = ctx
        .get::<agent::LlmProbe>()
        .unwrap()
        .collect("scripted", "m", "", "hi")
        .await
        .unwrap();
    assert_eq!(chunks.last().unwrap(), "\"deployed copy\"");
}
