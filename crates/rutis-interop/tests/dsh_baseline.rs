#![cfg(unix)]
//! Real published Cordis plugins mounted through rutis-interop, compared with
//! the same calls in native Cordis. Scenarios live in `interop/baseline`;
//! run `npm --prefix interop/baseline ci` first, otherwise this test skips.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rutis_interop::{Error, Process};
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Deserialize)]
struct Scenario {
    name: String,
    entry: PathBuf,
    service: String,
    methods: Vec<String>,
    config: Value,
    calls: Vec<Call>,
}

#[derive(Deserialize)]
struct Call {
    method: String,
    args: Value,
    capture: Option<String>,
}

fn node(dir: &Path, args: &[&str]) -> Value {
    let output = std::process::Command::new("node")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "node {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node prints JSON")
}

fn substitute(value: &Value, captured: &Map<String, Value>) -> Value {
    match value {
        Value::String(text) if text.starts_with('$') => {
            captured.get(&text[1..]).cloned().unwrap_or(Value::Null)
        }
        Value::Array(items) => items
            .iter()
            .map(|item| substitute(item, captured))
            .collect(),
        Value::Object(fields) => fields
            .iter()
            .map(|(key, item)| (key.clone(), substitute(item, captured)))
            .collect(),
        other => other.clone(),
    }
}

fn outcome(result: Result<Value, Error>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(Error::Remote { name, message, .. }) => {
            json!({ "error": { "name": name, "message": message } })
        }
        Err(error) => json!({ "error": { "name": "Binding", "message": error.to_string() } }),
    }
}

async fn mounted(package: &Path, scenario: &Scenario) -> Value {
    let manifest = json!({ &scenario.service: scenario.methods });
    let process =
        match Process::launch(package, &scenario.entry, scenario.config.clone(), manifest).await {
            Ok(process) => process,
            Err(error) => {
                return json!({ "available": false, "results": [], "error": error.to_string() })
            }
        };
    let handle = process.service(&scenario.service);
    let mut results = Vec::new();
    let mut captured = Map::new();
    if let Some(handle) = &handle {
        for call in &scenario.calls {
            let args = substitute(&call.args, &captured);
            let result = process.call_async(handle, &call.method, args).await;
            if let (Some(name), Ok(value)) = (&call.capture, &result) {
                captured.insert(name.clone(), value.clone());
            }
            results.push(outcome(result));
        }
    }
    let _ = process.dispose().await;
    json!({ "available": handle.is_some(), "results": results })
}

#[tokio::test(flavor = "multi_thread")]
async fn published_plugins_match_native_cordis() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let baseline = root.join("interop/baseline");
    if !baseline.join("node_modules").exists() {
        eprintln!(
            "skipped: run `npm --prefix interop/baseline ci` to install the baseline plugins"
        );
        return;
    }
    let package = root.join("interop/node").canonicalize().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let scratch = directory.path().to_str().unwrap();

    let scenarios: Vec<Scenario> =
        serde_json::from_value(node(&baseline, &["scenarios.mjs", scratch])).unwrap();
    let native = node(&baseline, &["native.mjs", scratch]);
    let mut interop = Map::new();
    for scenario in &scenarios {
        interop.insert(scenario.name.clone(), mounted(&package, scenario).await);
    }

    let normalize = |value: &Value| -> Value {
        serde_json::from_str(&value.to_string().replace(scratch, "$DIR")).unwrap()
    };
    let native = normalize(&native);
    let interop = normalize(&Value::Object(interop));
    let mut summary = BTreeMap::new();
    let mut mismatches = Vec::new();
    for scenario in &scenarios {
        let (left, right) = (&native[&scenario.name], &interop[&scenario.name]);
        let calls = scenario.calls.len();
        let matched = (0..calls)
            .filter(|&i| left["results"][i] == right["results"][i])
            .count();
        let available = left["available"] == right["available"];
        summary.insert(
            scenario.name.clone(),
            format!(
                "available native={} interop={} | calls matched {matched}/{calls}",
                left["available"], right["available"]
            ),
        );
        if !available || matched != calls {
            mismatches.push(format!(
                "{}:\n  native  {left}\n  interop {right}",
                scenario.name
            ));
        }
    }
    for (name, line) in &summary {
        eprintln!("{name:12} {line}");
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
