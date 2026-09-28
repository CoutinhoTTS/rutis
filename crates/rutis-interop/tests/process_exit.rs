#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use rutis_interop::{Error, Process};
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn process_exit_fails_both_pending_and_subsequent_calls() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    // A native plugin: no protocol callbacks or wire declarations in its code.
    plugin
        .write_all(
            br#"
        export function apply(ctx) {
            let waiting = false;
            ctx.provide('lifecycle', {
                wait() { waiting = true; return new Promise(() => {}); },
                started() { return waiting; },
                crash() { process.exit(17); },
            });
        }
    "#,
        )
        .unwrap();
    let node_package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../interop/node")
        .canonicalize()
        .unwrap();
    let process = Process::launch(
        &node_package,
        plugin.path(),
        json!({}),
        json!({ "lifecycle": ["wait", "started", "crash"] }),
    )
    .await
    .unwrap();
    let pending = {
        let process = process.clone();
        tokio::spawn(async move { process.call_async("lifecycle", "wait", json!([])).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(
        process.call("lifecycle", "started", json!([])).unwrap(),
        json!(true)
    );
    assert!(matches!(
        process.call("lifecycle", "crash", json!([])),
        Err(Error::Transport(_))
    ));
    assert!(tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    assert!(process.call("lifecycle", "started", json!([])).is_err());
    assert!(process.dispose().await.is_err());
}
