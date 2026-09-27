use std::future::IntoFuture;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, DispatchMode, Effect, Event, Listener, Plugin, TypeKey};
use tokio::sync::oneshot;

struct Ping(u32);

impl Event for Ping {
    const NAME: &'static str = "dispatch-observation-ping";
    type Value = u32;
}

struct Terminal;

impl rutis::Terminal<Ping> for Terminal {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
    ) -> BoxFuture<'a, Result<u32, CordisError>> {
        Box::pin(async { Ok(7) })
    }
}

struct Count(Arc<AtomicUsize>);

impl Listener<Ping> for Count {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u32>, CordisError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    }
}

struct Capture(Arc<Mutex<Option<Ctx>>>);

impl Plugin for Capture {
    fn name(&self) -> &str {
        "observation-child"
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
    (&view).into_future().await.unwrap();
    let ctx = slot.lock().unwrap().take().unwrap();
    (view, ctx)
}

#[tokio::test]
async fn old_context_still_notifies_non_instance_dispatch_observers() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let (view, old) = child(&root).await;
    tokio::time::timeout(Duration::from_secs(2), view.restart())
        .await
        .expect("restart timed out")
        .unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let observer = bus
        .observe_dispatch(&root, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    bus.emit(&old, &rutis::EventKey::of(), Arc::new(Ping(1)))
        .expect("default event dispatch");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(2), view.dispose())
        .await
        .expect("view dispose timed out")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), observer.dispose())
        .await
        .expect("observer dispose timed out")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), root.shutdown())
        .await
        .expect("root shutdown timed out")
        .unwrap();
}

#[tokio::test]
async fn all_modes_observe_before_listener_selection_even_when_empty() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_hook = seen.clone();
    let observer = bus
        .observe_dispatch(&root, move |attempt| {
            let value = attempt.event.downcast_ref::<Ping>().unwrap().0;
            seen_hook.lock().unwrap().push((
                attempt.mode,
                attempt.key.clone(),
                attempt.emitter,
                attempt.emitter_instance,
                value,
            ));
        })
        .unwrap();

    bus.emit(&root, &rutis::EventKey::of(), Arc::new(Ping(1)))
        .expect("default event dispatch");
    assert_eq!(seen.lock().unwrap().len(), 1); // no business listener or task

    let serial = bus.serial(&root, &rutis::EventKey::of(), &Ping(2));
    assert_eq!(seen.lock().unwrap().len(), 1); // future not polled
    assert_eq!(serial.await.unwrap(), None);
    assert_eq!(seen.lock().unwrap().len(), 2);

    let parallel = bus.parallel(&root, &rutis::EventKey::of(), Arc::new(Ping(3)));
    assert_eq!(seen.lock().unwrap().len(), 2);
    parallel.await.unwrap();

    let waterfall = bus.waterfall(&root, &rutis::EventKey::of(), &Ping(4), Terminal);
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert_eq!(waterfall.await.unwrap(), 7);

    bus.emit(
        &root,
        &rutis::EventKey::dynamic("dynamic"),
        Arc::new(Ping(5)),
    )
    .expect("default event dispatch");
    {
        let entries = seen.lock().unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries.iter().map(|e| e.0).collect::<Vec<_>>(),
            [
                DispatchMode::Emit,
                DispatchMode::Serial,
                DispatchMode::Parallel,
                DispatchMode::Waterfall,
                DispatchMode::Emit,
            ]
        );
        assert_eq!(entries[4].1, TypeKey::keyed_dynamic::<Ping>("dynamic"));
        assert!(entries[..4].iter().all(|e| e.1 == TypeKey::of::<Ping>()));
        assert!(entries.iter().all(|e| e.2 == root.root_view().unwrap().id));
        assert!(entries.iter().all(|e| e.3 == root.instance()));
        assert_eq!(
            entries.iter().map(|e| e.4).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
    }
    observer.dispose().await.unwrap();
}

#[tokio::test]
async fn observer_can_register_listener_before_snapshot() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let count = Arc::new(AtomicUsize::new(0));
    let bus_hook = bus.clone();
    let root_hook = root.clone();
    let count_hook = count.clone();
    bus.observe_dispatch(&root, move |attempt| {
        if attempt.event.downcast_ref::<Ping>().unwrap().0 == 9 {
            bus_hook
                .on::<Ping>(
                    &root_hook,
                    &rutis::EventKey::of(),
                    Count(count_hook.clone()),
                )
                .unwrap();
        }
    })
    .unwrap();
    bus.serial(&root, &rutis::EventKey::of(), &Ping(9))
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn observer_scope_and_instance_admission_are_isolated() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let (a_view, a) = child(&root).await;
    let (_b_view, b) = child(&root).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    for (label, owner) in [("root", &root), ("a", &a), ("b", &b)] {
        let seen_hook = seen.clone();
        bus.observe_dispatch(owner, move |attempt| {
            seen_hook
                .lock()
                .unwrap()
                .push((label, attempt.event.downcast_ref::<Ping>().unwrap().0));
        })
        .unwrap();
    }
    bus.emit(&a, &rutis::EventKey::of(), Arc::new(Ping(1)))
        .expect("default event dispatch");
    bus.emit(&b, &rutis::EventKey::of(), Arc::new(Ping(2)))
        .expect("default event dispatch");
    bus.emit(
        &a,
        &rutis::EventKey::of().instance(a.instance()),
        Arc::new(Ping(3)),
    )
    .unwrap();
    assert!(matches!(
        bus.emit(
            &b,
            &rutis::EventKey::of().instance(a.instance()),
            Arc::new(Ping(4))
        ),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [
            ("root", 1),
            ("a", 1),
            ("root", 2),
            ("b", 2),
            ("root", 3),
            ("a", 3),
        ]
    );
    a_view.shutdown().await.unwrap();
    bus.emit(&b, &rutis::EventKey::of(), Arc::new(Ping(5)))
        .expect("default event dispatch");
    assert_eq!(seen.lock().unwrap().last(), Some(&("b", 5)));
}

#[tokio::test]
async fn observer_panic_and_sink_panic_do_not_stop_dispatch() {
    let failures = Arc::new(AtomicUsize::new(0));
    let failures_sink = failures.clone();
    let root = Ctx::root_with_sink(
        tokio::runtime::Handle::current(),
        Arc::new(move |_| {
            failures_sink.fetch_add(1, Ordering::SeqCst);
            panic!("sink panic");
        }),
    );
    let bus = root.events().clone();
    bus.observe_dispatch(&root, |_| panic!("observer panic"))
        .unwrap();
    let reached = Arc::new(AtomicUsize::new(0));
    bus.on(&root, &rutis::EventKey::of(), Count(reached.clone()))
        .unwrap();
    bus.serial(&root, &rutis::EventKey::of(), &Ping(1))
        .await
        .unwrap();
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    assert_eq!(reached.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_selected_synchronous_observer() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let (view, ctx) = child(&root).await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release = Arc::new(Mutex::new(release_rx));
    let entered_hook = entered.clone();
    let release_hook = release.clone();
    bus.observe_dispatch(&ctx, move |_| {
        if let Some(tx) = entered_hook.lock().unwrap().take() {
            tx.send(()).unwrap();
        }
        release_hook.lock().unwrap().recv().unwrap();
    })
    .unwrap();
    let bus_emit = bus.clone();
    let ctx_emit = ctx.clone();
    let emit = tokio::task::spawn_blocking(move || {
        bus_emit.emit(
            &ctx_emit,
            &rutis::EventKey::of().instance(ctx_emit.instance()),
            Arc::new(Ping(1)),
        )
    });
    entered_rx.await.unwrap();
    let shutdown = view.shutdown();
    tokio::pin!(shutdown);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut shutdown)
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    assert!(matches!(emit.await.unwrap(), Err(CordisError::Closed)));
    shutdown.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_disposal_waits_for_in_flight_observer_and_prevents_new_calls() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered_tx)));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release = Arc::new(Mutex::new(release_rx));
    let hits = Arc::new(AtomicUsize::new(0));
    let entered_hook = entered.clone();
    let release_hook = release.clone();
    let hits_hook = hits.clone();
    let observer = bus
        .observe_dispatch(&root, move |_| {
            hits_hook.fetch_add(1, Ordering::SeqCst);
            if let Some(tx) = entered_hook.lock().unwrap().take() {
                tx.send(()).unwrap();
            }
            release_hook.lock().unwrap().recv().unwrap();
        })
        .unwrap();
    let bus_emit = bus.clone();
    let root_emit = root.clone();
    let emit = tokio::task::spawn_blocking(move || {
        bus_emit
            .emit(&root_emit, &rutis::EventKey::of(), Arc::new(Ping(1)))
            .expect("default event dispatch")
    });
    entered_rx.await.unwrap();
    let disposal = observer.dispose();
    tokio::pin!(disposal);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut disposal)
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    emit.await.unwrap();
    disposal.await.unwrap();
    bus.emit(&root, &rutis::EventKey::of(), Arc::new(Ping(2)))
        .expect("default event dispatch");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn observer_reentry_observes_nested_dispatch_and_business_listener_runs() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let inner_hits = Arc::new(AtomicUsize::new(0));

    // Register a business listener; it will be taken by the nested emit inside
    // the observer. The outer serial will find no hooks left.
    bus.on(&root, &rutis::EventKey::of(), Count(inner_hits.clone()))
        .unwrap();

    {
        let attempts = attempts.clone();
        let bus_obs = bus.clone();
        let root_obs = root.clone();
        bus.observe_dispatch(&root, move |attempt| {
            let value = attempt.event.downcast_ref::<Ping>().unwrap().0;
            attempts.lock().unwrap().push(value);
            if value == 1 {
                bus_obs
                    .emit(&root_obs, &rutis::EventKey::of(), Arc::new(Ping(2)))
                    .expect("default event dispatch");
                // Nested emit takes hooks and spawns a tail task synchronously
                // before returning. The outer serial will see empty hooks.
            }
        })
        .unwrap();
    }

    // Use serial for the outer dispatch; the observer runs during observation,
    // then serial finds no hooks and returns immediately.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        bus.serial(&root, &rutis::EventKey::of(), &Ping(1)),
    )
    .await
    .expect("outer serial did not deadlock");
    assert!(result.unwrap().is_none(), "outer serial had no hooks left");

    // Both outer (1) and nested (2) dispatch attempts were observed.
    assert_eq!(*attempts.lock().unwrap(), vec![1, 2]);

    // The inner emit's tail task runs the business listener asynchronously.
    // Yield cooperatively until it completes.
    for _ in 0..100 {
        if inner_hits.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(inner_hits.load(Ordering::SeqCst), 1);
}
