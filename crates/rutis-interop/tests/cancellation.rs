#![cfg(unix)]
//! Dropping an async call cancels it: the AbortSignal the Cordis method
//! received aborts, the late reply is discarded, and the session goes on.

use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use rutis_interop::rpc::Value;
use rutis_interop::Process;
use serde_json::json;

const PLUGIN: &str = r#"
export function apply(ctx) {
  let aborted = false
  ctx.provide('work', {
    async wait(signal) {
      await new Promise((resolve, reject) => {
        signal.addEventListener('abort', () => { aborted = true; reject(signal.reason) })
      })
    },
    aborted() { return aborted },
    ping() { return 'pong' },
  })
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_call_aborts_its_signal_and_the_session_continues() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin.write_all(PLUGIN.as_bytes()).unwrap();
    let process = Process::launch(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node"),
        plugin.path(),
        json!({}),
        json!({ "work": ["wait", "aborted", "ping"] }),
    )
    .await
    .unwrap();
    let work = process.service("work").unwrap();

    let waited = tokio::time::timeout(
        Duration::from_millis(100),
        process.invoke_async(&work, "wait", vec![Value::Signal]),
    )
    .await;
    assert!(waited.is_err(), "the call only ends when cancelled");

    // The abort reaches the method, whose rejection arrives after the caller
    // gave up: it is discarded instead of breaking the session.
    let mut aborted = false;
    for _ in 0..50 {
        aborted = process.call(&work, "aborted", json!([])).unwrap() == json!(true);
        if aborted && process.connection().orphans() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(aborted, "the Cordis method's AbortSignal was not aborted");
    assert!(process.connection().orphans() > 0);
    assert_eq!(
        process.call(&work, "ping", json!([])).unwrap(),
        json!("pong")
    );
    process.dispose().await.unwrap();
}

// PR #73 review of 6794a3a: the reply to the invoke has been delivered (a
// Promise reference) but the caller has not polled again to await it; the
// call is dropped in that window and must still be cancelled.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_between_the_reply_and_its_await_still_cancels() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin.write_all(PLUGIN.as_bytes()).unwrap();
    let process = Process::launch(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node"),
        plugin.path(),
        json!({}),
        json!({ "work": ["wait", "aborted", "ping"] }),
    )
    .await
    .unwrap();
    let work = process.service("work").unwrap();
    {
        let call = process.invoke_async(&work, "wait", vec![Value::Signal]);
        let mut call = std::pin::pin!(call);
        // One poll sends the invoke; the reply then arrives unpolled.
        std::future::poll_fn(|cx| {
            let _ = call.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut aborted = false;
    for _ in 0..50 {
        aborted = process.call(&work, "aborted", json!([])).unwrap() == json!(true);
        if aborted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(aborted, "the call was dropped without a cancel");
    process.dispose().await.unwrap();
}
