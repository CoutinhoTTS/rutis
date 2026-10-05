//! Rows of a remote runtime: a Python runtime on another "machine" (a
//! process listening on a WebSocket, started by no one here), reached by a
//! link. The loader manages its rows as it does a local runtime's: the
//! rows inject a Rust service and provide one back, and they stop when the
//! runtime goes away. Needs a Python with `websockets`
//! (RUTIS_INTEROP_PYTHON, else python3).
#![cfg(all(unix, feature = "python"))]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::{
    Credential, IdentityPlugin, LinkConfig, LinkPlugin, RuntimeAccessPlugin, StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Reply, Value as RpcValue};
use rutis_interop::{host_key, HostDispatch, RuntimePlugin};
use rutis_loader::{
    Chain, InteropResolver, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeRowsPlugin,
};
use rutis_transport_websocket::{Config, WebSocketPlugin};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

const WEATHER: &str = r#"
inject = ["clock"]


class Weather:
    def __init__(self, clock):
        self.clock = clock

    def today(self):
        return f"remote at {self.clock.now()}"


provides = {"weather": Weather}


def apply(ctx, config):
    ctx.provide("weather", Weather(ctx.use("clock")))
"#;

#[derive(Clone, Default)]
struct Clock(Arc<Mutex<u64>>);

impl HostDispatch for Clock {
    fn invoke(&self, _method: &str, _args: RpcValue) -> Reply {
        let mut now = self.0.lock().unwrap();
        *now += 1;
        Ok(json!(*now).into())
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "now": "sync" }))
    }
}

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

async fn eventually<T>(mut check: impl FnMut() -> Option<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(found) = check() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Start the remote runtime: `gpu`, accepting `main` with a token.
async fn remote_python(project: &Path) -> (tokio::process::Child, String) {
    let python = std::env::var("RUTIS_INTEROP_PYTHON").unwrap_or_else(|_| "python3".into());
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/python");
    let mut child = tokio::process::Command::new(python)
        .args(["-m", "rutis_runtime", "listen:ws://127.0.0.1:0/rutis"])
        .args(["--id", "gpu", "--peer", "main"])
        .arg(project)
        .env("PYTHONPATH", sdk)
        .env("RUTIS_INTEROP_TOKEN", "main-token")
        .current_dir(project)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
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
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (child, address)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_python_runtime_runs_loader_rows() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("weather_plugin.py"), WEATHER).unwrap();
    let (mut runtime_process, address) = remote_python(project.path()).await;

    let root = Ctx::root().unwrap();
    let clock = Clock::default();
    root.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(clock.clone()))
        .unwrap();
    (&root.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
        .await
        .unwrap();
    root.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).present(id("gpu"), Credential::Bearer("main-token".into())),
    ));
    let link = LinkPlugin::new(
        LinkConfig::dial(id("gpu"), "websocket", "main", &address).require("runtime"),
    );
    let link_state = link.state();
    root.plugin(link);
    root.plugin(RuntimeAccessPlugin::new(id("gpu"), "py"));
    let python = RuntimePlugin::remote("py");
    let rows = Arc::new(InteropResolver::modules(python.handle()));
    let handle = python.handle();
    root.plugin(python);
    let plugin_loader = LoaderPlugin::new(
        Chain::new().with_shared(rows.clone()),
        LoaderOptions::default(),
    );
    let loader = plugin_loader.handle();
    root.plugin(plugin_loader).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(rows));

    let patches: Vec<Patch> = serde_json::from_value(json!([{
        "insert": [{ "id": "w", "name": "py:weather_plugin", "config": {} }]
    }]))
    .unwrap();
    loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();

    // The row runs over there, and its service is here.
    let weather = eventually(
        || root.get_as::<dyn HostDispatch>(host_key("weather")),
        "the remote row's service",
    )
    .await;
    assert!(handle.is_remote() && handle.supports("leaf"));
    let today = weather.invoke("today", json!([]).into()).unwrap();
    assert_eq!(today.json().unwrap(), json!("remote at 1"));
    drop(weather);

    // The runtime goes away: its rows stop and their services go.
    runtime_process.start_kill().unwrap();
    eventually(
        || {
            root.get_as::<dyn HostDispatch>(host_key("weather"))
                .is_none()
                .then_some(())
        },
        "the withdrawal",
    )
    .await;
    eventually(
        || {
            matches!(
                *link_state.borrow(),
                rutis_bridge::LinkState::Waiting { .. }
            )
            .then_some(())
        },
        "the link retrying",
    )
    .await;
    root.shutdown().await.unwrap();
}

/// The same with a remote Node runtime: the row names an npm package that
/// exists only where the runtime runs.
#[cfg(feature = "node")]
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_node_runtime_resolves_and_runs_npm_rows() {
    let project = tempfile::tempdir().unwrap();
    let anchor = project.path().join("package.json");
    std::fs::write(&anchor, "{}").unwrap();
    let package = project.path().join("node_modules/remote-weather");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        json!({
            "name": "remote-weather", "version": "1.0.0", "type": "module", "main": "index.mjs",
            "rutis": { "provides": { "weather": { "today": "sync" } } }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        package.join("index.mjs"),
        "export const inject = ['clock']\nexport function apply(ctx) {\n  ctx.provide('weather', { today() { return `node at ${ctx.clock.now()}` } })\n}\n",
    )
    .unwrap();
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let mut child = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(runtime.join("src/runner.mjs"))
        .arg("listen:ws://127.0.0.1:0/rutis")
        .args(["--id", "edge", "--peer", "main"])
        .arg(&anchor)
        .env("RUTIS_INTEROP_TOKEN", "main-token")
        .current_dir(&runtime)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
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
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    let root = Ctx::root().unwrap();
    root.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock::default()))
        .unwrap();
    (&root.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
        .await
        .unwrap();
    root.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main"))
            .present(id("edge"), Credential::Bearer("main-token".into())),
    ));
    root.plugin(LinkPlugin::new(
        LinkConfig::dial(id("edge"), "websocket", "main", &address).require("runtime"),
    ));
    root.plugin(RuntimeAccessPlugin::new(id("edge"), "node"));
    let node = RuntimePlugin::remote("node");
    let mut catalog = rutis_loader::ServiceCatalog::new();
    catalog.register_shared("clock");
    let rows = Arc::new(InteropResolver::node(node.handle()).with_catalog(&catalog));
    root.plugin(node);
    let options = LoaderOptions {
        catalog,
        ..LoaderOptions::default()
    };
    let plugin_loader = LoaderPlugin::new(Chain::new().with_shared(rows.clone()), options);
    let loader = plugin_loader.handle();
    root.plugin(plugin_loader).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(rows));

    let patches: Vec<Patch> = serde_json::from_value(json!([{
        "insert": [
            { "id": "w", "name": "remote-weather", "config": {} },
            { "id": "x", "name": "not-installed-there", "config": {} }
        ]
    }]))
    .unwrap();
    loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();
    let weather = eventually(
        || root.get_as::<dyn HostDispatch>(host_key("weather")),
        "the remote npm row's service",
    )
    .await;
    assert_eq!(
        weather
            .invoke("today", json!([]).into())
            .unwrap()
            .json()
            .unwrap(),
        json!("node at 1")
    );
    // A package the runtime does not have is unresolved, not a failure.
    let missing = eventually(
        || {
            let entry = loader.get("x")?;
            match entry.status {
                rutis_loader::EntryStatus::Unresolved(error) => Some(error),
                _ => None,
            }
        },
        "the missing package unresolved",
    )
    .await;
    assert!(
        matches!(missing, rutis_loader::LoaderError::NotFound { .. }),
        "{missing:?}"
    );
    drop(weather);
    child.start_kill().unwrap();
    root.shutdown().await.unwrap();
}

/// The rows of a remote runtime share its one control session: both
/// services come from the same session, and unloading one row leaves the
/// session and the other row's service.
#[tokio::test(flavor = "multi_thread")]
async fn remote_rows_share_the_runtime_session() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("weather_plugin.py"), WEATHER).unwrap();
    std::fs::write(
        project.path().join("second_plugin.py"),
        "class Second:\n    def ping(self):\n        return 'pong'\n\n\nprovides = {\"second\": Second}\n\n\ndef apply(ctx, config):\n    ctx.provide(\"second\", Second())\n",
    )
    .unwrap();
    let (_runtime_process, address) = remote_python(project.path()).await;

    let root = Ctx::root().unwrap();
    root.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock::default()))
        .unwrap();
    (&root.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
        .await
        .unwrap();
    root.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).present(id("gpu"), Credential::Bearer("main-token".into())),
    ));
    root.plugin(LinkPlugin::new(
        LinkConfig::dial(id("gpu"), "websocket", "main", &address).require("runtime"),
    ));
    root.plugin(RuntimeAccessPlugin::new(id("gpu"), "py"));
    let python = RuntimePlugin::remote("py");
    let rows = Arc::new(InteropResolver::modules(python.handle()));
    root.plugin(python);
    let plugin_loader = LoaderPlugin::new(
        Chain::new().with_shared(rows.clone()),
        LoaderOptions::default(),
    );
    let loader = plugin_loader.handle();
    root.plugin(plugin_loader).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(rows));
    let layer = |rows: Value| -> Vec<Layer> {
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        vec![Layer::new("rows", patches)]
    };
    loader
        .reconcile(
            layer(json!([
                { "id": "w", "name": "py:weather_plugin", "config": {} },
                { "id": "s", "name": "py:second_plugin", "config": {} }
            ])),
            None,
        )
        .await
        .unwrap();
    let weather = eventually(
        || root.get_as::<dyn HostDispatch>(host_key("weather")),
        "weather",
    )
    .await;
    let second = eventually(
        || root.get_as::<dyn HostDispatch>(host_key("second")),
        "second",
    )
    .await;
    assert_eq!(
        weather.origin(),
        second.origin(),
        "both rows run on one session"
    );
    let session = weather.origin().unwrap().to_owned();
    drop(weather);

    loader
        .reconcile(
            layer(json!([{ "id": "s", "name": "py:second_plugin", "config": {} }])),
            None,
        )
        .await
        .unwrap();
    eventually(
        || {
            root.get_as::<dyn HostDispatch>(host_key("weather"))
                .is_none()
                .then_some(())
        },
        "weather unloaded",
    )
    .await;
    assert_eq!(second.origin(), Some(session.as_str()), "the session stays");
    let pong = tokio::task::spawn_blocking(move || second.invoke("ping", json!([]).into()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pong.json().unwrap(), json!("pong"));
    root.shutdown().await.unwrap();
}
