//! Records a current protocol gap; passing this probe is not error-shape
//! compatibility. Full error/reference transport remains a P2 requirement.
#![cfg(unix)]

use std::io::Write;
use std::path::Path;

use rutis_interop::{Error, Process};
use serde_json::json;

#[tokio::test(flavor = "current_thread")]
async fn current_method_protocol_loses_remote_aggregate_members_and_cause() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin
        .write_all(
            br#"
        export function apply(ctx) {
            const cause = new Error('cause');
            const error = new AggregateError([
                new TypeError('first'),
                new AggregateError([new Error('inner')], 'nested'),
            ], 'outer', { cause });
            const shape = value => ({
                name: value.name, message: value.message,
                cause: value.cause ? shape(value.cause) : null,
                errors: value.errors ? value.errors.map(shape) : null,
            });
            ctx.provide('errors', {
                nativeShape() { return shape(error); },
                fail() { throw error; },
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
        json!({ "errors": ["nativeShape", "fail"] }),
    )
    .await
    .unwrap();
    let native = peer.call("errors", "nativeShape", json!([]));
    let crossed = peer.call("errors", "fail", json!([]));
    peer.dispose().await.unwrap();
    let native = native.unwrap();
    assert_eq!(native["cause"]["message"], "cause");
    assert_eq!(native["errors"][0]["name"], "TypeError");
    assert_eq!(native["errors"][1]["errors"][0]["message"], "inner");
    // The current error variant has no fields for the information above.
    assert!(matches!(crossed, Err(Error::Remote { name, message })
        if name == "AggregateError" && message == "outer"));
}
