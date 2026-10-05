//! Conformance suites (feature `conformance`): what any implementation of
//! the session protocol, or of the runtime row contract, must do, as checks
//! an implementation runs against itself. Each check panics with what was
//! broken.
//!
//! **Session** ([`session`]): the far end serves the target `conformance`:
//!
//! | method | does |
//! | --- | --- |
//! | `echo(value)` | returns `value` |
//! | `apply(fn, value)` | returns `fn(value)`, calling back during the call |
//! | `later(value)` | returns a future of `value` |
//! | `fail(name, message)` | throws an error named `name` |
//! | `hold(fn)` / `fire(value)` / `drop()` | keeps `fn`, calls it, lets it go |
//! | `abortable(signal)` | returns a future that ends once `signal` aborts |
//! | `aborted()` | whether such a signal aborted |
//! | `reenter(fn)` | returns `fn()`; `fn` calls `echo` while the far end waits |
//!
//! [`Fixture`] is this implementation of it, for sessions between two Rust
//! endpoints.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::rpc::{settle, Connection, Dispatch, Reference, Reply, Value};
use crate::Error;

const TARGET: &str = "conformance";

/// The `conformance` target, for a Rust far end.
#[derive(Default)]
pub struct Fixture {
    held: Mutex<Option<Reference>>,
    aborted: Arc<AtomicBool>,
}

impl Dispatch for Fixture {
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: Value) -> Reply {
        if target != TARGET {
            return Err(Error::Value(format!("no target {target}")));
        }
        let mut args = args.list()?.into_iter();
        let mut next = || args.next().unwrap_or(Value::Undefined);
        match method {
            "echo" => Ok(next()),
            "apply" => {
                let callback = next().reference()?;
                callback.call(Value::List(vec![next()]))
            }
            "later" => {
                let value = next();
                Ok(Value::future(async move {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Ok(value)
                }))
            }
            "fail" => {
                let name: String = crate::decode(next().json()?)?;
                let message: String = crate::decode(next().json()?)?;
                Err(Error::Remote {
                    name,
                    message,
                    graph: None,
                })
            }
            "hold" => {
                *self.held.lock().unwrap() = Some(next().reference()?);
                Ok(Value::Undefined)
            }
            "fire" => {
                let held = self.held.lock().unwrap().clone();
                held.ok_or_else(|| Error::Value("nothing held".into()))?
                    .call(Value::List(vec![next()]))
            }
            "drop" => {
                self.held.lock().unwrap().take();
                Ok(Value::Undefined)
            }
            "abortable" => {
                // The signal argument aborts when the caller drops the call;
                // a Rust callee sees it as the future being dropped.
                let aborted = self.aborted.clone();
                struct Mark(Arc<AtomicBool>);
                impl Drop for Mark {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                Ok(Value::future(async move {
                    let _mark = Mark(aborted);
                    std::future::pending::<()>().await;
                    Ok(Value::Undefined)
                }))
            }
            "aborted" => Ok(json!(self.aborted.load(Ordering::SeqCst)).into()),
            "reenter" => {
                let callback = next().reference()?;
                let _ = peer;
                callback.call(Value::List(vec![]))
            }
            _ => Err(Error::Value(format!("no method {method}"))),
        }
    }
}

fn data(value: serde_json::Value) -> Value {
    Value::Data(value)
}

/// Run the session checks against `far`, a ready session whose far end
/// serves `conformance`. Synchronous calls block: run it from a thread that
/// may block (`spawn_blocking`).
pub fn session(far: &Connection) {
    let call = |method: &str, args: Vec<Value>| far.invoke(TARGET, method, Value::List(args));
    let runtime = tokio::runtime::Handle::current();

    // Data crosses unchanged.
    let sample = json!({ "text": "a\nb ✓", "n": [1, 2.5, -3, 9007199254740991_u64], "none": null, "yes": true });
    let echoed = call("echo", vec![data(sample.clone())])
        .expect("echo")
        .json()
        .unwrap();
    assert_eq!(echoed, sample, "echo returns data unchanged");

    // A callback runs during the synchronous call that passed it.
    let doubled = call(
        "apply",
        vec![
            Value::callback(|args| {
                let [n]: [i64; 1] = crate::decode_value(args)?;
                Ok(json!(n * 2).into())
            }),
            data(json!(21)),
        ],
    )
    .expect("apply")
    .json()
    .unwrap();
    assert_eq!(doubled, json!(42), "a callback answers during the call");

    // Async results arrive.
    let later = runtime
        .block_on(async { settle(call("later", vec![data(json!("soon"))])?).await })
        .expect("later");
    assert_eq!(
        later.json().unwrap(),
        json!("soon"),
        "a future resolves to its value"
    );

    // Errors cross with their name.
    match call(
        "fail",
        vec![data(json!("TypeError")), data(json!("bad input"))],
    ) {
        Err(Error::Remote { name, message, .. }) => {
            assert_eq!(name, "TypeError", "an error keeps its name");
            assert!(
                message.contains("bad input"),
                "an error keeps its message: {message}"
            );
        }
        other => panic!("fail must throw, got {other:?}"),
    }

    // A reference kept by the far end stays callable until let go.
    call(
        "hold",
        vec![Value::callback(|args| {
            let [n]: [i64; 1] = crate::decode_value(args)?;
            Ok(json!(n + 1).into())
        })],
    )
    .expect("hold");
    assert_eq!(
        call("fire", vec![data(json!(1))])
            .expect("fire")
            .json()
            .unwrap(),
        json!(2)
    );
    assert_eq!(
        call("fire", vec![data(json!(41))])
            .expect("fire")
            .json()
            .unwrap(),
        json!(42)
    );
    call("drop", vec![]).expect("drop");
    assert!(
        call("fire", vec![data(json!(0))]).is_err(),
        "a dropped reference is gone"
    );

    // Dropping an async call aborts the far end's signal.
    runtime.block_on(async {
        let pending = far.invoke_async(TARGET, "abortable", Value::List(vec![Value::Signal]));
        let _ = tokio::time::timeout(Duration::from_millis(50), async {
            let reply = pending.await?;
            settle(reply).await
        })
        .await;
    });
    let aborted = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        call("aborted", vec![])
            .ok()
            .and_then(|value| value.json().ok())
            == Some(json!(true))
    });
    assert!(aborted, "cancelling a call aborts its signal");

    // While the far end waits on a synchronous callback, the callback may
    // call the far end again: the call belongs to the chain it waits for.
    let reentrant = far.clone();
    let nested = call(
        "reenter",
        vec![Value::callback(move |_| {
            reentrant.invoke(TARGET, "echo", Value::List(vec![data(json!("nested"))]))
        })],
    )
    .expect("reenter")
    .json()
    .unwrap();
    assert_eq!(
        nested,
        json!("nested"),
        "a nested call reaches the waiting far end"
    );
}

/// **Runtime** ([`runtime`]): a runtime loads, as a row, a plugin `entry`
/// that injects `clock` (a rutis host service with `now`, sync) and
/// provides `weather`:
///
/// | method | does |
/// | --- | --- |
/// | `today()` | `"Oslo at <clock.now()>"`, sync |
/// | `later()` | `"Oslo later"`, async |
/// | `each(fn)` | `[fn("mon"), fn("tue")]`, calling back during the call |
/// | `crash()` | ends the runtime process with status 17 |
///
/// The repository has it for Node (`interop/node/test/fixtures/
/// conformance-weather.mjs`) and Python (`interop/python/tests/
/// conformance_weather.py`).
#[cfg(unix)]
pub async fn runtime(process: Arc<crate::Process>, entry: &std::path::Path, started_here: bool) {
    use crate::{host_key, row_projection, HostDispatch};
    use std::sync::atomic::AtomicUsize;

    struct Clock(AtomicUsize);
    impl HostDispatch for Clock {
        fn invoke(&self, method: &str, _args: Value) -> Reply {
            assert_eq!(method, "now", "the plugin calls only clock.now");
            Ok(json!(self.0.fetch_add(1, Ordering::SeqCst)).into())
        }
        fn methods(&self) -> Option<serde_json::Value> {
            Some(json!({ "now": "sync" }))
        }
    }

    async fn eventually(mut check: impl FnMut() -> bool, what: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    let described = process.describe_row(entry).await.expect("rows.schema");
    assert_eq!(
        described.inject,
        ["clock"],
        "the plugin declares what it injects"
    );
    let provides = serde_json::Map::from_iter([(
        "weather".to_owned(),
        json!({ "today": "sync", "later": "async", "each": "sync", "crash": "sync" }),
    )]);
    let lease = process
        .lease_host("clock", Arc::new(Clock(AtomicUsize::new(0))), None)
        .await
        .expect("hosts.provide");
    let ctx = rutis::Ctx::root().unwrap();
    let projection = row_projection(&provides);
    projection.attach(&ctx, process.clone()).unwrap();
    let load = || {
        process.load_row_exporting(
            "w",
            entry,
            json!({}),
            &[],
            &[],
            &provides,
            projection.clone(),
        )
    };
    load().await.expect("rows.load");
    let key = host_key("weather");
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_some(),
        "the weather service",
    )
    .await;
    let weather = ctx.get_as::<dyn HostDispatch>(key.clone()).unwrap();

    let sync = weather.clone();
    let today = tokio::task::spawn_blocking(move || sync.invoke("today", json!([]).into()))
        .await
        .unwrap()
        .expect("today");
    assert_eq!(
        today.json().unwrap(),
        json!("Oslo at 0"),
        "a sync call that calls a host service"
    );
    let later = settle(weather.invoke("later", json!([]).into()).unwrap())
        .await
        .expect("later");
    assert_eq!(later.json().unwrap(), json!("Oslo later"), "an async call");
    let callback = Value::callback(|args| {
        let [day]: [String; 1] = crate::decode_value(args)?;
        Ok(json!(day.to_uppercase()).into())
    });
    let sync = weather.clone();
    let days =
        tokio::task::spawn_blocking(move || sync.invoke("each", Value::List(vec![callback])))
            .await
            .unwrap()
            .expect("each");
    assert_eq!(
        days.json().unwrap(),
        json!(["MON", "TUE"]),
        "a callback during a sync call"
    );
    drop(weather);

    process.unload_row("w").await.expect("rows.unload");
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_none(),
        "the withdrawal",
    )
    .await;

    // A crash ends the session; a process started here says how it ended.
    load().await.expect("rows.load again");
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_some(),
        "the service again",
    )
    .await;
    let weather = ctx.get_as::<dyn HostDispatch>(key.clone()).unwrap();
    let crashed = tokio::task::spawn_blocking(move || weather.invoke("crash", json!([]).into()))
        .await
        .unwrap();
    match crashed {
        Err(Error::Transport(message)) if started_here => assert_eq!(
            message, "Cordis process exited with exit status: 17",
            "the session ends with how the process ended"
        ),
        Err(Error::Transport(_)) => {}
        other => panic!("the crash should end the session, got {other:?}"),
    }
    process.closed().await;
    let expected = started_here.then_some("exited with exit status: 17");
    assert_eq!(process.exit_status().as_deref(), expected);
    projection.close();
    drop(lease);
    ctx.shutdown().await.unwrap();
}
