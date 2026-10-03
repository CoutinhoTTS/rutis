//! Small host-facing additions: boxed plugins, `Ctx::view`, `ServiceChanged`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, EventKey, Listener, Plugin, PluginId, ServiceChange,
    ServiceChanged, TypeKey,
};

struct Named(&'static str);

impl Plugin for Named {
    fn name(&self) -> &str {
        self.0
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

struct Provides(u32);

impl Plugin for Provides {
    fn name(&self) -> &str {
        "provides"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(self.0)?;
            Ok(Effect::Done)
        })
    }
}

/// Mounts a child plugin from inside apply.
struct Parent;

impl Plugin for Parent {
    fn name(&self) -> &str {
        "parent"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.plugin(Named("child"));
            Ok(Effect::Done)
        })
    }
}

#[tokio::test]
async fn boxed_plugins_mount() {
    let root = Ctx::root().unwrap();
    let chosen: Vec<Box<dyn Plugin>> = vec![Box::new(Named("a")), Box::new(Provides(7))];
    let mut views = Vec::new();
    for plugin in chosen {
        views.push(root.plugin(plugin));
    }
    for view in &views {
        view.await.unwrap();
    }
    assert_eq!(views[0].name(), "a");
    assert_eq!(*root.get::<u32>().unwrap(), 7);
}

#[tokio::test]
async fn view_finds_a_fiber_by_id() {
    let root = Ctx::root().unwrap();
    let parent = root.plugin(Parent);
    (&parent).await.unwrap();
    let child = root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|p| p.name == "child")
        .unwrap()
        .id;
    let view = root.view(child).unwrap();
    assert_eq!(view.name(), "child");
    assert_eq!(root.view(parent.id).unwrap().id, parent.id);
    assert!(root.view(PluginId(u64::MAX)).is_none());

    parent.dispose().await.unwrap();
    drop(view);
    assert!(root.view(child).is_none());
}

#[derive(Clone, Default)]
struct Changes(Arc<Mutex<Vec<(TypeKey, PluginId, ServiceChange)>>>);

impl Listener<ServiceChanged> for Changes {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a ServiceChanged,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        self.0
            .lock()
            .unwrap()
            .push((e.key.clone(), e.provider, e.change));
        Box::pin(async { Ok(None) })
    }
}

async fn wait_for(changes: &Changes, len: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while changes.0.lock().unwrap().len() < len {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ServiceChanged not emitted");
}

#[tokio::test]
async fn service_changes_are_announced() {
    let root = Ctx::root().unwrap();
    let changes = Changes::default();
    root.events()
        .on(&root, &EventKey::of(), changes.clone())
        .unwrap();

    let provider = root.plugin(Provides(1));
    (&provider).await.unwrap();
    wait_for(&changes, 1).await;
    provider.dispose().await.unwrap();
    wait_for(&changes, 2).await;

    let seen = changes.0.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (TypeKey::of::<u32>(), provider.id, ServiceChange::Provided),
            (TypeKey::of::<u32>(), provider.id, ServiceChange::Removed),
        ]
    );
}

/// Disposes itself during apply when told to.
struct Quitter;

impl Plugin for Quitter {
    fn name(&self) -> &str {
        "quitter"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.dispose_self()?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test]
async fn a_plugin_can_dispose_itself() {
    let root = Ctx::root().unwrap();
    let sibling = root.plugin(Named("sibling"));
    let quitter = root.plugin(Quitter);
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut watch = quitter.watch();
        while watch.borrow().state != rutis::FiberState::Disposed {
            watch.changed().await.unwrap();
        }
    })
    .await
    .expect("disposed");
    (&sibling).await.unwrap();
    assert_eq!(sibling.state().state, rutis::FiberState::Active);
    assert!(root.dispose_self().is_err(), "the root shuts down instead");
}

/// Keeps the context of every generation it applies in.
struct Keeper(Arc<Mutex<Vec<Ctx>>>);

impl Plugin for Keeper {
    fn name(&self) -> &str {
        "keeper"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        self.0.lock().unwrap().push(ctx.clone());
        Box::pin(async { Ok(Effect::Done) })
    }
}

async fn settled(view: &rutis::FiberView, state: rutis::FiberState) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut watch = view.watch();
        while watch.borrow().state != state {
            watch.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never reached {state:?}: {:?}", view.state()));
}

#[tokio::test]
async fn a_stale_context_cannot_dispose_the_next_generation() {
    let root = Ctx::root().unwrap();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let view = root.plugin(Keeper(kept.clone()));
    (&view).await.unwrap();
    let first = kept.lock().unwrap()[0].clone();
    view.restart().await.unwrap();
    assert!(matches!(
        first.dispose_self(),
        Err(CordisError::InactiveEffect)
    ));
    assert_eq!(view.state().state, rutis::FiberState::Active);

    // The current generation still can.
    let second = kept.lock().unwrap()[1].clone();
    second.dispose_self().unwrap();
    settled(&view, rutis::FiberState::Disposed).await;
}

#[tokio::test]
async fn dispose_self_during_a_restart_is_refused() {
    let root = Ctx::root().unwrap();
    let kept = Arc::new(Mutex::new(Vec::new()));
    let view = root.plugin(Keeper(kept.clone()));
    (&view).await.unwrap();
    let first = kept.lock().unwrap()[0].clone();
    let generation = view.state().generation;
    // The restart has cancelled the generation and is queued, not done.
    let restart = tokio::spawn(view.restart());
    tokio::task::yield_now().await;
    assert!(matches!(
        first.dispose_self(),
        Err(CordisError::InactiveEffect)
    ));
    restart.await.unwrap().unwrap();
    assert_eq!(view.state().state, rutis::FiberState::Active);
    assert!(view.state().generation > generation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispose_self_racing_a_restart_is_all_or_nothing() {
    for _ in 0..200 {
        let root = Ctx::root().unwrap();
        let kept = Arc::new(Mutex::new(Vec::new()));
        let view = root.plugin(Keeper(kept.clone()));
        (&view).await.unwrap();
        let first = kept.lock().unwrap()[0].clone();
        let restart = tokio::spawn(view.restart());
        let disposed = tokio::task::spawn_blocking(move || first.dispose_self())
            .await
            .unwrap();
        let _ = restart.await.unwrap();
        // Accepted: the fiber ends disposed. Refused: the restart's
        // generation runs on.
        let expected = match disposed {
            Ok(()) => rutis::FiberState::Disposed,
            Err(CordisError::InactiveEffect) => rutis::FiberState::Active,
            Err(other) => panic!("{other:?}"),
        };
        settled(&view, expected).await;
        root.shutdown().await.unwrap();
    }
}

/// Records each build's config.
struct Recording(Arc<Mutex<Vec<u32>>>);

impl rutis::PluginFactory<u32> for Recording {
    fn validate_config(&self, config: &u32) -> Result<(), CordisError> {
        if *config == 0 {
            return Err(CordisError::Validation {
                issues: vec!["zero".into()],
            });
        }
        Ok(())
    }

    fn build(&self, config: &u32) -> Result<Box<dyn Plugin>, CordisError> {
        self.0.lock().unwrap().push(*config);
        Ok(Box::new(Named("recorded")))
    }
}

#[tokio::test]
async fn set_config_stores_without_restarting() {
    let root = Ctx::root().unwrap();
    let builds = Arc::new(Mutex::new(Vec::new()));
    let view = root.plugin_with(Recording(builds.clone()), 1u32);
    (&view).await.unwrap();
    let generation = view.state().generation;

    view.set_config(2u32).unwrap();
    assert_eq!(*view.current_config::<u32>().unwrap(), 2);
    assert_eq!(view.state().generation, generation, "no restart");
    assert!(view.set_config(0u32).is_err(), "validated");
    assert!(view.set_config("wrong type").is_err());
    assert_eq!(*view.current_config::<u32>().unwrap(), 2);

    // The next load builds from it.
    view.restart().await.unwrap();
    assert_eq!(*builds.lock().unwrap(), [1, 2]);
    assert_ne!(view.instance(), root.instance());
}
