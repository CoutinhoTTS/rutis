//! Error graphs must survive a Rust relay without losing native JS structure.
#![cfg(unix)]
use std::io::Write;
use std::path::Path;

use rutis_interop::{rpc::Value, Error, Process};
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn nested_aggregate_cause_and_shared_identity_survive_a_rust_roundtrip() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin
        .write_all(
            br#"
      export function apply(ctx) {
        const cause = new Error('cause');
        cause.cause = cause;
        const error = new AggregateError([
          new TypeError('first', { cause }),
          new AggregateError([new RangeError('inner')], 'nested'),
        ], 'outer', { cause });
        const shape = e => ({
          name: e.name, message: e.message, stack: e.stack,
          cause: e.cause?.message,
          errors: e.errors?.map(shape),
        });
        const inspect = e => ({
          shape: shape(e), aggregate: e instanceof AggregateError,
          type: e.errors[0] instanceof TypeError,
          nested: e.errors[1].errors[0] instanceof RangeError,
          shared: e.cause === e.errors[0].cause,
          cycle: e.cause.cause === e.cause,
        });
        ctx.provide('errors', {
          native() { return inspect(error); },
          fail() { throw error; },
          relay(callback) { try { callback(); } catch (e) { return inspect(e); } },
        });
      }
    "#,
        )
        .unwrap();
    let node = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let peer = Process::launch(
        &node,
        plugin.path(),
        json!({}),
        json!({ "errors": ["native", "fail", "relay"] }),
    )
    .await
    .unwrap();
    let native = peer.call("errors", "native", json!([])).unwrap();
    let crossed = peer.call("errors", "fail", json!([])).unwrap_err();
    assert!(
        matches!(&crossed, Error::Remote { name, message, graph: Some(_) }
        if name == "AggregateError" && message == "outer")
    );
    let callback = Value::callback(move |_| Err(crossed.clone()));
    let returned = peer
        .connection()
        .invoke("errors", "relay", Value::List(vec![callback]))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(returned, native);
    for field in ["aggregate", "type", "nested", "shared", "cycle"] {
        assert_eq!(returned[field], true);
    }
    peer.dispose().await.unwrap();
}
