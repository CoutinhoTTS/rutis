//! Typed dependencies (#50): declared once as a type, passed to `apply`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{
    BoxFuture, CordisError, Ctx, Deps, Effect, FiberState, FiberView, Plugin, TypeKey, Typed,
    TypedPlugin,
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

/// Loads once its `Llm` is ready; the first generation waits in `apply`
/// until the dependency is withdrawn, then reads it as a typed plugin does.
struct LosesItsDependency {
    generations: Arc<std::sync::atomic::AtomicUsize>,
    seen: Seen,
    injects: Vec<TypeKey>,
}

impl Plugin for LosesItsDependency {
    fn name(&self) -> &str {
        "loses-its-dependency"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let generation = self
                .generations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if generation == 0 {
                // Withdrawing the provider cancels this generation.
                ctx.cancelled().await;
            }
            let (llm,) = <(Arc<Llm>,) as Deps>::resolve(ctx)?;
            self.seen.lock().unwrap().push((llm.0, false));
            Ok(Effect::Done)
        })
    }
}

#[tokio::test]
async fn a_dependency_lost_before_it_is_read_returns_the_plugin_to_pending() {
    let ctx = Ctx::root().unwrap();
    let first = ctx.provide(Llm(1)).unwrap();
    let seen = Seen::default();
    let view = ctx.plugin(LosesItsDependency {
        generations: Default::default(),
        seen: seen.clone(),
        injects: vec![TypeKey::of::<Llm>()],
    });
    reach(&view, FiberState::Loading).await;

    // The gate was open; the dependency goes before apply reads it.
    soon(first.dispose()).await.unwrap();
    reach(&view, FiberState::Pending).await;
    assert!(view.state().error.is_none());
    assert!(seen.lock().unwrap().is_empty());

    // It loads again when the dependency comes back.
    ctx.provide(Llm(2)).unwrap();
    reach(&view, FiberState::Active).await;
    assert_eq!(*seen.lock().unwrap(), [(2, false)]);
}

/// Claims a lost dependency while every dependency is present.
struct FalseClaim(Vec<TypeKey>);

impl Plugin for FalseClaim {
    fn name(&self) -> &str {
        "false-claim"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.0
    }

    fn apply<'a>(&'a self, _: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Err(CordisError::InjectUnsatisfied(vec!["Llm".into()])) })
    }
}

#[tokio::test]
async fn the_claim_is_a_plain_failure_while_dependencies_are_present() {
    let ctx = Ctx::root().unwrap();
    ctx.provide(Llm(1)).unwrap();
    let view = ctx.plugin(FalseClaim(vec![TypeKey::of::<Llm>()]));
    let error = soon(&view).await.expect_err("fails instead of looping");
    assert!(matches!(*error, CordisError::InjectUnsatisfied(_)));
    assert_eq!(view.state().state, FiberState::Failed);
}
