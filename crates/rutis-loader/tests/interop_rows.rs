//! JavaScript plugins as loader rows (P6): one shared Cordis Context,
//! services resolved between rows natively, per-row load/update/unload,
//! isolate and inject forwarded, schemastery schema exported.
#![cfg(all(unix, feature = "interop"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::Ctx;
use rutis_interop::rpc::{Reply, Value as RpcValue};
use rutis_interop::{Host, HostDispatch};
use rutis_loader::{
    Chain, EntryStatus, InteropResolver, Layer, LoaderError, LoaderOptions, LoaderPlugin, Patch,
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
    let resolver = InteropResolver::new(node_package(), node_package().join("package.json"))
        .with_hosts(vec![Host {
            name: "probe".into(),
            methods: json!({ "record": "sync" }),
            dispatch: Arc::new(probe.clone()),
        }]);
    let root = Ctx::root().unwrap();
    let plugin = LoaderPlugin::new(Chain::new().with(resolver), LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();

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
    let resolver = InteropResolver::new(node_package(), node_package().join("package.json"))
        .with_hosts(vec![Host {
            name: "probe".into(),
            methods: json!({ "record": "sync" }),
            dispatch: Arc::new(probe.clone()),
        }]);
    let root = Ctx::root().unwrap();
    let plugin = LoaderPlugin::new(Chain::new().with(resolver), LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
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

async fn interop_loader(probe: &Probe) -> (Ctx, rutis_loader::Loader) {
    let resolver = InteropResolver::new(node_package(), node_package().join("package.json"))
        .with_hosts(vec![Host {
            name: "probe".into(),
            methods: json!({ "record": "sync" }),
            dispatch: Arc::new(probe.clone()),
        }]);
    let root = Ctx::root().unwrap();
    let plugin = LoaderPlugin::new(Chain::new().with(resolver), LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    (root, loader)
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
    let (root, loader) = interop_loader(&probe).await;
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
