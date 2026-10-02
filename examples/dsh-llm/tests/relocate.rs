//! A built binary runs against a deployed copy of its npm project.
#![cfg(all(unix, dsh_llm))]

mod common;

use std::sync::Arc;

use common::{mounted, Scripted};
use dsh_llm_mount::dsh;

#[tokio::test(flavor = "multi_thread")]
async fn mounts_load_from_the_npm_project_named_at_run_time() {
    let deployed = tempfile::tempdir().unwrap();
    let copy = deployed.path().join("cordis");
    // A deployment ships the npm project with its dependencies resolved.
    let status = std::process::Command::new("cp")
        .arg("-RL")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/cordis"))
        .arg(&copy)
        .status()
        .unwrap();
    assert!(status.success());
    // Mark the copy, so the probe shows which project was loaded.
    let probe = copy.join("probe.ts");
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
        .get::<dsh::LlmProbe>()
        .unwrap()
        .collect("scripted", "m", "", "hi")
        .await
        .unwrap();
    assert_eq!(chunks.last().unwrap(), "\"deployed copy\"");
}
