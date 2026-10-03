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
