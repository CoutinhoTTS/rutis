//! The session conformance suite (feature `testing`): what any
//! implementation of the session protocol must do, as checks an
//! implementation runs against itself. Each check panics with what was
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

use crate::session::rpc::{settle, Connection, Dispatch, Reference, Reply, Value};
use crate::session::Error;

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
                let name: String = crate::session::decode(next().json()?)?;
                let message: String = crate::session::decode(next().json()?)?;
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
                let [n]: [i64; 1] = crate::session::decode_value(args)?;
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
            let [n]: [i64; 1] = crate::session::decode_value(args)?;
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
