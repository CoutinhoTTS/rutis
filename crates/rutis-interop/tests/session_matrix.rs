#![cfg(unix)]
//! One set of session semantics over every channel: the Node and the
//! Python runtime, each connected on an inherited socket (`fd:3`), on a
//! socket path it dials back, and over a loopback WebSocket it listens on
//! (dialed by Rust and attached). Describe, load with exports, lease a host,
//! sync and async calls, a callback into Rust during a synchronous call,
//! unload with withdrawal, and how a crash ends the session.

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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    Inherit,
    DialBack,
    WebSocket,
}

/// The Python interpreter: one with `websockets` for WebSocket channels
/// (`RUTIS_INTEROP_PYTHON`), else `python3`.
fn python() -> String {
    std::env::var("RUTIS_INTEROP_PYTHON").unwrap_or_else(|_| "python3".into())
}

/// Start `runtime` on a fresh project, connected as `via` says.
async fn start(runtime: Runtime, via: Via, dir: &Path) -> (Arc<Process>, PathBuf) {
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
            let launcher = Launcher::new(python())
                .arg("-m")
                .arg("rutis_runtime")
                .env("PYTHONPATH", path);
            (launcher, dir.to_owned(), PathBuf::from("weather_plugin"))
        }
    };
    if via == Via::WebSocket {
        return (listening(launcher, &anchor).await, entry);
    }
    let launcher = match via {
        Via::Inherit => launcher.inherit_fd(),
        _ => launcher,
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
    .unwrap_or_else(|error| panic!("{runtime:?} ({via:?}) failed to start: {error}"));
    (process, entry)
}

/// Start the runtime listening on a loopback WebSocket, dial it, attach.
async fn listening(launcher: Launcher, anchor: &Path) -> Arc<Process> {
    use rutis_bridge::{Credential, Dial, Identity, StaticIdentity, Transport};
    use rutis_channel::PeerId;
    use tokio::io::AsyncBufReadExt;

    let mut command = tokio::process::Command::new(&launcher.program);
    command
        .args(&launcher.args)
        .envs(launcher.env.iter().map(|(name, value)| (name, value)))
        .arg("listen:ws://127.0.0.1:0/rutis")
        .args(["--id", "runtime", "--peer", "main"])
        .arg(anchor)
        .env("RUTIS_INTEROP_TOKEN", "controller-token")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &launcher.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let address = loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("the runtime's address");
        if let Some(address) = line.strip_prefix("rutis-interop: listening on ") {
            break address.to_owned();
        }
    };
    // Keep reading its stderr, and keep it running for the test's length.
    tokio::spawn(async move {
        while let Ok(Some(_)) = lines.next_line().await {}
        let _ = child.wait().await;
    });
    let transport = rutis_transport_websocket::WebSocketTransport::start(
        rutis_transport_websocket::Config::new(),
    )
    .unwrap();
    let runtime = PeerId::new("runtime").unwrap();
    let identity: Arc<dyn Identity> =
        Arc::new(StaticIdentity::new(PeerId::new("main").unwrap()).present(
            runtime.clone(),
            Credential::Bearer("controller-token".into()),
        ));
    let channel = transport
        .dial(
            &Dial::address(address)
                .peer(runtime)
                .identity(identity)
                .protocol(format!("rutis.{}", rutis_interop::ENDPOINT_PROTOCOL)),
        )
        .await
        .unwrap();
    // The transport's threads must outlive the channel: leak it for the test.
    std::mem::forget(transport);
    let format = rutis_interop::rpc::Format::Endpoint(
        rutis_interop::rpc::Endpoint::rust(PeerId::new("main").unwrap())
            .expect(PeerId::new("runtime").unwrap()),
    );
    let process = Process::attach(channel, Mount::default(), format)
        .await
        .unwrap();
    assert!(
        process.connection().supports("runtime"),
        "a runner declares its contract"
    );
    process
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

async fn semantics(runtime: Runtime, via: Via) {
    let case = format!("{runtime:?} via {via:?}");
    let dir = tempfile::tempdir().unwrap();
    let (process, entry) = start(runtime, via, dir.path()).await;

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
    match (weather.invoke("crash", json!([]).into()), via) {
        // An attached runtime is no child of ours: the session just ends.
        (Err(Error::Transport(_)), Via::WebSocket) => {}
        (Err(Error::Transport(message)), _) => assert_eq!(
            message, "Cordis process exited with exit status: 17",
            "{case}"
        ),
        (other, _) => panic!("{case}: the crash should end the session, got {other:?}"),
    }
    process.closed().await;
    let expected = (via != Via::WebSocket).then_some("exited with exit status: 17");
    assert_eq!(process.exit_status().as_deref(), expected, "{case}");
    drop(weather);
    projection.close();
    drop(lease);
    ctx.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn node_on_an_inherited_socket() {
    semantics(Runtime::Node, Via::Inherit).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_dialing_a_socket_path() {
    semantics(Runtime::Node, Via::DialBack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_listening_on_a_websocket() {
    semantics(Runtime::Node, Via::WebSocket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_on_an_inherited_socket() {
    semantics(Runtime::Python, Via::Inherit).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_dialing_a_socket_path() {
    semantics(Runtime::Python, Via::DialBack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_listening_on_a_websocket() {
    semantics(Runtime::Python, Via::WebSocket).await;
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
