use std::future::{poll_fn, Future};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::task::Poll;
use std::time::Duration;

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, Event, EventKey, EventOptions, EventPattern, FiberState,
    Listener, Next, PatternListener, PatternWaterfallListener, Plugin, SyncEvent, SyncNext,
    SyncWaterfallListener, WaterfallListener,
};
use tokio::sync::{oneshot, Notify, Semaphore};

struct Ping(u64);
impl Event for Ping {
    const NAME: &'static str = "ping";
    type Value = u64;
}
impl SyncEvent for Ping {}
struct Other;
impl Event for Other {
    const NAME: &'static str = "ping";
    type Value = u64;
}

struct PanicCreatingFuture;
impl Listener<Ping> for PanicCreatingFuture {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        panic!("panic creating listener future")
    }
}
impl PatternListener<Ping> for PanicCreatingFuture {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: EventKey<Ping>,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        panic!("panic creating listener future")
    }
}

#[tokio::test]
async fn serial_catches_user_panic_while_creating_exact_and_pattern_futures() {
    let root = Ctx::root().unwrap();
    root.events()
        .on(&root, &EventKey::of(), PanicCreatingFuture)
        .unwrap();
    root.events()
        .on_pattern(&root, EventPattern::prefix("panic/"), PanicCreatingFuture)
        .unwrap();
    for key in [EventKey::of(), EventKey::named("panic/created")] {
        let error = root
            .events()
            .serial(&root, &key, &Ping(0))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("panic creating listener future"));
    }
    // The pattern's flight is released even when construction panics.
    root.shutdown().await.unwrap();
}

struct Capture(Mutex<Option<oneshot::Sender<Ctx>>>);
impl Plugin for Capture {
    fn name(&self) -> &str {
        "capture"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            if let Some(sender) = self.0.lock().unwrap().take() {
                assert!(sender.send(ctx.clone()).is_ok());
            }
            Ok(Effect::Done)
        })
    }
}
async fn child(root: &Ctx) -> (rutis::FiberView, Ctx) {
    let (sender, receiver) = oneshot::channel();
    let view = root.plugin(Capture(Mutex::new(Some(sender))));
    (&view).await.unwrap();
    (view, receiver.await.unwrap())
}
async fn pending<F: Future>(future: std::pin::Pin<&mut F>) {
    let mut future = future;
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("completed before the accepted callback exited"),
    })
    .await;
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition did not become true");
}

type Log = Arc<Mutex<Vec<String>>>;
struct Record {
    label: &'static str,
    log: Log,
    result: Option<u64>,
}
fn record(label: &'static str, log: &Log) -> Record {
    Record {
        label,
        log: log.clone(),
        result: None,
    }
}
impl Listener<Ping> for Record {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(self.label.into());
            Ok(self.result)
        })
    }
}
impl PatternListener<Ping> for Record {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        key: EventKey<Ping>,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{}", self.label, key.name().unwrap()));
            Ok(self.result)
        })
    }
}
struct Around(Log, &'static str);
impl WaterfallListener<Ping> for Around {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Ping,
        next: Next<'a, Ping>,
    ) -> BoxFuture<'a, Result<u64, CordisError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(format!("{} in", self.1));
            let value = next.call().await?;
            self.0.lock().unwrap().push(format!("{} out", self.1));
            Ok(value + 1)
        })
    }
}
impl PatternWaterfallListener<Ping> for Around {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: EventKey<Ping>,
        event: &'a Ping,
        next: Next<'a, Ping>,
    ) -> BoxFuture<'a, Result<u64, CordisError>> {
        assert_eq!(key.name(), Some("room/1"));
        WaterfallListener::call(self, ctx, event, next)
    }
}
impl SyncWaterfallListener<Ping> for Around {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Ping,
        next: SyncNext<'a, Ping>,
    ) -> Result<u64, CordisError> {
        self.0.lock().unwrap().push(format!("{} in", self.1));
        let value = next.call()?;
        self.0.lock().unwrap().push(format!("{} out", self.1));
        Ok(value + 1)
    }
}

#[tokio::test]
#[allow(deprecated)]
async fn deprecated_named_wrappers_share_keys_order_once_and_waterfall() {
    let root = Ctx::root().unwrap();
    let bus = root.events();
    let key = EventKey::<Ping>::named("legacy");
    let log: Log = Arc::default();
    bus.on_keyed(&root, "legacy", record("normal", &log))
        .unwrap();
    bus.on_keyed_opt(
        &root,
        "legacy",
        record("prepend", &log),
        EventOptions {
            prepend: true,
            ..Default::default()
        },
    )
    .unwrap();
    bus.once_keyed(&root, "legacy", record("once", &log))
        .unwrap();
    assert_eq!(bus.serial(&root, &key, &Ping(0)).await.unwrap(), None);
    assert_eq!(*log.lock().unwrap(), ["prepend", "normal", "once"]);
    log.lock().unwrap().clear();
    assert_eq!(
        bus.serial_keyed(&root, String::from("legacy"), &Ping(0))
            .await
            .unwrap(),
        None
    );
    assert_eq!(*log.lock().unwrap(), ["prepend", "normal"]);
    log.lock().unwrap().clear();
    bus.parallel_keyed(&root, "legacy", Arc::new(Ping(0)))
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 2);
    log.lock().unwrap().clear();
    bus.emit_keyed(&root, "legacy", Arc::new(Ping(0)));
    until(|| log.lock().unwrap().len() == 2).await;
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|label| *label == "normal")
            .count(),
        1
    );
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|label| *label == "prepend")
            .count(),
        1
    );
    assert_eq!(
        bus.serial_keyed(&root, "other", &Ping(0)).await.unwrap(),
        None
    );
    log.lock().unwrap().clear();
    bus.on_waterfall_keyed(&root, "legacy", Around(log.clone(), "old"))
        .unwrap();
    bus.on_waterfall(&root, &key, Around(log.clone(), "new"))
        .unwrap();
    fn terminal<'a>(_: &'a Ctx, event: &'a Ping) -> BoxFuture<'a, Result<u64, CordisError>> {
        Box::pin(async move { Ok(event.0) })
    }
    assert_eq!(
        bus.waterfall_keyed(&root, "legacy", &Ping(3), terminal)
            .await
            .unwrap(),
        5
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["old in", "new in", "new out", "old out"]
    );
    log.lock().unwrap().clear();
    assert_eq!(
        bus.waterfall(&root, &key, &Ping(3), terminal)
            .await
            .unwrap(),
        5
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["old in", "new in", "new out", "old out"]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
#[allow(deprecated)]
async fn deprecated_instance_wrappers_share_typed_keys_and_reject_siblings() {
    let root = Ctx::root().unwrap();
    let (view, owner) = child(&root).await;
    let (_, sibling) = child(&root).await;
    let bus = root.events();
    let id = owner.instance();
    let key = EventKey::<Ping>::of().instance(id);
    let log: Log = Arc::default();
    bus.on_instance(
        &owner,
        id,
        Record {
            label: "old",
            log: log.clone(),
            result: Some(41),
        },
    )
    .unwrap();
    bus.on(&owner, &key, record("new", &log)).unwrap();
    assert_eq!(bus.serial(&owner, &key, &Ping(0)).await.unwrap(), Some(41));
    assert_eq!(
        bus.serial_instance(&owner, id, &Ping(0)).await.unwrap(),
        Some(41)
    );
    assert_eq!(
        bus.serial(&sibling, &EventKey::of(), &Ping(0))
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        bus.on_instance(&sibling, id, record("wrong", &log)),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        bus.serial_instance(&sibling, id, &Ping(0)).await,
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        bus.parallel_instance(&sibling, id, Arc::new(Ping(0))).await,
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        bus.emit_instance(&sibling, id, Arc::new(Ping(0))),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    log.lock().unwrap().clear();
    bus.parallel_instance(&owner, id, Arc::new(Ping(0)))
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 2);
    bus.emit_instance(&owner, id, Arc::new(Ping(0))).unwrap();
    until(|| log.lock().unwrap().len() == 4).await;
    for label in ["old", "new"] {
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|entry| entry.as_str() == label)
                .count(),
            2
        );
    }
    view.shutdown().await.unwrap();
    assert!(bus.emit_instance(&owner, id, Arc::new(Ping(0))).is_err());
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn typed_identity_and_prefix_scope_are_distinct() {
    const KEY: EventKey<Ping> = EventKey::named("room/1");
    assert_eq!(KEY, EventKey::dynamic("room/1"));
    assert_ne!(EventKey::<Ping>::of(), EventKey::named("ping"));
    let root = Ctx::root().unwrap();
    let log: Log = Arc::default();
    root.events()
        .on_pattern(
            &root,
            EventPattern::prefix("room/"),
            Record {
                label: "all",
                log: log.clone(),
                result: None,
            },
        )
        .unwrap();
    root.events().serial(&root, &KEY, &Ping(0)).await.unwrap();
    root.events()
        .serial(&root, &EventKey::of(), &Ping(0))
        .await
        .unwrap();
    root.events()
        .serial(&root, &KEY.clone().instance(root.instance()), &Ping(0))
        .await
        .unwrap();
    root.events()
        .serial(&root, &EventKey::named("room/1"), &Other)
        .await
        .unwrap();
    assert_eq!(*log.lock().unwrap(), ["all:room/1"]);
    assert!(root
        .events()
        .on_pattern(
            &root,
            EventPattern::<Ping>::any_prefix(Vec::<String>::new()),
            Record {
                label: "bad",
                log,
                result: None
            }
        )
        .is_err());
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_and_pattern_share_registration_and_prepend_order() {
    let root = Ctx::root().unwrap();
    let key = EventKey::named("room/1");
    let log: Log = Arc::default();
    let bus = root.events();
    bus.on(
        &root,
        &key,
        Record {
            label: "exact",
            log: log.clone(),
            result: None,
        },
    )
    .unwrap();
    bus.on_pattern(
        &root,
        EventPattern::any_prefix(["room/", "room/1"]),
        Record {
            label: "group",
            log: log.clone(),
            result: None,
        },
    )
    .unwrap();
    bus.on_pattern_opt(
        &root,
        EventPattern::prefix("room/"),
        Record {
            label: "first",
            log: log.clone(),
            result: None,
        },
        EventOptions {
            prepend: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(bus.serial(&root, &key, &Ping(0)).await.unwrap(), None);
    assert_eq!(
        *log.lock().unwrap(),
        ["first:room/1", "exact", "group:room/1"]
    );
    let subscriptions = bus.subscriptions();
    assert_eq!(subscriptions.len(), 3);
    let exact = subscriptions
        .iter()
        .find(|entry| entry.prefixes.is_empty())
        .unwrap();
    assert_eq!(exact.selected, None);
    assert_eq!(exact.invoked, None);
    assert!(subscriptions
        .iter()
        .filter(|entry| !entry.prefixes.is_empty())
        .all(|entry| entry.selected == Some(1) && entry.invoked == Some(1)));
    log.lock().unwrap().clear();
    bus.parallel(&root, &key, Arc::new(Ping(0))).await.unwrap();
    assert_eq!(log.lock().unwrap().len(), 3);
    root.shutdown().await.unwrap();
    assert!(bus.subscriptions().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grouped_once_is_claimed_once_across_concurrent_names() {
    let root = Ctx::root().unwrap();
    let log: Log = Arc::default();
    root.events()
        .on_pattern_opt(
            &root,
            EventPattern::any_prefix(["room/", "room/a"]),
            Record {
                label: "once",
                log: log.clone(),
                result: None,
            },
            EventOptions {
                once: true,
                ..Default::default()
            },
        )
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        for name in ["room/a", "room/b"] {
            let root = root.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                root.handle()
                    .block_on(
                        root.events()
                            .serial(&root, &EventKey::named(name), &Ping(0)),
                    )
                    .unwrap();
            });
        }
        barrier.wait();
    });
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(root.events().subscriptions().is_empty());
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn waterfall_pattern_wraps_and_instance_options_work() {
    let root = Ctx::root().unwrap();
    let key = EventKey::named("room/1");
    let log: Log = Arc::default();
    root.events()
        .on_waterfall(&root, &key, Around(log.clone(), "exact"))
        .unwrap();
    root.events()
        .on_waterfall_pattern_opt(
            &root,
            EventPattern::prefix("room/"),
            Around(log.clone(), "pattern"),
            EventOptions {
                prepend: true,
                ..Default::default()
            },
        )
        .unwrap();
    fn terminal<'a>(_: &'a Ctx, event: &'a Ping) -> BoxFuture<'a, Result<u64, CordisError>> {
        Box::pin(async move { Ok(event.0) })
    }
    assert_eq!(
        root.events()
            .waterfall(&root, &key, &Ping(1), terminal)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["pattern in", "exact in", "exact out", "pattern out"]
    );
    let scoped = key.instance(root.instance());
    root.events()
        .on_waterfall_opt(
            &root,
            &scoped,
            Around(log, "scoped"),
            EventOptions {
                once: true,
                prepend: true,
            },
        )
        .unwrap();
    assert_eq!(
        root.events()
            .waterfall(&root, &scoped, &Ping(1), terminal)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        root.events()
            .waterfall(&root, &scoped, &Ping(1), terminal)
            .await
            .unwrap(),
        1
    );
    let (view, ctx) = child(&root).await;
    let private = EventKey::named("private").instance(ctx.instance());
    assert!(matches!(
        root.events()
            .waterfall(&root, &private, &Ping(0), terminal)
            .await,
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    view.shutdown().await.unwrap();
    assert!(matches!(
        ctx.events()
            .waterfall(&ctx, &private, &Ping(0), terminal)
            .await,
        Err(CordisError::Closed)
    ));
    root.shutdown().await.unwrap();
}

struct GatePattern {
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
    log: Arc<Mutex<Vec<(String, u64)>>>,
    done: Arc<Notify>,
}
impl PatternListener<Ping> for GatePattern {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        key: EventKey<Ping>,
        event: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Box::pin(async move {
            if key.name() == Some("room/a") && event.0 == 0 {
                self.entered.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
            self.log
                .lock()
                .unwrap()
                .push((key.name().unwrap().into(), event.0));
            self.done.notify_one();
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pattern_only_emit_preserves_each_name_without_global_tail() {
    let root = Ctx::root().unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    root.events()
        .on_pattern(
            &root,
            EventPattern::prefix("room/"),
            GatePattern {
                entered: entered.clone(),
                release: release.clone(),
                log: log.clone(),
                done: Arc::new(Notify::new()),
            },
        )
        .unwrap();
    root.events()
        .emit(&root, &EventKey::named("room/a"), Arc::new(Ping(0)))
        .unwrap();
    entered.notified().await;
    root.events()
        .emit(&root, &EventKey::named("room/a"), Arc::new(Ping(1)))
        .unwrap();
    root.events()
        .emit(&root, &EventKey::named("room/b"), Arc::new(Ping(2)))
        .unwrap();
    until(|| log.lock().unwrap().len() == 1).await;
    assert_eq!(*log.lock().unwrap(), [("room/b".into(), 2)]);
    release.add_permits(1);
    until(|| log.lock().unwrap().len() == 3).await;
    assert_eq!(
        *log.lock().unwrap(),
        [
            ("room/b".into(), 2),
            ("room/a".into(), 0),
            ("room/a".into(), 1)
        ]
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn sync_bail_short_circuits_and_once_selection_is_not_actual_invocation() {
    let root = Ctx::root().unwrap();
    let key = EventKey::named("room/1");
    let skipped = Arc::new(AtomicUsize::new(0));
    let stop = root
        .events()
        .on_sync(&root, &key, |_: &Ctx, _: &Ping| Ok(Some(7)))
        .unwrap();
    let calls = skipped.clone();
    root.events()
        .on_sync_pattern_opt(
            &root,
            EventPattern::prefix("room/"),
            move |_: &Ctx, _: EventKey<Ping>, _: &Ping| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            },
            EventOptions {
                once: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        root.events().bail_sync(&root, &key, &Ping(0)).unwrap(),
        Some(7)
    );
    assert_eq!(skipped.load(Ordering::SeqCst), 0);
    assert_eq!(root.events().subscriptions().len(), 1);
    stop.dispose().await.unwrap();
    assert_eq!(
        root.events().bail_sync(&root, &key, &Ping(0)).unwrap(),
        None
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn sync_waterfall_borrows_mutex_and_veto_skips_terminal() {
    let root = Ctx::root().unwrap();
    let key = EventKey::of();
    let log: Log = Arc::default();
    root.events()
        .on_waterfall_sync(&root, &key, Around(log.clone(), "outer"))
        .unwrap();
    root.events()
        .on_waterfall_sync(&root, &key, Around(log.clone(), "inner"))
        .unwrap();
    let storage = Mutex::new(10);
    let mut terminal_calls = 0;
    {
        let mut locked = storage.lock().unwrap();
        let value = root
            .events()
            .waterfall_sync(&root, &key, &Ping(3), |_, event| {
                terminal_calls += 1;
                Ok(*locked + event.0)
            })
            .unwrap();
        *locked = value;
        assert_eq!((*locked, terminal_calls), (15, 1));
    }
    assert_eq!(
        *log.lock().unwrap(),
        ["outer in", "inner in", "inner out", "outer out"]
    );
    root.events()
        .on_waterfall_sync_opt(
            &root,
            &key,
            |_: &Ctx, _: &Ping, _: SyncNext<'_, Ping>| Ok(99),
            EventOptions {
                prepend: true,
                once: true,
            },
        )
        .unwrap();
    assert_eq!(
        root.events()
            .waterfall_sync(&root, &key, &Ping(0), |_, _| {
                terminal_calls += 1;
                Ok(0)
            })
            .unwrap(),
        99
    );
    assert_eq!(terminal_calls, 1);
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn sync_patterns_expose_keys_and_share_order_for_both_modes() {
    let root = Ctx::root().unwrap();
    let key = EventKey::named("room/1");
    root.events()
        .on_sync(&root, &key, |_: &Ctx, _: &Ping| Ok(Some(10)))
        .unwrap();
    root.events()
        .on_sync_pattern_opt(
            &root,
            EventPattern::any_prefix(["room/", "room/1"]),
            |_: &Ctx, key: EventKey<Ping>, _: &Ping| {
                assert_eq!(key.name(), Some("room/1"));
                Ok(Some(20))
            },
            EventOptions {
                prepend: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        root.events().bail_sync(&root, &key, &Ping(0)).unwrap(),
        Some(20)
    );
    root.events()
        .on_waterfall_sync_pattern(
            &root,
            EventPattern::prefix("room/"),
            |_: &Ctx, key: EventKey<Ping>, _: &Ping, next: SyncNext<'_, Ping>| {
                assert_eq!(key.name(), Some("room/1"));
                Ok(next.call()? + 1)
            },
        )
        .unwrap();
    assert_eq!(
        root.events()
            .waterfall_sync(&root, &key, &Ping(0), |_, _| Ok(4))
            .unwrap(),
        5
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn sync_listener_reentry_rejects_all_bail_and_waterfall_combinations() {
    fn nested(ctx: &Ctx, key: &EventKey<Ping>, waterfall: bool) -> Result<u64, CordisError> {
        if waterfall {
            ctx.events().waterfall_sync(ctx, key, &Ping(0), |_, _| {
                panic!("reentrant terminal must not execute")
            })
        } else {
            ctx.events()
                .bail_sync(ctx, key, &Ping(0))
                .map(|value| value.unwrap_or(0))
        }
    }
    for outer_waterfall in [false, true] {
        for inner_waterfall in [false, true] {
            let root = Ctx::root().unwrap();
            let key = EventKey::<Ping>::named("reentry");
            let observed = Arc::new(AtomicUsize::new(0));
            let count = observed.clone();
            root.events()
                .observe_dispatch(&root, move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                })
                .unwrap();
            let inner_key = key.clone();
            if outer_waterfall {
                root.events()
                    .on_waterfall_sync(
                        &root,
                        &key,
                        move |ctx: &Ctx, _: &Ping, _: SyncNext<'_, Ping>| {
                            nested(ctx, &inner_key, inner_waterfall)
                        },
                    )
                    .unwrap();
            } else {
                root.events()
                    .on_sync(&root, &key, move |ctx: &Ctx, _: &Ping| {
                        nested(ctx, &inner_key, inner_waterfall).map(Some)
                    })
                    .unwrap();
            }
            // A second attempt also verifies that the first error released the guard.
            for attempt in 1..=2 {
                let result = if outer_waterfall {
                    root.events()
                        .waterfall_sync(&root, &key, &Ping(0), |_, _| Ok(0))
                } else {
                    root.events()
                        .bail_sync(&root, &key, &Ping(0))
                        .map(|value| value.unwrap_or(0))
                };
                assert!(matches!(result, Err(CordisError::ReentrantEvent { .. })));
                assert_eq!(observed.load(Ordering::SeqCst), attempt);
            }
            root.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_sync_key_can_execute_on_two_threads_at_once() {
    for waterfall in [false, true] {
        let root = Ctx::root().unwrap();
        let key = EventKey::<Ping>::named("concurrent");
        let (entered_tx, entered_rx) = oneshot::channel();
        let entered = Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = Mutex::new(release_rx);
        let gate = move |event: &Ping| {
            if event.0 == 0 {
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                release.lock().unwrap().recv().unwrap();
            }
        };
        if waterfall {
            root.events()
                .on_waterfall_sync(
                    &root,
                    &key,
                    move |_: &Ctx, event: &Ping, next: SyncNext<'_, Ping>| {
                        gate(event);
                        next.call()
                    },
                )
                .unwrap();
        } else {
            root.events()
                .on_sync(&root, &key, move |_: &Ctx, event: &Ping| {
                    gate(event);
                    Ok(Some(event.0 + 7))
                })
                .unwrap();
        }
        let caller = root.clone();
        let first_key = key.clone();
        let first = tokio::task::spawn_blocking(move || {
            if waterfall {
                caller
                    .events()
                    .waterfall_sync(&caller, &first_key, &Ping(0), |_, event| Ok(event.0 + 7))
            } else {
                caller
                    .events()
                    .bail_sync(&caller, &first_key, &Ping(0))
                    .map(|value| value.unwrap())
            }
        });
        entered_rx.await.unwrap();
        // The first call is still in its listener when this thread calls the same key.
        let second = if waterfall {
            root.events()
                .waterfall_sync(&root, &key, &Ping(1), |_, event| Ok(event.0 + 7))
        } else {
            root.events()
                .bail_sync(&root, &key, &Ping(1))
                .map(|value| value.unwrap())
        };
        release_tx.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), 7);
        assert_eq!(second.unwrap(), 8);
        root.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sync_reentry_covers_observer_terminal_modes_and_bus_identity() {
    let root = Ctx::root().unwrap();
    let other = Ctx::root().unwrap();
    let key = EventKey::<Ping>::named("same");
    let checks = Arc::new(AtomicUsize::new(0));
    let checked = checks.clone();
    root.events()
        .observe_dispatch(&root, move |attempt| {
            if attempt.mode == rutis::DispatchMode::WaterfallSync {
                // This observer must be rejected before another observation starts.
                assert!(matches!(root_call(attempt.event), Some(0)));
                checked.fetch_add(1, Ordering::SeqCst);
            }
        })
        .unwrap();
    fn root_call(event: &(dyn std::any::Any + Send + Sync)) -> Option<u64> {
        event.downcast_ref::<Ping>().map(|event| event.0)
    }
    let observed_ctx = root.clone();
    let observed_key = key.clone();
    let guard_seen = Arc::new(AtomicUsize::new(0));
    let guard_check = guard_seen.clone();
    root.events()
        .observe_dispatch(&root, move |_| {
            if matches!(
                observed_ctx
                    .events()
                    .bail_sync(&observed_ctx, &observed_key, &Ping(0)),
                Err(CordisError::ReentrantEvent { .. })
            ) {
                guard_check.fetch_add(1, Ordering::SeqCst);
            }
        })
        .unwrap();
    let result = root
        .events()
        .waterfall_sync(&root, &key, &Ping(0), |_, _| {
            assert!(matches!(
                root.events().bail_sync(&root, &key, &Ping(0)),
                Err(CordisError::ReentrantEvent { .. })
            ));
            assert_eq!(
                other
                    .events()
                    .waterfall_sync(&other, &key, &Ping(0), |_, _| Ok(5))?,
                5
            );
            root.events()
                .waterfall_sync(&root, &EventKey::named("different"), &Ping(0), |_, _| Ok(6))
        })
        .unwrap();
    assert_eq!(result, 6);
    assert_eq!(guard_seen.load(Ordering::SeqCst), 2);
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    root.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
}

#[tokio::test]
async fn sync_error_is_preserved_and_panic_reports_once_even_with_panicking_sink() {
    let reports = Arc::new(AtomicUsize::new(0));
    let seen = reports.clone();
    let root = Ctx::root_with_sink(
        tokio::runtime::Handle::current(),
        Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("sink");
        }),
    );
    let key = EventKey::of();
    let fail = root
        .events()
        .on_sync(&root, &key, |_: &Ctx, _: &Ping| {
            Err(CordisError::Validation {
                issues: vec!["original".into()],
            })
        })
        .unwrap();
    assert!(
        matches!(root.events().bail_sync(&root, &key, &Ping(0)), Err(CordisError::Validation { issues }) if issues == ["original"])
    );
    assert_eq!(reports.load(Ordering::SeqCst), 0);
    fail.dispose().await.unwrap();
    root.events()
        .on_waterfall_sync(&root, &key, Around(Arc::default(), "outer"))
        .unwrap();
    root.events()
        .on_waterfall_sync(
            &root,
            &key,
            |_: &Ctx, _: &Ping, _: SyncNext<'_, Ping>| -> Result<u64, CordisError> {
                panic!("inner")
            },
        )
        .unwrap();
    assert!(matches!(
        root.events()
            .waterfall_sync(&root, &key, &Ping(0), |_, _| Ok(0)),
        Err(CordisError::SyncEventPanicked(_))
    ));
    assert_eq!(reports.load(Ordering::SeqCst), 1);
    assert!(matches!(
        root.events().waterfall_sync(
            &root,
            &EventKey::named("terminal"),
            &Ping(0),
            |_, _| -> Result<u64, CordisError> { panic!("terminal") }
        ),
        Err(CordisError::SyncEventPanicked(_))
    ));
    assert_eq!(reports.load(Ordering::SeqCst), 2);
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_listener_sync_terminal_is_drained_by_shutdown_dispose_and_restart() {
    for operation in ["shutdown", "dispose", "restart"] {
        let root = Ctx::root().unwrap();
        let view = root.root_view().unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let caller = root.clone();
        let dispatch = tokio::task::spawn_blocking(move || {
            caller
                .events()
                .waterfall_sync(&caller, &EventKey::of(), &Ping(0), |_, _| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(1)
                })
        });
        entered_rx.await.unwrap();
        let finish = match operation {
            "shutdown" => root.shutdown(),
            "dispose" => view.dispose(),
            _ => view.restart(),
        };
        let mut finish = pin!(finish);
        pending(finish.as_mut()).await;
        if operation != "shutdown" {
            until(|| view.state().state != FiberState::Active).await;
            assert_eq!(view.state().state, FiberState::Unloading);
        }
        pending(finish.as_mut()).await;
        assert!(root
            .events()
            .bail_sync(&root, &EventKey::of(), &Ping(0))
            .is_err());
        release_tx.send(()).unwrap();
        assert_eq!(dispatch.await.unwrap().unwrap(), 1);
        finish.await.unwrap();
        root.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thousand_early_sync_disposals_reject_new_selection_and_wait_for_old_calls() {
    let root = Ctx::root().unwrap();
    for _ in 0..1000 {
        let key = EventKey::named("blocking");
        let (entered_tx, entered_rx) = oneshot::channel();
        let entered = Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = Mutex::new(release_rx);
        let listener = root
            .events()
            .on_sync(&root, &key, move |_: &Ctx, _: &Ping| {
                if let Some(sender) = entered.lock().unwrap().take() {
                    sender.send(()).unwrap();
                }
                release.lock().unwrap().recv().unwrap();
                Ok(Some(9))
            })
            .unwrap();
        let caller = root.clone();
        let dispatch = tokio::task::spawn_blocking(move || {
            caller
                .events()
                .bail_sync(&caller, &EventKey::named("blocking"), &Ping(0))
        });
        entered_rx.await.unwrap();
        let mut removing = pin!(listener.dispose());
        pending(removing.as_mut()).await;
        until(|| root.events().subscriptions().is_empty()).await;
        assert_eq!(
            root.events().bail_sync(&root, &key, &Ping(0)).unwrap(),
            None
        );
        pending(removing.as_mut()).await;
        release_tx.send(()).unwrap();
        assert_eq!(dispatch.await.unwrap().unwrap(), Some(9));
        removing.await.unwrap();
    }
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_sync_bail_and_waterfall_drain_during_removal_and_subtree_shutdown() {
    for waterfall in [false, true] {
        for shutdown in [false, true] {
            let root = Ctx::root().unwrap();
            let (view, owner) = child(&root).await;
            let (_, emitter) = child(&owner).await;
            let key = EventKey::<Ping>::named("scoped").instance(owner.instance());
            let (entered_tx, entered_rx) = oneshot::channel();
            let entered = Mutex::new(Some(entered_tx));
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release = Mutex::new(release_rx);
            let gate = move || {
                if let Some(sender) = entered.lock().unwrap().take() {
                    sender.send(()).unwrap();
                }
                release.lock().unwrap().recv().unwrap();
            };
            let listener = if waterfall {
                root.events()
                    .on_waterfall_sync(
                        &owner,
                        &key,
                        move |_: &Ctx, _: &Ping, next: SyncNext<'_, Ping>| {
                            gate();
                            Ok(next.call()? + 9)
                        },
                    )
                    .unwrap()
            } else {
                root.events()
                    .on_sync(&owner, &key, move |_: &Ctx, _: &Ping| {
                        gate();
                        Ok(Some(10))
                    })
                    .unwrap()
            };
            let dispatch_key = key.clone();
            let caller = emitter.clone();
            let dispatch = tokio::task::spawn_blocking(move || {
                if waterfall {
                    caller
                        .events()
                        .waterfall_sync(&caller, &dispatch_key, &Ping(0), |_, _| Ok(1))
                } else {
                    caller
                        .events()
                        .bail_sync(&caller, &dispatch_key, &Ping(0))
                        .map(|v| v.unwrap())
                }
            });
            entered_rx.await.unwrap();
            let finish = if shutdown {
                view.shutdown()
            } else {
                listener.dispose()
            };
            let mut finish = pin!(finish);
            pending(finish.as_mut()).await;
            if shutdown {
                assert!(emitter
                    .events()
                    .bail_sync(&emitter, &key, &Ping(0))
                    .is_err());
            } else {
                until(|| root.events().subscriptions().is_empty()).await;
                assert_eq!(
                    emitter
                        .events()
                        .bail_sync(&emitter, &key, &Ping(0))
                        .unwrap(),
                    None
                );
                assert_eq!(
                    emitter
                        .events()
                        .waterfall_sync(&emitter, &key, &Ping(0), |_, _| Ok(1))
                        .unwrap(),
                    1
                );
            }
            assert!(matches!(
                root.events().bail_sync(&root, &key, &Ping(0)),
                Err(CordisError::InstanceOutOfScope { .. })
            ));
            pending(finish.as_mut()).await;
            release_tx.send(()).unwrap();
            assert_eq!(dispatch.await.unwrap().unwrap(), 10);
            finish.await.unwrap();
            view.shutdown().await.unwrap();
            assert!(root.events().subscriptions().is_empty());
            root.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn sync_instance_isolation_stale_context_and_self_shutdown() {
    let root = Ctx::root().unwrap();
    let (view, ctx) = child(&root).await;
    let (_, sibling) = child(&root).await;
    let scoped = EventKey::<Ping>::named("scoped").instance(ctx.instance());
    ctx.events()
        .on_sync(&ctx, &scoped, |_: &Ctx, _: &Ping| Ok(Some(8)))
        .unwrap();
    assert_eq!(
        ctx.events().bail_sync(&ctx, &scoped, &Ping(0)).unwrap(),
        Some(8)
    );
    assert!(matches!(
        root.events().bail_sync(&root, &scoped, &Ping(0)),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        sibling.events().bail_sync(&sibling, &scoped, &Ping(0)),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    let sibling_key = EventKey::<Ping>::named("scoped").instance(sibling.instance());
    sibling
        .events()
        .on_sync(&sibling, &sibling_key, |_: &Ctx, _: &Ping| Ok(Some(9)))
        .unwrap();
    assert_eq!(
        sibling
            .events()
            .bail_sync(&sibling, &sibling_key, &Ping(0))
            .unwrap(),
        Some(9)
    );
    assert!(matches!(
        ctx.events().bail_sync(&ctx, &sibling_key, &Ping(0)),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    assert!(matches!(
        sibling
            .events()
            .waterfall_sync(&sibling, &scoped, &Ping(0), |_, _| {
                panic!("out-of-scope terminal must not execute")
            }),
        Err(CordisError::InstanceOutOfScope { .. })
    ));
    view.restart().await.unwrap();
    assert!(matches!(
        ctx.events().bail_sync(&ctx, &scoped, &Ping(0)),
        Err(CordisError::StaleGeneration { .. })
    ));
    assert!(ctx
        .events()
        .on_sync(&ctx, &scoped, |_: &Ctx, _: &Ping| Ok(None))
        .is_err());
    view.shutdown().await.unwrap();
    let own = root.clone();
    root.events()
        .on_sync(&root, &EventKey::of(), move |_: &Ctx, _: &Ping| {
            drop(own.shutdown());
            Ok(Some(1))
        })
        .unwrap();
    assert_eq!(
        root.events()
            .bail_sync(&root, &EventKey::of(), &Ping(0))
            .unwrap(),
        Some(1)
    );
    root.shutdown().await.unwrap();
}

#[tokio::test]
async fn thousand_registrations_and_disposals_leave_no_subscriptions() {
    let root = Ctx::root().unwrap();
    for i in 0..1000 {
        let key = EventKey::<Ping>::dynamic(format!("room/{i}"));
        let a = root
            .events()
            .on_sync(&root, &key, |_: &Ctx, _: &Ping| Ok(None))
            .unwrap();
        let b = root
            .events()
            .on_sync_pattern(
                &root,
                EventPattern::prefix("room/"),
                |_: &Ctx, _: EventKey<Ping>, _: &Ping| Ok(None),
            )
            .unwrap();
        assert_eq!(
            root.events().bail_sync(&root, &key, &Ping(0)).unwrap(),
            None
        );
        a.dispose().await.unwrap();
        b.dispose().await.unwrap();
        assert!(root.events().subscriptions().is_empty());
    }
    root.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pattern_shutdown_drains_old_callback_and_excludes_new_named_dispatches() {
    for root_owner in [false, true] {
        let root = Ctx::root().unwrap();
        let (view, owner) = if root_owner {
            (root.root_view().unwrap(), root.clone())
        } else {
            child(&root).await
        };
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        root.events()
            .on_pattern(
                &owner,
                EventPattern::prefix("room/"),
                GatePattern {
                    entered: entered.clone(),
                    release: release.clone(),
                    log: log.clone(),
                    done: Arc::new(Notify::new()),
                },
            )
            .unwrap();
        root.events()
            .emit(&root, &EventKey::named("room/a"), Arc::new(Ping(0)))
            .unwrap();
        entered.notified().await;
        let mut shutdown = pin!(if root_owner {
            root.shutdown()
        } else {
            view.shutdown()
        });
        pending(shutdown.as_mut()).await;
        // Legacy non-instance dispatch still returns Ok for an old context,
        // but the closing pattern owner cannot admit another callback.
        root.events()
            .serial(&root, &EventKey::named("room/new"), &Ping(1))
            .await
            .unwrap();
        assert_eq!(root.events().subscriptions()[0].selected, Some(1));
        pending(shutdown.as_mut()).await;
        release.add_permits(1);
        shutdown.await.unwrap();
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(root.events().subscriptions().is_empty());
        root.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sync_observer_can_register_before_snapshot_or_close_admission() {
    let root = Ctx::root().unwrap();
    let observed = root.clone();
    let once = AtomicUsize::new(0);
    root.events()
        .observe_dispatch(&root, move |attempt| {
            if attempt.mode == rutis::DispatchMode::BailSync
                && once.fetch_add(1, Ordering::SeqCst) == 0
            {
                observed
                    .events()
                    .on_sync(&observed, &EventKey::of(), |_: &Ctx, _: &Ping| Ok(Some(33)))
                    .unwrap();
            }
        })
        .unwrap();
    assert_eq!(
        root.events()
            .bail_sync(&root, &EventKey::of(), &Ping(0))
            .unwrap(),
        Some(33)
    );
    let body_ctx = root.clone();
    root.events()
        .on_waterfall_sync(
            &root,
            &EventKey::named("body"),
            move |_: &Ctx, _: &Ping, _: SyncNext<'_, Ping>| {
                body_ctx
                    .events()
                    .bail_sync(&body_ctx, &EventKey::named("body"), &Ping(0))
                    .map(|_| 0)
            },
        )
        .unwrap();
    assert!(matches!(
        root.events()
            .waterfall_sync(&root, &EventKey::named("body"), &Ping(0), |_, _| Ok(0)),
        Err(CordisError::ReentrantEvent { .. })
    ));
    let closing = root.clone();
    root.events()
        .observe_dispatch(&root, move |attempt| {
            if attempt.mode == rutis::DispatchMode::WaterfallSync {
                drop(closing.shutdown());
            }
        })
        .unwrap();
    let mut called = false;
    assert!(matches!(
        root.events()
            .waterfall_sync(&root, &EventKey::of(), &Ping(0), |_, _| {
                called = true;
                Ok(0)
            }),
        Err(CordisError::Closed)
    ));
    assert!(!called);
    root.shutdown().await.unwrap();
}
