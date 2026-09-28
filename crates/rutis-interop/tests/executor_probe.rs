//! Scheduling probes, not the production callback protocol or deadlock detector.

use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn rust_sync_wait_can_complete_a_send_async_callback_via_a_background_runtime() {
    let origin = runtime();
    let background = runtime();
    let background_handle = background.handle().clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let worker = std::thread::spawn(move || background.block_on(stopped));
    // A real Node peer requests a Rust callback before returning its response.
    // The fixture only exchanges scalar values; no framework is patched.
    let mut node = Command::new("node")
        .args([
            "-e",
            r#"
            const lines = require('node:readline').createInterface({ input: process.stdin });
            lines.on('line', line => {
                const message = JSON.parse(line);
                if (message.type === 'invoke') {
                    process.stdout.write(JSON.stringify({ type: 'callback' }) + '\n');
                } else if (message.type === 'callback-result') {
                    process.stdout.write(JSON.stringify({ value: message.value + 1 }) + '\n');
                    lines.close();
                    process.stdin.destroy();
                }
            });
        "#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let writer = Arc::new(Mutex::new(node.stdin.take().unwrap()));
    let reader = BufReader::new(node.stdout.take().unwrap());
    let (complete, completed) = mpsc::channel();
    let reply_writer = writer.clone();
    let io = std::thread::spawn(move || {
        for line in reader.lines() {
            let message: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            if message["type"] == "callback" {
                let reply_writer = reply_writer.clone();
                background_handle.spawn(async move {
                    // These resources are created on the live background runtime.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let value = tokio::spawn(async { 41 }).await.unwrap();
                    writeln!(
                        reply_writer.lock().unwrap(),
                        "{{\"type\":\"callback-result\",\"value\":{value}}}"
                    )
                    .unwrap();
                });
            } else {
                complete.send(message["value"].as_i64().unwrap()).unwrap();
                break;
            }
        }
    });
    let result = origin.block_on(async {
        writeln!(writer.lock().unwrap(), "{{\"type\":\"invoke\"}}").unwrap();
        // Deliberately occupy the only origin-runtime thread, as a sync call does.
        completed.recv_timeout(Duration::from_secs(5))
    });
    drop(writer);
    // Always reap the owned peer, including on a failed scheduling experiment.
    let _ = node.kill();
    node.wait().unwrap();
    io.join().unwrap();
    let _ = stop.send(());
    worker.join().unwrap().unwrap();
    assert_eq!(result.unwrap(), 42);
}

fn needs_original_runtime<F: Future<Output = ()> + Send + 'static>(make: impl FnOnce() -> F) {
    let origin = runtime();
    let (first_poll, polled) = mpsc::channel();
    let (complete, completed) = tokio::sync::oneshot::channel();
    let worker = origin.block_on(async {
        let future = make();
        let worker = std::thread::spawn(move || {
            let background = runtime();
            background.block_on(async move {
                let mut future = std::pin::pin!(future);
                let mut first_poll = Some(first_poll);
                std::future::poll_fn(|cx| {
                    let state = future.as_mut().poll(cx);
                    if let Some(sender) = first_poll.take() {
                        sender.send(state.is_pending()).unwrap();
                    }
                    state
                })
                .await;
                let _ = complete.send(());
            });
        });
        assert!(polled.recv_timeout(Duration::from_secs(5)).unwrap());
        worker
    });
    let mut completed = completed;
    // The background thread is live; only the origin runtime is undriven.
    std::thread::sleep(Duration::from_millis(100));
    assert!(matches!(
        completed.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    // Resuming the original driver, without changing the future or background
    // executor, lets it finish. Timeout alone is not treated as a cycle detector.
    origin.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), completed)
            .await
            .unwrap()
            .unwrap();
    });
    worker.join().unwrap();
}

#[test]
fn send_future_with_a_saved_origin_handle_still_needs_the_origin_scheduler() {
    needs_original_runtime(|| {
        let handle = tokio::runtime::Handle::current();
        async move { handle.spawn(async {}).await.unwrap() }
    });
}

#[test]
fn send_future_with_an_existing_timer_still_needs_the_origin_driver() {
    needs_original_runtime(|| {
        // Unlike a timer created inside the background async body, this timer
        // captures the origin runtime before its Send future changes threads.
        tokio::time::sleep(Duration::from_millis(30))
    });
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn node_sync_wait_can_pump_a_rust_callback_but_not_timer_or_promise_continuations() {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("probe.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../interop/node/test/fixtures/sync-callback-probe.mjs");
    let child = tokio::process::Command::new("node")
        .arg(script)
        .arg(&socket)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut stream = stream.into_std().unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"],
        "invoke"
    );
    // The Node timer is due during the synchronous call, not merely queued
    // after it. Its callback still cannot run until the JS stack yields.
    std::thread::sleep(Duration::from_millis(40));
    writeln!(stream, "{{\"type\":\"callback\",\"value\":40}}").unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let callback: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(callback["type"], "callback-result");
    assert_eq!(callback["value"], 41);
    writeln!(stream, "{{\"type\":\"result\",\"value\":42}}").unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({ "result": 42, "callbacks": 1, "resumed": true })
    );
}
