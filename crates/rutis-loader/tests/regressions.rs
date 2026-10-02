//! Regressions from the review of #96, one test per finding.

mod common;

use common::*;
use rutis::FiberState;
use rutis_loader::{
    apply_edit, apply_patches, Edit, Editable, EntryStatus, Layer, LoaderError, NewEntry, Version,
};
use serde_json::json;

/// Finding 1: Moving a group keeps the children another patch inserted into it.
#[test]
fn moving_a_group_keeps_separately_inserted_children() {
    let layers = vec![Layer::new(
        "user",
        patches(json!([
            { "insert": [{ "id": "g", "group": true, "config": [] }] },
            { "id": "g", "insert": [{ "id": "c", "name": "echo" }] },
            { "id": "c", "disabled": true },
            { "insert": [{ "id": "z", "name": "echo" }] }
        ])),
    )];
    let moved = apply_edit(
        &layers,
        0,
        &Edit::Move {
            id: "g".into(),
            parent: None,
            position: None,
        },
    )
    .unwrap();
    let composed = apply_patches(&[Layer::new("user", moved)]);
    assert!(composed.warnings.is_empty(), "{:?}", composed.warnings);
    let order: Vec<_> = composed.flat.iter().filter_map(|r| r.id.clone()).collect();
    assert_eq!(order, ["z", "g", "c"]);
    let c = composed
        .flat
        .iter()
        .find(|r| r.id.as_deref() == Some("c"))
        .unwrap();
    assert_eq!(c.value["disabled"], json!(true));
}

/// Finding 2: A respawned group keeps its new children registered even though the
/// old instance's cleanup runs after the new one started.
#[tokio::test]
async fn respawned_group_keeps_its_children_tracked() {
    let h = Harness::new();
    let (_root, loader) = mount(h.resolver(), MemStore::default()).await;
    let layers = |nested: bool| {
        let g = json!({ "id": "g", "group": true, "config": [
            { "id": "c", "name": "echo", "config": { "label": "c" } }
        ] });
        let rows = if nested {
            json!([{ "id": "h", "group": true, "config": [g] }])
        } else {
            json!([g, { "id": "h", "group": true, "config": [] }])
        };
        vec![Layer::new("user", patches(json!([{ "insert": rows }])))]
    };
    loader.reconcile(layers(false), None).await.unwrap();
    let old = loader.get("c").unwrap().plugin;
    loader.reconcile(layers(true), None).await.unwrap();
    assert_eq!(state(&loader, "c"), Some(FiberState::Active));
    assert_ne!(loader.get("c").unwrap().plugin, old);
    assert_eq!(state(&loader, "g"), Some(FiberState::Active));
}

/// Finding 3: An update the plugin rejects is a failure, and the loader reports the
/// config the plugin still runs.
#[tokio::test]
async fn rejected_external_update_is_reported() {
    let h = Harness::new();
    let (_root, loader) = mount(h.resolver(), MemStore::default()).await;
    let layers = |invalid: bool| {
        vec![Layer::new(
            "user",
            patches(json!([{ "insert": [
                { "id": "a", "name": "echo", "config": { "label": "a", "invalid": invalid } }
            ] }])),
        )]
    };
    loader.reconcile(layers(false), None).await.unwrap();
    let plugin = loader.get("a").unwrap().plugin;
    h.take_log();

    let report = loader.reconcile(layers(true), None).await.unwrap();
    assert_eq!(report.new_failures.len(), 1, "{report:?}");
    assert_eq!(report.new_failures[0].id, "a");
    let entry = loader.get("a").unwrap();
    assert!(matches!(entry.rejected, Some(LoaderError::Rejected { .. })));
    assert_eq!(entry.plugin, plugin);
    assert_eq!(state(&loader, "a"), Some(FiberState::Active));
    assert_eq!(
        loader.evaluated("a").unwrap().unwrap(),
        json!({ "label": "a", "invalid": false })
    );
    assert!(h.take_log().is_empty(), "the old instance kept running");

    // The same desired config is retried, still rejected, not new.
    let report = loader.reconcile(layers(true), None).await.unwrap();
    assert!(report.new_failures.is_empty(), "{report:?}");
    assert_eq!(report.failures.len(), 1);
    // A valid config clears it.
    let report = loader.reconcile(layers(false), None).await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert!(loader.get("a").unwrap().rejected.is_none());
}

/// Finding 4: A row loaded from the layers has its config validated before start.
#[tokio::test]
async fn first_load_validates_the_config() {
    let h = Harness::new();
    let (_root, loader) = mount(h.resolver(), MemStore::default()).await;
    let report = loader
        .reconcile(
            vec![Layer::new(
                "user",
                patches(json!([{ "insert": [
                    { "id": "a", "name": "echo", "config": { "label": "a", "invalid": true } }
                ] }])),
            )],
            None,
        )
        .await
        .unwrap();
    assert_eq!(report.new_failures.len(), 1, "{report:?}");
    assert!(matches!(
        loader.get("a").unwrap().status,
        EntryStatus::Unresolved(LoaderError::Rejected { .. })
    ));
    assert!(h.take_log().is_empty(), "nothing was applied");
}

/// Finding 5: An override patch whose name assertion fails is inert; an edit must
/// not merge into it.
#[test]
fn edits_skip_override_patches_with_a_failing_name_assertion() {
    let layers = vec![
        Layer::new(
            "base",
            patches(
                json!([{ "insert": [{ "id": "a", "name": "echo", "config": { "label": "old" } }] }]),
            ),
        ),
        Layer::new(
            "user",
            patches(json!([{ "id": "a", "name": "different", "config": { "label": "ignored" } }])),
        ),
    ];
    let updated = apply_edit(
        &layers,
        1,
        &Edit::Update {
            id: "a".into(),
            config: json!({ "label": "new" }),
        },
    )
    .unwrap();
    let composed = apply_patches(&[layers[0].clone(), Layer::new("user", updated.clone())]);
    assert_eq!(composed.rows[0]["config"], json!({ "label": "new" }));
    // The inert patch is left as the user wrote it.
    assert_eq!(updated[0], layers[1].patches[0]);

    // Owned rows: a mismatching override is not folded into the insert.
    let owned = vec![Layer::new(
        "user",
        patches(json!([
            { "insert": [{ "id": "a", "name": "echo", "config": { "label": "keep" } }] },
            { "id": "a", "name": "different", "disabled": true }
        ])),
    )];
    let updated = apply_edit(
        &owned,
        0,
        &Edit::Update {
            id: "a".into(),
            config: json!({ "label": "new" }),
        },
    )
    .unwrap();
    let composed = apply_patches(&[Layer::new("user", updated)]);
    assert_eq!(composed.rows[0]["config"], json!({ "label": "new" }));
    assert!(composed.rows[0].get("disabled").is_none());
}

/// Finding 6: After the host shut down, `flush` writes nothing.
#[tokio::test]
async fn flush_after_shutdown_is_closed() {
    let h = Harness::new();
    let store = MemStore::default();
    let (root, loader) = mount(h.resolver(), store.clone()).await;
    loader
        .reconcile(
            vec![Layer::new("user", vec![])],
            Some(Editable::new("user", store.version())),
        )
        .await
        .unwrap();
    store.set_fail(true);
    let created = loader
        .create(
            NewEntry {
                id: Some("a".into()),
                name: "echo".into(),
                ..NewEntry::default()
            },
            None,
            None,
        )
        .await;
    assert!(matches!(created.err(), Some(LoaderError::PersistFailed(_))));
    root.shutdown().await.unwrap();
    store.set_fail(false);
    assert!(matches!(loader.flush().await, Err(LoaderError::Closed)));
    assert!(store.patches().is_empty());
    let _ = Version::default();
}
