//! The runtime conformance suite: the same leaf plugins written in
//! JavaScript (`definePlugin`, in the Node runtime) and in Python (in the
//! Python runtime) behave the same under rutis-loader, and use each other's
//! services across the two processes.
#![cfg(all(unix, feature = "interop"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{Ctx, FiberState, FiberView};
use rutis_interop::rpc::{Reply, Value as RpcValue};
use rutis_interop::{host_key, CordisRuntimePlugin, HostDispatch};
use rutis_loader::{
    Chain, EntryStatus, InteropResolver, Layer, Loader, LoaderOptions, LoaderPlugin, Patch,
    RuntimeRowsPlugin, ServiceCatalog,
};
use serde_json::{json, Value};

// ── The plugins, once per language ──────────────────────────────

const PY_PROVIDER: &str = r#"
class Weather:
    native = True

    def today(self):
        return "py sunny"

    async def later(self):
        return "py later"


provides = {"py_weather": Weather}
inject = ["probe"]


def apply(ctx, config):
    probe = ctx.use("probe")
    ctx.provide("py_weather", Weather())
    probe.record("py provider: start")
    return lambda: probe.record("py provider: bye")
"#;

const PY_CONSUMER: &str = r#"
inject = ["js_weather", "probe"]


async def apply(ctx, config):
    probe = ctx.use("probe")
    weather = ctx.use("js_weather")
    probe.record(f"py consumer: {weather.today()} / {await weather.later()}")
    return lambda: probe.record("py consumer: bye")
"#;

const PY_LOCAL: &str = r#"
inject = ["py_weather", "probe"]


def apply(ctx, config):
    probe = ctx.use("probe")
    native = getattr(ctx.use("py_weather"), "native", False)
    probe.record(f"py local: {'native' if native else 'proxy'}")
    ctx.effect(lambda: probe.record("py local: bye"))
"#;

const PY_GATED: &str = r#"
inject = ["llm", "probe"]


def apply(ctx, config):
    probe = ctx.use("probe")
    probe.record(f"py gated: {ctx.use('llm').ask()}")
    return lambda: probe.record("py gated: bye")
"#;

const JS_PROVIDER: &str = r#"
import { definePlugin } from 'PLUGIN'
class Weather {
  native = true
  today() { return 'js sunny' }
  async later() { return 'js later' }
}
export default definePlugin({
  inject: ['probe'],
  provides: { js_weather: { today: 'sync', later: 'async' } },
  apply(ctx) {
    const probe = ctx.use('probe')
    ctx.provide('js_weather', new Weather())
    probe.record('js provider: start')
    return () => probe.record('js provider: bye')
  },
})
"#;

const JS_CONSUMER: &str = r#"
import { definePlugin } from 'PLUGIN'
export default definePlugin({
  inject: ['py_weather', 'probe'],
  async apply(ctx) {
    const probe = ctx.use('probe')
    const weather = ctx.use('py_weather')
    probe.record(`js consumer: ${weather.today()} / ${await weather.later()}`)
    return () => probe.record('js consumer: bye')
  },
})
"#;

const JS_LOCAL: &str = r#"
import { definePlugin } from 'PLUGIN'
export default definePlugin({
  inject: ['js_weather', 'probe'],
  apply(ctx) {
    const probe = ctx.use('probe')
    probe.record(`js local: ${ctx.use('js_weather').native === true ? 'native' : 'proxy'}`)
    ctx.effect(() => probe.record('js local: bye'))
  },
})
"#;

const JS_GATED: &str = r#"
import { definePlugin } from 'PLUGIN'
export default definePlugin({
  inject: ['llm', 'probe'],
  apply(ctx) {
    const probe = ctx.use('probe')
    probe.record(`js gated: ${ctx.use('llm').ask()}`)
    return () => probe.record('js gated: bye')
  },
})
"#;

// ── Harness ─────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);

impl HostDispatch for Probe {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply {
        assert_eq!(method, "record");
        let [line]: [String; 1] = rutis_interop::decode_value(args)?;
        self.0.lock().unwrap().push(line);
        Ok(RpcValue::Undefined)
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "record": "sync" }))
    }
}

impl Probe {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn position(&self, line: &str) -> Option<usize> {
        self.0.lock().unwrap().iter().position(|l| l == line)
    }

    async fn wait_for(&self, line: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.position(line).is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{line:?} not recorded: {:?}", self.lines()));
    }
}

struct Llm;

impl HostDispatch for Llm {
    fn invoke(&self, method: &str, _args: RpcValue) -> Reply {
        assert_eq!(method, "ask");
        Ok(json!("42").into())
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "ask": "sync" }))
    }
}

fn interop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop")
}

/// Plugin files: Python modules in `py/`, JavaScript in `js/`.
fn write_plugins(dir: &Path) -> (PathBuf, PathBuf) {
    let py = dir.join("py");
    let js = dir.join("js");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::create_dir_all(&js).unwrap();
    for (name, text) in [
        ("py_provider", PY_PROVIDER),
        ("py_consumer", PY_CONSUMER),
        ("py_local", PY_LOCAL),
        ("py_gated", PY_GATED),
    ] {
        std::fs::write(py.join(format!("{name}.py")), text).unwrap();
    }
    let plugin = interop()
        .join("node/src/plugin.mjs")
        .canonicalize()
        .unwrap();
    let plugin = format!("file://{}", plugin.display());
    for (name, text) in [
        ("js_provider", JS_PROVIDER),
        ("js_consumer", JS_CONSUMER),
        ("js_local", JS_LOCAL),
        ("js_gated", JS_GATED),
    ] {
        std::fs::write(
            js.join(format!("{name}.mjs")),
            text.replace("PLUGIN", &plugin),
        )
        .unwrap();
    }
    (py, js)
}

struct Fixture {
    root: Ctx,
    loader: Loader,
    node: FiberView,
    python: FiberView,
    probe: Probe,
    py: PathBuf,
    js: PathBuf,
    _dir: tempfile::TempDir,
}

/// Both runtimes, one loader, and the second stage of each runtime.
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (py, js) = write_plugins(dir.path());
    let probe = Probe::default();
    let root = Ctx::root().unwrap();
    root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .unwrap();
    let mut catalog = ServiceCatalog::new();
    for name in ["probe", "llm", "py_weather", "js_weather"] {
        catalog.register_shared(name);
    }
    let node =
        CordisRuntimePlugin::new(interop().join("node"), interop().join("node/package.json"));
    let python = CordisRuntimePlugin::python(interop().join("python"), &py);
    let node_rows = Arc::new(InteropResolver::new(node.handle()).with_catalog(&catalog));
    let python_rows = Arc::new(InteropResolver::modules(python.handle()).with_catalog(&catalog));
    let node = root.plugin(node);
    let python = root.plugin(python);
    let options = LoaderOptions {
        catalog,
        ..LoaderOptions::default()
    };
    let chain = Chain::new()
        .with_shared(python_rows.clone())
        .with_shared(node_rows.clone());
    let plugin = LoaderPlugin::new(chain, options);
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(node_rows));
    root.plugin(RuntimeRowsPlugin::new(python_rows));
    Fixture {
        root,
        loader,
        node,
        python,
        probe,
        py,
        js,
        _dir: dir,
    }
}

fn rows(rows: Vec<Value>) -> Vec<Layer> {
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
    vec![Layer::new("rows", patches)]
}

fn py_row(id: &str, module: &str) -> Value {
    json!({ "id": id, "name": format!("py:{module}"), "config": {} })
}

fn js_row(fixture: &Fixture, id: &str, file: &str) -> Value {
    let path = fixture.js.join(format!("{file}.mjs"));
    json!({ "id": id, "name": path.to_string_lossy(), "config": {} })
}

fn row_state(loader: &Loader, id: &str) -> Option<FiberState> {
    match loader.get(id)?.status {
        EntryStatus::Running(snapshot) => Some(snapshot.state),
        _ => None,
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn all_rows(fixture: &Fixture) -> Vec<Value> {
    vec![
        py_row("pp", "py_provider"),
        py_row("pc", "py_consumer"),
        py_row("pl", "py_local"),
        js_row(fixture, "jp", "js_provider"),
        js_row(fixture, "jc", "js_consumer"),
        js_row(fixture, "jl", "js_local"),
    ]
}

// ── The suite ───────────────────────────────────────────────────

/// Both runtimes start cold with rows that use each other's services: no
/// runtime waits for the other, and every row starts.
#[tokio::test(flavor = "multi_thread")]
async fn both_runtimes_start_cold_and_use_each_other() {
    let fixture = fixture().await;
    let report = fixture
        .loader
        .reconcile(rows(all_rows(&fixture)), None)
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    let probe = &fixture.probe;
    probe.wait_for("py consumer: js sunny / js later").await;
    probe.wait_for("js consumer: py sunny / py later").await;
    // Within one process, a service is the provider's own object.
    probe.wait_for("py local: native").await;
    probe.wait_for("js local: native").await;
    assert_eq!(fixture.node.state().state, FiberState::Active);
    assert_eq!(fixture.python.state().state, FiberState::Active);
    fixture.root.shutdown().await.unwrap();
}

/// A provider that goes stops exactly its users, in either language, before
/// it stops itself; everything else keeps running.
#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_goes_stops_only_its_users() {
    for (provider, users, others) in [
        ("pp", ["jc", "pl"], ["pc", "jl"]),
        ("jp", ["pc", "jl"], ["jc", "pl"]),
    ] {
        let fixture = fixture().await;
        let loader = &fixture.loader;
        let all = all_rows(&fixture);
        loader.reconcile(rows(all.clone()), None).await.unwrap();
        until("every row to run", || {
            ["pp", "pc", "pl", "jp", "jc", "jl"]
                .iter()
                .all(|id| row_state(loader, id) == Some(FiberState::Active))
        })
        .await;

        let remaining: Vec<Value> = all.into_iter().filter(|r| r["id"] != provider).collect();
        loader.reconcile(rows(remaining), None).await.unwrap();
        let (language, prefix) = if provider == "pp" {
            ("py", "py")
        } else {
            ("js", "js")
        };
        let provider_bye = format!("{prefix} provider: bye");
        fixture.probe.wait_for(&provider_bye).await;
        until("the users to wait", || {
            users
                .iter()
                .all(|id| row_state(loader, id) == Some(FiberState::Pending))
        })
        .await;
        for id in others {
            assert_eq!(
                row_state(loader, id),
                Some(FiberState::Active),
                "{id} keeps running"
            );
        }
        // Users stop before the provider does.
        let user_byes: Vec<String> = match language {
            "py" => vec!["js consumer: bye".into(), "py local: bye".into()],
            _ => vec!["py consumer: bye".into(), "js local: bye".into()],
        };
        let provider_at = fixture.probe.position(&provider_bye).unwrap();
        for bye in &user_byes {
            let at = fixture
                .probe
                .position(bye)
                .unwrap_or_else(|| panic!("{bye} missing"));
            assert!(
                at < provider_at,
                "{bye} before {provider_bye}: {:?}",
                fixture.probe.lines()
            );
        }
        fixture.root.shutdown().await.unwrap();
    }
}

/// What a plugin injects gates it in rutis: it waits for the service, and
/// stops when it goes. The same for both languages.
#[tokio::test(flavor = "multi_thread")]
async fn injected_services_gate_the_plugin() {
    let fixture = fixture().await;
    let loader = &fixture.loader;
    loader
        .reconcile(
            rows(vec![
                py_row("pg", "py_gated"),
                js_row(&fixture, "jg", "js_gated"),
            ]),
            None,
        )
        .await
        .unwrap();
    until("both rows to know their injects", || {
        ["pg", "jg"].iter().all(|id| {
            loader
                .get(id)
                .is_some_and(|entry| entry.meta["inject"].is_array())
        })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(row_state(loader, "pg"), Some(FiberState::Pending));
    assert_eq!(row_state(loader, "jg"), Some(FiberState::Pending));

    let llm = fixture
        .root
        .provide_as::<dyn HostDispatch>(host_key("llm"), Arc::new(Llm))
        .unwrap();
    fixture.probe.wait_for("py gated: 42").await;
    fixture.probe.wait_for("js gated: 42").await;
    llm.dispose().await.unwrap();
    fixture.probe.wait_for("py gated: bye").await;
    fixture.probe.wait_for("js gated: bye").await;
    until("both rows to wait again", || {
        row_state(loader, "pg") == Some(FiberState::Pending)
            && row_state(loader, "jg") == Some(FiberState::Pending)
    })
    .await;
    fixture.root.shutdown().await.unwrap();
}

/// The Python process ends on its own: its rows wait, the Node runtime and
/// its rows that do not use Python services keep running.
#[tokio::test(flavor = "multi_thread")]
async fn a_dead_python_process_stops_only_its_rows() {
    let fixture = fixture().await;
    let loader = &fixture.loader;
    std::fs::write(
        fixture.py.join("py_exit.py"),
        "import os, threading\n\ndef apply(ctx, config):\n    threading.Timer(0.2, lambda: os._exit(17)).start()\n",
    )
    .unwrap();
    let mut all = all_rows(&fixture);
    loader.reconcile(rows(all.clone()), None).await.unwrap();
    fixture.probe.wait_for("js local: native").await;
    fixture.probe.wait_for("py local: native").await;
    all.push(py_row("px", "py_exit"));
    loader.reconcile(rows(all), None).await.unwrap();
    until("the Python rows to wait", || {
        ["pp", "pc", "pl", "jc"]
            .iter()
            .all(|id| row_state(loader, id) == Some(FiberState::Pending))
    })
    .await;
    assert_eq!(row_state(loader, "jp"), Some(FiberState::Active));
    assert_eq!(row_state(loader, "jl"), Some(FiberState::Active));
    assert_eq!(fixture.python.state().state, FiberState::Active);
    fixture.root.shutdown().await.unwrap();
}
