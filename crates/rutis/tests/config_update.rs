//! D32 配置热更新契约测试(design-config-hot-update-and-dynamic-events-2026-09-21 §1.7)。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, PluginFactory, TypeKey,
};

// ── 测试助手 ─────────────────────────────────────────────────────

/// 会说当前 config 的服务(装载后由消费方 get 断言其值)。
#[derive(Debug, Clone, PartialEq)]
struct ConfigSvc {
    label: String,
    n: u32,
}

/// 工厂构造的插件:provide 一个由 config 派生的 ConfigSvc,
/// 记录 apply / 清理次数与顺序。
struct ConfigPlugin {
    svc: ConfigSvc,
    apply_log: Arc<Mutex<Vec<String>>>,
    cleanups: Arc<AtomicUsize>,
    fail_apply: bool,
}

impl Plugin for ConfigPlugin {
    fn name(&self) -> &str {
        "config-plugin"
    }

    fn validate(&self) -> Result<(), CordisError> {
        if self.svc.n == u32::MAX {
            // 实例级校验失败通道(dry-run 第 3 步)
            Err(CordisError::Validation {
                issues: vec!["bad instance".into()],
            })
        } else {
            Ok(())
        }
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        let svc = self.svc.clone();
        let log = self.apply_log.clone();
        let cleanups = self.cleanups.clone();
        let fail = self.fail_apply;
        Box::pin(async move {
            log.lock().unwrap().push(format!("apply:{}", svc.label));
            if fail {
                return Err(CordisError::PluginFailed("apply boom".into()));
            }
            ctx.provide(ConfigSvc { ..svc })?;
            Ok(Effect::Disposer(Box::new(move || {
                cleanups.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })))
        })
    }
}

#[derive(Clone)]
struct TestFactory {
    apply_log: Arc<Mutex<Vec<String>>>,
    cleanups: Arc<AtomicUsize>,
}

impl TestFactory {
    fn new() -> Self {
        Self {
            apply_log: Arc::new(Mutex::new(Vec::new())),
            cleanups: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// 测试 config:n≥1000 时走指定失败通道:
/// 1000 = validate_config 失败,1001 = build 失败,u32::MAX = 实例 validate 失败。
#[derive(Debug, Clone, PartialEq)]
struct TestConfig {
    label: String,
    n: u32,
}

impl PluginFactory<TestConfig> for TestFactory {
    fn name(&self) -> &str {
        "test-factory"
    }

    fn validate_config(&self, config: &TestConfig) -> Result<(), CordisError> {
        if config.n == 1000 {
            Err(CordisError::Validation {
                issues: vec!["config invalid".into()],
            })
        } else {
            Ok(())
        }
    }

    fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
        if config.n == 1001 {
            return Err(CordisError::PluginFailed("build boom".into()));
        }
        Ok(Box::new(ConfigPlugin {
            svc: ConfigSvc {
                label: config.label.clone(),
                n: config.n,
            },
            apply_log: self.apply_log.clone(),
            cleanups: self.cleanups.clone(),
            fail_apply: false,
        }))
    }
}

fn cfg(label: &str, n: u32) -> TestConfig {
    TestConfig {
        label: label.into(),
        n,
    }
}

async fn settle(view: &FiberView) {
    view.clone().await.expect("settle");
}

// ── 1. Active 态 update:新 config 生效 + 恰好一次清理 ──────────────

#[tokio::test]
async fn update_active_applies_new_config_and_cleans_once() {
    let ctx = Ctx::root().unwrap();
    let factory = TestFactory::new();
    let view = ctx.plugin_with(factory.clone(), cfg("v1", 1));
    settle(&view).await;
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v1");
    let gen_before = view.state().generation;

    view.update(cfg("v2", 2)).await.expect("update");
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(view.state().generation, gen_before + 1);
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v2");
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().n, 2);
    // 旧代清理恰好一次,两代 apply 各一次
    assert_eq!(factory.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(
        factory.apply_log.lock().unwrap().as_slice(),
        ["apply:v1", "apply:v2"]
    );
}

// ── 2. dry-run 三种失败:Err 返回,现状不动 ─────────────────────────

#[tokio::test]
async fn update_dry_run_failure_leaves_state_untouched() {
    for (n, what) in [
        (1000u32, "validate_config"),
        (1001, "build"),
        (u32::MAX, "instance"),
    ] {
        let ctx = Ctx::root().unwrap();
        let factory = TestFactory::new();
        let view = ctx.plugin_with(factory.clone(), cfg("v1", 1));
        settle(&view).await;

        let err = view.update(cfg("bad", n)).await.expect_err(what);
        assert!(
            matches!(
                *err,
                CordisError::Validation { .. } | CordisError::PluginFailed(_)
            ),
            "{what}: unexpected {err:?}"
        );
        // 现状不动:状态/服务/清理计数不变
        assert_eq!(view.state().state, FiberState::Active);
        assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v1");
        assert_eq!(factory.cleanups.load(Ordering::SeqCst), 0);
        assert_eq!(factory.apply_log.lock().unwrap().len(), 1);
    }
}

// ── 3. Pending 态 update:依赖到位后用新 config 装载 ────────────────

#[tokio::test]
async fn update_pending_uses_new_config_when_gate_opens() {
    #[derive(Debug)]
    struct GateDep;
    let factory = TestFactory::new();
    let _f = factory.clone();

    let ctx = Ctx::root().unwrap();
    // 工厂模式 + 静态 injects 声明(D32f)
    struct GatedFactory {
        inner: TestFactory,
        injects: Vec<TypeKey>,
    }
    impl PluginFactory<TestConfig> for GatedFactory {
        fn name(&self) -> &str {
            "gated-factory"
        }
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            self.inner.build(config)
        }
    }

    let view = ctx.plugin_with(
        GatedFactory {
            inner: factory,
            injects: vec![TypeKey::of::<GateDep>()],
        },
        cfg("v1", 1),
    );
    // 依赖未到:Pending
    settle(&view).await;
    assert_eq!(view.state().state, FiberState::Pending);

    // Pending 态热更新(不该触发装载)
    tokio::time::timeout(std::time::Duration::from_secs(5), view.update(cfg("v2", 2)))
        .await
        .expect("update on Pending returns")
        .expect("update");
    settle(&view).await;
    assert_eq!(view.state().state, FiberState::Pending);

    // 门开:装载用的是新 config
    ctx.provide(GateDep).unwrap();
    settle(&view).await;
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v2");
    // v1 从未装载
    assert_eq!(_f.apply_log.lock().unwrap().as_slice(), ["apply:v2"]);
}

// ── 4. Failed 态 update:修复配置热修复 ────────────────────────────

#[tokio::test]
async fn update_failed_recovers_with_new_config() {
    // 工厂按 config 分支:n=0 造一个 apply 必失败的插件,否则正常。
    // 同一 fiber 的 factory 固定,config 才是热修复的变量。
    struct BranchFactory;
    impl PluginFactory<TestConfig> for BranchFactory {
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            if config.n == 0 {
                Ok(Box::new(BoomPlugin))
            } else {
                Ok(Box::new(ConfigPlugin {
                    svc: ConfigSvc {
                        label: config.label.clone(),
                        n: config.n,
                    },
                    apply_log: Arc::new(Mutex::new(Vec::new())),
                    cleanups: Arc::new(AtomicUsize::new(0)),
                    fail_apply: false,
                }))
            }
        }
    }
    struct BoomPlugin;
    impl Plugin for BoomPlugin {
        fn name(&self) -> &str {
            "boom"
        }
        fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            Box::pin(async move { Err(CordisError::PluginFailed("boom".into())) })
        }
    }

    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(BranchFactory, cfg("bad", 0));
    let err = view.clone().await.expect_err("load fails");
    assert!(matches!(*err, CordisError::PluginFailed(_)));
    assert_eq!(view.state().state, FiberState::Failed);

    // 热修复:换 config 走正常分支
    view.update(cfg("fixed", 5)).await.expect("recover");
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "fixed");
}

// ── 5. update 后消费者重载 ────────────────────────────────────────

#[tokio::test]
async fn update_evicts_consumers_who_reload() {
    // provider 工厂插件:提供 ConfigSvc
    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(TestFactory::new(), cfg("p1", 1));
    settle(&view).await;

    // 消费者:注入 ConfigSvc,每次 apply 记录读到的服务标签(提供方代际决定)
    let loads = Arc::new(AtomicUsize::new(0));
    struct Consumer {
        loads: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl Plugin for Consumer {
        fn name(&self) -> &str {
            "consumer"
        }
        fn injects(&self) -> &[TypeKey] {
            Box::leak(Box::new([TypeKey::of::<ConfigSvc>()]))
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let loads = self.loads.clone();
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock()
                    .unwrap()
                    .push(ctx.get::<ConfigSvc>().unwrap().label.clone());
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Effect::Done)
            })
        }
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let consumer = ctx.plugin(Consumer {
        loads: loads.clone(),
        seen: seen.clone(),
    });
    consumer.clone().await.expect("consumer loads");
    assert_eq!(seen.lock().unwrap().as_slice(), ["p1"]);

    // provider 热更新:消费者被驱逐并自动重载,读到新值
    view.update(cfg("p2", 2)).await.expect("update");
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    assert_eq!(seen.lock().unwrap().as_slice(), ["p1", "p2"]);
}

// ── 6. 并发 update × dispose / update × update ─────────────────────

#[tokio::test]
async fn concurrent_update_and_dispose_settle_correctly() {
    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(TestFactory::new(), cfg("v1", 1));
    settle(&view).await;

    // dispose 先登记:update 拒绝
    let dispose = view.dispose();
    let upd = view.update(cfg("v2", 2)).await;
    assert!(matches!(upd, Err(e) if matches!(*e, CordisError::InactiveEffect)));
    dispose.await.expect("dispose");
    assert_eq!(view.state().state, FiberState::Disposed);

    // 并发双 update:mailbox FIFO,后者覆盖,两次 join 均落定
    let ctx2 = Ctx::root().unwrap();
    let view2 = ctx2.plugin_with(TestFactory::new(), cfg("v1", 1));
    settle(&view2).await;
    let a = view2.update(cfg("v2", 2));
    let b = view2.update(cfg("v3", 3));
    let (a, b) = tokio::join!(a, b);
    a.expect("first update");
    b.expect("second update");
    assert_eq!(ctx2.get::<ConfigSvc>().unwrap().label, "v3");
}

// ── 7. 工厂 injects 门控 ───────────────────────────────────────────

#[tokio::test]
async fn factory_injects_gate_plugin_until_ready() {
    #[derive(Debug)]
    struct Need;
    struct F {
        injects: Vec<TypeKey>,
    }
    impl PluginFactory<TestConfig> for F {
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            Ok(Box::new(ConfigPlugin {
                svc: ConfigSvc {
                    label: config.label.clone(),
                    n: config.n,
                },
                apply_log: Arc::new(Mutex::new(Vec::new())),
                cleanups: Arc::new(AtomicUsize::new(0)),
                fail_apply: false,
            }))
        }
    }
    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(
        F {
            injects: vec![TypeKey::of::<Need>()],
        },
        cfg("v1", 1),
    );
    settle(&view).await;
    assert_eq!(view.state().state, FiberState::Pending);
    assert!(ctx.get::<ConfigSvc>().is_none());

    ctx.provide(Need).unwrap();
    settle(&view).await;
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v1");
}

// ── 8. 静态 fiber / 类型不匹配的 update 报错 ───────────────────────

#[tokio::test]
async fn update_rejects_static_fiber_and_type_mismatch() {
    struct StaticPlugin;
    impl Plugin for StaticPlugin {
        fn name(&self) -> &str {
            "static"
        }
        fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            Box::pin(async move { Ok(Effect::Done) })
        }
    }
    let ctx = Ctx::root().unwrap();
    let static_view = ctx.plugin(StaticPlugin);
    static_view.clone().await.expect("loads");
    // 静态 fiber:无工厂
    let err = static_view.update(cfg("x", 1)).await.err().unwrap();
    assert!(matches!(*err, CordisError::Validation { .. }));

    // 工厂 fiber + 错误 config 类型
    let fview = ctx.plugin_with(TestFactory::new(), cfg("v1", 1));
    settle(&fview).await;
    #[derive(Debug, Clone, PartialEq)]
    struct OtherConfig;
    let err = fview.update(OtherConfig).await.err().unwrap();
    assert!(matches!(*err, CordisError::Validation { .. }));
    // 现状不动
    assert_eq!(fview.state().state, FiberState::Active);
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v1");
}

// ── 9. 依赖驱动重载用当前 config 重造实例 ──────────────────────────

#[tokio::test]
async fn dependency_reload_rebuilds_with_current_config() {
    // provider(普通 provide)+ 工厂消费者:apply 里读 config 决定服务值
    #[derive(Debug)]
    struct Dep;
    struct RecordFactory {
        seen: Arc<Mutex<Vec<u32>>>,
        injects: Vec<TypeKey>,
    }
    struct RecordingPlugin {
        n: u32,
        seen: Arc<Mutex<Vec<u32>>>,
    }
    impl Plugin for RecordingPlugin {
        fn name(&self) -> &str {
            "recording"
        }
        fn injects(&self) -> &[TypeKey] {
            Box::leak(Box::new([TypeKey::of::<Dep>()]))
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let n = self.n;
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(n);
                ctx.provide(ConfigSvc {
                    label: format!("gen{n}"),
                    n,
                })?;
                Ok(Effect::Done)
            })
        }
    }
    impl PluginFactory<TestConfig> for RecordFactory {
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            Ok(Box::new(RecordingPlugin {
                n: config.n,
                seen: self.seen.clone(),
            }))
        }
    }

    let ctx = Ctx::root().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let view = ctx.plugin_with(
        RecordFactory {
            seen: seen.clone(),
            injects: vec![TypeKey::of::<Dep>()],
        },
        cfg("v1", 1),
    );
    // 依赖未到:Pending
    settle(&view).await;
    assert_eq!(seen.lock().unwrap().len(), 0);

    let d1 = ctx.provide(Dep).unwrap();
    settle(&view).await;
    assert_eq!(seen.lock().unwrap().as_slice(), [1]);

    // 依赖摘除 → 驱逐回 Pending;重新提供 → 重载仍用当前 config(n=1)
    d1.dispose().await.expect("evict");
    settle(&view).await;
    assert_eq!(view.state().state, FiberState::Pending);
    let d2 = ctx.provide(Dep).unwrap();
    settle(&view).await;
    assert_eq!(seen.lock().unwrap().as_slice(), [1, 1]);
    drop(d2);

    // update 后的依赖驱动重载 → 用新 config
    view.update(cfg("v2", 2)).await.expect("update");
    assert_eq!(seen.lock().unwrap().as_slice(), [1, 1, 2]);
}

// ── 10. current_config 快照 ───────────────────────────────────────

#[tokio::test]
async fn current_config_returns_snapshot() {
    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(TestFactory::new(), cfg("v1", 1));
    settle(&view).await;
    assert_eq!(view.current_config::<TestConfig>().unwrap().label, "v1");
    view.update(cfg("v2", 2)).await.expect("update");
    assert_eq!(view.current_config::<TestConfig>().unwrap().label, "v2");
    // 类型不符 → None
    assert!(view.current_config::<String>().is_none());
}

// ── 状态等待助手(watch,无 sleep) ─────────────────────────────
// 注意:`want` 必须是受控稳定的中间态或终态(本文件用 gate 保持 Loading/
// Unloading 窗口)——watch 是 last-value 语义,等一个瞬时翻过的状态会永等
// (不会假通过,只会挂起)。

async fn wait_until_state(view: &FiberView, want: FiberState) {
    let mut rx = view.watch();
    loop {
        if rx.borrow().state == want {
            return;
        }
        rx.changed().await.expect("fiber driver alive");
    }
}

// ── 11. Loading 态 update(apply 运行中)──────────────────────────
// 状态矩阵 Loading 行 + §1.5 "update × 运行中 apply":cancel 使 apply
// 协作退出 → 卸载重载新 config。

#[tokio::test]
async fn update_during_loading_converges_to_new_config() {
    use tokio::sync::Notify;

    // apply 等门:制造 Loading 窗口(v1 的 apply 在此挂起等待,直到
    // update 的 cancel_current 触发协作退出;v2 代即时完成)。
    struct SlowFactory {
        gate: Arc<Notify>,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl PluginFactory<TestConfig> for SlowFactory {
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            let gate = self.gate.clone();
            let seen = self.seen.clone();
            let label = config.label.clone();
            Ok(Box::new(SlowPlugin { gate, seen, label }))
        }
    }
    struct SlowPlugin {
        gate: Arc<Notify>,
        seen: Arc<Mutex<Vec<String>>>,
        label: String,
    }
    impl Plugin for SlowPlugin {
        fn name(&self) -> &str {
            "slow"
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let gate = self.gate.clone();
            let seen = self.seen.clone();
            let label = self.label.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(format!("apply:{label}"));
                let _ = ctx.provide(ConfigSvc {
                    label: label.clone(),
                    n: 1,
                });
                // 只有 v1 慢(制造 Loading 窗口);v2 即时完成——
                // 测试主线程无法在 update 的 await 期间发 gate 信号。
                if label != "v1" {
                    return Ok(Effect::Done);
                }
                tokio::select! {
                    // 协作取消:update 的 cancel_current 让这里返回
                    _ = ctx.cancelled() => Err(CordisError::PluginFailed("cancelled".into())),
                    _ = gate.notified() => Ok(Effect::Done),
                }
            })
        }
    }

    let ctx = Ctx::root().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(Notify::new());
    let view = ctx.plugin_with(
        SlowFactory {
            gate: gate.clone(),
            seen: seen.clone(),
        },
        cfg("v1", 1),
    );
    // 等装载进入 Loading(apply 已启动)
    wait_until_state(&view, FiberState::Loading).await;
    assert_eq!(seen.lock().unwrap().as_slice(), ["apply:v1"]);

    // Loading 态热更新:cancel → apply v1 协作退出 → 重载新 config
    // (v2 的 apply 即时完成,无需 gate 信号)
    view.update(cfg("v2", 2))
        .await
        .expect("update during loading");
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(seen.lock().unwrap().as_slice(), ["apply:v1", "apply:v2"]);
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v2");
}

// ── 12. Unloading 态 update(清理进行中)─────────────────────────
// 状态矩阵 Unloading 行:update 排队,驱动串行处理完卸载后收敛新 config。

#[tokio::test]
async fn update_during_unloading_converges_to_new_config() {
    use tokio::sync::Notify;

    #[derive(Debug)]
    struct Dep;
    let drain_gate = Arc::new(Notify::new());

    struct GatedFactory {
        gate: Arc<Notify>,
        injects: Vec<TypeKey>,
    }
    impl PluginFactory<TestConfig> for GatedFactory {
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            let gate = self.gate.clone();
            let label = config.label.clone();
            Ok(Box::new(GatedPlugin { gate, label }))
        }
    }
    struct GatedPlugin {
        gate: Arc<Notify>,
        label: String,
    }
    impl Plugin for GatedPlugin {
        fn name(&self) -> &str {
            "gated"
        }
        fn injects(&self) -> &[TypeKey] {
            Box::leak(Box::new([TypeKey::of::<Dep>()]))
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let gate = self.gate.clone();
            let label = self.label.clone();
            Box::pin(async move {
                ctx.provide(ConfigSvc { label, n: 1 })?;
                Ok(Effect::AsyncDisposer(Box::new(move || {
                    let gate = gate.clone();
                    Box::pin(async move {
                        gate.notified().await;
                        Ok(())
                    })
                })))
            })
        }
    }

    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(
        GatedFactory {
            gate: drain_gate.clone(),
            injects: vec![TypeKey::of::<Dep>()],
        },
        cfg("v1", 1),
    );
    let d = ctx.provide(Dep).unwrap();
    view.clone().await.expect("loads");
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v1");

    // 驱逐 → 卸载(慢清理卡在 Unloading)。dispose future 必须 spawn 跑,
    // 否则 evict 流程未启动、Unloading 永不出现。
    let d_dispose = d.dispose();
    let evicted = tokio::spawn(async move {
        let _ = d_dispose.await;
    });
    wait_until_state(&view, FiberState::Unloading).await;

    // Unloading 态 update:排队等清理完成后收敛新 config
    drain_gate.notify_one();
    let _ = evicted.await;
    view.update(cfg("v2", 2))
        .await
        .expect("update during unloading");
    assert_eq!(view.state().state, FiberState::Pending); // 依赖仍缺(Dep 已摘)
                                                         // 重新提供依赖 → 用新 config 装载
    let _d2 = ctx.provide(Dep).unwrap();
    view.clone().await.expect("reload");
    assert_eq!(ctx.get::<ConfigSvc>().unwrap().label.clone(), "v2");
}

// ── 13. update × 驱逐并发:双方收敛 ────────────────────────────
// §1.5 "update × 驱逐":provider 换代驱逐消费者,消费者自身也在热更新,
// mailbox FIFO 收敛到"双方都用各自当前 config 重载"。

#[tokio::test]
async fn concurrent_provider_reload_and_consumer_update_settle() {
    #[derive(Debug, Clone)]
    struct Dep(String);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // provider 工厂:config 决定 Dep 的值(热更新换值)
    struct ProviderFactory;
    impl PluginFactory<TestConfig> for ProviderFactory {
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            let value = config.label.clone();
            Ok(Box::new(DepProvider { value }))
        }
    }
    struct DepProvider {
        value: String,
    }
    impl Plugin for DepProvider {
        fn name(&self) -> &str {
            "dep-provider"
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let dep = Dep(self.value.clone());
            Box::pin(async move {
                ctx.provide(dep)?;
                Ok(Effect::Done)
            })
        }
    }

    // 消费者工厂:注入 Dep,apply 记录 (自身 config label, Dep 值)
    struct ConsumerFactory {
        seen: Arc<Mutex<Vec<String>>>,
        injects: Vec<TypeKey>,
    }
    impl PluginFactory<TestConfig> for ConsumerFactory {
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            let seen = self.seen.clone();
            let label = config.label.clone();
            Ok(Box::new(Consumer { seen, label }))
        }
    }
    struct Consumer {
        seen: Arc<Mutex<Vec<String>>>,
        label: String,
    }
    impl Plugin for Consumer {
        fn name(&self) -> &str {
            "consumer"
        }
        fn injects(&self) -> &[TypeKey] {
            Box::leak(Box::new([TypeKey::of::<Dep>()]))
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let seen = self.seen.clone();
            let mine = self.label.clone();
            let dep = ctx.get::<Dep>().unwrap();
            Box::pin(async move {
                seen.lock().unwrap().push(format!("{mine}/{}", dep.0));
                Ok(Effect::Done)
            })
        }
    }

    let ctx = Ctx::root().unwrap();
    let provider = ctx.plugin_with(ProviderFactory, cfg("p1", 1));
    let consumer = ctx.plugin_with(
        ConsumerFactory {
            seen: seen.clone(),
            injects: vec![TypeKey::of::<Dep>()],
        },
        cfg("c1", 1),
    );
    consumer.clone().await.expect("consumer loads on p1");
    assert_eq!(seen.lock().unwrap().as_slice(), ["c1/p1"]);

    // 并发:provider 热更新(换代驱逐消费者)+ 消费者自身热更新
    let pu = provider.update(cfg("p2", 2));
    let cu = consumer.update(cfg("c2", 2));
    let (a, b) = tokio::join!(pu, cu);
    a.expect("provider update");
    b.expect("consumer update");
    // 消费者最终用自身新 config(c2)重载,且读到 provider 新代值(p2)
    wait_until_state(&consumer, FiberState::Active).await;
    consumer.clone().await.expect("consumer settled");
    let log = seen.lock().unwrap().clone();
    assert_eq!(
        log.last().unwrap(),
        "c2/p2",
        "final reload reads new provider gen: {log:?}"
    );
}

// ── 15. build panic → Failed,驱动存活、join 不挂起 ──────────────
// 评审 #1:build 是用户回调,panic 边界与 validate 对称。

#[tokio::test]
async fn factory_build_panic_fails_load_without_hanging() {
    struct PanicFactory;
    impl PluginFactory<TestConfig> for PanicFactory {
        fn build(&self, _config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            panic!("build boom");
        }
    }

    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin_with(PanicFactory, cfg("v1", 1));
    // settle 返回错误(而非永等):panic 被转为装载失败
    let err = view.clone().await.expect_err("build panic fails load");
    assert!(
        matches!(*err, CordisError::PluginFailed(ref m) if m.to_string().contains("build boom")),
        "{err:?}"
    );
    assert_eq!(view.state().state, FiberState::Failed);
    // 驱动仍存活:restart 可重试(build 再 panic 则再 Failed,不挂起)
    let err2 = view.restart().await.expect_err("restart fails again");
    assert!(matches!(*err2, CordisError::PluginFailed(_)));
    assert_eq!(view.state().state, FiberState::Failed);
}

// ── 19. Failed 粘性:依赖摘除保持 Failed,依赖恢复重试装载(审计 #2) ──
// cordis:FAILED + 依赖摘除 → epoch 早退,状态保持 Failed、错误持续可见;
// 依赖恢复 → epoch 变化 → reload。此前 rutis 把 Failed 算作 loaded,
// 摘除时降级 Pending 把错误藏进 settle 通道——已修。

#[tokio::test]
async fn failed_stays_failed_when_dependency_removed_but_retries_when_restored() {
    use rutis::TypeKey;

    #[derive(Debug)]
    struct FlakyDep;
    // 工厂声明依赖 FlakyDep;build 只在 n==1 时失败(apply 失败路径)
    struct FlakyFactory2;
    impl PluginFactory<TestConfig> for FlakyFactory2 {
        fn injects(&self) -> &[TypeKey] {
            Box::leak(Box::new([TypeKey::of::<FlakyDep>()]))
        }
        fn build(&self, config: &TestConfig) -> Result<Box<dyn Plugin>, CordisError> {
            if config.n == 1 {
                Ok(Box::new(BoomPlugin))
            } else {
                Ok(Box::new(ConfigPlugin {
                    svc: ConfigSvc {
                        label: config.label.clone(),
                        n: config.n,
                    },
                    apply_log: Arc::new(Mutex::new(Vec::new())),
                    cleanups: Arc::new(AtomicUsize::new(0)),
                    fail_apply: false,
                }))
            }
        }
    }
    struct BoomPlugin;
    impl Plugin for BoomPlugin {
        fn name(&self) -> &str {
            "boom"
        }
        fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            Box::pin(async move { Err(CordisError::PluginFailed("boom".into())) })
        }
    }

    let ctx = Ctx::root().unwrap();
    let dep = ctx.provide(FlakyDep).unwrap();
    let view = ctx.plugin_with(FlakyFactory2, cfg("bad", 1));
    // 依赖满足但 apply 失败 → Failed
    view.clone().await.expect_err("loads then fails");
    assert_eq!(view.state().state, FiberState::Failed);

    // 依赖摘除:保持 Failed(错误仍可见,不降级 Pending)。
    // dep.dispose().await 返回 = 驱逐流程 join 完消费者 RefreshDepsJoin,
    // 状态已收敛——直接断言,无需等待。
    dep.dispose().await.expect("evict dep");
    assert_eq!(
        view.state().state,
        FiberState::Failed,
        "FAILED is sticky on dep removal"
    );
    let err = view
        .clone()
        .await
        .expect_err("settle still reports the load error");
    assert!(matches!(*err, CordisError::PluginFailed(_)));

    // 依赖恢复:Failed 重试装载(仍失败 → 仍 Failed,错误持续)
    let _dep2 = ctx.provide(FlakyDep).unwrap();
    view.clone().await.expect_err("retries and fails again");
    assert_eq!(view.state().state, FiberState::Failed);
}
