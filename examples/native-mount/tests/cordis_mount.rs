#![cfg(unix)]

#[tokio::test]
async fn cordis_mounts_the_original_rust_plugin() {
    let node = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let mut child = tokio::process::Command::new("node")
        .args([
            "--test",
            "--test-isolation=none",
            "test/fixtures/rust-mount.test.mjs",
            "test/fixtures/event-mount.test.mjs",
        ])
        .current_dir(node)
        .env("RUTIS_BINDINGS", concat!(env!("OUT_DIR"), "/rutis.mjs"))
        .env("RUTIS_EXECUTABLE", env!("CARGO_BIN_EXE_rutis-counter"))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait())
        .await
        .expect("Cordis interoperability tests timed out")
        .unwrap();
    assert!(
        status.success(),
        "Cordis interoperability tests failed: {status}"
    );
}
