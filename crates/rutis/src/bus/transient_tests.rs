//! 终态释放(0.2.1)单元验证:keyed 通道摘除监听后删除空条目,
//! 派发尾链任务完成后自摘——随实例 churn 的通道与尾链不残留。

use super::*;
use crate::ctx::Ctx;
use crate::error::CordisError;
use crate::event::{Event, Listener, Next};
use crate::BoxFuture;
use std::time::Duration;

struct Ping;

impl Event for Ping {
    const NAME: &'static str = "test::TransientPing";
    type Value = ();
}
impl crate::SyncEvent for Ping {}

struct Capture(tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<Ctx>>>);
impl crate::Plugin for Capture {
    fn name(&self) -> &str {
        "capture-sync-context"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let sender = self.0.lock().await.take().unwrap();
            assert!(sender.send(ctx.clone()).is_ok());
            Ok(Effect::Done)
        })
    }
}

#[tokio::test]
async fn thousand_sync_subtrees_release_exact_and_pattern_tables() {
    let root = Ctx::root().unwrap();
    let bus = root.events().clone();
    for i in 0..1000 {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let view = root.plugin(Capture(tokio::sync::Mutex::new(Some(sender))));
        (&view).await.unwrap();
        let ctx = receiver.await.unwrap();
        let key = crate::EventKey::<Ping>::dynamic(format!("room/{i}"));
        let scoped = key.clone().instance(ctx.instance());
        bus.on_sync(&ctx, &scoped, |_: &Ctx, _: &Ping| Ok(None))
            .unwrap();
        bus.on_sync_pattern(
            &ctx,
            crate::EventPattern::prefix("room/"),
            |_: &Ctx, _: crate::EventKey<Ping>, _: &Ping| Ok(None),
        )
        .unwrap();
        bus.on_waterfall_sync(
            &ctx,
            &scoped,
            |_: &Ctx, _: &Ping, next: crate::SyncNext<'_, Ping>| next.call(),
        )
        .unwrap();
        bus.on_waterfall_sync_pattern(
            &ctx,
            crate::EventPattern::prefix("room/"),
            |_: &Ctx, _: crate::EventKey<Ping>, _: &Ping, next: crate::SyncNext<'_, Ping>| {
                next.call()
            },
        )
        .unwrap();
        bus.bail_sync(&ctx, &scoped, &Ping).unwrap();
        bus.bail_sync(&ctx, &key, &Ping).unwrap();
        bus.waterfall_sync(&ctx, &scoped, &Ping, |_, _| Ok(()))
            .unwrap();
        bus.waterfall_sync(&ctx, &key, &Ping, |_, _| Ok(()))
            .unwrap();
        view.shutdown().await.unwrap();
        let inner = bus.inner.lock().unwrap();
        assert!(inner.sync_hooks.is_empty());
        assert!(inner.sync_wf_hooks.is_empty());
        assert_eq!(inner.sync_hooks.pattern_count(), 0);
        assert_eq!(inner.sync_wf_hooks.pattern_count(), 0);
    }
    root.shutdown().await.unwrap();
}

struct Nop;

impl Listener<Ping> for Nop {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async { Ok(None) })
    }
}

struct NopWaterfall;

impl crate::PatternListener<Ping> for Nop {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: crate::EventKey<Ping>,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async { Ok(None) })
    }
}

#[tokio::test]
async fn thousand_pattern_groups_release_indexes_and_tail_keys() {
    let ctx = Ctx::root().unwrap();
    let bus = ctx.events().clone();
    for i in 0..1000 {
        let listener = bus
            .on_pattern(&ctx, crate::EventPattern::any_prefix(["ch/", "ch/1"]), Nop)
            .unwrap();
        bus.emit(
            &ctx,
            &crate::EventKey::dynamic(format!("ch/{i}")),
            Arc::new(Ping),
        )
        .unwrap();
        listener.dispose().await.unwrap();
        assert_eq!(bus.inner.lock().unwrap().hooks.pattern_count(), 0);
    }
    // Each flight exits before the owner drain completes. TailCleanup drops
    // first, so disposal is also a deterministic tail completion barrier.
    assert!(bus.inner.lock().unwrap().dispatch_tail.is_empty());
    ctx.shutdown().await.unwrap();
}

impl crate::event::WaterfallListener<Ping> for NopWaterfall {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a Ping,
        next: Next<'a, Ping>,
    ) -> BoxFuture<'a, Result<(), CordisError>> {
        Box::pin(async move { next.call().await })
    }
}

#[tokio::test]
async fn keyed_channels_and_dispatch_tails_prune() {
    let ctx = Ctx::root().expect("runtime in scope");
    let bus = ctx.events().clone();
    for i in 0..25 {
        let name = format!("ch/{i}");
        let listener = bus
            .on::<Ping>(&ctx, &crate::EventKey::dynamic(name.clone()), Nop)
            .unwrap();
        let wf = bus
            .on_waterfall::<Ping>(&ctx, &crate::EventKey::dynamic(name.clone()), NopWaterfall)
            .unwrap();
        bus.emit::<Ping>(
            &ctx,
            &crate::EventKey::dynamic(name.clone()),
            Arc::new(Ping),
        )
        .expect("default event dispatch");
        listener.dispose().await.unwrap();
        wf.dispose().await.unwrap();
    }
    // 最后一次派发任务完成后自摘尾链;监听条目已在 dispose 内删除
    for _ in 0..500 {
        let empty = {
            let inner = bus.inner.lock().unwrap();
            inner.hooks.is_empty() && inner.wf_hooks.is_empty() && inner.dispatch_tail.is_empty()
        };
        if empty {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    {
        let inner = bus.inner.lock().unwrap();
        assert!(
            inner.hooks.is_empty() && inner.wf_hooks.is_empty(),
            "keyed channels must drop empty listener lists"
        );
        assert!(
            inner.dispatch_tail.is_empty(),
            "dispatch tails must self-remove on completion"
        );
    }
    // 摘除后再次注册/派发同键通道仍工作(条目按需重建)
    let listener = bus
        .on::<Ping>(&ctx, &crate::EventKey::dynamic("ch/0"), Nop)
        .unwrap();
    bus.emit::<Ping>(&ctx, &crate::EventKey::dynamic("ch/0"), Arc::new(Ping))
        .expect("default event dispatch");
    listener.dispose().await.unwrap();
}

#[tokio::test]
async fn dispatch_observers_prune_after_repeated_registration() {
    let ctx = Ctx::root().unwrap();
    let bus = ctx.events().clone();
    for _ in 0..1000 {
        let observer = bus.observe_dispatch(&ctx, |_| {}).unwrap();
        assert_eq!(bus.observer_count(), 1);
        observer.dispose().await.unwrap();
        assert_eq!(bus.observer_count(), 0);
    }
}
