//! P5: a plugin that disposes itself disables its row; volatile config
//! fields are applied in place.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rutis::{
    BoxFuture, CordisError, Ctx, Effect, EventKey, FiberState, Listener, Plugin, PluginFactory,
};
use rutis_loader::{
    volatile_key, Builtins, Editable, EntryStatus, Layer, LoaderChanged, LoaderOptions,
    LoaderPlugin, NewEntry, Resolved, Resolver, VolatileUpdate,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

type Log = Arc<Mutex<Vec<String>>>;

#[derive(Deserialize, JsonSchema)]
struct Tunable {
    #[serde(default)]
    name: String,
    /// Changed in place.
    #[serde(default)]
    #[schemars(extend("x-volatile" = true))]
    level: u32,
    /// Dispose itself during apply.
    #[serde(default)]
    quit: bool,
}

struct TunablePlugin {
    name: String,
    level: u32,
    quit: bool,
    log: Log,
}

struct Updates(Log);

impl Listener<VolatileUpdate> for Updates {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a VolatileUpdate,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        self.0
            .lock()
            .unwrap()
            .push(format!("volatile {:?} {}", e.paths, e.config["level"]));
        Box::pin(async { Ok(None) })
    }
}

impl Plugin for TunablePlugin {
    fn name(&self) -> &str {
        "tunable"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push(format!("apply {} {}", self.name, self.level));
            if self.quit {
                ctx.dispose_self()?;
            }
            ctx.events()
                .on(ctx, &volatile_key(ctx), Updates(self.log.clone()))?;
            Ok(Effect::Done)
        })
    }
}

struct TunableFactory(Log);

impl PluginFactory<Tunable> for TunableFactory {
    fn name(&self) -> &str {
        "tunable"
    }

    fn build(&self, config: &Tunable) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(TunablePlugin {
            name: config.name.clone(),
            level: config.level,
            quit: config.quit,
            log: self.0.clone(),
        }))
    }
}

struct Tunables(Builtins);

impl Resolver for Tunables {
    fn resolve<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Arc<Resolved>, rutis_loader::LoaderError>> {
        self.0.resolve(name)
    }
}

async fn setup(
    store: &MemStore,
) -> (
    Ctx,
    rutis_loader::Loader,
    Log,
    Arc<Mutex<Vec<LoaderChanged>>>,
) {
    let log = Log::default();
    let mut builtins = Builtins::new();
    builtins.register::<Tunable, _>("tunable", TunableFactory(log.clone()));
    let root = Ctx::root().unwrap();
    let plugin = LoaderPlugin::new(
        Tunables(builtins),
        LoaderOptions {
            persist: Arc::new(store.clone()),
            ..LoaderOptions::default()
        },
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    let changes = Arc::new(Mutex::new(Vec::new()));
    root.events()
        .on(
            &root,
            &EventKey::<LoaderChanged>::of(),
            Changes(changes.clone()),
        )
        .unwrap();
    (root, loader, log, changes)
}

struct Changes(Arc<Mutex<Vec<LoaderChanged>>>);

impl Listener<LoaderChanged> for Changes {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a LoaderChanged,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        self.0.lock().unwrap().push(e.clone());
        Box::pin(async { Ok(None) })
    }
}

async fn eventually(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition");
}

fn self_disposed(changes: &Arc<Mutex<Vec<LoaderChanged>>>) -> Option<(String, Option<String>)> {
    changes.lock().unwrap().iter().find_map(|c| match c {
        LoaderChanged::SelfDisposed { id, error } => Some((id.clone(), error.clone())),
        _ => None,
    })
}

#[tokio::test]
async fn a_plugin_that_disposes_itself_disables_its_row() {
    let store = MemStore::default();
    let (_root, loader, _log, changes) = setup(&store).await;
    loader
        .reconcile(
            vec![
                Layer::new("base", patches(json!([{ "insert": [{ "id": "lower", "name": "tunable", "config": { "quit": true } }] }]))),
                Layer::new("user", vec![]),
            ],
            Some(Editable::new("user", store.version())),
        )
        .await
        .unwrap();
    eventually(|| self_disposed(&changes).is_some()).await;
    assert_eq!(self_disposed(&changes), Some(("lower".into(), None)));
    eventually(|| matches!(loader.get("lower").unwrap().status, EntryStatus::Disabled)).await;
    assert_eq!(
        store.patches(),
        patches(json!([{ "id": "lower", "disabled": true }]))
    );
}

#[tokio::test]
async fn without_an_editable_layer_the_failure_is_reported() {
    let store = MemStore::default();
    let (_root, loader, _log, changes) = setup(&store).await;
    loader
        .reconcile(
            vec![Layer::new("base", patches(json!([{ "insert": [{ "id": "q", "name": "tunable", "config": { "quit": true } }] }])))],
            None,
        )
        .await
        .unwrap();
    eventually(|| self_disposed(&changes).is_some()).await;
    let (_, error) = self_disposed(&changes).unwrap();
    assert!(error.unwrap().contains("no editable layer"));
}

#[tokio::test]
async fn loader_disposals_are_not_self_disposals() {
    let store = MemStore::default();
    let (_root, loader, _log, changes) = setup(&store).await;
    reconcile(
        &loader,
        patches(json!([{ "insert": [{ "id": "t", "name": "tunable" }] }])),
        &store,
    )
    .await;
    loader.set_disabled("t", true).await.unwrap();
    loader.set_disabled("t", false).await.unwrap();
    loader
        .reconcile(vec![Layer::new("base", vec![])], None)
        .await
        .unwrap();
    // Monitors decide after the fibers are gone and report only a positive
    // finding; give them time to (wrongly) report one.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(self_disposed(&changes).is_none());
}

#[tokio::test]
async fn volatile_fields_change_in_place() {
    let store = MemStore::default();
    let (_root, loader, log, _changes) = setup(&store).await;
    reconcile(&loader, vec![], &store).await;
    loader
        .create(
            NewEntry {
                id: Some("t".into()),
                name: "tunable".into(),
                config: json!({ "name": "a", "level": 1 }),
                ..NewEntry::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    let view = loader.get("t").unwrap().view.unwrap();
    let generation = view.state().generation;
    log.lock().unwrap().clear();

    loader
        .update("t", json!({ "name": "a", "level": 2 }))
        .await
        .unwrap();
    eventually(|| !log.lock().unwrap().is_empty()).await;
    assert_eq!(*log.lock().unwrap(), ["volatile [[\"level\"]] 2"]);
    assert_eq!(view.state().generation, generation, "not restarted");
    assert_eq!(
        loader.evaluated("t").unwrap().unwrap(),
        json!({ "name": "a", "level": 2 })
    );
    assert!(store.patches()[0].insert.as_ref().unwrap()[0]["config"]["level"] == json!(2));

    // A restart builds from the stored config.
    log.lock().unwrap().clear();
    loader.restart("t").await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["apply a 2"]);

    // An ordinary field restarts it.
    log.lock().unwrap().clear();
    loader
        .update("t", json!({ "name": "b", "level": 2 }))
        .await
        .unwrap();
    assert_eq!(*log.lock().unwrap(), ["apply b 2"]);
    assert_eq!(view.state().state, FiberState::Active);
    let _: Value = json!(null);
}
