#![cfg(unix)]
//! Rows exporting services to rutis and using host services leased one by
//! one: what rutis-loader builds JavaScript rows on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_interop::rpc::{settle, Reply, Value as RpcValue};
use rutis_interop::{host_key, row_projection, HostDispatch, Mount, Process};
use serde_json::{json, Value};

const WEATHER: &str = r#"
export const name = 'weather'
export const inject = ['clock']
export const Config = {
  type: 'object', meta: {},
  dict: { city: { type: 'string', meta: { default: 'Paris' } } },
  '~standard': { validate: value => ({ value }) },
}
export function apply(ctx, config) {
  ctx.provide('weather', {
    today() { return `${config.city} at ${ctx.clock.now()}` },
    async later() { return `${config.city} later` },
  })
}
"#;

// Reports whether `clock` is registered in the Context, read from outside
// any plugin's inject.
const INSPECTOR: &str = r#"
export function apply(ctx) {
  ctx.provide('inspector', { has(name) { return ctx.get(name, false) !== undefined } })
}
"#;

struct Clock(Arc<AtomicUsize>);

impl HostDispatch for Clock {
    fn invoke(&self, method: &str, _args: RpcValue) -> Reply {
        assert_eq!(method, "now");
        Ok(json!(self.0.fetch_add(1, Ordering::SeqCst)).into())
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "now": "sync" }))
    }
}

fn package() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node")
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

async fn rows_process(dir: &Path) -> Arc<Process> {
    let anchor = write(dir, "package.json", "{}");
    Process::mount(
        &package(),
        Mount {
            anchor: Some(&anchor),
            ..Mount::default()
        },
    )
    .await
    .unwrap()
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

#[tokio::test(flavor = "multi_thread")]
async fn describe_reports_config_inject_and_provides() {
    let dir = tempfile::tempdir().unwrap();
    let process = rows_process(dir.path()).await;
    assert!(process.supports("rows.v2") && process.supports("hosts"));
    write(
        dir.path(),
        "weather/package.json",
        r#"{ "rutis": { "provides": { "weather": { "today": "sync", "later": "async" } } } }"#,
    );
    let entry = write(dir.path(), "weather/index.mjs", WEATHER);
    let described = process.describe_row(&entry).await.unwrap();
    assert_eq!(described.inject, ["clock"]);
    assert_eq!(
        Value::Object(described.provides),
        json!({ "weather": { "today": "sync", "later": "async" } })
    );
    assert_eq!(
        described.config.unwrap()["properties"]["city"]["default"],
        json!("Paris")
    );
    // A plugin outside any package with `rutis.provides` provides nothing.
    let inspector = write(dir.path(), "inspector.mjs", INSPECTOR);
    let described = process.describe_row(&inspector).await.unwrap();
    assert!(described.inject.is_empty() && described.provides.is_empty());
    process.dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn row_services_reach_rutis_and_hosts_are_leased() {
    let dir = tempfile::tempdir().unwrap();
    let process = rows_process(dir.path()).await;
    let entry = write(dir.path(), "weather.mjs", WEATHER);
    let inspector = write(dir.path(), "inspector.mjs", INSPECTOR);
    // Exported only to be called here: its projection is never attached.
    let shape = json!({ "inspector": { "has": "sync" } });
    let shape = shape.as_object().unwrap();
    process
        .load_row_exporting(
            "inspector",
            &inspector,
            json!({}),
            &[],
            &[],
            shape,
            row_projection(shape),
        )
        .await
        .unwrap();
    eventually(|| process.service("inspector").is_some(), "the inspector").await;
    let has_clock = || -> bool {
        let handle = process.service("inspector").unwrap();
        rutis_interop::decode(process.call(&handle, "has", json!(["clock"])).unwrap()).unwrap()
    };

    assert!(!has_clock());
    // Two leases register `clock` once; it stays until the last is released.
    let ticks = Arc::new(AtomicUsize::new(0));
    let clock: Arc<dyn HostDispatch> = Arc::new(Clock(ticks.clone()));
    let first = process
        .lease_host("clock", clock.clone(), None)
        .await
        .unwrap();
    let second = process
        .lease_host("clock", clock.clone(), None)
        .await
        .unwrap();
    assert!(has_clock());

    let ctx = Ctx::root().unwrap();
    let provides = json!({ "weather": { "today": "sync", "later": "async" } });
    let provides = provides.as_object().unwrap();
    let projection = row_projection(provides);
    projection.attach(&ctx, process.clone()).unwrap();
    process
        .load_row_exporting(
            "weather",
            &entry,
            json!({ "city": "Oslo" }),
            &[],
            &[],
            provides,
            projection.clone(),
        )
        .await
        .unwrap();
    let key = host_key("weather");
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_some(),
        "the weather service",
    )
    .await;
    let weather = ctx.get_as::<dyn HostDispatch>(key.clone()).unwrap();
    assert_eq!(
        weather
            .invoke("today", json!([]).into())
            .unwrap()
            .json()
            .unwrap(),
        json!("Oslo at 0")
    );
    let later = settle(weather.invoke("later", json!([]).into()).unwrap())
        .await
        .unwrap();
    assert_eq!(later.json().unwrap(), json!("Oslo later"));
    assert_eq!(weather.methods(), Some(provides["weather"].clone()));
    assert!(std::ptr::eq(weather.origin().unwrap(), &*process));
    assert!(weather.invoke("tomorrow", json!([]).into()).is_err());
    drop(weather);

    // A second row cannot export the same name.
    let error = process
        .load_row_exporting(
            "again",
            &entry,
            json!({}),
            &[],
            &[],
            provides,
            projection.clone(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already exported"), "{error}");

    // Unloading the row withdraws its service from rutis.
    process.unload_row("weather").await.unwrap();
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_none(),
        "the withdrawal",
    )
    .await;
    projection.close();

    first.release().await.unwrap();
    assert!(has_clock(), "one lease is left");
    second.release().await.unwrap();
    assert!(!has_clock(), "the last lease withdraws it");
    process.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
