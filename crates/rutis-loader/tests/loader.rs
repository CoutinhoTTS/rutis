//! P1 acceptance tests (design §十七).

mod common;

use std::sync::atomic::Ordering;

use common::*;
use rutis::FiberState;
use rutis_loader::{apply_patches, Editable, EntryStatus, Layer, LoaderError, NewEntry, Version};
use serde_json::{json, Value};

fn base() -> Vec<rutis_loader::Patch> {
    patches(json!([{ "insert": [
        { "id": "a", "name": "echo", "config": { "label": "a1" } },
        { "id": "g", "name": "group", "group": true, "config": [
            { "id": "c", "name": "echo", "config": { "label": "c1" } }
        ] }
    ] }]))
}

async fn setup() -> (Harness, MemStore, rutis::Ctx, rutis_loader::Loader) {
    let harness = Harness::new();
    let store = MemStore::default();
    let (root, loader) = mount(harness.resolver(), store.clone()).await;
    reconcile(&loader, base(), &store).await;
    (harness, store, root, loader)
}

fn new_entry(id: &str, name: &str, config: Value) -> NewEntry {
    NewEntry {
        id: Some(id.into()),
        name: name.into(),
        config,
        ..NewEntry::default()
    }
}

#[tokio::test]
async fn create_runs_and_update_keeps_the_plugin_id() {
    let (harness, store, _root, loader) = setup().await;
    assert_eq!(state(&loader, "a"), Some(FiberState::Active));
    assert_eq!(state(&loader, "c"), Some(FiberState::Active));

    let (id, view) = loader
        .create(new_entry("n", "echo", json!({ "label": "n1" })), None, None)
        .await
        .unwrap();
    assert_eq!(id, "n");
    let plugin = view.unwrap().id;
    assert_eq!(state(&loader, "n"), Some(FiberState::Active));

    loader.update("n", json!({ "label": "n2" })).await.unwrap();
    assert_eq!(loader.get("n").unwrap().plugin, Some(plugin));
    assert!(harness
        .take_log()
        .ends_with(&["cleanup:n1".into(), "apply:n2".into()]));
    // Both edits are stored in the user layer.
    let stored = apply_patches(&[
        Layer::new("base", base()),
        Layer::new("user", store.patches()),
    ]);
    let n = stored
        .flat
        .iter()
        .find(|r| r.id.as_deref() == Some("n"))
        .unwrap();
    assert_eq!(n.value["config"], json!({ "label": "n2" }));
}

#[tokio::test]
async fn invalid_update_changes_nothing() {
    let (harness, store, _root, loader) = setup().await;
    harness.take_log();
    let saves = store.inner.lock().unwrap().saves.len();
    let err = loader
        .update("a", json!({ "label": "x", "invalid": true }))
        .await
        .unwrap_err();
    assert!(matches!(err, LoaderError::Rejected { .. }), "{err:?}");
    assert_eq!(state(&loader, "a"), Some(FiberState::Active));
    assert!(harness.take_log().is_empty());
    assert_eq!(store.inner.lock().unwrap().saves.len(), saves);
    assert!(loader.layers()[1].patches.is_empty());
}

#[tokio::test]
async fn disable_and_enable() {
    let (harness, store, _root, loader) = setup().await;
    harness.take_log();
    loader.set_disabled("a", true).await.unwrap();
    assert!(matches!(
        loader.get("a").unwrap().status,
        EntryStatus::Disabled
    ));
    assert_eq!(harness.take_log(), vec!["cleanup:a1"]);
    assert_eq!(
        store.patches(),
        patches(json!([{ "id": "a", "disabled": true }]))
    );

    loader.set_disabled("a", false).await.unwrap();
    assert_eq!(state(&loader, "a"), Some(FiberState::Active));
    assert_eq!(harness.take_log(), vec!["apply:a1"]);
}

#[tokio::test]
async fn ownership_and_upper_overrides() {
    let harness = Harness::new();
    let store = MemStore::default();
    let (_root, loader) = mount(harness.resolver(), store.clone()).await;
    loader
        .reconcile(
            vec![
                Layer::new("base", base()),
                Layer::new("user", vec![]),
                Layer::new(
                    "overlay",
                    patches(json!([{ "id": "a", "config": { "label": "o" } }])),
                ),
            ],
            Some(Editable::new("user", store.version())),
        )
        .await
        .unwrap();

    for err in [
        loader.remove("a").await.unwrap_err(),
        loader.move_to("a", Some("g"), None).await.unwrap_err(),
        loader.rename_module("a", "echo2").await.unwrap_err(),
    ] {
        assert!(matches!(err, LoaderError::NotOwned { .. }), "{err:?}");
    }
    let err = loader
        .update("a", json!({ "label": "u" }))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LoaderError::OverriddenByLayer { layer, .. } if layer == "overlay"),
        "{err:?}"
    );
    // Not overridden above: a lower row can be disabled.
    loader.set_disabled("c", true).await.unwrap();

    // Owned rows can be renamed, moved and removed.
    loader
        .create(new_entry("n", "echo", json!({ "label": "n" })), None, None)
        .await
        .unwrap();
    loader.rename_module("n", "echo2").await.unwrap();
    loader.move_to("n", Some("g"), None).await.unwrap();
    assert_eq!(loader.get("n").unwrap().parent.as_deref(), Some("g"));
    loader.remove("n").await.unwrap();
    assert!(loader.get("n").is_none());
    assert_eq!(
        store.patches(),
        patches(json!([{ "id": "c", "disabled": true }]))
    );
}

#[tokio::test]
async fn create_inside_a_lower_group_only_appends() {
    let (_h, store, _root, loader) = setup().await;
    let err = loader
        .create(new_entry("x", "echo", json!({})), Some("g"), Some(0))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, LoaderError::Unsupported(_)), "{err:?}");
    loader
        .create(
            new_entry("x", "echo", json!({ "label": "x" })),
            Some("g"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.patches(),
        patches(
            json!([{ "id": "g", "insert": [{ "id": "x", "name": "echo", "config": { "label": "x" } }] }])
        )
    );
    // Before an owned sibling, a position works.
    loader
        .create(
            new_entry("y", "echo", json!({ "label": "y" })),
            Some("g"),
            Some(1),
        )
        .await
        .unwrap();
    let order: Vec<String> = loader
        .entries()
        .into_iter()
        .filter(|e| e.parent.as_deref() == Some("g"))
        .map(|e| e.id)
        .collect();
    assert_eq!(order, ["c", "y", "x"]);
}

#[tokio::test]
async fn apply_failure_rolls_back() {
    let (harness, store, _root, loader) = setup().await;
    let plugin = loader.get("a").unwrap().plugin;
    harness.take_log();
    let err = loader
        .update("a", json!({ "label": "bad", "fail_apply": true }))
        .await
        .unwrap_err();
    assert!(matches!(err, LoaderError::ApplyFailed { .. }), "{err:?}");
    assert_eq!(state(&loader, "a"), Some(FiberState::Active));
    assert_eq!(loader.get("a").unwrap().plugin, plugin);
    assert!(harness
        .take_log()
        .ends_with(&["fail:bad".into(), "apply:a1".into()]));
    assert!(store.patches().is_empty());
    assert!(loader.layers()[1].patches.is_empty());

    // create waits for apply too, and rolls back.
    let err = loader
        .create(
            new_entry("z", "echo", json!({ "fail_apply": true })),
            None,
            None,
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(err, LoaderError::ApplyFailed { .. }), "{err:?}");
    assert!(loader.get("z").is_none());
}

#[tokio::test]
async fn rollback_failure_reports_both() {
    let (harness, store, _root, loader) = setup().await;
    loader
        .create(
            new_entry("f", "flaky", json!({ "label": "f1" })),
            None,
            None,
        )
        .await
        .unwrap();
    let saved = store.patches();
    harness.broken.store(true, Ordering::SeqCst);
    let err = loader
        .update("f", json!({ "label": "f2" }))
        .await
        .unwrap_err();
    let LoaderError::RollbackFailed { apply, rollback } = err else {
        panic!("{err:?}");
    };
    assert_eq!(apply[0].id, "f");
    assert_eq!(rollback[0].id, "f");
    assert_eq!(state(&loader, "f"), Some(FiberState::Failed));
    // Layer and storage keep the old config.
    assert_eq!(store.patches(), saved);
    assert_eq!(
        loader.evaluated("f").unwrap().unwrap(),
        json!({ "label": "f1" })
    );
}

#[tokio::test]
async fn pending_dependency_counts_as_settled() {
    let (_h, store, _root, loader) = setup().await;
    let (_, view) = loader
        .create(new_entry("k", "consumer", json!({})), None, None)
        .await
        .unwrap();
    assert!(view.is_some());
    assert_eq!(state(&loader, "k"), Some(FiberState::Pending));
    assert!(!store.patches().is_empty());

    loader
        .create(
            new_entry("p", "provider", json!({ "label": "p1" })),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(state(&loader, "k"), Some(FiberState::Active));
}

#[tokio::test]
async fn provider_update_reloads_consumers() {
    let (harness, _store, _root, loader) = setup().await;
    loader
        .create(
            new_entry("p", "provider", json!({ "label": "p1" })),
            None,
            None,
        )
        .await
        .unwrap();
    loader
        .create(
            new_entry("k", "consumer", json!({ "label": "k" })),
            None,
            None,
        )
        .await
        .unwrap();
    harness.take_log();
    loader.update("p", json!({ "label": "p2" })).await.unwrap();
    let log = harness.take_log();
    assert!(log.contains(&"consume:p2".to_owned()), "{log:?}");
    assert_eq!(state(&loader, "k"), Some(FiberState::Active));
}

#[tokio::test]
async fn rename_updates_in_place_or_respawns() {
    let (_h, _store, _root, loader) = setup().await;
    loader
        .create(new_entry("n", "echo", json!({ "label": "n" })), None, None)
        .await
        .unwrap();
    let before = loader.get("n").unwrap().plugin;
    loader.rename_module("n", "echo2").await.unwrap();
    assert_eq!(
        loader.get("n").unwrap().plugin,
        before,
        "same factory and injects: in place"
    );

    // Another factory (identity) with the same injects: respawned too.
    loader.rename_module("n", "provider").await.unwrap();
    let provider = loader.get("n").unwrap().plugin;
    assert_ne!(provider, before, "different factory: respawned");
    loader.rename_module("n", "consumer").await.unwrap();
    let after = loader.get("n").unwrap().plugin;
    assert_ne!(after, before, "different injects: respawned");
    assert_eq!(state(&loader, "n"), Some(FiberState::Pending));
}

#[tokio::test]
async fn reconcile_paths_and_failure_accounting() {
    let (_h, _store, _root, loader) = setup().await;
    let a = loader.get("a").unwrap().plugin;
    let c = loader.get("c").unwrap().plugin;
    let layers = |extra: Value| {
        vec![
            Layer::new("base", base()),
            Layer::new("user", patches(extra)),
        ]
    };
    // A config change updates in place; an unknown module is a new failure.
    let report = loader
        .reconcile(
            layers(json!([
                { "id": "a", "config": { "label": "a2" } },
                { "insert": [{ "id": "bad", "name": "nope" }] }
            ])),
            None,
        )
        .await
        .unwrap();
    assert_eq!(loader.get("a").unwrap().plugin, a);
    assert_eq!(loader.get("c").unwrap().plugin, c);
    assert_eq!(report.new_failures.len(), 1, "{report:?}");
    assert_eq!(report.new_failures[0].id, "bad");
    assert!(matches!(
        loader.get("bad").unwrap().status,
        EntryStatus::Unresolved(_)
    ));

    // The same broken row again is not new.
    let report = loader
        .reconcile(
            layers(json!([
                { "id": "a", "config": { "label": "a2" } },
                { "insert": [{ "id": "bad", "name": "nope" }] },
                { "id": "g", "disabled": true }
            ])),
            None,
        )
        .await
        .unwrap();
    assert!(report.new_failures.is_empty(), "{report:?}");
    assert_eq!(report.failures.len(), 1);
    assert!(matches!(
        loader.get("c").unwrap().status,
        EntryStatus::Inactive
    ));
    // Without an editable layer, edits are refused.
    let err = loader.set_disabled("a", true).await.unwrap_err();
    assert!(matches!(err, LoaderError::NoEditableLayer), "{err:?}");

    // Removing a row disposes it.
    loader
        .reconcile(vec![Layer::new("base", base())], None)
        .await
        .unwrap();
    assert!(loader.get("bad").is_none());
    assert_eq!(state(&loader, "c"), Some(FiberState::Active));
}

#[tokio::test]
async fn unresolved_then_reload() {
    let (harness, _store, _root, loader) = setup().await;
    loader
        .reconcile(
            vec![
                Layer::new("base", base()),
                Layer::new("user", patches(json!([{ "insert": [{ "id": "l", "name": "late", "config": { "label": "l" } }] }]))),
            ],
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        loader.get("l").unwrap().status,
        EntryStatus::Unresolved(LoaderError::NotFound { .. })
    ));
    harness.late.store(true, Ordering::SeqCst);
    let report = loader.reload("l").await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(state(&loader, "l"), Some(FiberState::Active));
}

#[tokio::test]
async fn groups_cascade() {
    let (harness, _store, _root, loader) = setup().await;
    harness.take_log();
    loader.set_disabled("g", true).await.unwrap();
    assert_eq!(harness.take_log(), vec!["cleanup:c1"]);
    assert!(matches!(
        loader.get("c").unwrap().status,
        EntryStatus::Inactive
    ));
    loader.set_disabled("g", false).await.unwrap();
    assert_eq!(harness.take_log(), vec!["apply:c1"]);
    assert_eq!(state(&loader, "c"), Some(FiberState::Active));
}

#[tokio::test]
async fn schema_comes_from_the_config_type() {
    let (_h, _store, _root, loader) = setup().await;
    let schema = loader.schema_of("echo").await.unwrap().unwrap();
    assert!(schema["properties"]["label"].is_object(), "{schema}");
    assert!(loader.schema_of("plain").await.unwrap().is_none());
    assert!(loader.get("a").unwrap().schema.is_some());
}

#[tokio::test]
async fn persist_failure_keeps_the_queue_until_flush() {
    let (_h, store, _root, loader) = setup().await;
    store.set_fail(true);
    let err = loader.set_disabled("a", true).await.unwrap_err();
    assert!(matches!(err, LoaderError::PersistFailed(_)), "{err:?}");
    // The edit is live and queued.
    assert!(matches!(
        loader.get("a").unwrap().status,
        EntryStatus::Disabled
    ));
    assert_eq!(loader.pending().len(), 1);
    store.set_fail(false);
    loader.flush().await.unwrap();
    assert!(loader.pending().is_empty());
    assert_eq!(
        store.patches(),
        patches(json!([{ "id": "a", "disabled": true }]))
    );
}

#[tokio::test]
async fn saved_layer_restores_the_same_tree() {
    let (_h, store, _root, loader) = setup().await;
    loader
        .create(new_entry("n", "echo", json!({ "label": "n" })), None, None)
        .await
        .unwrap();
    loader
        .create(
            new_entry("m", "echo", json!({ "label": "m" })),
            Some("g"),
            None,
        )
        .await
        .unwrap();
    loader.update("a", json!({ "label": "a2" })).await.unwrap();
    loader.set_disabled("c", true).await.unwrap();
    loader.rename_module("n", "echo2").await.unwrap();
    loader.move_to("n", Some("g"), Some(1)).await.unwrap();
    loader.remove("m").await.unwrap();
    let live: Vec<Value> = loader.entries().into_iter().map(|e| e.options).collect();

    let harness = Harness::new();
    let (_root2, fresh) = mount(harness.resolver(), store.clone()).await;
    reconcile(&fresh, base(), &store).await;
    let restored: Vec<Value> = fresh.entries().into_iter().map(|e| e.options).collect();
    assert_eq!(restored, live);
}

#[tokio::test]
async fn concurrent_writers_keep_every_edit() {
    let store = MemStore::default();
    let (h1, h2) = (Harness::new(), Harness::new());
    let (_r1, one) = mount(h1.resolver(), store.clone()).await;
    let (_r2, two) = mount(h2.resolver(), store.clone()).await;
    reconcile(&one, base(), &store).await;
    reconcile(&two, base(), &store).await;

    // A fails to save, another writer commits C, then B conflicts.
    store.set_fail(true);
    assert!(one.update("a", json!({ "label": "A" })).await.is_err());
    store.set_fail(false);
    two.set_disabled("c", true).await.unwrap();
    one.create(new_entry("b", "echo", json!({ "label": "B" })), None, None)
        .await
        .unwrap();

    assert!(one.pending().is_empty());
    let stored = apply_patches(&[
        Layer::new("base", base()),
        Layer::new("user", store.patches()),
    ]);
    let row = |id: &str| {
        stored
            .flat
            .iter()
            .find(|r| r.id.as_deref() == Some(id))
            .unwrap()
            .value
            .clone()
    };
    assert_eq!(row("a")["config"], json!({ "label": "A" }));
    assert_eq!(row("c")["disabled"], json!(true));
    assert_eq!(row("b")["config"], json!({ "label": "B" }));
    // `one` picked up C while replaying.
    assert!(matches!(
        one.get("c").unwrap().status,
        EntryStatus::Disabled
    ));
}

#[tokio::test]
async fn replay_drops_edits_that_no_longer_apply() {
    let store = MemStore::default();
    let (h1, h2) = (Harness::new(), Harness::new());
    let (r1, one) = mount(h1.resolver(), store.clone()).await;
    let (_r2, two) = mount(h2.resolver(), store.clone()).await;
    reconcile(&one, base(), &store).await;
    one.create(new_entry("x", "echo", json!({ "label": "x" })), None, None)
        .await
        .unwrap();
    reconcile(&two, base(), &store).await;

    let dropped = Dropped::default();
    r1.events()
        .on(&r1, &rutis::EventKey::of(), dropped.clone())
        .unwrap();

    store.set_fail(true);
    assert!(one.update("x", json!({ "label": "x2" })).await.is_err());
    store.set_fail(false);
    two.remove("x").await.unwrap();
    one.set_disabled("a", true).await.unwrap();

    assert!(one.pending().is_empty());
    assert!(one.get("x").is_none());
    assert_eq!(
        store.patches(),
        patches(json!([{ "id": "a", "disabled": true }]))
    );
    // Events are dispatched asynchronously; wait for the one we expect.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while dropped.0.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("PendingEditDropped was not emitted");
    assert_eq!(
        *dropped.0.lock().unwrap(),
        vec![rutis_loader::Edit::Update {
            id: "x".into(),
            config: json!({ "label": "x2" })
        }]
    );
}

#[tokio::test]
async fn concurrent_edits_do_not_deadlock() {
    let (_h, _store, _root, loader) = setup().await;
    loader
        .create(new_entry("n", "echo", json!({ "label": "n" })), None, None)
        .await
        .unwrap();
    let (one, two) = (loader.clone(), loader.clone());
    let (u, r) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            async move { one.update("n", json!({ "label": "n2" })).await },
            async move { two.remove("n").await },
        )
    })
    .await
    .expect("deadlock");
    // Edits are serialized: the update either ran first or found no row.
    assert!(r.is_ok());
    assert!(u.is_ok() || matches!(u, Err(LoaderError::UnknownEntry(_))));
    assert!(loader.get("n").is_none());
}

#[tokio::test]
async fn edits_after_shutdown_are_closed() {
    let (_h, _store, root, loader) = setup().await;
    root.shutdown().await.unwrap();
    let err = loader.set_disabled("a", true).await.unwrap_err();
    assert!(matches!(err, LoaderError::Closed), "{err:?}");
    let _ = Version::default();
}

#[derive(Clone, Default)]
struct Dropped(std::sync::Arc<std::sync::Mutex<Vec<rutis_loader::Edit>>>);

impl rutis::Listener<rutis_loader::PendingEditDropped> for Dropped {
    fn call<'a>(
        &'a self,
        _ctx: &'a rutis::Ctx,
        e: &'a rutis_loader::PendingEditDropped,
    ) -> rutis::BoxFuture<'a, Result<Option<()>, rutis::CordisError>> {
        self.0.lock().unwrap().push(e.edit.clone());
        Box::pin(async { Ok(None) })
    }
}
