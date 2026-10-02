//! Overlay layers and all-or-nothing reload.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use rutis::{BoxFuture, FiberState};
use rutis_loader::{
    Builtins, Editable, EntryStatus, Layer, LoaderError, LoaderOptions, Resolved, Resolver,
};
use serde_json::json;

#[tokio::test]
async fn overlays_survive_reconcile_and_stay_out_of_storage() {
    let h = Harness::new();
    let store = MemStore::default();
    let (_root, loader) = mount(h.resolver(), store.clone()).await;
    reconcile(
        &loader,
        patches(json!([{ "insert": [{ "id": "a", "name": "echo" }] }])),
        &store,
    )
    .await;

    let report = loader
        .set_overlay(
            "dev",
            Some(patches(
                json!([{ "insert": [{ "id": "d", "name": "echo", "config": { "label": "d" } }] }]),
            )),
        )
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(state(&loader, "d"), Some(FiberState::Active));

    // The application reconciles its own layers: the overlay stays.
    reconcile(
        &loader,
        patches(json!([{ "insert": [{ "id": "a", "name": "echo" }] }])),
        &store,
    )
    .await;
    assert_eq!(state(&loader, "d"), Some(FiberState::Active));
    assert_eq!(loader.overlays().len(), 1);

    // Overlay rows are not the user's to edit, and nothing reaches storage.
    let err = loader.set_disabled("d", true).await.unwrap_err();
    assert!(matches!(err, LoaderError::NotOwned { .. }), "{err:?}");
    assert!(store.patches().is_empty());
    assert_eq!(
        loader.editable(),
        Some(Editable::new("user", store.version()))
    );

    // Overlays cannot shadow application layers; removing one disposes its rows.
    assert!(loader.set_overlay("user", Some(vec![])).await.is_err());
    loader.set_overlay("dev", None).await.unwrap();
    assert!(loader.get("d").is_none());
    assert!(loader.overlays().is_empty());
}

/// Each resolve of `mod` serves the next scripted version.
struct Versions {
    versions: Vec<Result<Arc<Resolved>, LoaderError>>,
    next: AtomicUsize,
}

impl Resolver for Versions {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        Box::pin(async move {
            if name != "mod" {
                return Err(LoaderError::NotFound { name: name.into() });
            }
            let i = self
                .next
                .fetch_add(1, Ordering::SeqCst)
                .min(self.versions.len() - 1);
            self.versions[i].clone()
        })
    }
}

/// A fresh `Resolved` for the harness module `kind` (`echo` or `flaky`).
async fn module(h: &Harness, kind: &str) -> Arc<Resolved> {
    h.resolver().resolve(kind).await.unwrap()
}

#[tokio::test]
async fn reload_is_all_or_nothing() {
    let h = Harness::new();
    let v1 = module(&h, "echo").await;
    let v_bad_apply = module(&h, "flaky").await;
    let v_good = {
        let mut b = Builtins::new();
        // Same factory name and injects as echo: an in-place swap.
        b.register_raw("mod", PlainEcho, None);
        b.resolve("mod").await.unwrap()
    };
    let resolver = Versions {
        versions: vec![
            Ok(v1.clone()),
            Err(LoaderError::Resolve {
                name: "mod".into(),
                message: "build broken".into(),
            }),
            Ok(v_bad_apply),
            Ok(v_good.clone()),
        ],
        next: AtomicUsize::new(0),
    };
    let (_root, loader) = mount_with_resolver(resolver).await;
    loader
        .reconcile(
            vec![Layer::new("rows", patches(json!([{ "insert": [{ "id": "m", "name": "mod", "config": { "label": "m" } }] }])))],
            None,
        )
        .await
        .unwrap();
    let plugin = loader.get("m").unwrap().plugin;
    assert_eq!(state(&loader, "m"), Some(FiberState::Active));

    // The new version does not resolve: the running one stays.
    let err = loader.reload("m").await.unwrap_err();
    assert!(matches!(err, LoaderError::Resolve { .. }), "{err:?}");
    assert_eq!(loader.get("m").unwrap().plugin, plugin);
    assert_eq!(state(&loader, "m"), Some(FiberState::Active));

    // It resolves but fails in apply: rolled back to the previous module.
    h.broken.store(true, Ordering::SeqCst);
    let err = loader.reload("m").await.unwrap_err();
    assert!(matches!(err, LoaderError::ApplyFailed { .. }), "{err:?}");
    assert_eq!(state(&loader, "m"), Some(FiberState::Active));
    h.broken.store(false, Ordering::SeqCst);

    // A good one swaps in.
    let report = loader.reload("m").await.unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(state(&loader, "m"), Some(FiberState::Active));
    assert!(matches!(
        loader.get("m").unwrap().status,
        EntryStatus::Running(_)
    ));
}

struct PlainEcho;

impl rutis::PluginFactory<serde_json::Value> for PlainEcho {
    fn name(&self) -> &str {
        "echo"
    }

    fn build(
        &self,
        _config: &serde_json::Value,
    ) -> Result<Box<dyn rutis::Plugin>, rutis::CordisError> {
        Ok(Box::new(Quiet))
    }
}

struct Quiet;

impl rutis::Plugin for Quiet {
    fn name(&self) -> &str {
        "quiet"
    }

    fn apply<'a>(
        &'a self,
        _ctx: &'a rutis::Ctx,
    ) -> BoxFuture<'a, Result<rutis::Effect, rutis::CordisError>> {
        Box::pin(async { Ok(rutis::Effect::Done) })
    }
}

async fn mount_with_resolver(resolver: impl Resolver) -> (rutis::Ctx, rutis_loader::Loader) {
    let root = rutis::Ctx::root().unwrap();
    let plugin = rutis_loader::LoaderPlugin::new(resolver, LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    (root, loader)
}
