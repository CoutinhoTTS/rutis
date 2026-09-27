//! 终态释放(0.2.1)单元验证:瞬态子插件退出后,mount 记录与 root 级
//! provide 的清理记录、provided 记账在长寿 root 上回到基线。

use super::*;
use crate::{BoxFuture, CordisError, Effect, Event, Listener, Plugin};
use std::time::Duration;

struct Noop;

impl Plugin for Noop {
    fn name(&self) -> &str {
        "noop"
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

/// mount 记录经子 fiber 退出路径 drain 自摘,root 的 effects 列表
/// 不随插件 churn 累积。
#[tokio::test]
async fn churn_mount_records_release_from_root() {
    let ctx = Ctx::root().expect("runtime in scope");
    let root = ctx.weak_fiber().upgrade().expect("root alive");
    for _ in 0..25 {
        let view = ctx.plugin(Noop);
        view.dispose().await.unwrap();
        drop(view);
    }
    // 释放尾随 TaskDone(dispose 的 join 点),有界等待列表清空
    for _ in 0..500 {
        if root.effects.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        root.effects.lock().unwrap().is_empty(),
        "mount records must not accumulate on a long-lived root"
    );
}

/// root 级 provide 的 evict 清理记录与 provided 记账随 Disposer 释放。
#[tokio::test]
async fn churn_root_provides_release_accounting() {
    let ctx = Ctx::root().expect("runtime in scope");
    let root = ctx.weak_fiber().upgrade().expect("root alive");
    for i in 0..25 {
        let key = TypeKey::keyed_dynamic::<u32>(format!("svc/{i}"));
        let disposer = ctx.provide_as::<u32>(key, Arc::new(i)).unwrap();
        disposer.dispose().await.unwrap();
    }
    assert!(
        root.provided.lock().unwrap().is_empty(),
        "provided accounting must shrink per released binding"
    );
    assert!(
        root.effects.lock().unwrap().is_empty(),
        "evict cleanups must self-remove after drain"
    );
}

/// Stage the interleaving where dispose passed the closing check, but its
/// terminal task and Dispose intent arrive after Shutdown was enqueued.
#[tokio::test]
async fn shutdown_completes_dispose_task_queued_behind_it() {
    let ctx = Ctx::root().unwrap();
    let root = ctx.weak_fiber().upgrade().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    ctx.effect({
        let entered = entered.clone();
        let release = release.clone();
        move || {
            Effect::AsyncDisposer(Box::new(move || {
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(())
                })
            }))
        }
    })
    .unwrap();

    let shutdown = ctx.shutdown();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let dispose_task = TransitionTask::new();
    root.transition.lock().unwrap().terminal_task = Some(dispose_task.clone());
    root.post(Intent::Dispose);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), shutdown)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), join_task(&dispose_task))
        .await
        .expect("dispose task stranded behind Shutdown")
        .unwrap();
}

struct ProbeEvent;
impl Event for ProbeEvent {
    const NAME: &'static str = "rutis-test-instance-churn";
    type Value = ();
}

struct IdleListener;
impl Listener<ProbeEvent> for IdleListener {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a ProbeEvent,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async { Ok(None) })
    }
}

struct ChurnPlugin(Vec<TypeKey>);
impl Plugin for ChurnPlugin {
    fn name(&self) -> &str {
        "churn-plugin"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.0
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide_as(TypeKey::instance::<u64>(ctx.instance()), Arc::new(1u64))?;
            ctx.events().on(
                ctx,
                &crate::EventKey::of().instance(ctx.instance()),
                IdleListener,
            )?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::test]
async fn thousand_shutdowns_reclaim_all_root_side_records() {
    let ctx = Ctx::root().unwrap();
    let root = ctx.weak_fiber().upgrade().unwrap();
    let service = ctx.provide(1u32).unwrap();
    for _ in 0..1000 {
        let view = ctx.plugin(ChurnPlugin(vec![TypeKey::of::<u32>()]));
        (&view).await.unwrap();
        view.shutdown().await.unwrap();
        assert_eq!(view.state().state, FiberState::Disposed);
        assert!(view.inner.driver.lock().unwrap().is_none());
        assert_eq!(root.children.lock().unwrap().len(), 0);
        assert_eq!(root.effects.lock().unwrap().len(), 1);
        assert_eq!(root.effect_index.lock().unwrap().len(), 1);
        assert!(view.inner.effect_index.lock().unwrap().is_empty());
        assert_eq!(ctx.shared().registry.table_counts(), (1, 0));
        assert_eq!(ctx.events().table_counts(), (0, 0, 0));
        drop(view);
    }
    service.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
