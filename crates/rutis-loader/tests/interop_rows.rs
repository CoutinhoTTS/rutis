//! JavaScript plugins as loader rows (P6): one shared Cordis Context,
//! services resolved between rows natively, per-row load/update/unload,
//! isolate and inject forwarded, schemastery schema exported.
#![cfg(all(unix, feature = "interop"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{Ctx, FiberState, FiberView};
use rutis_interop::rpc::{Reply, Value as RpcValue};
use rutis_interop::{host_key, CordisRuntimePlugin, HostDispatch};
use rutis_loader::{
    Chain, EntryStatus, InteropResolver, Layer, Loader, LoaderError, LoaderOptions, LoaderPlugin,
    Patch,
};
use serde_json::{json, Value};

const PROVIDER: &str = r#"
export const name = 'provider'
// A schemastery-shaped schema, with the standard-schema hook Cordis calls.
export const Config = {
  type: 'object', meta: {},
  dict: {
    who: { type: 'string', meta: { default: 'world', description: 'who to greet' } },
    level: { type: 'number', meta: { default: 1, volatile: true } },
  },
  '~standard': { validate: value => ({ value }) },
}
export function apply(ctx, config) {
  ctx.provide('greeter', { hello: () => `hello ${config.who}` })
}
"#;

const CONSUMER: &str = r#"
export const name = 'consumer'
export const inject = ['greeter', 'probe']
export function apply(ctx, config) {
  ctx.probe.record(`${config.tag}: ${ctx.greeter.hello()}`)
  ctx.effect(() => () => ctx.probe.record(`${config.tag}: bye`))
}
"#;

const FLAG: &str = r#"
export const name = 'flag'
export function apply(ctx) { ctx.provide('late', { on: true }) }
"#;

#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);

impl HostDispatch for Probe {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply {
        assert_eq!(method, "record");
        let [line]: [String; 1] = rutis_interop::decode_value(args)?;
        self.0.lock().unwrap().push(line);
        Ok(RpcValue::Undefined)
    }
}

impl Probe {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    async fn wait_for(&self, line: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.0.lock().unwrap().iter().any(|l| l == line) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{line:?} not recorded: {:?}", self.0.lock().unwrap()));
    }
}

fn node_package() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node")
}

fn row(id: &str, file: &Path, config: Value, extra: Value) -> Value {
    let mut row = json!({ "id": id, "name": file.to_string_lossy(), "config": config });
    if let Value::Object(extra) = extra {
        row.as_object_mut().unwrap().extend(extra);
    }
    row
}

#[tokio::test(flavor = "multi_thread")]
async fn javascript_rows() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    let provider = write("provider.mjs", PROVIDER);
    let consumer = write("consumer.mjs", CONSUMER);
    let flag = write("flag.mjs", FLAG);

    let probe = Probe::default();
    let (root, loader, _runtime) = interop_loader(&probe).await;

    let layer = |rows: Vec<Value>| -> Vec<Layer> {
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        vec![Layer::new("rows", patches)]
    };
    let base = vec![
        row("p", &provider, json!({ "who": "rust" }), json!(null)),
        row("c", &consumer, json!({ "tag": "c" }), json!(null)),
        // A separate scope for `greeter`: its own provider and consumer.
        row(
            "p2",
            &provider,
            json!({ "who": "boxed" }),
            json!({ "isolate": { "greeter": "box" } }),
        ),
        row(
            "c2",
            &consumer,
            json!({ "tag": "c2" }),
            json!({ "isolate": { "greeter": "box" } }),
        ),
        // An empty private scope: the consumer finds no greeter.
        row(
            "c3",
            &consumer,
            json!({ "tag": "c3" }),
            json!({ "isolate": { "greeter": true } }),
        ),
        // Waits for `late`, which no row provides yet.
        row(
            "c4",
            &consumer,
            json!({ "tag": "c4" }),
            json!({ "inject": ["late"] }),
        ),
    ];
    let report = loader.reconcile(layer(base.clone()), None).await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    probe.wait_for("c: hello rust").await;
    probe.wait_for("c2: hello boxed").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let lines = probe.take();
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("c3") || l.starts_with("c4")),
        "{lines:?}"
    );

    // The schemastery schema arrives as JSON Schema.
    let schema = loader.get("p").unwrap().schema.unwrap();
    assert_eq!(schema["properties"]["who"]["default"], "world");
    assert_eq!(schema["properties"]["level"]["x-volatile"], true);

    // A config update reloads the provider; Cordis reloads its consumer.
    let mut updated = base.clone();
    updated[0] = row("p", &provider, json!({ "who": "there" }), json!(null));
    // `late` arrives: the gated consumer starts.
    updated.push(row("f", &flag, json!({}), json!(null)));
    loader
        .reconcile(layer(updated.clone()), None)
        .await
        .unwrap();
    probe.wait_for("c: hello there").await;
    probe.wait_for("c4: hello there").await;
    assert!(probe.take().contains(&"c: bye".to_owned()));

    // Removing a row disposes it on the Cordis side.
    updated.retain(|r| r["id"] != "c");
    loader.reconcile(layer(updated), None).await.unwrap();
    probe.wait_for("c: bye").await;

    // A name that is no package here does not resolve.
    loader
        .reconcile(
            layer(vec![json!({ "id": "x", "name": "no-such-package" })]),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        loader.get("x").unwrap().status,
        EntryStatus::Unresolved(LoaderError::NotFound { .. })
    ));

    root.shutdown().await.unwrap();
}

/// `level` is a volatile reference, as schemastery's `meta.volatile` makes it.
const TUNABLE: &str = r#"
import { createVolatile } from 'COSMOKIT'
export const name = 'tunable'
export const inject = ['probe']
export const Config = {
  type: 'object', meta: {},
  dict: {
    tag: { type: 'string', meta: {} },
    level: { type: 'number', meta: { default: 1, volatile: true } },
  },
  '~standard': { validate: value => ({ value: { ...value, level: createVolatile(value.level ?? 1) } }) },
}
export function apply(ctx, config) {
  ctx.probe.record(`${config.tag}: start ${config.level.get()}`)
  ctx.on('loader/volatile-update', paths => {
    ctx.probe.record(`${config.tag}: ${JSON.stringify(paths)} ${config.level.get()}`)
  })
  ctx.effect(() => () => ctx.probe.record(`${config.tag}: bye`))
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn volatile_changes_reach_cordis_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let cosmokit = node_package()
        .join("node_modules/@deepseek-ai/cosmokit/lib/index.js")
        .canonicalize()
        .unwrap();
    let tunable = dir.path().join("tunable.mjs");
    std::fs::write(
        &tunable,
        TUNABLE.replace("COSMOKIT", &format!("file://{}", cosmokit.display())),
    )
    .unwrap();

    let probe = Probe::default();
    let (root, loader, _runtime) = interop_loader(&probe).await;
    let layer = |level: u32| -> Vec<Layer> {
        let rows = vec![
            row(
                "t",
                &tunable,
                json!({ "tag": "t", "level": level }),
                json!(null),
            ),
            // Behind an inject gate, the plugin runs one fiber deeper.
            row(
                "g",
                &tunable,
                json!({ "tag": "g", "level": level }),
                json!({ "inject": ["probe"] }),
            ),
        ];
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        vec![Layer::new("rows", patches)]
    };

    let report = loader.reconcile(layer(1), None).await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    probe.wait_for("t: start 1").await;
    probe.wait_for("g: start 1").await;
    assert_eq!(
        loader.get("t").unwrap().schema.unwrap()["properties"]["level"]["x-volatile"],
        true
    );
    let fibers = (
        loader.get("t").unwrap().plugin,
        loader.get("g").unwrap().plugin,
    );
    probe.take();

    loader.reconcile(layer(5), None).await.unwrap();
    probe.wait_for(r#"t: [["level"]] 5"#).await;
    probe.wait_for(r#"g: [["level"]] 5"#).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let lines = probe.take();
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("bye") || l.contains("start")),
        "{lines:?}"
    );
    assert_eq!(
        (
            loader.get("t").unwrap().plugin,
            loader.get("g").unwrap().plugin
        ),
        fibers,
        "no restart on the rutis side either"
    );

    root.shutdown().await.unwrap();
}

/// Same schema, but the parsed config holds plain values: nothing to commit
/// into, so a volatile change must take an ordinary update.
const PLAIN: &str = r#"
export const name = 'plain'
export const inject = ['probe']
export const Config = {
  type: 'object', meta: {},
  dict: {
    tag: { type: 'string', meta: {} },
    level: { type: 'number', meta: { default: 1, volatile: true } },
  },
  '~standard': { validate: value => ({ value }) },
}
export function apply(ctx, config) {
  ctx.probe.record(`${config.tag}: start ${config.level}`)
}
"#;

/// The probe host, the Cordis runtime, then the loader with its rows.
async fn interop_loader(probe: &Probe) -> (Ctx, Loader, FiberView) {
    let root = Ctx::root().unwrap();
    root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .unwrap();
    let runtime = CordisRuntimePlugin::new(node_package(), node_package().join("package.json"))
        .host("probe", json!({ "record": "sync" }));
    let resolver = InteropResolver::new(runtime.handle());
    let runtime = root.plugin(runtime);
    (&runtime).await.unwrap();
    let plugin = LoaderPlugin::new(Chain::new().with(resolver), LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    (root, loader, runtime)
}

#[tokio::test(flavor = "multi_thread")]
async fn volatile_changes_are_never_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let cosmokit = node_package()
        .join("node_modules/@deepseek-ai/cosmokit/lib/index.js")
        .canonicalize()
        .unwrap();
    let write = |name: &str, text: String| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    // Waits on its own inject for `late`.
    let waiting = write(
        "waiting.mjs",
        TUNABLE
            .replace("COSMOKIT", &format!("file://{}", cosmokit.display()))
            .replace("['probe']", "['probe', 'late']"),
    );
    let plain = write("plain.mjs", PLAIN.to_owned());
    let flag = write("flag.mjs", FLAG.to_owned());

    let probe = Probe::default();
    let (root, loader, _runtime) = interop_loader(&probe).await;
    let layer = |level: u32, late: bool| -> Vec<Layer> {
        let mut rows = vec![
            row(
                "w",
                &waiting,
                json!({ "tag": "w", "level": level }),
                json!(null),
            ),
            row(
                "p",
                &plain,
                json!({ "tag": "p", "level": level }),
                json!(null),
            ),
        ];
        if late {
            rows.push(row("f", &flag, json!({}), json!(null)));
        }
        let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
        vec![Layer::new("rows", patches)]
    };

    let report = loader.reconcile(layer(1, false), None).await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    probe.wait_for("p: start 1").await;

    // No volatile reference to commit into: the plain row restarts with it.
    loader.reconcile(layer(5, false), None).await.unwrap();
    probe.wait_for("p: start 5").await;
    // The waiting row got the update while pending (sent before p's, on the
    // same connection); once `late` arrives it starts from it.
    loader.reconcile(layer(5, true), None).await.unwrap();
    probe.wait_for("w: start 5").await;
    let lines = probe.take();
    assert!(!lines.contains(&"w: start 1".to_owned()), "{lines:?}");

    root.shutdown().await.unwrap();
}

// ── The runtime is a plugin ─────────────────────────────────────

const EXIT: &str = r#"
export const name = 'exit'
export function apply() { setTimeout(() => process.exit(17), 200) }
"#;

fn rows(rows: Vec<Value>) -> Vec<Layer> {
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": rows }])).unwrap();
    vec![Layer::new("rows", patches)]
}

fn row_state(loader: &Loader, id: &str) -> Option<FiberState> {
    match loader.get(id)?.status {
        EntryStatus::Running(snapshot) => Some(snapshot.state),
        _ => None,
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn write_plugins(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    (
        write("provider.mjs", PROVIDER),
        write("consumer.mjs", CONSUMER),
        write("exit.mjs", EXIT),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_unload_before_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, consumer, _) = write_plugins(dir.path());
    let probe = Probe::default();
    let (root, loader, runtime) = interop_loader(&probe).await;
    let base = vec![
        row("p", &provider, json!({ "who": "rust" }), json!(null)),
        row("c", &consumer, json!({ "tag": "c" }), json!(null)),
    ];
    loader.reconcile(rows(base), None).await.unwrap();
    probe.wait_for("c: hello rust").await;

    // The row's own cleanup still reaches Cordis: the process outlives it.
    runtime.dispose().await.unwrap();
    probe.wait_for("c: bye").await;
    until("rows waiting for the runtime", || {
        row_state(&loader, "c") == Some(FiberState::Pending)
            && row_state(&loader, "p") == Some(FiberState::Pending)
    })
    .await;

    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_process_stops_the_rows_until_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, consumer, exit) = write_plugins(dir.path());
    let probe = Probe::default();
    let (root, loader, runtime) = interop_loader(&probe).await;
    let base = vec![
        row("p", &provider, json!({ "who": "rust" }), json!(null)),
        row("c", &consumer, json!({ "tag": "c" }), json!(null)),
    ];
    loader.reconcile(rows(base.clone()), None).await.unwrap();
    probe.wait_for("c: hello rust").await;
    probe.take();

    let mut dying = base.clone();
    dying.push(row("x", &exit, json!({}), json!(null)));
    loader.reconcile(rows(dying), None).await.unwrap();
    // The runtime withdraws its service; rows wait instead of holding a
    // dead process, and the runtime itself stays up for a restart.
    until("rows waiting after the crash", || {
        ["p", "c", "x"]
            .iter()
            .all(|id| row_state(&loader, id) == Some(FiberState::Pending))
    })
    .await;
    assert_eq!(runtime.state().state, FiberState::Active);
    let runtime_key = rutis::TypeKey::of::<rutis_interop::CordisRuntime>();
    let waiting = root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|plugin| plugin.name.ends_with("consumer.mjs"))
        .unwrap();
    assert_eq!(waiting.state, FiberState::Pending);
    assert!(
        waiting.injects.iter().any(|dep| dep.key == runtime_key),
        "the row shows it waits for the runtime: {:?}",
        waiting.injects
    );

    loader.reconcile(rows(base), None).await.unwrap();
    runtime.restart().await.unwrap();
    probe.wait_for("c: hello rust").await;
    until("rows running again", || {
        row_state(&loader, "c") == Some(FiberState::Active)
    })
    .await;

    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_waits_for_its_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, consumer, _) = write_plugins(dir.path());
    let probe = Probe::default();
    let root = Ctx::root().unwrap();
    let runtime = CordisRuntimePlugin::new(node_package(), node_package().join("package.json"))
        .host("probe", json!({ "record": "sync" }));
    let resolver = InteropResolver::new(runtime.handle());
    let runtime = root.plugin(runtime);
    let plugin = LoaderPlugin::new(Chain::new().with(resolver), LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    assert_eq!(runtime.state().state, FiberState::Pending);

    let base = vec![
        row("p", &provider, json!({ "who": "rust" }), json!(null)),
        row("c", &consumer, json!({ "tag": "c" }), json!(null)),
    ];
    let report = loader.reconcile(rows(base), None).await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    // Resolved without Node: no schema yet, and the row says why.
    let entry = loader.get("p").unwrap();
    assert!(entry.schema.is_none());
    assert!(
        entry.meta["schema"]
            .as_str()
            .unwrap()
            .contains("not running"),
        "{}",
        entry.meta
    );
    assert_eq!(row_state(&loader, "c"), Some(FiberState::Pending));

    let host = root
        .provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .unwrap();
    probe.wait_for("c: hello rust").await;

    // The host goes: the runtime and its rows stop with it.
    host.dispose().await.unwrap();
    until("everything waiting for the host", || {
        runtime.state().state == FiberState::Pending
            && row_state(&loader, "c") == Some(FiberState::Pending)
    })
    .await;

    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_that_cannot_start_does_not_block_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let (provider, _, _) = write_plugins(dir.path());
    let root = Ctx::root().unwrap();
    // No Node runtime here: the mount fails.
    let runtime = CordisRuntimePlugin::new(dir.path(), node_package().join("package.json"));
    let resolver = InteropResolver::new(runtime.handle());
    let runtime = root.plugin(runtime);
    let _ = (&runtime).await;
    assert_eq!(runtime.state().state, FiberState::Failed);
    let plugin = LoaderPlugin::new(Chain::new().with(resolver), LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();

    let report = tokio::time::timeout(
        Duration::from_secs(10),
        loader.reconcile(
            rows(vec![row("p", &provider, json!({}), json!(null))]),
            None,
        ),
    )
    .await
    .expect("resolution does not wait for a failed runtime")
    .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(row_state(&loader, "p"), Some(FiberState::Pending));
    assert!(loader.get("p").unwrap().meta["schema"].is_string());

    root.shutdown().await.unwrap();
}
