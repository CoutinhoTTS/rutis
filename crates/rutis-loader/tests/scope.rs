//! P2: service catalog (config `isolate` / `inject`) and the expression hook.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use rutis::FiberState;
use rutis_loader::{
    Editable, EntryStatus, ExprScope, Expressions, Isolate, Layer, LoaderError, LoaderOptions,
    NewEntry, ServiceCatalog,
};
use serde::Serialize;
use serde_json::{json, Value};

/// A readable host service, like dsh's startup parameters.
#[derive(Serialize)]
struct Startup {
    port: u16,
}

/// `has:<name>`, `!has:<name>`, `read:<name>.<field>`, `json:<literal>`.
struct Mini;

impl Expressions for Mini {
    fn evaluate(&self, expr: &str, scope: &ExprScope<'_>) -> Result<Value, LoaderError> {
        if let Some(name) = expr.strip_prefix("!has:") {
            return Ok(Value::Bool(!scope.has(name)?));
        }
        if let Some(name) = expr.strip_prefix("has:") {
            return Ok(Value::Bool(scope.has(name)?));
        }
        if let Some(path) = expr.strip_prefix("read:") {
            let (name, field) = path.split_once('.').unwrap();
            return Ok(scope
                .read(name)?
                .and_then(|v| v.get(field).cloned())
                .unwrap_or(Value::Null));
        }
        if let Some(literal) = expr.strip_prefix("json:") {
            return serde_json::from_str(literal)
                .map_err(|e| LoaderError::Expression(e.to_string()));
        }
        Err(LoaderError::Expression(format!("unsupported: {expr}")))
    }
}

fn catalog() -> ServiceCatalog {
    let mut catalog = ServiceCatalog::new();
    catalog
        .register::<Dep>("dep")
        .readable::<Startup>("startup");
    catalog
}

async fn setup(expressions: bool) -> (Harness, MemStore, rutis::Ctx, rutis_loader::Loader) {
    let harness = Harness::new();
    let store = MemStore::default();
    let (root, loader) = mount_with(
        harness.resolver(),
        LoaderOptions {
            persist: Arc::new(store.clone()),
            catalog: catalog(),
            expressions: expressions.then(|| Arc::new(Mini) as Arc<dyn Expressions>),
        },
    )
    .await;
    (harness, store, root, loader)
}

async fn apply(
    loader: &rutis_loader::Loader,
    store: &MemStore,
    rows: Value,
) -> rutis_loader::ReconcileReport {
    loader
        .reconcile(
            vec![
                Layer::new("base", patches(json!([{ "insert": rows }]))),
                Layer::new("user", store.patches()),
            ],
            Some(Editable::new("user", store.version())),
        )
        .await
        .unwrap()
}

fn expr(source: &str) -> Value {
    json!({ "__jsExpr": source })
}

#[tokio::test]
async fn isolate_private_and_shared() {
    let (h, store, _root, loader) = setup(false).await;
    let report = apply(
        &loader,
        &store,
        json!([
            { "id": "a", "group": true, "isolate": { "dep": true }, "config": [
                { "id": "pa", "name": "provider", "config": { "label": "A" } },
                { "id": "ka", "name": "consumer", "config": { "label": "ka" } }
            ] },
            { "id": "b", "group": true, "isolate": { "dep": "shared" }, "config": [
                { "id": "pb", "name": "provider", "config": { "label": "B" } }
            ] },
            { "id": "c", "group": true, "isolate": { "dep": "shared" }, "config": [
                { "id": "kc", "name": "consumer", "config": { "label": "kc" } }
            ] },
            { "id": "kr", "name": "consumer", "config": { "label": "kr" } }
        ]),
    )
    .await;
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(state(&loader, "ka"), Some(FiberState::Active));
    assert_eq!(state(&loader, "kc"), Some(FiberState::Active));
    // The root sees neither isolated provider.
    assert_eq!(state(&loader, "kr"), Some(FiberState::Pending));
    let log = h.take_log();
    assert!(log.contains(&"consume:A".to_owned()), "{log:?}");
    assert!(log.contains(&"consume:B".to_owned()), "{log:?}");
}

#[tokio::test]
async fn inject_gates_a_row() {
    let (_h, store, _root, loader) = setup(false).await;
    apply(
        &loader,
        &store,
        json!([{ "id": "e", "name": "echo", "inject": ["dep"], "config": { "label": "e" } }]),
    )
    .await;
    assert_eq!(state(&loader, "e"), Some(FiberState::Pending));
    loader
        .create(
            NewEntry {
                id: Some("p".into()),
                name: "provider".into(),
                config: json!({ "label": "p" }),
                ..NewEntry::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(state(&loader, "e"), Some(FiberState::Active));
}

#[tokio::test]
async fn unknown_services_and_intercept_config_are_unresolved() {
    let (_h, store, _root, loader) = setup(false).await;
    let report = apply(
        &loader,
        &store,
        json!([
            { "id": "u", "name": "echo", "inject": ["dep", "nope"], "isolate": { "other": true } },
            { "id": "i", "name": "echo", "inject": { "dep": { "level": 1 } } }
        ]),
    )
    .await;
    assert_eq!(report.failures.len(), 2, "{report:?}");
    match loader.get("u").unwrap().status {
        EntryStatus::Unresolved(LoaderError::UnknownService(names)) => {
            assert_eq!(names, ["other", "nope"]);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        loader.get("i").unwrap().status,
        EntryStatus::Unresolved(LoaderError::Unsupported(_))
    ));
}

#[tokio::test]
async fn changing_the_scope_respawns_and_persists() {
    let (_h, store, _root, loader) = setup(false).await;
    apply(
        &loader,
        &store,
        json!([{ "id": "e", "name": "echo", "config": { "label": "e" } }]),
    )
    .await;
    let before = loader.get("e").unwrap().plugin;

    let mut isolate = BTreeMap::new();
    isolate.insert("dep".to_owned(), Isolate::Shared("x".into()));
    loader.set_isolate("e", isolate).await.unwrap();
    let after = loader.get("e").unwrap().plugin;
    assert_ne!(after, before);

    loader.set_inject("e", vec!["dep".into()]).await.unwrap();
    assert_eq!(state(&loader, "e"), Some(FiberState::Pending));
    assert_eq!(
        store.patches(),
        patches(json!([{ "id": "e", "isolate": { "dep": "x" }, "inject": ["dep"] }]))
    );
    // Unknown names are refused before anything changes.
    let err = loader
        .set_inject("e", vec!["nope".into()])
        .await
        .unwrap_err();
    assert!(matches!(err, LoaderError::UnknownService(_)), "{err:?}");
    // Clearing removes the field again.
    loader.set_inject("e", vec![]).await.unwrap();
    assert_eq!(state(&loader, "e"), Some(FiberState::Active));
}

#[tokio::test]
async fn disabled_and_config_expressions() {
    let (_h, store, root, loader) = setup(true).await;
    let rows = json!([
        { "id": "x", "name": "plain", "disabled": expr("!has:startup"),
          "config": { "port": expr("read:startup.port"), "fixed": 1 } }
    ]);
    apply(&loader, &store, rows.clone()).await;
    assert!(matches!(
        loader.get("x").unwrap().status,
        EntryStatus::Disabled
    ));

    let provided = root.provide(Startup { port: 3080 }).unwrap();
    apply(&loader, &store, rows.clone()).await;
    assert_eq!(state(&loader, "x"), Some(FiberState::Active));
    let plugin = loader.get("x").unwrap().plugin;
    assert_eq!(
        loader.evaluated("x").unwrap().unwrap(),
        json!({ "port": 3080, "fixed": 1 })
    );
    // The row keeps the raw expression.
    assert_eq!(
        loader.get("x").unwrap().options["config"]["port"],
        expr("read:startup.port")
    );

    // A new value is picked up by the next reconcile, in place.
    provided.dispose().await.unwrap();
    let _again = root.provide(Startup { port: 4000 }).unwrap();
    apply(&loader, &store, rows).await;
    assert_eq!(loader.get("x").unwrap().plugin, plugin);
    assert_eq!(
        loader.evaluated("x").unwrap().unwrap(),
        json!({ "port": 4000, "fixed": 1 })
    );
}

#[tokio::test]
async fn expression_failures_are_reported() {
    let (_h, store, _root, loader) = setup(true).await;
    let report = apply(
        &loader,
        &store,
        json!([
            { "id": "r", "name": "plain", "config": { "v": expr("read:dep.x") } },
            { "id": "w", "name": "plain", "config": { "v": expr("weird") } },
            { "id": "d", "name": "plain", "disabled": expr("has:nope") }
        ]),
    )
    .await;
    assert_eq!(report.failures.len(), 3, "{report:?}");
    assert!(matches!(
        loader.get("r").unwrap().status,
        EntryStatus::Unresolved(LoaderError::NotReadable(_))
    ));
    assert!(matches!(
        loader.get("w").unwrap().status,
        EntryStatus::Unresolved(LoaderError::Expression(_))
    ));
    assert!(matches!(
        loader.get("d").unwrap().status,
        EntryStatus::Unresolved(LoaderError::UnknownService(_))
    ));
}

#[tokio::test]
async fn without_an_evaluator_expressions_are_unresolved() {
    let (_h, store, _root, loader) = setup(false).await;
    let report = apply(
        &loader,
        &store,
        json!([
            { "id": "x", "name": "plain", "config": { "v": expr("json:1") } },
            // Not an expression: a second key makes it a plain object.
            { "id": "y", "name": "plain", "config": { "v": { "__jsExpr": "json:1", "other": 1 } } }
        ]),
    )
    .await;
    assert_eq!(report.failures.len(), 1, "{report:?}");
    assert!(matches!(
        loader.get("x").unwrap().status,
        EntryStatus::Unresolved(LoaderError::Expression(_))
    ));
    assert_eq!(state(&loader, "y"), Some(FiberState::Active));
}

#[tokio::test]
async fn edits_keep_raw_expressions() {
    let (_h, store, root, loader) = setup(true).await;
    let _startup = root.provide(Startup { port: 1 }).unwrap();
    apply(&loader, &store, json!([])).await;
    loader
        .create(
            NewEntry {
                id: Some("n".into()),
                name: "plain".into(),
                config: json!({ "port": expr("read:startup.port") }),
                ..NewEntry::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        loader.evaluated("n").unwrap().unwrap(),
        json!({ "port": 1 })
    );
    assert_eq!(
        store.patches(),
        patches(json!([{ "insert": [
            { "id": "n", "name": "plain", "config": { "port": expr("read:startup.port") } }
        ] }]))
    );
    // A failing expression is caught by the dry run.
    let err = loader
        .update("n", json!({ "port": expr("weird") }))
        .await
        .unwrap_err();
    assert!(matches!(err, LoaderError::Expression(_)), "{err:?}");
}
