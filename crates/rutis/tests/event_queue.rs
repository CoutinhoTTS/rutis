//! The same original plugin API runs against native and external queue storage.
//! This queue is an in-process test adapter, not the Cordis transport.
use std::any::TypeId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, Event, EventKey, EventOptions, EventPattern, EventQueue,
    EventRegistration, Listener, ListenerKind, Next, PatternListener, Plugin, SyncEvent, SyncNext,
    Terminal, TypeKey, WaterfallListener,
};
use tokio::sync::{oneshot, Notify};

struct Ping(u64);
impl Event for Ping {
    const NAME: &'static str = "ping";
    type Value = u64;
}
impl SyncEvent for Ping {}
struct Local;
impl Event for Local {
    const NAME: &'static str = "ping";
    type Value = u64;
}

#[derive(Default)]
struct Queue {
    entries: Arc<Mutex<Vec<EventRegistration>>>,
    fail: AtomicBool,
    duplicate: AtomicBool,
    cleanup_signal: Option<Arc<Notify>>,
}
impl EventQueue for Queue {
    fn handles(&self, event: TypeId) -> bool {
        event == TypeId::of::<Ping>()
    }
    fn register(&self, registration: EventRegistration) -> Result<Effect, CordisError> {
        let id = registration.subscription().id;
        let mut entries = self.entries.lock().unwrap();
        if registration.subscription().prepend {
            entries.insert(0, registration);
        } else {
            entries.push(registration);
        }
        let entries = self.entries.clone();
        let signal = self.cleanup_signal.clone();
        Ok(Effect::Disposer(Box::new(move || {
            entries
                .lock()
                .unwrap()
                .retain(|entry| entry.subscription().id != id);
            if let Some(signal) = signal {
                signal.notify_one();
            }
            Ok(())
        })))
    }
    fn select(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        kind: ListenerKind,
    ) -> Result<Vec<EventRegistration>, CordisError> {
        // Reenter a bus read: adapter code must not hold framework locks.
        let _ = ctx.events().subscriptions();
        if self.fail.load(Ordering::SeqCst) {
            return Err(CordisError::PluginFailed("queue offline".into()));
        }
        let mut entries: Vec<_> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.matches(key, kind))
            .cloned()
            .collect();
        if self.duplicate.load(Ordering::SeqCst) {
            entries.extend(entries.clone());
        }
        Ok(entries)
    }
}
fn root(queue: Option<Arc<Queue>>) -> Ctx {
    match queue {
        Some(queue) => Ctx::root_with_event_queue(
            tokio::runtime::Handle::current(),
            Arc::new(|e| panic!("{e}")),
            queue,
        ),
        None => Ctx::root().unwrap(),
    }
}
type Log = Arc<Mutex<Vec<u64>>>;
struct Record {
    id: u64,
    log: Log,
    value: Option<u64>,
}
impl Listener<Ping> for Record {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        e: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(self.id + e.0);
            Ok(self.value)
        })
    }
}
impl PatternListener<Ping> for Record {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        _: EventKey<Ping>,
        e: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Listener::call(self, ctx, e)
    }
}
fn record(id: u64, log: &Log, value: Option<u64>) -> Record {
    Record {
        id,
        log: log.clone(),
        value,
    }
}
struct Add(u64);
impl WaterfallListener<Ping> for Add {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Ping,
        next: Next<'a, Ping>,
    ) -> BoxFuture<'a, Result<u64, CordisError>> {
        Box::pin(async move { Ok(next.call().await? + self.0) })
    }
}
struct End;
impl Terminal<Ping> for End {
    fn call<'a>(&'a self, _: &'a Ctx, event: &'a Ping) -> BoxFuture<'a, Result<u64, CordisError>> {
        Box::pin(async move { Ok(event.0) })
    }
}

#[tokio::test]
async fn serial_waterfall_and_sync_modes_match_native() {
    for queued in [false, true] {
        let queue = Arc::new(Queue::default());
        let ctx = root(queued.then(|| queue.clone()));
        let log = Log::default();
        let key = EventKey::dynamic("flow/run");
        ctx.events().on(&ctx, &key, record(1, &log, None)).unwrap();
        ctx.events()
            .on_opt(
                &ctx,
                &key,
                record(2, &log, None),
                EventOptions {
                    prepend: true,
                    once: false,
                },
            )
            .unwrap();
        ctx.events()
            .once(&ctx, &key, record(3, &log, Some(0)))
            .unwrap();
        ctx.events()
            .on(&ctx, &key, record(4, &log, Some(4)))
            .unwrap();
        assert_eq!(
            ctx.events().serial(&ctx, &key, &Ping(0)).await.unwrap(),
            Some(0)
        );
        assert_eq!(
            ctx.events().serial(&ctx, &key, &Ping(0)).await.unwrap(),
            Some(4)
        );
        assert_eq!(*log.lock().unwrap(), [2, 1, 3, 2, 1, 4]);
        ctx.events().on_waterfall(&ctx, &key, Add(10)).unwrap();
        ctx.events().on_waterfall(&ctx, &key, Add(20)).unwrap();
        assert_eq!(
            ctx.events()
                .waterfall(&ctx, &key, &Ping(5), End)
                .await
                .unwrap(),
            35
        );
        ctx.events()
            .on_sync(&ctx, &key, |_: &Ctx, _: &Ping| Ok(Some(0)))
            .unwrap();
        ctx.events()
            .on_sync(&ctx, &key, |_: &Ctx, _: &Ping| panic!("must short circuit"))
            .unwrap();
        assert_eq!(
            ctx.events().bail_sync(&ctx, &key, &Ping(0)).unwrap(),
            Some(0)
        );
        ctx.events()
            .on_waterfall_sync(&ctx, &key, |_: &Ctx, _: &Ping, next: SyncNext<'_, Ping>| {
                Ok(next.call()? + 10)
            })
            .unwrap();
        let borrowed = std::rc::Rc::new(5);
        assert_eq!(
            ctx.events()
                .waterfall_sync(&ctx, &key, &Ping(0), |_, _| Ok(*borrowed))
                .unwrap(),
            15
        );
        ctx.shutdown().await.unwrap();
        assert!(queue.entries.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn emit_preserves_per_key_queue_and_once_snapshot() {
    struct Emit {
        log: Log,
        done: Arc<Notify>,
    }
    impl Listener<Ping> for Emit {
        fn call<'a>(
            &'a self,
            _: &'a Ctx,
            e: &'a Ping,
        ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.log.lock().unwrap().push(e.0);
                self.done.notify_one();
                Ok(None)
            })
        }
    }
    for queued in [false, true] {
        let ctx = root(queued.then(|| Arc::new(Queue::default())));
        let log = Log::default();
        let done = Arc::new(Notify::new());
        let key = EventKey::of();
        ctx.events()
            .on(
                &ctx,
                &key,
                Emit {
                    log: log.clone(),
                    done: done.clone(),
                },
            )
            .unwrap();
        ctx.events()
            .once(&ctx, &key, record(100, &log, None))
            .unwrap();
        for n in 0..20 {
            ctx.events().emit(&ctx, &key, Arc::new(Ping(n))).unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while log.lock().unwrap().len() != 21 {
                done.notified().await;
            }
        })
        .await
        .unwrap();
        let expected = [vec![0, 100], (1..20).collect()].concat();
        assert_eq!(*log.lock().unwrap(), expected);
        ctx.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn parallel_starts_every_listener_and_waits_for_all_errors() {
    struct Parallel(Arc<tokio::sync::Barrier>, &'static str);
    impl Listener<Ping> for Parallel {
        fn call<'a>(
            &'a self,
            _: &'a Ctx,
            _: &'a Ping,
        ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
            Box::pin(async move {
                self.0.wait().await;
                Err(CordisError::PluginFailed(self.1.into()))
            })
        }
    }
    for queued in [false, true] {
        let ctx = root(queued.then(|| Arc::new(Queue::default())));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        for name in ["first failure", "second failure"] {
            ctx.events()
                .on(&ctx, &EventKey::of(), Parallel(barrier.clone(), name))
                .unwrap();
        }
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            ctx.events()
                .parallel(&ctx, &EventKey::of(), Arc::new(Ping(0))),
        )
        .await
        .unwrap()
        .unwrap_err();
        let text = format!("{error:?}");
        assert!(text.contains("first failure") && text.contains("second failure"));
        ctx.shutdown().await.unwrap();
    }
}

struct Capture(Mutex<Option<oneshot::Sender<Ctx>>>);
impl Plugin for Capture {
    fn name(&self) -> &str {
        "original-plugin"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            assert!(self
                .0
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(ctx.clone())
                .is_ok());
            Ok(Effect::Done)
        })
    }
}
async fn child(root: &Ctx) -> (rutis::FiberView, Ctx) {
    let (tx, rx) = oneshot::channel();
    let fiber = root.plugin(Capture(Mutex::new(Some(tx))));
    (&fiber).await.unwrap();
    (fiber, rx.await.unwrap())
}

#[tokio::test]
async fn patterns_instances_and_unhandled_types_remain_separate() {
    for queued in [false, true] {
        let queue = Arc::new(Queue::default());
        let ctx = root(queued.then(|| queue.clone()));
        let log = Log::default();
        let (fiber, owner) = child(&ctx).await;
        let instance = EventKey::of().instance(owner.instance());
        owner
            .events()
            .on(&owner, &instance, record(1, &log, None))
            .unwrap();
        ctx.events()
            .on_pattern(&ctx, EventPattern::prefix("path/"), record(2, &log, None))
            .unwrap();
        ctx.events()
            .serial(&ctx, &EventKey::dynamic("path/a"), &Ping(0))
            .await
            .unwrap();
        ctx.events()
            .serial(&ctx, &EventKey::dynamic("other/a"), &Ping(0))
            .await
            .unwrap();
        owner
            .events()
            .serial(&owner, &instance, &Ping(0))
            .await
            .unwrap();
        assert!(matches!(
            ctx.events().serial(&ctx, &instance, &Ping(0)).await,
            Err(CordisError::InstanceOutOfScope { .. })
        ));
        assert_eq!(*log.lock().unwrap(), [2, 1]);
        let metrics = ctx
            .events()
            .subscriptions()
            .into_iter()
            .find(|s| !s.prefixes.is_empty())
            .unwrap();
        assert_eq!((metrics.selected, metrics.invoked), (Some(1), Some(1)));
        struct LocalListener;
        impl Listener<Local> for LocalListener {
            fn call<'a>(
                &'a self,
                _: &'a Ctx,
                _: &'a Local,
            ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
                Box::pin(async { Ok(Some(42)) })
            }
        }
        ctx.events()
            .on(&ctx, &EventKey::of(), LocalListener)
            .unwrap();
        queue.fail.store(true, Ordering::SeqCst);
        assert_eq!(
            ctx.events()
                .serial(&ctx, &EventKey::of(), &Local)
                .await
                .unwrap(),
            Some(42)
        );
        fiber.dispose().await.unwrap();
        assert!(owner
            .events()
            .serial(&owner, &instance, &Ping(0))
            .await
            .is_err());
        ctx.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_snapshot_does_not_consume_once_and_retained_registration_cannot_revive() {
    let queue = Arc::new(Queue::default());
    let ctx = root(Some(queue.clone()));
    let log = Log::default();
    let key = EventKey::of();
    let disposer = ctx
        .events()
        .once(&ctx, &key, record(1, &log, Some(1)))
        .unwrap();
    queue.duplicate.store(true, Ordering::SeqCst);
    assert!(ctx.events().serial(&ctx, &key, &Ping(0)).await.is_err());
    queue.duplicate.store(false, Ordering::SeqCst);
    queue.fail.store(true, Ordering::SeqCst);
    assert!(ctx.events().serial(&ctx, &key, &Ping(0)).await.is_err());
    queue.fail.store(false, Ordering::SeqCst);
    let registration = queue.entries.lock().unwrap()[0].clone();
    let erased = registration.subscription().key.clone();
    let mut listener = registration.select(&ctx, &erased).unwrap().unwrap();
    assert_eq!(
        *listener
            .call(&Ping(0))
            .await
            .unwrap()
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        1
    );
    assert!(listener.call(&Ping(0)).await.is_err());
    assert_eq!(
        ctx.events().serial(&ctx, &key, &Ping(0)).await.unwrap(),
        None
    );
    disposer.dispose().await.unwrap();
    assert!(registration.select(&ctx, &erased).unwrap().is_none());
    assert_eq!(*log.lock().unwrap(), [1]);
    let other = root(Some(Arc::new(Queue::default())));
    assert!(registration.select(&other, &erased).is_err());
    other.shutdown().await.unwrap();
    ctx.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_removal_starts_external_cleanup_before_draining_pattern() {
    struct Waiting {
        started: Arc<Notify>,
        released: Arc<Notify>,
    }
    impl PatternListener<Ping> for Waiting {
        fn call<'a>(
            &'a self,
            _: &'a Ctx,
            _: EventKey<Ping>,
            _: &'a Ping,
        ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
            Box::pin(async move {
                self.started.notify_one();
                self.released.notified().await;
                Ok(None)
            })
        }
    }
    let released = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    let queue = Arc::new(Queue {
        cleanup_signal: Some(released.clone()),
        ..Default::default()
    });
    let ctx = root(Some(queue.clone()));
    let (fiber, owner) = child(&ctx).await;
    let disposer = owner
        .events()
        .on_pattern(
            &owner,
            EventPattern::prefix("wait/"),
            Waiting {
                started: started.clone(),
                released,
            },
        )
        .unwrap();
    ctx.events()
        .emit(&ctx, &EventKey::dynamic("wait/one"), Arc::new(Ping(0)))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), disposer.dispose())
        .await
        .unwrap()
        .unwrap();
    assert!(queue.entries.lock().unwrap().is_empty());
    fiber.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_registration_has_no_effect_or_subscription() {
    struct Reject;
    impl EventQueue for Reject {
        fn handles(&self, _: TypeId) -> bool {
            true
        }
        fn register(&self, _: EventRegistration) -> Result<Effect, CordisError> {
            Err(CordisError::PluginFailed("registration rejected".into()))
        }
        fn select(
            &self,
            _: &Ctx,
            _: &TypeKey,
            _: ListenerKind,
        ) -> Result<Vec<EventRegistration>, CordisError> {
            Ok(vec![])
        }
    }
    let ctx = Ctx::root_with_event_queue(
        tokio::runtime::Handle::current(),
        Arc::new(|_| {}),
        Arc::new(Reject),
    );
    let before = ctx.root_view().unwrap().effects().len();
    for _ in 0..100 {
        assert!(ctx
            .events()
            .on(&ctx, &EventKey::of(), record(1, &Log::default(), None))
            .is_err());
    }
    assert!(ctx.events().subscriptions().is_empty());
    assert_eq!(ctx.root_view().unwrap().effects().len(), before);
    ctx.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_dispatch_can_call_sync_and_borrowed_waterfall_listeners() {
    use rutis::{EventContinuation, EventTerminal, SyncEventContinuation, SyncEventTerminal};
    use std::any::Any;
    type Payload = dyn Any + Send + Sync;
    type Value = Box<dyn Any + Send>;
    struct AsyncEnd;
    impl EventTerminal for AsyncEnd {
        fn call<'a>(
            &'a mut self,
            _: &'a Ctx,
            _: &'a Payload,
        ) -> BoxFuture<'a, Result<Value, CordisError>> {
            Box::pin(async { Ok(Box::new(5_u64) as Value) })
        }
    }
    struct SyncEnd<'a>(&'a std::rc::Rc<u64>);
    impl SyncEventTerminal for SyncEnd<'_> {
        fn call(&mut self, _: &Ctx, _: &Payload) -> Result<Value, CordisError> {
            Ok(Box::new(**self.0))
        }
    }
    let queue = Arc::new(Queue::default());
    let ctx = root(Some(queue.clone()));
    let key = EventKey::of();
    ctx.events()
        .on_sync(&ctx, &key, |_: &Ctx, _: &Ping| Ok(Some(7)))
        .unwrap();
    ctx.events().on_waterfall(&ctx, &key, Add(10)).unwrap();
    ctx.events()
        .on_waterfall_sync(&ctx, &key, |_: &Ctx, _: &Ping, next: SyncNext<'_, Ping>| {
            Ok(next.call()? + 20)
        })
        .unwrap();
    let entries = queue.entries.lock().unwrap().clone();
    let key = entries[0].subscription().key.clone();
    let mut sync = entries[0].select(&ctx, &key).unwrap().unwrap();
    assert_eq!(
        *sync
            .call_sync(&Ping(0))
            .unwrap()
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    assert!(sync.call_sync(&Ping(0)).is_err());
    let mut waterfall = entries[1].select(&ctx, &key).unwrap().unwrap();
    let mut terminal = AsyncEnd;
    let value = waterfall
        .call_waterfall(
            &Ping(0),
            EventContinuation::new(&ctx, &key, &Ping(0), &mut terminal),
        )
        .await
        .unwrap();
    assert_eq!(*value.downcast::<u64>().unwrap(), 15);
    let mut waterfall_sync = entries[2].select(&ctx, &key).unwrap().unwrap();
    let borrowed = std::rc::Rc::new(3);
    let mut terminal = SyncEnd(&borrowed);
    let value = waterfall_sync
        .call_waterfall_sync(
            &Ping(0),
            SyncEventContinuation::new(&ctx, &key, &Ping(0), &mut terminal),
        )
        .unwrap();
    assert_eq!(*value.downcast::<u64>().unwrap(), 23);
    drop((sync, waterfall, waterfall_sync));
    ctx.shutdown().await.unwrap();
}

#[tokio::test]
async fn whole_plugin_unload_retains_native_wait_before_effects_rule() {
    use std::future::{poll_fn, Future};
    use std::task::Poll;
    struct Waiting(Arc<Notify>, Arc<Notify>);
    impl PatternListener<Ping> for Waiting {
        fn call<'a>(
            &'a self,
            _: &'a Ctx,
            _: EventKey<Ping>,
            _: &'a Ping,
        ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
            Box::pin(async move {
                self.0.notify_one();
                self.1.notified().await;
                Ok(None)
            })
        }
    }
    for queued in [false, true] {
        let ctx = root(queued.then(|| Arc::new(Queue::default())));
        let (fiber, owner) = child(&ctx).await;
        let started = Arc::new(Notify::new());
        let released = Arc::new(Notify::new());
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleanup = cleaned.clone();
        owner
            .effect(|| {
                Effect::Disposer(Box::new(move || {
                    cleanup.store(true, Ordering::SeqCst);
                    Ok(())
                }))
            })
            .unwrap();
        owner
            .events()
            .on_pattern(
                &owner,
                EventPattern::prefix("wait/"),
                Waiting(started.clone(), released.clone()),
            )
            .unwrap();
        ctx.events()
            .emit(&ctx, &EventKey::named("wait/one"), Arc::new(Ping(0)))
            .unwrap();
        started.notified().await;
        let mut unload = std::pin::pin!(fiber.dispose());
        poll_fn(|cx| match unload.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("unload must retain accepted callback"),
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while fiber.state().state != rutis::FiberState::Unloading {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!cleaned.load(Ordering::SeqCst));
        released.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(3), unload)
            .await
            .unwrap()
            .unwrap();
        assert!(cleaned.load(Ordering::SeqCst));
        ctx.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn empty_sync_waterfall_holds_emitter_until_borrowed_terminal_returns() {
    use std::future::{poll_fn, Future};
    use std::task::Poll;
    for queued in [false, true] {
        let ctx = root(queued.then(|| Arc::new(Queue::default())));
        let (fiber, owner) = child(&ctx).await;
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            owner
                .events()
                .waterfall_sync(&owner, &EventKey::of(), &Ping(0), |_, _| {
                    started_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap();
                    Ok(7)
                })
                .unwrap()
        });
        started_rx.await.unwrap();
        let mut unload = std::pin::pin!(fiber.dispose());
        poll_fn(|cx| match unload.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("terminal still borrows emitter"),
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while fiber.state().state != rutis::FiberState::Unloading {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(thread.join().unwrap(), 7);
        unload.await.unwrap();
        ctx.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn registration_crossing_unload_rolls_back_external_entry() {
    struct Paused {
        queue: Queue,
        started: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl EventQueue for Paused {
        fn handles(&self, event: TypeId) -> bool {
            self.queue.handles(event)
        }
        fn register(&self, registration: EventRegistration) -> Result<Effect, CordisError> {
            let cleanup = self.queue.register(registration)?;
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            Ok(cleanup)
        }
        fn select(
            &self,
            ctx: &Ctx,
            key: &TypeKey,
            kind: ListenerKind,
        ) -> Result<Vec<EventRegistration>, CordisError> {
            self.queue.select(ctx, key, kind)
        }
    }
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let queue = Arc::new(Paused {
        queue: Queue::default(),
        started: Mutex::new(Some(started_tx)),
        release: Mutex::new(release_rx),
    });
    let ctx = Ctx::root_with_event_queue(
        tokio::runtime::Handle::current(),
        Arc::new(|e| panic!("{e}")),
        queue.clone(),
    );
    let (fiber, owner) = child(&ctx).await;
    let thread = std::thread::spawn(move || {
        owner
            .events()
            .on(&owner, &EventKey::of(), record(1, &Log::default(), None))
    });
    started_rx.await.unwrap();
    // dispose publishes intent eagerly; registration owns a lease, but must
    // not hold admission or transition while the adapter is paused.
    let unload = fiber.dispose();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while fiber.state().state != rutis::FiberState::Unloading {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    assert!(thread.join().unwrap().is_err());
    tokio::time::timeout(std::time::Duration::from_secs(3), unload)
        .await
        .unwrap()
        .unwrap();
    assert!(queue.queue.entries.lock().unwrap().is_empty());
    assert!(ctx.events().subscriptions().is_empty());
    ctx.shutdown().await.unwrap();
}
