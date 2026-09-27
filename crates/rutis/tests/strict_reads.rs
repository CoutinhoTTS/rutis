use std::future::IntoFuture;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{
    BoxFuture, CordisError, Ctx, DependencyStatus, Effect, Event, FiberState, Listener, Plugin,
    ServiceReadFailure, TypeKey,
};

struct Capture {
    injects: Vec<TypeKey>,
    slot: Arc<Mutex<Option<Ctx>>>,
}

impl Plugin for Capture {
    fn name(&self) -> &str {
        "strict-read-capture"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            *self.slot.lock().unwrap() = Some(ctx.clone());
            Ok(Effect::Done)
        })
    }
}

async fn child(parent: &Ctx, injects: Vec<TypeKey>) -> (rutis::FiberView, Ctx) {
    let slot = Arc::new(Mutex::new(None));
    let view = parent.plugin(Capture {
        injects,
        slot: slot.clone(),
    });
    (&view).into_future().await.unwrap();
    let ctx = slot.lock().unwrap().take().unwrap();
    (view, ctx)
}

struct SelfProvider;

impl Plugin for SelfProvider {
    fn name(&self) -> &str {
        "strict-read-self-provider"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(7u32)?;
            assert_eq!(*ctx.require::<u32>()?, 7);
            let cleanup_ctx = ctx.clone();
            Ok(Effect::Disposer(Box::new(move || {
                assert_eq!(*cleanup_ctx.require::<u32>()?, 7);
                Ok(())
            })))
        })
    }
}

struct Pulse;
impl Event for Pulse {
    const NAME: &'static str = "strict-read-pulse";
    type Value = ();
}

struct CallbackRead(Arc<Mutex<Vec<ServiceReadFailure>>>);
impl Listener<Pulse> for CallbackRead {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        _event: &'a Pulse,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(ctx.require::<u64>().unwrap_err().reason);
            Ok(None)
        })
    }
}

#[tokio::test]
async fn strict_reads_distinguish_declaration_readiness_and_inactive_context() {
    let root = Ctx::root().unwrap();
    let key = TypeKey::of::<u64>();
    let provider = root.provide(12u64).unwrap();
    let (declared_view, declared) = child(&root, vec![key.clone()]).await;
    let (plain_view, plain) = child(&root, vec![]).await;

    assert_eq!(*declared.require::<u64>().unwrap(), 12);
    assert_eq!(*plain.get::<u64>().unwrap(), 12);
    let line = line!() + 1;
    let error = plain.require::<u64>().unwrap_err();
    assert_eq!(error.reason, ServiceReadFailure::Undeclared);
    assert_eq!(error.location.line(), line);
    assert!(error.location.file().ends_with("strict_reads.rs"));
    assert_eq!(error.key, key);
    assert_eq!(error.plugin_id, plain_view.id);
    assert_eq!(error.instance, plain.instance());

    let accesses = &root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|plugin| plugin.id == plain_view.id)
        .unwrap()
        .accesses;
    assert!(accesses.iter().any(|access| {
        access.strict
            && !access.declared
            && access.failure == Some(ServiceReadFailure::Undeclared)
            && access.provider.is_none()
    }));

    provider.dispose().await.unwrap();
    assert_eq!(declared_view.state().state, FiberState::Pending);
    assert_eq!(declared.get::<u64>(), None);
    let unavailable_line = line!() + 1;
    let unavailable = declared.require::<u64>().unwrap_err();
    assert_eq!(unavailable.location.line(), unavailable_line);
    assert_eq!(
        unavailable.reason,
        ServiceReadFailure::Unavailable(DependencyStatus::Missing)
    );
    let access = root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|plugin| plugin.id == declared_view.id)
        .unwrap()
        .accesses
        .into_iter()
        .find(|access| access.failure.is_some())
        .unwrap();
    assert!(access.declared);
    assert_eq!(
        access.failure,
        Some(ServiceReadFailure::Unavailable(DependencyStatus::Missing))
    );

    plain_view.shutdown().await.unwrap();
    assert_eq!(
        plain.require::<u64>().unwrap_err().reason,
        ServiceReadFailure::Inactive
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn self_and_ancestor_declarations_respect_isolate_scope() {
    let root = Ctx::root().unwrap();
    root.provide(3u64).unwrap();
    let provider = root.plugin(SelfProvider);
    (&provider).await.unwrap();
    let key = TypeKey::of::<u64>();
    let (_parent_view, parent) = child(&root, vec![key.clone()]).await;
    let (_child_view, descendant) = child(&parent, vec![]).await;
    assert_eq!(*descendant.require::<u64>().unwrap(), 3);
    let isolate = parent.isolate(key.clone(), "other");
    let (_scoped_view, scoped) = child(&isolate, vec![]).await;
    assert_eq!(
        scoped.require::<u64>().unwrap_err().reason,
        ServiceReadFailure::Undeclared
    );
    assert!(scoped.get::<u64>().is_none());
    let access = root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|plugin| plugin.instance == descendant.instance())
        .unwrap()
        .accesses
        .into_iter()
        .find(|access| access.strict)
        .unwrap();
    assert!(access.declared);
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn instance_boundaries_hide_provider_and_old_ids() {
    let root = Ctx::root().unwrap();
    let (a_view, a) = child(&root, vec![]).await;
    let (_b_view, b) = child(&root, vec![]).await;
    let key_a = TypeKey::instance::<u64>(a.instance());
    let key_b = TypeKey::instance::<u64>(b.instance());
    a.provide_as(key_a.clone(), Arc::new(10u64)).unwrap();
    b.provide_as(key_b.clone(), Arc::new(20u64)).unwrap();
    assert_eq!(*a.require_as::<u64>(key_a.clone()).unwrap(), 10);
    assert_eq!(*b.require_as::<u64>(key_b).unwrap(), 20);
    for ctx in [&b, &root] {
        let error = ctx.require_as::<u64>(key_a.clone()).unwrap_err();
        assert_eq!(error.reason, ServiceReadFailure::OutOfScope);
        assert!(ctx.get_as::<u64>(key_a.clone()).is_none());
    }
    let foreign_root = Ctx::root().unwrap();
    assert_eq!(
        foreign_root
            .require_as::<u64>(key_a.clone())
            .unwrap_err()
            .reason,
        ServiceReadFailure::OutOfScope
    );
    a_view.shutdown().await.unwrap();
    assert_eq!(
        b.require_as::<u64>(key_a.clone()).unwrap_err().reason,
        ServiceReadFailure::OutOfScope
    );
    let access = root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|plugin| plugin.instance == b.instance())
        .unwrap()
        .accesses
        .into_iter()
        .find(|access| access.failure == Some(ServiceReadFailure::OutOfScope))
        .unwrap();
    assert!(access.provider.is_none());
    assert!(access.generation.is_none());
    foreign_root.shutdown().await.unwrap();
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn denied_reads_do_not_run_checks_or_change_bindings_and_callbacks_are_checked() {
    let root = Ctx::root().unwrap();
    let checks = Arc::new(AtomicUsize::new(0));
    let count = checks.clone();
    root.provide_as_with_check(TypeKey::of::<u64>(), Arc::new(5u64), move || {
        count.fetch_add(1, Ordering::SeqCst);
        true
    })
    .unwrap();
    let (view, ctx) = child(&root, vec![]).await;
    let before = root.diagnostics().bindings;
    assert_eq!(
        ctx.require::<u64>().unwrap_err().reason,
        ServiceReadFailure::Undeclared
    );
    assert_eq!(*ctx.get::<u64>().unwrap(), 5);
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert_eq!(root.diagnostics().bindings.len(), before.len());
    assert_eq!(
        root.diagnostics().bindings[0].generation,
        before[0].generation
    );

    let reasons = Arc::new(Mutex::new(Vec::new()));
    ctx.events()
        .on(&ctx, &rutis::EventKey::of(), CallbackRead(reasons.clone()))
        .unwrap();
    ctx.events()
        .serial(&ctx, &rutis::EventKey::of(), &Pulse)
        .await
        .unwrap();
    assert_eq!(
        *reasons.lock().unwrap(),
        vec![ServiceReadFailure::Undeclared]
    );
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    assert!(root
        .diagnostics()
        .plugins
        .into_iter()
        .find(|plugin| plugin.id == view.id)
        .unwrap()
        .accesses
        .iter()
        .any(|access| access.strict && access.failure == Some(ServiceReadFailure::Undeclared)));
    root.shutdown().await.unwrap();
}
