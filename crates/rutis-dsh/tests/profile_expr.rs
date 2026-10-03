//! `!!js` expressions evaluated in Rust against the same expressions in
//! JavaScript: every expression in the installed bundles, plus edge cases,
//! under two environments.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use rutis::{Ctx, TypeKey};
use rutis_dsh::profile::expr::{node_platform, Environment, HostFn, JsSubset};
use rutis_dsh::profile::{paths, yaml};
use rutis_loader::{ExprScope, Expressions, LoaderError, ServiceCatalog};
use serde_json::{json, Value};

fn collect(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map)
            if map.len() == 1 && map.get("__jsExpr").is_some_and(Value::is_string) =>
        {
            out.push(map["__jsExpr"].as_str().unwrap().to_owned());
        }
        Value::Object(map) => map.values().for_each(|v| collect(v, out)),
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        _ => {}
    }
}

const EXTRA: &[&str] = &[
    "1 + 2 * 3 - 4 / 2",
    "'a' + 1 + 2",
    "[1, 'x', null].includes('x')",
    "['a', 'b'].join('-')",
    "'Hello'.toLowerCase().startsWith('he')",
    "null ?? undefined ?? 0 ?? 5",
    "0 || '' || 'fallback'",
    "1 && 2 && 0",
    "!!'x' === true",
    "undefined == null",
    "undefined === null",
    "Number('  12  ') + Number('') + Number(true)",
    "Number('x') === Number('x')",
    "String(12.5) + String(null)",
    "Boolean('') || Boolean([])",
    "JSON.parse('{\"a\": [1, 2]}').a[1]",
    "ctx.webStartup?.missing?.deeper",
    "ctx.get('webStartup').port > 3000 ? 'high' : 'low'",
    "3 % 2 === 1",
    "-'3' + +'4'",
    "process.platform === 'nope'",
    "'abc'.length",
    "process.getBuiltinModule('node:path').join('a', '..', 'b', '')",
];

fn services() -> Value {
    json!({
        "profileContext": { "name": "desktop", "dir": "/profiles/desktop" },
        "webStartup": { "port": 3081, "host": "0.0.0.0", "openBrowser": false, "trustedHosts": ["a.test"] },
        "webRuntime": { "trustedHosts": [] },
        "headlessStartup": { "task": "t", "sessionId": "s", "json": true }
    })
}

fn rust_eval(
    exprs: &[String],
    vars: &HashMap<String, String>,
    cwd: &str,
    home: &str,
    with_profile: bool,
) -> Vec<Value> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let root = Ctx::root().unwrap();
        let mut catalog = ServiceCatalog::new();
        let mut disposers = Vec::new();
        for (name, value) in services().as_object().unwrap() {
            let key = TypeKey::keyed_dynamic::<Value>(name.clone());
            catalog.readable_keyed::<Value>(name.clone(), key.clone());
            if name == "profileContext" && !with_profile {
                continue;
            }
            disposers.push(
                root.provide_as::<Value>(key, Arc::new(value.clone()))
                    .unwrap(),
            );
        }
        let home = home.to_owned();
        let mut functions: HashMap<String, HostFn> = HashMap::new();
        functions.insert(
            "dshHomePath".into(),
            Arc::new(move |args: &[Value]| {
                let mut segments = vec![home.clone()];
                segments.extend(
                    args.iter()
                        .map(|a| a.as_str().unwrap_or_default().to_owned()),
                );
                Ok(Value::String(paths::join(&segments)))
            }),
        );
        let js = JsSubset::new(Environment {
            vars: Some(vars.clone()),
            cwd: Some(cwd.to_owned()),
            platform: node_platform().into(),
            functions,
        });
        let scope = ExprScope::new(Some(&root), &catalog);
        exprs
            .iter()
            .map(|e| match js.evaluate(e, &scope) {
                Ok(v) => json!({ "value": v }),
                Err(error) => json!({ "error": error.to_string() }),
            })
            .collect()
    })
}

fn compare(vars: &[(&str, &str)], with_profile: bool) {
    let Some(files) = support::bundle_patch_files() else {
        eprintln!("skipped: the dsh npm project is not installed");
        return;
    };
    let mut exprs = Vec::new();
    for file in files {
        collect(
            &yaml::parse(&std::fs::read_to_string(file).unwrap()).unwrap(),
            &mut exprs,
        );
    }
    exprs.sort();
    exprs.dedup();
    assert!(exprs.len() > 20, "{exprs:?}");
    exprs.extend(EXTRA.iter().map(|s| s.to_string()));

    let home = "/home/u/.dsh";
    let mut services = services();
    if !with_profile {
        services.as_object_mut().unwrap().remove("profileContext");
    }
    let mut all_vars: Vec<(&str, &str)> = vars.to_vec();
    all_vars.push(("DSH_HOME", home));
    let request = json!({ "exprs": exprs, "services": services, "cwd": "/" }).to_string();
    // Both sides run in "/", which exists for Node.
    let expected = support::node_with(&["eval"], [request.as_str()], &all_vars);
    let vars_map: HashMap<String, String> = all_vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let ours = rust_eval(&exprs, &vars_map, "/", home, with_profile);
    for ((expr, theirs), mine) in exprs.iter().zip(expected.as_array().unwrap()).zip(&ours) {
        match (theirs.get("value"), mine.get("value")) {
            (Some(a), Some(b)) => assert_eq!(a, b, "{expr}"),
            (None, None) => {}
            _ => panic!("{expr}: node {theirs}, rust {mine}"),
        }
    }
}

#[test]
fn bundle_expressions_match_javascript_with_a_desktop_profile() {
    compare(
        &[
            ("DSH_PRIMARY_RUNTIME", "/opt/dsh/runtime"),
            ("DSH_PERMISSION_MODE", "danger-full-access"),
            ("DSH_MAX_TOKENS_AS_SUCCESS", "false"),
            ("DSH_CONTEXT_WINDOW", "5000"),
            ("DSH_TOOLS_MODE", "minimal"),
            ("DSH_TELEMETRY_MODE", ""),
            ("DSH_SYSTEM_PROMPT", "Be brief."),
        ],
        true,
    );
}

#[test]
fn bundle_expressions_match_javascript_with_nothing_set() {
    compare(&[], false);
}

#[test]
fn unsupported_syntax_and_unknown_names_are_errors() {
    let catalog = ServiceCatalog::new();
    let scope = ExprScope::new(None, &catalog);
    let js = JsSubset::new(Environment::current());
    for source in [
        "require('fs')",
        "process.exit()",
        "a = 1",
        "`template`",
        "ctx.get('nope')",
        "x => x",
    ] {
        let err = js.evaluate(source, &scope).unwrap_err();
        assert!(
            matches!(
                err,
                LoaderError::Expression(_) | LoaderError::UnknownService(_)
            ),
            "{source}: {err:?}"
        );
    }
}
