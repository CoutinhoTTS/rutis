#![cfg(unix)]
//! A group of plugins shares one Cordis Context: dependencies between them
//! resolve natively, and a dependency nothing in the group provides fails
//! the mount with its name.

use std::io::Write;
use std::path::Path;

use rutis_bridge::runtime::Process;
use serde_json::json;

fn plugin(source: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    file.write_all(source.as_bytes()).unwrap();
    file
}

const PROVIDER: &str = r#"
export function apply(ctx, config) {
  ctx.provide('clock', { now() { return config.now } })
}
"#;

const CONSUMER: &str = r#"
export const inject = ['clock']
export function apply(ctx) {
  ctx.provide('greeter', { greet(name) { return `${name} at ${ctx.clock.now()}` } })
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn dependencies_resolve_within_the_group() {
    let (provider, consumer) = (plugin(PROVIDER), plugin(CONSUMER));
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../node/rutis-runtime");
    // The consumer is loaded first: Cordis waits for its dependency natively.
    let process = Process::launch_group(
        &package,
        &[
            (consumer.path(), json!({})),
            (provider.path(), json!({ "now": 42 })),
        ],
        json!({ "greeter": ["greet"], "clock": ["now"] }),
        None,
    )
    .await
    .unwrap();
    let greeter = process.service("greeter").unwrap();
    assert_eq!(
        process.call(&greeter, "greet", json!(["ada"])).unwrap(),
        json!("ada at 42")
    );
    process.dispose().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_dependency_missing_from_the_group_is_named() {
    let consumer = plugin(CONSUMER);
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../node/rutis-runtime");
    let error = match Process::launch_group(
        &package,
        &[(consumer.path(), json!({}))],
        json!({ "greeter": ["greet"] }),
        None,
    )
    .await
    {
        Ok(_) => panic!("the mount must fail"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("unresolved") && error.contains("(clock)"),
        "{error}"
    );
}
