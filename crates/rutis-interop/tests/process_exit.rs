#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use rutis_interop::{Error, Process};
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn cleanup_releases_an_in_flight_call() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin
        .write_all(
            br#"
        export function apply(ctx) {
            let resolve, started = false;
            const stopped = new Promise(done => { resolve = done; });
            ctx.provide('lifecycle', {
                wait() { started = true; return stopped; },
                started() { return started; },
            });
            ctx.effect(() => () => resolve('disposed'));
        }
    "#,
        )
        .unwrap();
    let node_package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let process = Process::launch(
        &node_package,
        plugin.path(),
        json!({}),
        json!({"lifecycle": ["wait", "started"]}),
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
    let disposed = tokio::time::timeout(Duration::from_secs(2), process.dispose()).await;
    if disposed.is_err() {
        pending.abort();
        let _ = pending.await;
        panic!("disposal waited for the call that only its disposer can release");
    }
    disposed.unwrap().unwrap();
    assert_eq!(pending.await.unwrap().unwrap(), json!("disposed"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_handle_keeps_its_object_after_the_slot_is_replaced() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin
        .write_all(
            br#"
        export function apply(ctx) {
            ctx.provide('counter', {
                current() { return 1; },
                replace() { ctx.set('counter', { current() { return 2; } }); },
                nativeCurrent() { return ctx.counter.current(); },
            });
        }
    "#,
        )
        .unwrap();
    let node_package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let process = Process::launch(
        &node_package,
        plugin.path(),
        json!({}),
        json!({"counter": ["current", "replace", "nativeCurrent"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        process.call("counter", "current", json!([])).unwrap(),
        json!(1)
    );
    process.call("counter", "replace", json!([])).unwrap();
    let current = process.call("counter", "current", json!([]));
    let native_current = process.call("counter", "nativeCurrent", json!([]));
    // The slot now has a new handle for the replacement object.
    let replaced = process.service("counter").unwrap();
    let replaced_current = process.call(&replaced, "current", json!([]));
    process.dispose().await.unwrap();
    assert_eq!(current.unwrap(), json!(1));
    assert_eq!(native_current.unwrap(), json!(2));
    assert_ne!(replaced, "counter");
    assert_eq!(replaced_current.unwrap(), json!(2));
}

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
