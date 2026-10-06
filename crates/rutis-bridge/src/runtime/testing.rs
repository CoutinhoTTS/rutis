//! The runtime conformance suite (feature `testing`): what any language
//! runtime must do with rows, as checks it runs against itself. Each check
//! panics with what was broken.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::session::{settle, Error, Reply, Value};

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
/// The repository has it for Node (`node/rutis-runtime/test/fixtures/
/// conformance-weather.mjs`) and Python (`python/rutis/tests/
/// conformance_weather.py`).
pub async fn runtime(
    process: Arc<crate::runtime::Process>,
    entry: &std::path::Path,
    started_here: bool,
) {
    use crate::runtime::row_projection;
    use crate::session::{host_key, HostDispatch};
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
        let [day]: [String; 1] = crate::session::decode_value(args)?;
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
