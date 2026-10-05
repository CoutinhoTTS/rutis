#![cfg(unix)]
//! One set of session semantics over every local channel: the Node and the
//! Python runtime, each connected on an inherited socket (`fd:3`) and on a
//! socket path it dials back. Describe, load with exports, lease a host,
//! sync and async calls, a callback into Rust during a synchronous call,
//! unload with withdrawal, and the exit diagnostics of a crashed process.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_interop::rpc::{settle, Reply, Value as RpcValue};
use rutis_interop::{host_key, row_projection, Error, HostDispatch, Launcher, Mount, Process};
use serde_json::{json, Value};

const NODE_PLUGIN: &str = r#"
export const inject = ['clock']
export function apply(ctx) {
  ctx.provide('weather', {
    today() { return `Oslo at ${ctx.clock.now()}` },
    async later() { return 'Oslo later' },
    each(callback) { return ['mon', 'tue'].map(day => callback(day)) },
    crash() { process.exit(17) },
  })
}
"#;

const PYTHON_PLUGIN: &str = r#"
import os

inject = ["clock"]


class Weather:
    def __init__(self, clock):
        self.clock = clock

    def today(self):
        return f"Oslo at {self.clock.now()}"

    async def later(self):
        return "Oslo later"

    def each(self, callback):
        return [callback(day) for day in ("mon", "tue")]

    def crash(self):
        os._exit(17)


provides = {"weather": Weather}


def apply(ctx, config):
    ctx.provide("weather", Weather(ctx.use("clock")))
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

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[derive(Clone, Copy, Debug)]
enum Runtime {
    Node,
    Python,
}

/// Start `runtime` on a fresh project, connected as `inherit` says.
async fn start(runtime: Runtime, inherit: bool, dir: &Path) -> (Arc<Process>, PathBuf) {
    let (launcher, anchor, entry) = match runtime {
        Runtime::Node => {
            let package = repo().join("interop/node");
            let anchor = dir.join("package.json");
            std::fs::write(&anchor, "{}").unwrap();
            let entry = dir.join("weather.mjs");
            std::fs::write(&entry, NODE_PLUGIN).unwrap();
            let launcher = Launcher::new("node")
                .arg("--import")
                .arg("tsx")
                .arg(package.join("src/runner.mjs"))
                .cwd(&package);
            (launcher, anchor, entry)
        }
        Runtime::Python => {
            std::fs::write(dir.join("weather_plugin.py"), PYTHON_PLUGIN).unwrap();
            let mut path = repo().join("interop/python").into_os_string();
            path.push(":");
            path.push(dir);
            let launcher = Launcher::new("python3")
                .arg("-m")
                .arg("rutis_runtime")
                .env("PYTHONPATH", path);
            (launcher, dir.to_owned(), PathBuf::from("weather_plugin"))
        }
    };
    let launcher = match inherit {
        true => launcher.inherit_fd(),
        false => launcher,
    };
    let process = Process::mount(
        &repo().join("interop/node"),
        Mount {
            anchor: Some(&anchor),
            launcher: Some(&launcher),
            ..Mount::default()
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{runtime:?} (inherit {inherit}) failed to start: {error}"));
    (process, entry)
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

async fn semantics(runtime: Runtime, inherit: bool) {
    let case = format!("{runtime:?}, inherit {inherit}");
    let dir = tempfile::tempdir().unwrap();
    let (process, entry) = start(runtime, inherit, dir.path()).await;

    let described = process.describe_row(&entry).await.unwrap();
    assert_eq!(described.inject, ["clock"], "{case}");
    let provides = serde_json::Map::from_iter([(
        "weather".to_owned(),
        json!({ "today": "sync", "later": "async", "each": "sync", "crash": "sync" }),
    )]);
    let lease = process
        .lease_host("clock", Arc::new(Clock(Arc::default())), None)
        .await
        .unwrap();
    let ctx = Ctx::root().unwrap();
    let projection = row_projection(&provides);
    projection.attach(&ctx, process.clone()).unwrap();
    process
        .load_row_exporting(
            "w",
            &entry,
            json!({}),
            &[],
            &[],
            &provides,
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

    let today = weather.invoke("today", json!([]).into()).unwrap();
    assert_eq!(today.json().unwrap(), json!("Oslo at 0"), "{case}");
    let later = settle(weather.invoke("later", json!([]).into()).unwrap())
        .await
        .unwrap();
    assert_eq!(later.json().unwrap(), json!("Oslo later"), "{case}");
    let callback = RpcValue::callback(|args| {
        let [day]: [String; 1] = rutis_interop::decode_value(args)?;
        Ok(json!(day.to_uppercase()).into())
    });
    let days = weather
        .invoke("each", RpcValue::List(vec![callback]))
        .unwrap();
    assert_eq!(days.json().unwrap(), json!(["MON", "TUE"]), "{case}");
    drop(weather);

    process.unload_row("w").await.unwrap();
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_none(),
        "the withdrawal",
    )
    .await;

    // A crash ends the session with how the process ended.
    process
        .load_row_exporting(
            "w",
            &entry,
            json!({}),
            &[],
            &[],
            &provides,
            projection.clone(),
        )
        .await
        .unwrap();
    eventually(
        || ctx.get_as::<dyn HostDispatch>(key.clone()).is_some(),
        "the weather service again",
    )
    .await;
    let weather = ctx.get_as::<dyn HostDispatch>(key.clone()).unwrap();
    match weather.invoke("crash", json!([]).into()) {
        Err(Error::Transport(message)) => assert_eq!(
            message, "Cordis process exited with exit status: 17",
            "{case}"
        ),
        other => panic!("{case}: the crash should end the session, got {other:?}"),
    }
    process.closed().await;
    assert_eq!(
        process.exit_status().as_deref(),
        Some("exited with exit status: 17"),
        "{case}"
    );
    drop(weather);
    projection.close();
    drop(lease);
    ctx.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn node_on_an_inherited_socket() {
    semantics(Runtime::Node, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_dialing_a_socket_path() {
    semantics(Runtime::Node, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_on_an_inherited_socket() {
    semantics(Runtime::Python, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_dialing_a_socket_path() {
    semantics(Runtime::Python, false).await;
}

/// A runtime package that does not list `fd` in `rutisChannels` is started
/// the old way, dialing a socket path.
#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_without_fd_support_dials_back() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("runtime");
    std::fs::create_dir_all(package.join("src")).unwrap();
    let real = repo().join("interop/node");
    for file in std::fs::read_dir(real.join("src")).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_dir() {
            continue;
        }
        std::fs::copy(file.path(), package.join("src").join(file.file_name())).unwrap();
    }
    copy_dir(&real.join("src/channel"), &package.join("src/channel"));
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(real.join("package.json")).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("rutisChannels");
    std::fs::write(package.join("package.json"), manifest.to_string()).unwrap();
    std::os::unix::fs::symlink(real.join("node_modules"), package.join("node_modules")).unwrap();
    // The runner sees its channel argument: record it.
    let anchor = dir.path().join("package.json");
    std::fs::write(&anchor, "{}").unwrap();
    let probe = dir.path().join("argv.mjs");
    std::fs::write(
        &probe,
        "export function apply(ctx) { ctx.provide('argv', { channel() { return process.argv[2] } }) }",
    )
    .unwrap();
    let process = Process::launch(&package, &probe, json!({}), json!({ "argv": ["channel"] }))
        .await
        .unwrap();
    let channel = process.call("argv", "channel", json!([])).unwrap();
    let channel = channel.as_str().unwrap();
    assert!(!channel.starts_with("fd:"), "{channel}");
    assert!(channel.ends_with("peer.sock"), "{channel}");
    process.dispose().await.unwrap();

    // The real package takes fd 3.
    let process = Process::launch(&real, &probe, json!({}), json!({ "argv": ["channel"] }))
        .await
        .unwrap();
    assert_eq!(
        process.call("argv", "channel", json!([])).unwrap(),
        json!("fd:3")
    );
    process.dispose().await.unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for file in std::fs::read_dir(from).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), to.join(file.file_name())).unwrap();
    }
}
