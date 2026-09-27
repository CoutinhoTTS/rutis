use rutis::{BoxFuture, CordisError, Ctx, Effect, Event, FiberState, Plugin, TypeKey};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, Notify};

struct Ping;
impl Event for Ping {
    const NAME: &'static str = "ping";
    type Value = ();
}
fn listener<'a>(_: &'a Ctx, _: &'a Ping) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
    Box::pin(async { Ok(None) })
}

struct Probe {
    seen: Arc<Mutex<Vec<Ctx>>>,
    gate: Arc<Notify>,
    late: Arc<Mutex<Option<oneshot::Sender<CordisError>>>>,
    injects: Vec<TypeKey>,
}
impl Plugin for Probe {
    fn name(&self) -> &str {
        "generation-probe"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let old = ctx.clone();
            let first = {
                let mut seen = self.seen.lock().unwrap();
                seen.push(old.clone());
                seen.len() == 1
            };
            if first {
                let gate = self.gate.clone();
                let sender = self.late.lock().unwrap().take().unwrap();
                tokio::spawn(async move {
                    gate.notified().await;
                    let result = old.provide(77_u64).unwrap_err();
                    let _ = sender.send(result);
                });
            }
            Ok(Effect::Done)
        })
    }
}

async fn exercise() {
    let root = Ctx::root().unwrap();
    let provider = root.provide(1_u8).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(Notify::new());
    let (tx, rx) = oneshot::channel();
    let view = root.plugin(Probe {
        seen: seen.clone(),
        gate: gate.clone(),
        late: Arc::new(Mutex::new(Some(tx))),
        injects: vec![TypeKey::of::<u8>()],
    });
    (&view).await.unwrap();
    let first = seen.lock().unwrap()[0].clone();
    let isolated = first.isolate(TypeKey::of::<u64>(), "old-generation");
    provider.dispose().await.unwrap();
    assert_eq!(view.state().state, FiberState::Pending);
    assert!(matches!(
        first.effect(|| Effect::Done),
        Err(CordisError::InactiveGeneration { .. })
    ));
    assert!(matches!(
        first.provide(1_u64),
        Err(CordisError::InactiveGeneration { .. })
    ));

    let replacement = root.provide(2_u8).unwrap();
    (&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(seen.lock().unwrap().len(), 2);
    gate.notify_one();
    assert!(matches!(
        rx.await.unwrap(),
        CordisError::StaleGeneration { .. }
    ));
    assert!(matches!(
        first.effect(|| Effect::Done),
        Err(CordisError::StaleGeneration { .. })
    ));
    assert!(matches!(
        isolated.effect(|| Effect::Done),
        Err(CordisError::StaleGeneration { .. })
    ));
    assert!(isolated.cancellation_token().is_cancelled());
    assert!(matches!(
        first.events().on(&first, &rutis::EventKey::of(), listener),
        Err(CordisError::StaleGeneration { .. })
    ));
    let child = first.plugin(Noop);
    child.dispose().await.unwrap();
    assert_eq!(child.state().state, FiberState::Disposed);
    let current = seen.lock().unwrap()[1].clone();
    current.provide(55_u64).unwrap();
    replacement.dispose().await.unwrap();
    view.dispose().await.unwrap();
    root.shutdown().await.unwrap();
}

struct Noop;
impl Plugin for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    fn apply<'a>(&'a self, _: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

struct Fails {
    seen: Arc<Mutex<Option<Ctx>>>,
}
impl Plugin for Fails {
    fn name(&self) -> &str {
        "fails"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        *self.seen.lock().unwrap() = Some(ctx.clone());
        Box::pin(async { Err(CordisError::PluginFailed("intentional failure".into())) })
    }
}

#[tokio::test]
async fn rejects_registration_after_failed_apply() {
    let root = Ctx::root().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let view = root.plugin(Fails { seen: seen.clone() });
    assert!((&view).await.is_err());
    assert_eq!(view.state().state, FiberState::Failed);
    let old = seen.lock().unwrap().clone().unwrap();
    assert!(matches!(
        old.provide(1_u64),
        Err(CordisError::InactiveGeneration {
            state: FiberState::Failed,
            ..
        })
    ));
    view.dispose().await.unwrap();
    root.shutdown().await.unwrap();
}

#[test]
fn rejects_late_registration_current_thread() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise());
}

#[test]
fn rejects_late_registration_multi_thread() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise());
}
