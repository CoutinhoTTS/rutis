use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

use crate::error::{aggregate_arcs, join_panic_error, panic_error, CordisError};
use crate::fiber::FiberInner;
use crate::BoxFuture;

/// 插件装配交回的清理(D18:清理可报错;闭包捕获 owned `Ctx`,不传 `&Ctx`)。
pub enum Effect {
    /// 无清理。
    Done,
    /// 同步清理。
    Disposer(Box<dyn FnOnce() -> Result<(), CordisError> + Send>),
    /// 异步清理(在任务边界执行,panic 经 JoinError 包 `PluginFailed`,D30)。
    AsyncDisposer(Box<dyn FnOnce() -> BoxFuture<'static, Result<(), CordisError>> + Send>),
    /// 多个清理,按声明序逆序(LIFO)执行。
    Many(Vec<Effect>),
}

/// Whether an owned cleanup record is waiting or currently being drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectPhase {
    Live,
    Draining,
}

/// Read-only cleanup ownership tree. Child phases follow their owning record;
/// the tree does not track which individual leaf is currently executing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EffectMeta {
    pub label: String,
    pub phase: EffectPhase,
    pub children: Vec<EffectMeta>,
}

impl EffectMeta {
    fn set_phase(&mut self, phase: EffectPhase) {
        self.phase = phase;
        for child in &mut self.children {
            child.set_phase(phase);
        }
    }
}

impl Effect {
    fn kind(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Disposer(_) => "disposer",
            Self::AsyncDisposer(_) => "async disposer",
            Self::Many(_) => "many",
        }
    }

    fn into_cleanups(self, label: String, out: &mut Vec<Cleanup>) -> EffectMeta {
        let mut children = Vec::new();
        match self {
            Effect::Done => {}
            Effect::Disposer(f) => out.push(Cleanup::Sync(f)),
            Effect::AsyncDisposer(f) => out.push(Cleanup::Async(f)),
            Effect::Many(v) => {
                for (index, effect) in v.into_iter().enumerate() {
                    let child_label = format!("{index}: {}", effect.kind());
                    children.push(effect.into_cleanups(child_label, out));
                }
            }
        }
        EffectMeta {
            label,
            phase: EffectPhase::Live,
            children,
        }
    }
}

enum Cleanup {
    Sync(Box<dyn FnOnce() -> Result<(), CordisError> + Send>),
    Async(Box<dyn FnOnce() -> BoxFuture<'static, Result<(), CordisError>> + Send>),
}

/// EffectRecord(§四):执行与清理分离;清理恰好一次,重复调用 join 同一结果;
/// 严格 LIFO 串行;单错原样、多错聚合不压平;一个清理失败不阻止其余清理。
/// 宿主 fiber(`owner`):清理进 Done 后从宿主 effects 列表自摘——长寿
/// parent 下反复注册/清理的瞬态记录不残留(0.2.1 瞬态子插件释放)。
pub(crate) struct EffectRecord {
    st: Mutex<EffectState>,
    metadata: EffectMeta,
    notify: Notify,
    owner: Option<Weak<FiberInner>>,
}

enum EffectState {
    Live(Vec<Cleanup>),
    Draining,
    Done(Option<Arc<CordisError>>),
}

impl EffectRecord {
    pub(crate) fn new(effect: Effect, label: String, owner: Weak<FiberInner>) -> Arc<Self> {
        let mut cleanups = Vec::new();
        let metadata = effect.into_cleanups(label, &mut cleanups);
        Arc::new(Self {
            st: Mutex::new(EffectState::Live(cleanups)),
            metadata,
            notify: Notify::new(),
            owner: Some(owner),
        })
    }

    pub(crate) fn snapshot(&self) -> Option<EffectMeta> {
        let phase = match &*self.st.lock().unwrap() {
            EffectState::Live(_) => EffectPhase::Live,
            EffectState::Draining => EffectPhase::Draining,
            EffectState::Done(_) => return None,
        };
        let mut metadata = self.metadata.clone();
        metadata.set_phase(phase);
        Some(metadata)
    }

    /// 排干清理。首个调用者把清理移入独立任务执行,所有调用者(含被
    /// drop 后重来的)join 同一终态——取消安全:本 future 被 drop 不影响
    /// 清理进度,不会卡死在 Draining(评审 #4)。重复调用 join 同一结果
    /// (`exactly_once_same_error` 断言 `Arc` identity)。
    pub(crate) async fn drain(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
    ) -> Result<(), Arc<CordisError>> {
        let claimed = {
            let mut st = self.st.lock().unwrap();
            match &mut *st {
                EffectState::Live(_) => {
                    let live = std::mem::replace(&mut *st, EffectState::Draining);
                    match live {
                        EffectState::Live(cleanups) => Some(cleanups),
                        _ => unreachable!(),
                    }
                }
                EffectState::Draining | EffectState::Done(_) => None,
            }
        };

        if let Some(mut cleanups) = claimed {
            let this = self.clone();
            let runner = handle.clone();
            handle.spawn(async move {
                // 清理全程 panic 兜底(评审 #7/#11):任何 panic 转为
                // PluginFailed 错误,任务必定写回 Done,不卡 Draining
                let err = run_cleanups(&mut cleanups, &runner).await;
                *this.st.lock().unwrap() = EffectState::Done(err.clone());
                // 自摘与寄存(0.2.1)先于 notify:join 返回时宿主列表已无本记录。
                // 仅当记录仍在宿主 effects 列表(提前 drain,收集者不是 fiber
                // 级卸载)才寄存错误并自摘;drain_effects 整表收走的记录由其
                // join 直接收集,不重复寄存。锁内判定与 mem::take 互斥,无竞态。
                if let Some(owner) = this.owner.as_ref().and_then(Weak::upgrade) {
                    let mut effects = owner.effects.lock().unwrap();
                    let present = effects.iter().any(|r| Arc::ptr_eq(r, &this));
                    if present {
                        effects.retain(|r| !Arc::ptr_eq(r, &this));
                        if effects.capacity() > 64 && effects.len() * 4 < effects.capacity() {
                            effects.shrink_to_fit();
                        }
                        if let Some(e) = &err {
                            // Keep the effects lock until the error is queued.
                            // drain_effects takes the whole effects list under
                            // this lock, then consumes drained_errors: it must
                            // see either this record or its queued error.
                            owner.drained_errors.lock().unwrap().push(e.clone());
                        }
                    }
                    drop(effects);
                    let weak = Arc::downgrade(&this);
                    let mut index = owner.effect_index.lock().unwrap();
                    index.retain(|entry| !Weak::ptr_eq(entry, &weak) && entry.strong_count() > 0);
                    if index.capacity() > 64 && index.len() * 4 < index.capacity() {
                        index.shrink_to_fit();
                    }
                }
                this.notify.notify_waiters();
            });
        }
        self.join().await
    }

    async fn join(&self) -> Result<(), Arc<CordisError>> {
        loop {
            let notified = self.notify.notified();
            {
                let st = self.st.lock().unwrap();
                if let EffectState::Done(e) = &*st {
                    return match e.clone() {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                }
            }
            notified.await;
        }
    }
}

/// 严格 LIFO 串行执行清理;闭包调用(`f()`)与 Future poll 两处 panic
/// 边界都捕获为 `PluginFailed`(评审 #7),一个清理失败不阻止其余清理。
async fn run_cleanups(
    cleanups: &mut Vec<Cleanup>,
    handle: &tokio::runtime::Handle,
) -> Option<Arc<CordisError>> {
    let mut errors: Vec<CordisError> = Vec::new();
    while let Some(cleanup) = cleanups.pop() {
        let result = match cleanup {
            Cleanup::Sync(f) => match std::panic::catch_unwind(AssertUnwindSafe(f)) {
                Ok(r) => r,
                Err(p) => Err(panic_error(p)),
            },
            Cleanup::Async(f) => {
                // 闭包调用本身也在任务边界内(评审 #7)
                let fut = match std::panic::catch_unwind(AssertUnwindSafe(f)) {
                    Ok(fut) => fut,
                    Err(p) => {
                        errors.push(panic_error(p));
                        continue;
                    }
                };
                let joined = handle.spawn(fut);
                match joined.await {
                    Ok(r) => r,
                    // 取消不是 panic:into_panic 在取消场景会二次 panic(评审 P2);
                    // 无论哪种,EffectRecord 都最终进 Done,不卡 Draining
                    Err(join_err) => Err(join_panic_error(join_err)),
                }
            }
        };
        if let Err(e) = result {
            errors.push(e);
        }
    }
    aggregate_arcs(errors.into_iter().map(Arc::new).collect())
}

/// 清理句柄(D28):仅表示提前释放,不需要也不应再放回 `Effect`;
/// drop 不触发清理(fiber 卸载仍兜底)。
pub struct Disposer {
    run: Option<Box<dyn FnOnce() -> BoxFuture<'static, Result<(), Arc<CordisError>>> + Send>>,
}

impl Disposer {
    pub(crate) fn new(
        run: Box<dyn FnOnce() -> BoxFuture<'static, Result<(), Arc<CordisError>>> + Send>,
    ) -> Self {
        Self { run: Some(run) }
    }

    /// 提前释放并等待清理完成(幂等:重复调用 join 同一终态)。
    pub fn dispose(mut self) -> BoxFuture<'static, Result<(), Arc<CordisError>>> {
        match self.run.take() {
            Some(run) => run(),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

impl std::fmt::Debug for Disposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Disposer")
    }
}
