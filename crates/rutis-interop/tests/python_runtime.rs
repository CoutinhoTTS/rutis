#![cfg(unix)]
//! The Python runtime speaks the same protocol and row contract as the Node
//! one: describe, load with exports, lease hosts, sync and async calls,
//! callbacks into Rust from a synchronous call, unload.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_interop::rpc::{settle, Reply, Value as RpcValue};
use rutis_interop::{host_key, row_projection, HostDispatch, Launcher, Mount, Process};
use serde_json::{json, Value};

const WEATHER: &str = r#"
import asyncio

inject = ["clock"]
Config = {"type": "object", "properties": {"city": {"type": "string", "default": "Paris"}}}


class Weather:
    def __init__(self, clock, city):
        self.clock = clock
        self.city = city

    def today(self):
        return f"{self.city} at {self.clock.now()}"

    async def later(self):
        await asyncio.sleep(0.01)
        return f"{self.city} later"

    def each(self, callback):
        # A Rust callback, called back during this synchronous call.
        return [callback(day) for day in ("mon", "tue")]


provides = {"weather": Weather}


def apply(ctx, config):
    ctx.provide("weather", Weather(ctx.use("clock"), config.get("city", "Paris")))
    return lambda: print("weather: bye", flush=True)
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

fn sdk() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/python")
}

async fn python(project: &Path) -> Arc<Process> {
    let mut path = sdk().into_os_string();
    path.push(":");
    path.push(project);
    let launcher = Launcher::new("python3")
        .arg("-m")
        .arg("rutis_runtime")
        // No working directory of its own: it runs where the test does.
        .env("PYTHONPATH", path);
    Process::mount(
        &sdk(),
        Mount {
            anchor: Some(project),
            launcher: Some(&launcher),
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
async fn python_rows_follow_the_row_contract() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("weather_plugin.py"), WEATHER).unwrap();
    let process = python(dir.path()).await;
    assert!(process.supports("rows.v2") && process.supports("hosts") && process.supports("leaf"));

    let entry = Path::new("weather_plugin");
    let described = process.describe_row(entry).await.unwrap();
    assert_eq!(described.inject, ["clock"]);
    assert_eq!(
        Value::Object(described.provides.clone()),
        json!({ "weather": { "today": "sync", "later": "async", "each": "sync" } })
    );
    assert_eq!(
        described.config.unwrap()["properties"]["city"]["default"],
        json!("Paris")
    );

    let ticks = Arc::new(AtomicUsize::new(0));
    let lease = process
        .lease_host("clock", Arc::new(Clock(ticks.clone())), None)
        .await
        .unwrap();
    let ctx = Ctx::root().unwrap();
    let projection = row_projection(&described.provides);
    projection.attach(&ctx, process.clone()).unwrap();
    process
        .load_row_exporting(
            "w",
            entry,
            json!({ "city": "Oslo" }),
            &[],
            &[],
            &described.provides,
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
    let callback = RpcValue::callback(|args| {
        let [day]: [String; 1] = rutis_interop::decode_value(args)?;
        Ok(json!(day.to_uppercase()).into())
    });
    let days = weather
        .invoke("each", RpcValue::List(vec![callback]))
        .unwrap();
    assert_eq!(days.json().unwrap(), json!(["MON", "TUE"]));
    drop(weather);

    process.unload_row("w").await.unwrap();
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_none(),
        "the withdrawal",
    )
    .await;
    projection.close();
    lease.release().await.unwrap();
    process.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
