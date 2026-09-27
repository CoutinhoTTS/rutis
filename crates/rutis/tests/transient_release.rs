//! 瞬态子插件释放(0.2.1,黑盒):长寿 root 上反复注册/销毁插件,
//! 实例、监听器与服务绑定全部回收;root 持续可用。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Event, FiberState, Listener, Plugin, TypeKey};

struct Ping;

impl Event for Ping {
    const NAME: &'static str = "test::TransientReleasePing";
    type Value = ();
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

/// 每实例持有一个 keyed 服务、一条 keyed 监听;Drop 计数验证实例回收。
struct Probe {
    dropped: Arc<AtomicUsize>,
    channel: Arc<str>,
    service_key: TypeKey,
}

impl Plugin for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        let key = self.service_key.clone();
        let channel = self.channel.clone();
        Box::pin(async move {
            ctx.provide_as::<u32>(key, Arc::new(7))?;
            ctx.events()
                .on::<Ping>(ctx, &rutis::EventKey::dynamic(channel), Nop)?;
            Ok(Effect::Done)
        })
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

async fn await_active(ctx: &Ctx, deadline: Duration) {
    let view = ctx.root_view().unwrap();
    let start = tokio::time::Instant::now();
    loop {
        let mut last = view.watch();
        let snapshot = view.state();
        if snapshot.state == FiberState::Active || snapshot.state == FiberState::Failed {
            assert_eq!(snapshot.state, FiberState::Active, "plugin must go Active");
            return;
        }
        assert!(
            start.elapsed() < deadline,
            "plugin did not become Active in time"
        );
        tokio::select! {
            _ = tokio::time::sleep(deadline) => {},
            _ = last.changed() => {},
        }
    }
}

#[tokio::test]
async fn churn_on_long_lived_root_releases_instances() {
    let ctx = Ctx::root().expect("runtime in scope");
    let dropped = Arc::new(AtomicUsize::new(0));
    for i in 0..50 {
        let probe = Probe {
            dropped: dropped.clone(),
            channel: Arc::from(format!("branch/{i}")),
            service_key: TypeKey::keyed_dynamic::<u32>(format!("svc/{i}")),
        };
        let view = ctx.plugin(probe);
        await_active(&ctx, Duration::from_secs(5)).await;
        view.dispose().await.unwrap();
        drop(view);
        // 已销毁通道再派发:无监听器早退,不 panic
        ctx.events()
            .emit::<Ping>(
                &ctx,
                &rutis::EventKey::dynamic(format!("branch/{i}")),
                Arc::new(Ping),
            )
            .expect("default event dispatch");
    }
    // 实例析构尾随 dispose 完成点(mount 记录 drain 释放最后引用)
    let start = tokio::time::Instant::now();
    while dropped.load(Ordering::SeqCst) < 50 {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "instances leaked: {} of 50 dropped",
            dropped.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // churn 后 root 仍可复用
    let view = ctx.plugin(Probe {
        dropped: dropped.clone(),
        channel: Arc::from("branch/reuse"),
        service_key: TypeKey::keyed_dynamic::<u32>("svc/reuse"),
    });
    await_active(&ctx, Duration::from_secs(5)).await;
    view.dispose().await.unwrap();
}
