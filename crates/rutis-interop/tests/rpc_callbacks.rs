#![cfg(unix)]
use rutis_interop::{rpc::Value, Error, Process};
use serde_json::json;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

async fn launch() -> (tempfile::NamedTempFile, Arc<Process>) {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin
        .write_all(
            br#"
      export function apply(ctx) {
        let saved;
        const twice = x => x * 2;
        ctx.provide('callbacks', {
          apply(fn, n) { return fn(n); },
          nested(fn) { return fn(twice); },
          save(fn) { saved = fn; },
          fire(n) { return saved(n); },
          clear() { saved = undefined; },
          same(a, b) { return a === b; },
          echo(value) { return value; },
          getFunction() { return twice; },
          onlyInvoke(fn) { return fn(4) instanceof Promise; },
          async awaitCallback(fn) { return await fn(4); },
          current() { return 12; },
        });
      }
    "#,
        )
        .unwrap();
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let peer = Process::launch(&package, plugin.path(), json!({}), json!({"callbacks": ["apply", "nested", "save", "fire", "clear", "same", "echo", "getFunction", "onlyInvoke", "awaitCallback", "current"]})).await.unwrap();
    (plugin, peer)
}
fn number(value: Value) -> f64 {
    value.json().unwrap().as_f64().unwrap()
}
fn args(values: Vec<Value>) -> Value {
    Value::List(values)
}

#[tokio::test(flavor = "current_thread")]
async fn sync_wait_pumps_original_thread_callback_and_nested_calls() {
    let (_file, peer) = launch().await;
    let caller = std::thread::current().id();
    let connection = peer.connection().clone();
    let callback = Value::callback(move |values| {
        assert_eq!(std::thread::current().id(), caller);
        let n = number(values.list()?.remove(0));
        let current = number(connection.invoke("callbacks", "current", json!([]).into())?);
        Ok(json!(n + current).into())
    });
    let result = peer
        .connection()
        .invoke("callbacks", "apply", args(vec![callback, json!(2).into()]))
        .unwrap();
    assert_eq!(number(result), 14.0);
    let callback = Value::callback(|values| {
        let function = values.list()?.remove(0).reference()?;
        function.call(json!([21]).into())
    });
    assert_eq!(
        number(
            peer.connection()
                .invoke("callbacks", "nested", args(vec![callback]))
                .unwrap()
        ),
        42.0
    );
    peer.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn saved_callbacks_and_live_proxy_identity_survive_the_first_call() {
    let (_file, peer) = launch().await;
    let callback =
        Value::callback(|values| Ok(json!(number(values.list()?.remove(0)) + 1.0).into()));
    assert_eq!(
        peer.connection()
            .invoke(
                "callbacks",
                "same",
                args(vec![callback.clone(), callback.clone()])
            )
            .unwrap()
            .json()
            .unwrap(),
        json!(true)
    );
    peer.connection()
        .invoke("callbacks", "save", args(vec![callback]))
        .unwrap();
    assert_eq!(
        peer.call("callbacks", "fire", json!([9])).unwrap(),
        json!(10)
    );
    peer.call("callbacks", "clear", json!([])).unwrap();
    let first = peer
        .connection()
        .invoke("callbacks", "getFunction", json!([]).into())
        .unwrap();
    let second = peer
        .connection()
        .invoke("callbacks", "getFunction", json!([]).into())
        .unwrap();
    assert_eq!(
        peer.connection()
            .invoke(
                "callbacks",
                "same",
                args(vec![first.clone(), second.clone()])
            )
            .unwrap()
            .json()
            .unwrap(),
        json!(true)
    );
    drop(first);
    assert_eq!(
        number(
            second
                .clone()
                .reference()
                .unwrap()
                .call(json!([7]).into())
                .unwrap()
        ),
        14.0
    );
    // Return the proxy home while its owning message is in flight.
    let returned = peer
        .connection()
        .invoke("callbacks", "echo", args(vec![second]))
        .unwrap();
    assert_eq!(
        number(
            returned
                .reference()
                .unwrap()
                .call(json!([8]).into())
                .unwrap()
        ),
        16.0
    );
    peer.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn invoke_keeps_future_lazy_and_async_wait_uses_original_executor() {
    let (_file, peer) = launch().await;
    let polled = Arc::new(AtomicBool::new(false));
    let observed = polled.clone();
    let callback = Value::callback(move |_| {
        let polled = observed.clone();
        Ok(Value::future(async move {
            polled.store(true, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok(json!(42).into())
        }))
    });
    assert_eq!(
        peer.connection()
            .invoke("callbacks", "onlyInvoke", args(vec![callback.clone()]))
            .unwrap()
            .json()
            .unwrap(),
        json!(true)
    );
    assert!(!polled.load(Ordering::SeqCst));
    let future = peer
        .connection()
        .invoke_async("callbacks", "awaitCallback", args(vec![callback]))
        .await
        .unwrap()
        .reference()
        .unwrap();
    assert_eq!(number(future.wait_async().await.unwrap()), 42.0);
    assert!(polled.load(Ordering::SeqCst));
    peer.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sync_await_reports_known_executor_cycle_but_allows_ready_future() {
    let (_file, peer) = launch().await;
    for ready in [true, false] {
        let callback = Value::callback(move |_| {
            Ok(Value::future(async move {
                if !ready {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Ok(json!(42).into())
            }))
        });
        let future = peer
            .connection()
            .invoke("callbacks", "awaitCallback", args(vec![callback]))
            .unwrap()
            .reference()
            .unwrap();
        let result = future.wait();
        if ready {
            assert_eq!(number(result.unwrap()), 42.0);
        } else {
            assert!(
                matches!(result, Err(Error::Remote { ref name, .. }) if name == "SyncWaitCycle")
            );
        }
    }
    peer.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn node_sync_wait_pumps_callbacks_and_reports_its_executor_cycle() {
    use rutis_interop::rpc::{Connection, Dispatch, Reference, Reply};
    use std::sync::Mutex;
    struct Service(Mutex<Option<Reference>>);
    impl Dispatch for Service {
        fn invoke(&self, _: &Connection, _: &str, method: &str, args: Value) -> Reply {
            if method == "dispose" {
                return Ok(Value::Undefined);
            }
            let mut args = args.list()?;
            match method {
                "apply" => args.remove(0).reference()?.call(Value::List(args)),
                "add" => Ok(json!(number(args.remove(0)) + number(args.remove(0))).into()),
                "save" => {
                    *self.0.lock().unwrap() = Some(args.remove(0).reference()?);
                    Ok(Value::Undefined)
                }
                "fire" => {
                    let callback = self.0.lock().unwrap().clone().unwrap();
                    callback.call(Value::List(args))
                }
                "saved" => Ok(Value::Reference(self.0.lock().unwrap().clone().unwrap())),
                "clear" => {
                    self.0.lock().unwrap().take();
                    Ok(Value::Undefined)
                }
                "onlyInvoke" => Ok(json!(args
                    .remove(0)
                    .reference()?
                    .call(json!([]).into())?
                    .reference()?
                    .is_future())
                .into()),
                "awaitCallback" => {
                    let callback = args.remove(0).reference()?;
                    Ok(Value::future(async move {
                        callback
                            .call_async(json!([]).into())
                            .await?
                            .reference()?
                            .wait_async()
                            .await
                    }))
                }
                "syncAwait" => args
                    .remove(0)
                    .reference()?
                    .call(json!([]).into())?
                    .reference()?
                    .wait(),
                "dispose" => Ok(Value::Undefined),
                _ => Err(Error::Value(method.into())),
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../interop/node/test/fixtures/rpc-client.mjs");
    let mut child = tokio::process::Command::new("node")
        .arg(fixture)
        .arg(socket)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let peer = Connection::connect(
        stream.into_std().unwrap(),
        Arc::new(Service(Mutex::new(None))),
    )
    .unwrap();
    peer.ready().await.unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    peer.closed().await;
}

#[tokio::test(flavor = "current_thread")]
async fn independent_callback_progresses_while_original_runtime_is_blocked() {
    let (_file, peer) = launch().await;
    let owner = std::thread::current().id();
    let connection = peer.connection().clone();
    let callback = Value::callback(move |_| {
        connection.independent_future(move || async move {
            assert_ne!(std::thread::current().id(), owner);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let value = tokio::spawn(async { 42 }).await.unwrap();
            Ok(json!(value).into())
        })
    });
    let future = peer
        .connection()
        .invoke("callbacks", "awaitCallback", args(vec![callback]))
        .unwrap()
        .reference()
        .unwrap();
    assert_eq!(number(future.wait().unwrap()), 42.0);
    peer.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sync_wait_detects_an_async_callback_already_running_on_its_runtime() {
    let (_file, peer) = launch().await;
    let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let callback = Value::callback(move |_| {
        let started = started.clone();
        Ok(Value::future(async move {
            started.send(()).unwrap();
            std::future::pending().await
        }))
    });
    let future = peer
        .connection()
        .invoke_async("callbacks", "awaitCallback", args(vec![callback]))
        .await
        .unwrap()
        .reference()
        .unwrap();
    observed.recv().await.unwrap();
    assert!(matches!(future.wait(), Err(Error::Remote { name, .. }) if name == "SyncWaitCycle"));
    peer.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn close_cancels_shared_executor_even_with_a_retained_reference() {
    let (_file, peer) = launch().await;
    let (started, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let (dropped, mut completed) = tokio::sync::mpsc::unbounded_channel();
    struct OnDrop(tokio::sync::mpsc::UnboundedSender<()>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let connection = peer.connection().clone();
    let callback = Value::callback(move |_| {
        let (started, dropped) = (started.clone(), dropped.clone());
        connection.independent_future(move || async move {
            let _guard = OnDrop(dropped);
            started.send(()).unwrap();
            std::future::pending().await
        })
    });
    let future = peer
        .connection()
        .invoke_async("callbacks", "awaitCallback", args(vec![callback]))
        .await
        .unwrap()
        .reference()
        .unwrap();
    observed.recv().await.unwrap();
    peer.connection()
        .close(Error::Transport("explicit close".into()));
    tokio::time::timeout(std::time::Duration::from_secs(2), completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(future.wait_async().await.is_err());
}
