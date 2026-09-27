use std::future::IntoFuture;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{
    BoxFuture, CordisError, Ctx, DependencyStatus, Effect, Event, FiberState, Listener, Plugin,
    TypeKey,
};
use tokio::sync::{oneshot, Semaphore};

struct Capture(Arc<Mutex<Option<Ctx>>>);

struct PanicMetadata;
impl Plugin for PanicMetadata {
    fn name(&self) -> &str {
        panic!("closed fiber must not inspect plugin metadata")
    }
    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        unreachable!()
    }
}

impl Plugin for Capture {
    fn name(&self) -> &str {
        "capture"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(ctx.clone());
            Ok(Effect::Done)
        })
    }
}

async fn child(parent: &Ctx) -> (rutis::FiberView, Ctx) {
    let slot = Arc::new(Mutex::new(None));
    let view = parent.plugin(Capture(slot.clone()));
    (&view).await.unwrap();
    let ctx = slot.lock().unwrap().take().unwrap();
    (view, ctx)
}

struct Read {
    key: TypeKey,
    seen: Arc<Mutex<Vec<u64>>>,
}

struct ProbeRead(TypeKey);
impl Plugin for ProbeRead {
    fn name(&self) -> &str {
        "probe-read"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            assert!(ctx.get_as::<u64>(self.0.clone()).is_none());
            Ok(Effect::Done)
        })
    }
}

impl Plugin for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn injects(&self) -> &[TypeKey] {
        std::slice::from_ref(&self.key)
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let value = ctx.get_as::<u64>(self.key.clone()).unwrap();
            self.seen.lock().unwrap().push(*value);
            Ok(Effect::Done)
        })
    }
}

#[derive(Clone)]
struct Ping(u32);
impl Event for Ping {
    const NAME: &'static str = "instance-subtree-ping";
    type Value = ();
}

struct Record {
    log: Arc<Mutex<Vec<u32>>>,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Option<Arc<Semaphore>>,
}

impl Listener<Ping> for Record {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        event: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        let value = event.0;
        Box::pin(async move {
            if value == 1 {
                if let Some(tx) = self.entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                if let Some(gate) = &self.release {
                    let permit = gate.acquire().await.unwrap();
                    permit.forget();
                }
            }
            self.log.lock().unwrap().push(value);
            Ok(None)
        })
    }
}

struct Bail;
impl Listener<Ping> for Bail {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async { Ok(Some(())) })
    }
}

struct Fault;
impl Event for Fault {
    const NAME: &'static str = "instance-subtree-fault";
    type Value = ();
}
struct FailListener;
impl Listener<Fault> for FailListener {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Fault,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async { Err(CordisError::PluginFailed("listener".into())) })
    }
}

struct PanicCall;
impl Listener<Ping> for PanicCall {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        panic!("listener call failed before returning a future")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instance_emit_reports_synchronous_listener_panic() {
    let (tx, rx) = oneshot::channel();
    let sink = Arc::new(Mutex::new(Some(tx)));
    let root = Ctx::root_with_sink(tokio::runtime::Handle::current(), {
        let sink = sink.clone();
        Arc::new(move |error| {
            if let Some(tx) = sink.lock().unwrap().take() {
                let _ = tx.send(error);
            }
        })
    });
    let (view, ctx) = child(&root).await;
    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            PanicCall,
        )
        .unwrap();
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Arc::new(Ping(1)),
        )
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .unwrap()
        .unwrap();
    assert!(format!("{error:?}").contains("listener call failed"));
    view.shutdown().await.unwrap();
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instance_serial_short_circuits_and_parallel_aggregates() {
    let root = Ctx::root().unwrap();
    let (view, ctx) = child(&root).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    ctx.events()
        .on(&ctx, &rutis::EventKey::of().instance(ctx.instance()), Bail)
        .unwrap();
    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Record {
                log: log.clone(),
                entered: Mutex::new(None),
                release: None,
            },
        )
        .unwrap();
    let value = ctx
        .events()
        .serial(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            &Ping(2),
        )
        .await
        .unwrap();
    assert_eq!(value, Some(()));
    assert!(log.lock().unwrap().is_empty());

    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            FailListener,
        )
        .unwrap();
    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            FailListener,
        )
        .unwrap();
    let error = ctx
        .events()
        .parallel(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Arc::new(Fault),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CordisError::Aggregate { errors } if errors.len() == 2));
    view.shutdown().await.unwrap();
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instance_keys_enforce_subtree_and_explain_pending_dependency() {
    let root = Ctx::root().unwrap();
    let (_a, a) = child(&root).await;
    let (_b, b) = child(&root).await;
    let key_a = TypeKey::instance::<u64>(a.instance());
    let key_b = TypeKey::instance::<u64>(b.instance());
    let checks = Arc::new(AtomicUsize::new(0));
    let check_count = checks.clone();
    a.provide_as_with_check(key_a.clone(), Arc::new(10u64), move || {
        check_count.fetch_add(1, Ordering::SeqCst);
        true
    })
    .unwrap();
    b.provide_as(key_b.clone(), Arc::new(20u64)).unwrap();

    assert_eq!(*a.get_as::<u64>(key_a.clone()).unwrap(), 10);
    assert_eq!(*b.get_as::<u64>(key_b).unwrap(), 20);
    assert!(b.get_as::<u64>(key_a.clone()).is_none());
    assert!(root.get_as::<u64>(key_a.clone()).is_none());
    assert!(matches!(
        root.provide_as(key_a.clone(), Arc::new(99u64)),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    let (_nested, nested) = child(&a).await;
    assert_eq!(*nested.get_as::<u64>(key_a.clone()).unwrap(), 10);

    let pending = b.plugin(Read {
        key: key_a.clone(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    (&pending).await.unwrap();
    assert_eq!(pending.state().state, FiberState::Pending);
    let diag = root.diagnostics();
    let node = diag.plugins.iter().find(|p| p.id == pending.id).unwrap();
    assert_eq!(node.injects[0].status, DependencyStatus::OutOfScope);
    assert_ne!(node.instance, b.instance());
    assert_eq!(
        node.parent,
        root.diagnostics()
            .plugins
            .iter()
            .find(|p| p.instance == b.instance())
            .map(|p| p.id)
    );
    assert!(node.resolved_dependencies.is_empty());
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    let probe = b.plugin(ProbeRead(key_a.clone()));
    (&probe).await.unwrap();
    let diag = root.diagnostics();
    let access = &diag
        .plugins
        .iter()
        .find(|p| p.id == probe.id)
        .unwrap()
        .accesses[0];
    assert!(access.out_of_scope);
    assert!(access.provider.is_none());
    assert!(access.generation.is_none());
    root.shutdown().await.unwrap();
    assert_eq!(a.instance(), key_a.instance_id().unwrap());
    assert!(a.get_as::<u64>(key_a).is_none());
    let new_root = Ctx::root().unwrap();
    assert_ne!(new_root.instance(), root.instance());
    new_root.shutdown().await.unwrap();
}

#[tokio::test]
async fn registration_errors_prioritize_closed_then_inactive_then_instance_scope() {
    let root = Ctx::root().unwrap();
    let (disposed_view, disposed) = child(&root).await;
    let (closed_view, closed) = child(&root).await;
    let foreign = TypeKey::instance::<u64>(closed.instance());
    let id = closed.instance();

    assert!(matches!(
        root.provide_as(foreign.clone(), Arc::new(1u64)),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        root.events()
            .on(&root, &rutis::EventKey::of().instance(id), Bail),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        root.events().emit(
            &root,
            &rutis::EventKey::of().instance(id),
            Arc::new(Ping(1))
        ),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        root.events()
            .serial(&root, &rutis::EventKey::of().instance(id), &Ping(1))
            .await,
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        root.events()
            .parallel(
                &root,
                &rutis::EventKey::of().instance(id),
                Arc::new(Ping(1))
            )
            .await,
        Err(CordisError::InstanceOutOfScope { .. })
    ));

    disposed_view.dispose().await.unwrap();
    assert!(matches!(
        disposed.effect(|| Effect::Done),
        Err(CordisError::InactiveEffect)
    ));
    assert!(matches!(
        disposed.provide_as(foreign.clone(), Arc::new(1u64)),
        Err(CordisError::InactiveEffect)
    ));
    assert!(matches!(
        disposed
            .events()
            .on(&disposed, &rutis::EventKey::of().instance(id), Bail),
        Err(CordisError::InactiveEffect)
    ));
    assert!(matches!(
        disposed.events().emit(
            &disposed,
            &rutis::EventKey::of().instance(id),
            Arc::new(Ping(1))
        ),
        Err(CordisError::InactiveEffect)
    ));
    assert!(matches!(
        disposed
            .events()
            .serial(&disposed, &rutis::EventKey::of().instance(id), &Ping(1))
            .await,
        Err(CordisError::InactiveEffect)
    ));
    assert!(matches!(
        disposed
            .events()
            .parallel(
                &disposed,
                &rutis::EventKey::of().instance(id),
                Arc::new(Ping(1))
            )
            .await,
        Err(CordisError::InactiveEffect)
    ));

    closed_view.shutdown().await.unwrap();
    assert!(matches!(
        closed.effect(|| Effect::Done),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        closed.provide_as(foreign.clone(), Arc::new(1u64)),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        closed
            .events()
            .on(&closed, &rutis::EventKey::of().instance(id), Bail),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        closed.events().emit(
            &closed,
            &rutis::EventKey::of().instance(id),
            Arc::new(Ping(1))
        ),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        closed
            .events()
            .serial(&closed, &rutis::EventKey::of().instance(id), &Ping(1))
            .await,
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        closed
            .events()
            .parallel(
                &closed,
                &rutis::EventKey::of().instance(id),
                Arc::new(Ping(1))
            )
            .await,
        Err(CordisError::Closed)
    ));

    drop(closed_view);
    assert!(matches!(
        closed.effect(|| Effect::Done),
        Err(CordisError::Closed)
    ));
    root.shutdown().await.unwrap();
    assert!(matches!(
        root.effect(|| Effect::Done),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        root.provide_as(foreign.clone(), Arc::new(1u64)),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        root.events()
            .on(&root, &rutis::EventKey::of().instance(id), Bail),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        root.events().emit(
            &root,
            &rutis::EventKey::of().instance(id),
            Arc::new(Ping(1))
        ),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        root.events()
            .serial(&root, &rutis::EventKey::of().instance(id), &Ping(1))
            .await,
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        root.events()
            .parallel(
                &root,
                &rutis::EventKey::of().instance(id),
                Arc::new(Ping(1))
            )
            .await,
        Err(CordisError::Closed)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_service_reload_reaches_both_instances() {
    let root = Ctx::root().unwrap();
    let old = root.provide(7u64).unwrap();
    let (_a, a) = child(&root).await;
    let (_b, b) = child(&root).await;
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let read_a = a.plugin(Read {
        key: TypeKey::of::<u64>(),
        seen: seen_a.clone(),
    });
    let read_b = b.plugin(Read {
        key: TypeKey::of::<u64>(),
        seen: seen_b.clone(),
    });
    (&read_a).await.unwrap();
    (&read_b).await.unwrap();
    old.dispose().await.unwrap();
    root.provide(9u64).unwrap();
    (&read_a).await.unwrap();
    (&read_b).await.unwrap();
    assert_eq!(*seen_a.lock().unwrap(), vec![7, 9]);
    assert_eq!(*seen_b.lock().unwrap(), vec![7, 9]);
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn instance_service_reload_does_not_touch_sibling_consumer() {
    let root = Ctx::root().unwrap();
    let (_a_view, a) = child(&root).await;
    let (_b_view, b) = child(&root).await;
    let key_a = TypeKey::instance::<u64>(a.instance());
    let key_b = TypeKey::instance::<u64>(b.instance());
    let old_a = a.provide_as(key_a.clone(), Arc::new(1u64)).unwrap();
    b.provide_as(key_b.clone(), Arc::new(2u64)).unwrap();
    let seen_a = Arc::new(Mutex::new(Vec::new()));
    let seen_b = Arc::new(Mutex::new(Vec::new()));
    let read_a = a.plugin(Read {
        key: key_a.clone(),
        seen: seen_a.clone(),
    });
    let read_b = b.plugin(Read {
        key: key_b,
        seen: seen_b.clone(),
    });
    (&read_a).await.unwrap();
    (&read_b).await.unwrap();
    let b_generation = read_b.state().generation;

    old_a.dispose().await.unwrap();
    assert_eq!(read_a.state().state, FiberState::Pending);
    a.provide_as(key_a, Arc::new(3u64)).unwrap();
    (&read_a).await.unwrap();
    assert_eq!(*seen_a.lock().unwrap(), vec![1, 3]);
    assert_eq!(*seen_b.lock().unwrap(), vec![2]);
    assert_eq!(read_b.state().generation, b_generation);
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn fiber_instance_stays_stable_across_restart_update_and_dependency_reload() {
    let root = Ctx::root().unwrap();
    let (static_view, static_ctx) = child(&root).await;
    let static_id = static_ctx.instance();
    static_view.restart().await.unwrap();
    assert_eq!(
        root.diagnostics()
            .plugins
            .iter()
            .find(|plugin| plugin.id == static_view.id)
            .unwrap()
            .instance,
        static_id
    );

    let slot = Arc::new(Mutex::new(None));
    let factory_slot = slot.clone();
    let factory = root.plugin_from(
        move |_config: &u32| Ok(Box::new(Capture(factory_slot.clone())) as Box<dyn Plugin>),
        1u32,
    );
    (&factory).await.unwrap();
    let factory_id = slot.lock().unwrap().as_ref().unwrap().instance();
    factory.update(2u32).await.unwrap();
    assert_eq!(
        root.diagnostics()
            .plugins
            .iter()
            .find(|plugin| plugin.id == factory.id)
            .unwrap()
            .instance,
        factory_id
    );

    let old = root.provide(4u64).unwrap();
    let reader = root.plugin(Read {
        key: TypeKey::of::<u64>(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    (&reader).await.unwrap();
    let reader_id = root
        .diagnostics()
        .plugins
        .iter()
        .find(|plugin| plugin.id == reader.id)
        .unwrap()
        .instance;
    old.dispose().await.unwrap();
    root.provide(5u64).unwrap();
    (&reader).await.unwrap();
    assert_eq!(
        root.diagnostics()
            .plugins
            .iter()
            .find(|plugin| plugin.id == reader.id)
            .unwrap()
            .instance,
        reader_id
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_borrowed_serial_releases_shutdown_flight() {
    let root = Ctx::root().unwrap();
    let (view, ctx) = child(&root).await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let gate = Arc::new(Semaphore::new(0));
    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Record {
                log: Arc::new(Mutex::new(Vec::new())),
                entered: Mutex::new(Some(entered_tx)),
                release: Some(gate),
            },
        )
        .unwrap();
    let event = Ping(1);
    let mut dispatch = Box::pin(ctx.events().serial(
        &ctx,
        &rutis::EventKey::of().instance(ctx.instance()),
        &event,
    ));
    tokio::select! {
        result = &mut dispatch => panic!("serial completed before its gate: {result:?}"),
        result = entered_rx => result.unwrap(),
    }
    drop(dispatch);
    tokio::time::timeout(Duration::from_secs(2), view.shutdown())
        .await
        .unwrap()
        .unwrap();
    root.shutdown().await.unwrap();
}

struct StartOwnShutdown {
    view: rutis::FiberView,
    calls: Arc<AtomicUsize>,
}

impl Listener<Ping> for StartOwnShutdown {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            drop(self.view.shutdown());
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        })
    }
}

#[tokio::test]
async fn callback_can_start_own_shutdown_and_return() {
    let root = Ctx::root().unwrap();
    let (view, ctx) = child(&root).await;
    let calls = Arc::new(AtomicUsize::new(0));
    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            StartOwnShutdown {
                view: view.clone(),
                calls: calls.clone(),
            },
        )
        .unwrap();
    ctx.events()
        .serial(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            &Ping(1),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), view.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    root.shutdown().await.unwrap();
}

struct CountInjects {
    key: TypeKey,
    calls: Arc<AtomicUsize>,
}

impl Plugin for CountInjects {
    fn name(&self) -> &str {
        "count-injects"
    }

    fn injects(&self) -> &[TypeKey] {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::slice::from_ref(&self.key)
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_gate_reuses_registration_snapshot() {
    let root = Ctx::root().unwrap();
    root.provide(1u64).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let view = root.plugin(CountInjects {
        key: TypeKey::of::<u64>(),
        calls: calls.clone(),
    });
    (&view).await.unwrap();
    root.refresh();
    (&view).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    view.shutdown().await.unwrap();
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn instance_events_are_isolated_ordered_and_drained_by_shutdown() {
    let root = Ctx::root().unwrap();
    let (view_a, a) = child(&root).await;
    let (_view_b, b) = child(&root).await;
    let log_a = Arc::new(Mutex::new(Vec::new()));
    let log_b = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Semaphore::new(0));
    let (entered_tx, entered_rx) = oneshot::channel();
    a.events()
        .on(
            &a,
            &rutis::EventKey::of().instance(a.instance()),
            Record {
                log: log_a.clone(),
                entered: Mutex::new(Some(entered_tx)),
                release: Some(release.clone()),
            },
        )
        .unwrap();
    b.events()
        .on(
            &b,
            &rutis::EventKey::of().instance(b.instance()),
            Record {
                log: log_b.clone(),
                entered: Mutex::new(None),
                release: None,
            },
        )
        .unwrap();
    a.events()
        .emit(
            &a,
            &rutis::EventKey::of().instance(a.instance()),
            Arc::new(Ping(1)),
        )
        .unwrap();
    entered_rx.await.unwrap();
    a.events()
        .emit(
            &a,
            &rutis::EventKey::of().instance(a.instance()),
            Arc::new(Ping(3)),
        )
        .unwrap();
    b.events()
        .emit(
            &b,
            &rutis::EventKey::of().instance(b.instance()),
            Arc::new(Ping(2)),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while log_b.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(log_a.lock().unwrap().is_empty());

    let pending = view_a.shutdown();
    assert!(matches!(
        a.events().emit(
            &a,
            &rutis::EventKey::of().instance(a.instance()),
            Arc::new(Ping(4))
        ),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        a.provide_as(TypeKey::instance::<u32>(a.instance()), Arc::new(1u32)),
        Err(CordisError::Closed)
    ));
    assert!(matches!(
        a.events()
            .on(&a, &rutis::EventKey::of().instance(a.instance()), Bail),
        Err(CordisError::Closed)
    ));
    let closed_child = a.plugin(PanicMetadata);
    assert_eq!(closed_child.state().state, FiberState::Disposed);
    let mut waiter = tokio::spawn(pending);
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut waiter)
        .await
        .is_err());
    release.add_permits(1);
    waiter.await.unwrap().unwrap();
    assert_eq!(*log_a.lock().unwrap(), vec![1, 3]);
    assert_eq!(*log_b.lock().unwrap(), vec![2]);
    assert!(view_a.restart().await.is_err());
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_listener_is_excluded_from_new_ancestor_dispatches() {
    let root = Ctx::root().unwrap();
    let (_parent, parent) = child(&root).await;
    let (child_view, listener_ctx) = child(&parent).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Semaphore::new(0));
    let (entered_tx, entered_rx) = oneshot::channel();
    listener_ctx
        .events()
        .on(
            &listener_ctx,
            &rutis::EventKey::of().instance(parent.instance()),
            Record {
                log: log.clone(),
                entered: Mutex::new(Some(entered_tx)),
                release: Some(release.clone()),
            },
        )
        .unwrap();
    parent
        .events()
        .emit(
            &parent,
            &rutis::EventKey::of().instance(parent.instance()),
            Arc::new(Ping(1)),
        )
        .unwrap();
    entered_rx.await.unwrap();
    let closing = child_view.shutdown();
    parent
        .events()
        .emit(
            &parent,
            &rutis::EventKey::of().instance(parent.instance()),
            Arc::new(Ping(2)),
        )
        .unwrap();
    release.add_permits(1);
    closing.await.unwrap();
    assert_eq!(*log.lock().unwrap(), vec![1]);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_parallel_waiter_does_not_finish_dispatch_early() {
    let root = Ctx::root().unwrap();
    let (view, ctx) = child(&root).await;
    let release = Arc::new(Semaphore::new(0));
    let (entered_tx, entered_rx) = oneshot::channel();
    let log = Arc::new(Mutex::new(Vec::new()));
    ctx.events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Record {
                log: log.clone(),
                entered: Mutex::new(Some(entered_tx)),
                release: Some(release.clone()),
            },
        )
        .unwrap();
    let dispatch_ctx = ctx.clone();
    let waiting = tokio::spawn(async move {
        dispatch_ctx
            .events()
            .parallel(
                &dispatch_ctx,
                &rutis::EventKey::of().instance(dispatch_ctx.instance()),
                Arc::new(Ping(1)),
            )
            .await
    });
    entered_rx.await.unwrap();
    waiting.abort();
    let mut closing = tokio::spawn(view.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut closing)
            .await
            .is_err()
    );
    release.add_permits(1);
    closing.await.unwrap().unwrap();
    assert_eq!(*log.lock().unwrap(), vec![1]);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_instance_listener_disposal_waits_for_accepted_callback() {
    let root = Ctx::root().unwrap();
    let (view, ctx) = child(&root).await;
    let release = Arc::new(Semaphore::new(0));
    let (entered_tx, entered_rx) = oneshot::channel();
    let log = Arc::new(Mutex::new(Vec::new()));
    let listener = ctx
        .events()
        .on(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Record {
                log: log.clone(),
                entered: Mutex::new(Some(entered_tx)),
                release: Some(release.clone()),
            },
        )
        .unwrap();
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Arc::new(Ping(1)),
        )
        .unwrap();
    entered_rx.await.unwrap();

    let mut removing = tokio::spawn(listener.dispose());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut removing)
            .await
            .is_err()
    );
    release.add_permits(1);
    removing.await.unwrap().unwrap();
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::of().instance(ctx.instance()),
            Arc::new(Ping(2)),
        )
        .unwrap();
    assert_eq!(*log.lock().unwrap(), vec![1]);
    view.shutdown().await.unwrap();
    root.shutdown().await.unwrap();
}

struct FailCleanup;
impl Plugin for FailCleanup {
    fn name(&self) -> &str {
        "fail-cleanup"
    }
    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async {
            Ok(Effect::Disposer(Box::new(|| {
                Err(CordisError::PluginFailed("cleanup".into()))
            })))
        })
    }
}

struct FailApply;
impl Plugin for FailApply {
    fn name(&self) -> &str {
        "fail-apply"
    }
    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Err(CordisError::PluginFailed("apply".into())) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutting_down_failed_fiber_keeps_its_original_error() {
    let root = Ctx::root().unwrap();
    let view = root.plugin(FailApply);
    let original = (&view).await.unwrap_err();
    let closed = view.shutdown().await.unwrap_err();
    assert!(Arc::ptr_eq(&original, &closed));
    assert_eq!(view.state().state, FiberState::Disposed);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subtree_shutdown_aggregates_child_error_and_shares_result() {
    let root = Ctx::root().unwrap();
    let (parent, parent_ctx) = child(&root).await;
    let bad = parent_ctx.plugin(FailCleanup);
    (&bad).await.unwrap();
    let (one, two) = tokio::join!(parent.shutdown(), parent.shutdown());
    let one = one.unwrap_err();
    let two = two.unwrap_err();
    assert!(Arc::ptr_eq(&one, &two));
    assert_eq!(bad.state().state, FiberState::Disposed);
    assert!(parent.restart().await.is_err());
    root.shutdown().await.unwrap();
}

struct BlockCleanup {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Arc<Semaphore>,
}

impl Plugin for BlockCleanup {
    fn name(&self) -> &str {
        "block-cleanup"
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        let entered = self.entered.lock().unwrap().take().unwrap();
        let release = self.release.clone();
        Box::pin(async move {
            Ok(Effect::AsyncDisposer(Box::new(move || {
                Box::pin(async move {
                    let _ = entered.send(());
                    let permit = release.acquire().await.unwrap();
                    permit.forget();
                    Ok(())
                })
            })))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_queued_after_clean_shutdown_stays_successful() {
    let root = Ctx::root().unwrap();
    let (entered_tx, entered_rx) = oneshot::channel();
    let release = Arc::new(Semaphore::new(0));
    let view = root.plugin(BlockCleanup {
        entered: Mutex::new(Some(entered_tx)),
        release: release.clone(),
    });
    (&view).await.unwrap();
    let shutdown = view.shutdown();
    entered_rx.await.unwrap();
    let settle = (&view).into_future();
    release.add_permits(1);
    assert!(settle.await.is_ok());
    shutdown.await.unwrap();
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_parent_and_child_shutdown_join_without_cycle() {
    let root = Ctx::root().unwrap();
    let (parent, parent_ctx) = child(&root).await;
    let (nested, _nested_ctx) = child(&parent_ctx).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let p = parent.clone();
    let pbarrier = barrier.clone();
    let parent_waiter = tokio::spawn(async move {
        pbarrier.wait().await;
        p.shutdown().await
    });
    let c = nested.clone();
    let cbarrier = barrier.clone();
    let child_waiter = tokio::spawn(async move {
        cbarrier.wait().await;
        c.shutdown().await
    });
    barrier.wait().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        parent_waiter.await.unwrap().unwrap();
        child_waiter.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    assert_eq!(root.diagnostics().plugins.len(), 1);
    root.shutdown().await.unwrap();
}

struct DropProbe(Arc<AtomicUsize>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Plugin for DropProbe {
    fn name(&self) -> &str {
        "drop-probe"
    }
    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thousand_subtrees_leave_no_live_fibers() {
    let root = Ctx::root().unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    for n in 1..=1000 {
        let child = root.plugin(DropProbe(dropped.clone()));
        (&child).await.unwrap();
        if n == 1 {
            drop(child.shutdown());
        }
        child.shutdown().await.unwrap();
        drop(child);
        assert_eq!(dropped.load(Ordering::SeqCst), n);
    }
    assert_eq!(root.diagnostics().plugins.len(), 1);
    root.shutdown().await.unwrap();
}

struct SlowApply {
    started: Mutex<Option<oneshot::Sender<()>>>,
    cleaned: Arc<AtomicUsize>,
}

impl Plugin for SlowApply {
    fn name(&self) -> &str {
        "slow-apply"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            if let Some(tx) = self.started.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ctx.cancelled().await;
            let cleaned = self.cleaned.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    cleaned.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_keeps_cleanup_from_loading_apply() {
    let root = Ctx::root().unwrap();
    let (tx, rx) = oneshot::channel();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let view = root.plugin(SlowApply {
        started: Mutex::new(Some(tx)),
        cleaned: cleaned.clone(),
    });
    rx.await.unwrap();
    view.shutdown().await.unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(view.state().state, FiberState::Disposed);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispose_child_shutdown_and_parent_shutdown_converge() {
    for _ in 0..32 {
        let root = Ctx::root().unwrap();
        let (parent, ctx) = child(&root).await;
        let leaf = ctx.plugin(Capture(Arc::new(Mutex::new(None))));
        (&leaf).await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let dispose = tokio::spawn({
            let leaf = leaf.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                leaf.dispose().await
            }
        });
        let close_leaf = tokio::spawn({
            let leaf = leaf.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                leaf.shutdown().await
            }
        });
        let close_parent = tokio::spawn({
            let parent = parent.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                parent.shutdown().await
            }
        });
        barrier.wait().await;
        let (disposed, leaf_closed, parent_closed) =
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(dispose, close_leaf, close_parent)
            })
            .await
            .expect("concurrent shutdown deadlocked");
        disposed.unwrap().unwrap();
        leaf_closed.unwrap().unwrap();
        parent_closed.unwrap().unwrap();
        assert_eq!(leaf.state().state, FiberState::Disposed);
        assert_eq!(root.diagnostics().plugins.len(), 1);
        root.shutdown().await.unwrap();
    }
}

struct Supply(Arc<Mutex<Vec<&'static str>>>);
impl Plugin for Supply {
    fn name(&self) -> &str {
        "supply"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let log = self.0.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    log.lock().unwrap().push("provider");
                    Ok(())
                }))
            })?;
            ctx.provide(1u32)?;
            Ok(Effect::Done)
        })
    }
}

struct Use(Arc<Mutex<Vec<&'static str>>>);
impl Plugin for Use {
    fn name(&self) -> &str {
        "use"
    }
    fn injects(&self) -> &[TypeKey] {
        // A static declaration is sufficient for this unqualified service.
        static KEYS: std::sync::OnceLock<Vec<TypeKey>> = std::sync::OnceLock::new();
        KEYS.get_or_init(|| vec![TypeKey::of::<u32>()])
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            assert_eq!(*ctx.get::<u32>().unwrap(), 1);
            let log = self.0.clone();
            Ok(Effect::Disposer(Box::new(move || {
                log.lock().unwrap().push("consumer");
                Ok(())
            })))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_subtree_disposes_consumer_before_provider() {
    let root = Ctx::root().unwrap();
    let (parent, ctx) = child(&root).await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let consumer = ctx.plugin(Use(log.clone()));
    (&consumer).await.unwrap();
    assert_eq!(consumer.state().state, FiberState::Pending);
    let provider = ctx.plugin(Supply(log.clone()));
    (&provider).await.unwrap();
    (&consumer).await.unwrap();
    parent.shutdown().await.unwrap();
    assert_eq!(consumer.state().state, FiberState::Disposed);
    assert_eq!(provider.state().state, FiberState::Disposed);
    assert_eq!(*log.lock().unwrap(), vec!["consumer", "provider"]);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_outside_closed_subtree_can_reload_from_new_provider() {
    let root = Ctx::root().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let external = root.plugin(Use(log.clone()));
    (&external).await.unwrap();
    assert_eq!(external.state().state, FiberState::Pending);
    let (parent, ctx) = child(&root).await;
    let provider = ctx.plugin(Supply(log));
    (&provider).await.unwrap();
    (&external).await.unwrap();
    assert_eq!(external.state().state, FiberState::Active);
    parent.shutdown().await.unwrap();
    (&external).await.unwrap();
    assert_eq!(external.state().state, FiberState::Pending);
    root.provide(1u32).unwrap();
    (&external).await.unwrap();
    assert_eq!(external.state().state, FiberState::Active);
    root.shutdown().await.unwrap();
}
