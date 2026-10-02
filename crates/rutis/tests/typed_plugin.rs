//! Typed dependencies (#50): declared once as a type, passed to `apply`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, TypeKey, Typed, TypedPlugin,
};

#[derive(Debug)]
struct Llm(u32);
#[derive(Debug)]
struct Logger;
#[derive(Debug)]
struct Ready;

async fn soon<F: std::future::IntoFuture>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .expect("timed out")
}

async fn reach(view: &FiberView, state: FiberState) {
    let mut rx = view.watch();
    soon(rx.wait_for(|snapshot| snapshot.state == state))
        .await
        .expect("fiber dropped");
}

/// What each generation of `Chat` received.
type Seen = Arc<Mutex<Vec<(u32, bool)>>>;

struct Chat {
    seen: Seen,
    gates: Vec<TypeKey>,
}

impl TypedPlugin for Chat {
    type Deps = (Arc<Llm>, Option<Arc<Logger>>);

    fn name(&self) -> &str {
        "chat"
    }

    fn gates(&self) -> Vec<TypeKey> {
        self.gates.clone()
    }

    fn apply<'a>(
        &'a self,
        _ctx: &'a Ctx,
        (llm, logger): Self::Deps,
    ) -> BoxFuture<'a, Result<Effect, CordisError>> {
        self.seen.lock().unwrap().push((llm.0, logger.is_some()));
        Box::pin(async { Ok(Effect::Done) })
    }
}

fn chat(gates: Vec<TypeKey>) -> (Typed<Chat>, Seen) {
    let seen = Seen::default();
    let plugin = Typed::new(Chat {
        seen: seen.clone(),
        gates,
    });
    (plugin, seen)
}

#[tokio::test]
async fn required_dependencies_gate_and_arrive_as_arguments() {
    let ctx = Ctx::root().unwrap();
    let (plugin, seen) = chat(vec![]);
    // Only the required dependency gates; the optional one does not.
    assert_eq!(plugin.injects(), [TypeKey::of::<Llm>()]);
    let view = ctx.plugin(plugin);
    soon(&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Pending);

    ctx.provide(Llm(1)).unwrap();
    soon(&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(*seen.lock().unwrap(), [(1, false)]);
}

#[tokio::test]
async fn an_optional_dependency_is_passed_when_present() {
    let ctx = Ctx::root().unwrap();
    ctx.provide(Llm(1)).unwrap();
    ctx.provide(Logger).unwrap();
    let (plugin, seen) = chat(vec![]);
    let view = ctx.plugin(plugin);
    soon(&view).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), [(1, true)]);
}

#[tokio::test]
async fn gate_only_keys_hold_the_plugin_without_being_passed() {
    let ctx = Ctx::root().unwrap();
    ctx.provide(Llm(1)).unwrap();
    let (plugin, seen) = chat(vec![TypeKey::of::<Ready>(), TypeKey::of::<Llm>()]);
    // A gate key already among the dependencies is not repeated.
    assert_eq!(
        plugin.injects(),
        [TypeKey::of::<Llm>(), TypeKey::of::<Ready>()]
    );
    let view = ctx.plugin(plugin);
    soon(&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Pending);
    ctx.provide(Ready).unwrap();
    soon(&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(*seen.lock().unwrap(), [(1, false)]);
}

#[tokio::test]
async fn losing_a_required_dependency_evicts_and_the_next_one_is_passed() {
    let ctx = Ctx::root().unwrap();
    let first = ctx.provide(Llm(1)).unwrap();
    let (plugin, seen) = chat(vec![]);
    let view = ctx.plugin(plugin);
    soon(&view).await.unwrap();

    first.dispose().await.unwrap();
    reach(&view, FiberState::Pending).await;
    ctx.provide(Llm(2)).unwrap();
    reach(&view, FiberState::Active).await;
    assert_eq!(*seen.lock().unwrap(), [(1, false), (2, false)]);
}

/// Provides `Llm` to whoever asks, typed or not.
struct Provider;

impl TypedPlugin for Provider {
    type Deps = ();

    fn name(&self) -> &str {
        "provider"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, (): ()) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(Llm(7))?;
            Ok(Effect::Done)
        })
    }
}

/// An untyped consumer of `Llm`.
struct Untyped {
    got: Arc<Mutex<Option<u32>>>,
    injects: Vec<TypeKey>,
}

impl Plugin for Untyped {
    fn name(&self) -> &str {
        "untyped"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            *self.got.lock().unwrap() = Some(ctx.require::<Llm>()?.0);
            Ok(Effect::Done)
        })
    }
}

#[tokio::test]
async fn typed_and_untyped_plugins_depend_on_each_other() {
    let ctx = Ctx::root().unwrap();
    let got = Arc::new(Mutex::new(None));
    let untyped = ctx.plugin(Untyped {
        got: got.clone(),
        injects: vec![TypeKey::of::<Llm>()],
    });
    let (plugin, seen) = chat(vec![]);
    let typed = ctx.plugin(plugin);
    let provider = ctx.plugin(Typed::new(Provider));
    soon(&provider).await.unwrap();
    reach(&untyped, FiberState::Active).await;
    reach(&typed, FiberState::Active).await;
    assert_eq!(*got.lock().unwrap(), Some(7));
    assert_eq!(*seen.lock().unwrap(), [(7, false)]);

    // The provider going away evicts both kinds of consumer.
    provider.dispose().await.unwrap();
    reach(&untyped, FiberState::Pending).await;
    reach(&typed, FiberState::Pending).await;
}
