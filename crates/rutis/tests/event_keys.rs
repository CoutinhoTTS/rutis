//! D33 动态事件键契约测试(design-config-hot-update-and-dynamic-events-2026-09-21 §2.6)。
//!
//! 覆盖:keyed 通道隔离 / 动态名 / D31 尾链保序 / 四语义 keyed 变体 /
//! once·prepend / fiber 卸载清理 / parity 补拍(cordis events.spec 字符串名内核)。
//!
//! 等待全部用 Notify 信号(评审:固定 sleep 在 CI 高负载下有假红风险);
//! 负断言走确定性路径——无监听器的键 take_hooks 同步返回空,不产生任务。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, Event, FiberState, Listener, Next, Plugin, Terminal,
    TypeKey, WaterfallListener,
};
use tokio::sync::Notify;

// ── 测试事件与监听器(命名 struct,同 contract.rs 模式) ──────────────

#[derive(Debug, Clone)]
struct Ping {
    value: u32,
}

impl Event for Ping {
    const NAME: &'static str = "test::Ping";
    type Value = u32;
}

#[derive(Debug, Clone)]
struct Other;

impl Event for Other {
    const NAME: &'static str = "test::Other";
    type Value = u32;
}

type Hits = Arc<AtomicUsize>;
type Log = Arc<Mutex<Vec<u32>>>;

fn hits() -> Hits {
    Arc::new(AtomicUsize::new(0))
}

/// 等待条件成立(监听器完成动作后 notify_one;检查先行,permit 不丢失)。
async fn wait_until(cond: impl Fn() -> bool, notify: Arc<Notify>) {
    loop {
        if cond() {
            return;
        }
        notify.notified().await;
    }
}

/// 计数并可选记录事件值,完成后发信号。
struct Counting {
    hits: Hits,
    log: Option<Log>,
    done: Option<Arc<Notify>>,
}

impl Listener<Ping> for Counting {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u32>, CordisError>> {
        let hits = self.hits.clone();
        let v = e.value;
        let log = self.log.clone();
        let done = self.done.clone();
        Box::pin(async move {
            hits.fetch_add(1, Ordering::SeqCst);
            if let Some(log) = log {
                log.lock().unwrap().push(v);
            }
            if let Some(done) = done {
                done.notify_one();
            }
            Ok(None)
        })
    }
}

/// serial 短路值。
struct Bail(u32);

impl Listener<Ping> for Bail {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u32>, CordisError>> {
        Box::pin(async move { Ok(Some(self.0)) })
    }
}

/// 必错监听器。
struct Failing;

impl Listener<Ping> for Failing {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u32>, CordisError>> {
        Box::pin(async move { Err(CordisError::PluginFailed("boom".into())) })
    }
}

/// waterfall veto:不调 next,改写返回值。
struct Veto(u32);

impl WaterfallListener<Ping> for Veto {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _e: &'a Ping,
        _next: Next<'a, Ping>,
    ) -> BoxFuture<'a, Result<u32, CordisError>> {
        Box::pin(async move { Ok(self.0) })
    }
}

/// waterfall 中间件:值加事件值后放行。
struct AddEventValue;

impl WaterfallListener<Ping> for AddEventValue {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a Ping,
        next: Next<'a, Ping>,
    ) -> BoxFuture<'a, Result<u32, CordisError>> {
        Box::pin(async move {
            let v = next.call().await?;
            Ok(v + e.value)
        })
    }
}

/// waterfall 终态:返回固定值,计数到达次数。
struct TerminalCount {
    value: u32,
    reached: Hits,
}

impl Terminal<Ping> for TerminalCount {
    fn call<'a>(&'a self, _ctx: &'a Ctx, _e: &'a Ping) -> BoxFuture<'a, Result<u32, CordisError>> {
        let reached = self.reached.clone();
        let v = self.value;
        Box::pin(async move {
            reached.fetch_add(1, Ordering::SeqCst);
            Ok(v)
        })
    }
}

// ── 1. keyed 通道隔离 + 静态/动态同名互通 ──────────────────────────

#[tokio::test]
async fn keyed_channels_are_isolated_by_name() {
    let ctx = Ctx::root().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());
    let h_b = hits();
    let b_done = Arc::new(Notify::new());

    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("chan/a"),
            Counting {
                hits: hits(),
                log: Some(log.clone()),
                done: Some(done.clone()),
            },
        )
        .unwrap();
    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("chan/b"),
            Counting {
                hits: h_b.clone(),
                log: None,
                done: Some(b_done.clone()),
            },
        )
        .unwrap();

    // 只发 a:b 通道无任务(a 的链处理完即确定)
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("chan/a"),
            Arc::new(Ping { value: 7 }),
        )
        .expect("default event dispatch");
    wait_until(|| log.lock().unwrap().len() == 1, done.clone()).await;
    assert_eq!(log.lock().unwrap().as_slice(), [7]);
    assert_eq!(h_b.load(Ordering::SeqCst), 0);

    // 发 b:a 通道无任务(此前 a 的链已排干)
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("chan/b"),
            Arc::new(Ping { value: 9 }),
        )
        .expect("default event dispatch");
    wait_until(|| h_b.load(Ordering::SeqCst) == 1, b_done.clone()).await;
    assert_eq!(log.lock().unwrap().as_slice(), [7]);

    // 静态/动态同名互通:TypeKey::keyed 与 keyed_dynamic 按内容等值
    let k_static = TypeKey::keyed::<Ping>("chan/a");
    assert_eq!(k_static, TypeKey::keyed_dynamic::<Ping>("chan/a"));
    assert_ne!(k_static, TypeKey::keyed_dynamic::<Ping>("chan/x"));
    // 同名不同类型不等(类型烙在键里)
    assert_ne!(
        TypeKey::keyed_dynamic::<Ping>("chan/a"),
        TypeKey::keyed_dynamic::<Other>("chan/a")
    );
}

// ── 2. 运行时构造的动态名 ─────────────────────────────────────────

#[tokio::test]
async fn runtime_constructed_name_dispatches() {
    let ctx = Ctx::root().unwrap();
    let h = hits();
    let done = Arc::new(Notify::new());
    // 名字运行时才知道(如桥转发宿主事件)
    let name = format!("host/session-{}", 42);
    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic(name.clone()),
            Counting {
                hits: h.clone(),
                log: None,
                done: Some(done.clone()),
            },
        )
        .unwrap();
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic(name),
            Arc::new(Ping { value: 1 }),
        )
        .expect("default event dispatch");
    wait_until(|| h.load(Ordering::SeqCst) == 1, done.clone()).await;
}

// ── 3. D31 尾链:同名保序 ─────────────────────────────────────────

#[tokio::test]
async fn keyed_emit_preserves_emission_order_per_name() {
    let ctx = Ctx::root().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("ordered"),
            Counting {
                hits: hits(),
                log: Some(log.clone()),
                done: Some(done.clone()),
            },
        )
        .unwrap();

    // 并发背靠背 emit 同名:到达序 = 发射序(D31)
    let ctx2 = ctx.clone();
    tokio::spawn(async move {
        for i in 0..20u32 {
            ctx2.events()
                .emit(
                    &ctx2,
                    &rutis::EventKey::dynamic("ordered"),
                    Arc::new(Ping { value: i }),
                )
                .expect("default event dispatch");
        }
    })
    .await
    .unwrap();
    wait_until(|| log.lock().unwrap().len() == 20, done.clone()).await;
    let got = log.lock().unwrap().clone();
    assert_eq!(got, (0..20).collect::<Vec<u32>>());
}

// ── 4. 四语义 keyed 变体 ─────────────────────────────────────────

#[tokio::test]
async fn keyed_serial_short_circuits() {
    let ctx = Ctx::root().unwrap();
    ctx.events()
        .on::<Ping>(&ctx, &rutis::EventKey::dynamic("serial"), Bail(11))
        .unwrap();
    ctx.events()
        .on::<Ping>(&ctx, &rutis::EventKey::dynamic("serial"), Bail(22))
        .unwrap();

    let out = ctx
        .events()
        .serial(
            &ctx,
            &rutis::EventKey::dynamic("serial"),
            &Ping { value: 0 },
        )
        .await
        .unwrap();
    assert_eq!(out, Some(11)); // 注册序第一个短路

    // 空通道:None
    let out = ctx
        .events()
        .serial(&ctx, &rutis::EventKey::dynamic("empty"), &Ping { value: 0 })
        .await
        .unwrap();
    assert_eq!(out, None);
}

#[tokio::test]
async fn keyed_waterfall_veto_and_chain() {
    let ctx = Ctx::root().unwrap();

    // veto:不调 next,终态不可达
    ctx.events()
        .on_waterfall::<Ping>(&ctx, &rutis::EventKey::dynamic("wf"), Veto(99))
        .unwrap();
    let terminal = TerminalCount {
        value: 1,
        reached: hits(),
    };
    let reached = terminal.reached.clone();
    let out = ctx
        .events()
        .waterfall(
            &ctx,
            &rutis::EventKey::dynamic("wf"),
            &Ping { value: 1 },
            terminal,
        )
        .await
        .unwrap();
    assert_eq!(out, 99);
    assert_eq!(reached.load(Ordering::SeqCst), 0); // veto 生效,终态未达

    // 正常链:中间件 + 终态
    ctx.events()
        .on_waterfall::<Ping>(&ctx, &rutis::EventKey::dynamic("wf2"), AddEventValue)
        .unwrap();
    let out = ctx
        .events()
        .waterfall(
            &ctx,
            &rutis::EventKey::dynamic("wf2"),
            &Ping { value: 10 },
            TerminalCount {
                value: 5,
                reached: hits(),
            },
        )
        .await
        .unwrap();
    assert_eq!(out, 15); // 5 + 10
}

#[tokio::test]
async fn keyed_parallel_runs_all_and_aggregates() {
    let ctx = Ctx::root().unwrap();
    let h = hits();
    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("par"),
            Counting {
                hits: h.clone(),
                log: None,
                done: None,
            },
        )
        .unwrap();
    ctx.events()
        .on::<Ping>(&ctx, &rutis::EventKey::dynamic("par"), Failing)
        .unwrap();

    let out = ctx
        .events()
        .parallel(
            &ctx,
            &rutis::EventKey::dynamic("par"),
            Arc::new(Ping { value: 1 }),
        )
        .await;
    assert!(out.is_err()); // 聚合错误上抛
    assert_eq!(h.load(Ordering::SeqCst), 1); // 全部执行
}

// ── 5. once_keyed 恰好一次 + prepend 顺序 ─────────────────────────

#[tokio::test]
async fn keyed_once_fires_exactly_once() {
    let ctx = Ctx::root().unwrap();
    let h = hits();
    let done = Arc::new(Notify::new());
    ctx.events()
        .once::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("once"),
            Counting {
                hits: h.clone(),
                log: None,
                done: Some(done.clone()),
            },
        )
        .unwrap();
    for _ in 0..3 {
        ctx.events()
            .emit(
                &ctx,
                &rutis::EventKey::dynamic("once"),
                Arc::new(Ping { value: 1 }),
            )
            .expect("default event dispatch");
    }
    // 第一次派发取走 once 条目后,后续 emit 的 take_hooks 同步拿空,
    // 不再产生任务——h==1 即为终态(无"稍后再触发"的路径)。
    wait_until(|| h.load(Ordering::SeqCst) == 1, done.clone()).await;
    assert_eq!(h.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn keyed_prepend_runs_first() {
    let ctx = Ctx::root().unwrap();
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    struct Named(&'static str, Arc<Mutex<Vec<&'static str>>>, Arc<Notify>);
    impl Listener<Ping> for Named {
        fn call<'a>(
            &'a self,
            _ctx: &'a Ctx,
            _e: &'a Ping,
        ) -> BoxFuture<'a, Result<Option<u32>, CordisError>> {
            let order = self.1.clone();
            let name = self.0;
            let done = self.2.clone();
            Box::pin(async move {
                order.lock().unwrap().push(name);
                done.notify_one();
                Ok(None)
            })
        }
    }

    // 先注册 base,再 prepend front
    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("prep"),
            Named("base", order.clone(), done.clone()),
        )
        .unwrap();
    ctx.events()
        .on_opt::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("prep"),
            Named("front", order.clone(), done.clone()),
            rutis::EventOptions::default().prepend(true),
        )
        .unwrap();

    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("prep"),
            Arc::new(Ping { value: 1 }),
        )
        .expect("default event dispatch");
    wait_until(|| order.lock().unwrap().len() == 2, done.clone()).await;
    assert_eq!(order.lock().unwrap().as_slice(), ["front", "base"]);
}

// ── 6. 同名不同类型不串扰(类型烙在键里) ───────────────────────────

#[tokio::test]
async fn same_name_different_event_types_do_not_cross() {
    let ctx = Ctx::root().unwrap();
    let h_ping = hits();
    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("shared-name"),
            Counting {
                hits: h_ping.clone(),
                log: None,
                done: None,
            },
        )
        .unwrap();
    // Other 类型同名 emit:Ping 通道的注册表无此键(类型烙在键里),
    // take_hooks 同步拿空、不产生任务——无需等待,断言即确定性。
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("shared-name"),
            Arc::new(Other),
        )
        .expect("default event dispatch");
    assert_eq!(h_ping.load(Ordering::SeqCst), 0);
}

// ── 7. 监听器随注册方 fiber 卸载自动摘除(D28 keyed 路径) ──────────

#[tokio::test]
async fn keyed_listener_removed_with_owner_fiber() {
    let ctx = Ctx::root().unwrap();
    let h = hits();
    let done = Arc::new(Notify::new());

    struct Owner {
        hits: Hits,
        done: Arc<Notify>,
    }
    impl Plugin for Owner {
        fn name(&self) -> &str {
            "owner"
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let hits = self.hits.clone();
            let done = self.done.clone();
            Box::pin(async move {
                ctx.events().on::<Ping>(
                    ctx,
                    &rutis::EventKey::dynamic("owned"),
                    Counting {
                        hits,
                        log: None,
                        done: Some(done),
                    },
                )?;
                Ok(Effect::Done)
            })
        }
    }

    let owner = ctx.plugin(Owner {
        hits: h.clone(),
        done: done.clone(),
    });
    owner.clone().await.expect("owner loads");

    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("owned"),
            Arc::new(Ping { value: 1 }),
        )
        .expect("default event dispatch");
    wait_until(|| h.load(Ordering::SeqCst) == 1, done.clone()).await;

    owner.dispose().await.unwrap();
    assert_eq!(owner.state().state, FiberState::Disposed);
    // dispose().await 返回 = 卸载五步完成,监听器已从注册表摘除;
    // 再 emit 走 take_hooks 空快照的同步路径——不触发即确定性,无需等待。
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("owned"),
            Arc::new(Ping { value: 2 }),
        )
        .expect("default event dispatch");
    assert_eq!(h.load(Ordering::SeqCst), 1); // 卸载后不再收
}

// ── 8. parity 补拍:cordis events.spec 字符串事件名内核 ────────────
// cordis 原用例(events.spec.ts `ctx.on()` / `ctx.once()` / `ctx.waterfall()`)
// 原判"部分对拍(载体:字符串事件名)";keyed 落地后字符串名内核可全拍:
// 名字 → keyed 通道,语义(注册→触发、once 恰好一次、waterfall next 链)不变。
// dispose 后不再触发的另一半内核见上节 keyed_listener_removed_with_owner_fiber
// (dispose await 完成即注册表摘除,确定性)。

#[tokio::test]
async fn parity_string_event_name_on_once_waterfall() {
    let ctx = Ctx::root().unwrap();
    let h_on = hits();
    let h_once = hits();
    let on_done = Arc::new(Notify::new());
    let once_done = Arc::new(Notify::new());

    ctx.events()
        .on::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("evt/foo"),
            Counting {
                hits: h_on.clone(),
                log: None,
                done: Some(on_done.clone()),
            },
        )
        .unwrap();
    ctx.events()
        .once::<Ping>(
            &ctx,
            &rutis::EventKey::dynamic("evt/bar"),
            Counting {
                hits: h_once.clone(),
                log: None,
                done: Some(once_done.clone()),
            },
        )
        .unwrap();

    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("evt/foo"),
            Arc::new(Ping { value: 1 }),
        )
        .expect("default event dispatch");
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("evt/bar"),
            Arc::new(Ping { value: 1 }),
        )
        .expect("default event dispatch");
    ctx.events()
        .emit(
            &ctx,
            &rutis::EventKey::dynamic("evt/bar"),
            Arc::new(Ping { value: 1 }),
        )
        .expect("default event dispatch");
    wait_until(|| h_on.load(Ordering::SeqCst) == 1, on_done.clone()).await;
    wait_until(|| h_once.load(Ordering::SeqCst) == 1, once_done.clone()).await;

    // waterfall:无中间件直落终态(空链 = 注册序语义的最简内核)
    let out = ctx
        .events()
        .waterfall(
            &ctx,
            &rutis::EventKey::dynamic("evt/wf"),
            &Ping { value: 3 },
            TerminalCount {
                value: 0,
                reached: hits(),
            },
        )
        .await
        .unwrap();
    assert_eq!(out, 0);
}
