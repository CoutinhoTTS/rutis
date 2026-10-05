//! Python rows alone: no Node runtime is compiled or started.
#![cfg(all(unix, feature = "python"))]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rutis::Ctx;
use rutis_interop::rpc::{Reply, Value as RpcValue};
use rutis_interop::{host_key, HostDispatch, RuntimePlugin};
use rutis_loader::{
    Chain, InteropResolver, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeRowsPlugin,
};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);

impl HostDispatch for Probe {
    fn invoke(&self, _method: &str, args: RpcValue) -> Reply {
        let [line]: [String; 1] = rutis_interop::decode_value(args)?;
        self.0.lock().unwrap().push(line);
        Ok(RpcValue::Undefined)
    }

    fn methods(&self) -> Option<Value> {
        Some(json!({ "record": "sync" }))
    }
}

impl Probe {
    async fn wait_for(&self, line: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !self.0.lock().unwrap().iter().any(|l| l == line) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{line:?} not recorded: {:?}", self.0.lock().unwrap()));
    }
}

fn plugin(version: &str) -> String {
    format!(
        "inject = [\"probe\"]\n\n\ndef apply(ctx, config):\n    probe = ctx.use(\"probe\")\n    probe.record(\"{version}: start\")\n    return lambda: probe.record(\"{version}: bye\")\n"
    )
}

/// After the module's source changes, reloading the row runs the new code.
#[tokio::test(flavor = "multi_thread")]
async fn reloading_a_row_runs_the_edited_module() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("edited.py");
    std::fs::write(&module, plugin("v1")).unwrap();
    let probe = Probe::default();
    let root = Ctx::root().unwrap();
    root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .unwrap();
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/python");
    let python = RuntimePlugin::python(sdk, dir.path());
    let rows = Arc::new(InteropResolver::modules(python.handle()));
    root.plugin(python).await.unwrap();
    let plugin_loader = LoaderPlugin::new(
        Chain::new().with_shared(rows.clone()),
        LoaderOptions::default(),
    );
    let loader = plugin_loader.handle();
    root.plugin(plugin_loader).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(rows));

    let patches: Vec<Patch> = serde_json::from_value(
        json!([{ "insert": [{ "id": "e", "name": "py:edited", "config": {} }] }]),
    )
    .unwrap();
    loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();
    probe.wait_for("v1: start").await;

    std::fs::write(&module, plugin("v2")).unwrap();
    // A later modification time, even within the same clock tick.
    std::fs::File::options()
        .write(true)
        .open(&module)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(5))
        .unwrap();
    loader.reload("e").await.unwrap();
    probe.wait_for("v1: bye").await;
    probe.wait_for("v2: start").await;
    root.shutdown().await.unwrap();
}
