mod subscriptions;
pub(crate) mod sync;
pub use subscriptions::{EventSubscription, ListenerKind};
use subscriptions::{HookMetrics, HookTable};

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use crate::ctx::Ctx;
use crate::error::{join_panic_error, panic_error, CordisError};
use crate::event::{
    CatchUnwind, DynEvent, ErasedValue, Event, EventOptions, Listener, ListenerAdapter, Terminal,
    TerminalAdapter, WaterfallAdapter, WaterfallListener,
};
use crate::fiber::{FiberInner, PluginId};
use crate::key::{EventKey, EventPattern, InstanceId, TypeKey};
use crate::{BoxFuture, Disposer, Effect};

/// waterfall 链上的擦除续延:调用下一个监听器,最终落到终态续延。
pub(crate) struct ErasedNext<'a> {
    chain: &'a [Arc<Hook<Arc<dyn ErasedWaterfallCall>>>],
    index: usize,
    ctx: &'a Ctx,
    key: &'a TypeKey,
    event: &'a DynEvent,
    terminal: &'a mut (dyn ErasedTerminal + 'a),
}

impl<'a> ErasedNext<'a> {
    pub(crate) fn invoke(self) -> BoxFuture<'a, Result<ErasedValue, CordisError>> {
        if self.index < self.chain.len() {
            let ErasedNext {
                chain,
                index,
                ctx,
                key,
                event,
                terminal,
            } = self;
            chain[index].record_call();
            chain[index].call.call(
                ctx,
                key,
                event,
                ErasedNext {
                    chain,
                    index: index + 1,
                    ctx,
                    key,
                    event,
                    terminal,
                },
            )
        } else {
            let ErasedNext {
                ctx,
                key: _,
                event,
                terminal,
                ..
            } = self;
            terminal.call(ctx, event)
        }
    }
}

pub(crate) trait ErasedCall: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: &'a TypeKey,
        e: &'a DynEvent,
    ) -> BoxFuture<'a, Result<Option<ErasedValue>, CordisError>>;
}

pub(crate) trait ErasedWaterfallCall: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: &'a TypeKey,
        e: &'a DynEvent,
        next: ErasedNext<'a>,
    ) -> BoxFuture<'a, Result<ErasedValue, CordisError>>;
}

pub(crate) trait ErasedTerminal: Send {
    fn call<'a>(
        &'a mut self,
        ctx: &'a Ctx,
        e: &'a DynEvent,
    ) -> BoxFuture<'a, Result<ErasedValue, CordisError>>;
}

/// 注册的监听器条目(泛型合一,简化 S4):`C` 为擦除后的调用句柄。
struct Hook<C> {
    call: C,
    once: bool,
    prepend: bool,
    pattern: bool,
    // Dispatch only needs the callback and flags. Keep diagnostics and owner
    // checks off the cache lines walked by the common exact async path.
    meta: Box<HookMeta>,
}

struct HookMeta {
    id: u64,
    generation: u64,
    metrics: Option<Box<HookMetrics>>,
    owner: Weak<FiberInner>,
}

impl<C> std::ops::Deref for Hook<C> {
    type Target = HookMeta;
    fn deref(&self) -> &Self::Target {
        &self.meta
    }
}

/// Which public dispatch operation produced an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    Emit,
    Serial,
    Parallel,
    Waterfall,
    BailSync,
    WaterfallSync,
}

/// A dispatch before its business listener snapshot is selected. An observed
/// instance attempt can still lose a race with subtree shutdown and be rejected.
pub struct DispatchAttempt<'a> {
    pub key: &'a TypeKey,
    pub mode: DispatchMode,
    pub emitter: PluginId,
    pub emitter_instance: InstanceId,
    pub event: &'a (dyn std::any::Any + Send + Sync),
}

type ObserverCall = dyn for<'a> Fn(&DispatchAttempt<'a>) + Send + Sync;

struct DispatchObserver {
    call: Arc<ObserverCall>,
    owner: Weak<FiberInner>,
}

/// An accepted scoped, pattern or synchronous dispatch owns fibers that must wait
/// for the callback snapshot. Dropping the owner releases all counts.
struct EventFlight(Vec<Arc<FiberInner>>);

impl EventFlight {
    fn from_owners(mut owners: Vec<Arc<FiberInner>>) -> Self {
        owners.sort_by_key(|fiber| fiber.id);
        owners.dedup_by_key(|fiber| fiber.id);
        for fiber in &owners {
            fiber.begin_event();
        }
        Self(owners)
    }

    fn new<C>(ctx: &Ctx, id: InstanceId, hooks: &[Arc<Hook<C>>]) -> Self {
        let mut owners = Vec::new();
        if let Some(owner) = ctx.instance_owner(id) {
            owners.push(owner);
        }
        if let Some(emitter) = ctx.weak_fiber().upgrade() {
            owners.push(emitter);
        }
        for hook in hooks {
            if let Some(owner) = hook.owner.upgrade() {
                owners.push(owner);
            }
        }
        Self::from_owners(owners)
    }
}

impl Drop for EventFlight {
    fn drop(&mut self) {
        for fiber in &self.0 {
            fiber.finish_event();
        }
    }
}

struct TailCleanup {
    bus: EventBus,
    key: TypeKey,
    generation: u64,
}

impl Drop for TailCleanup {
    fn drop(&mut self) {
        let mut inner = self.bus.inner.lock().unwrap();
        if matches!(inner.dispatch_tail.get(&self.key), Some((cur, _)) if *cur == self.generation) {
            inner.dispatch_tail.remove(&self.key);
            shrink_if_sparse(&mut inner.dispatch_tail);
        }
    }
}

fn insert_hook<C>(list: &mut Vec<Arc<Hook<C>>>, hook: Arc<Hook<C>>, prepend: bool) {
    if prepend {
        list.insert(0, hook);
    } else {
        list.push(hook);
    }
}

/// 稀疏即收缩(0.2.5):同 registry `shrink_if_sparse`。
fn shrink_if_sparse<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    if map.capacity() > 64 && map.len() * 4 < map.capacity() {
        map.shrink_to_fit();
    }
}
fn retain_hook<C>(list: &mut Vec<Arc<Hook<C>>>, hook: &Arc<Hook<C>>) {
    list.retain(|h| !Arc::ptr_eq(h, hook));
}

#[derive(Default)]
struct BusInner {
    /// 注册面键 = TypeKey(D33:限定名通道;非 keyed 注册 qualifier 为 None)。
    hooks: HookTable<Arc<dyn ErasedCall>>,
    wf_hooks: HookTable<Arc<dyn ErasedWaterfallCall>>,
    sync_hooks: HookTable<Arc<dyn sync::ErasedSyncCall>>,
    sync_wf_hooks: HookTable<Arc<dyn sync::ErasedSyncWaterfallCall>>,
    next_hook_id: u64,
    observers: Vec<Arc<DispatchObserver>>,
    /// 同事件键的派发尾链(D31):每次 emit 的派发任务 await 上一个,
    /// 保证同键多次 emit 按发射序执行(修 spawn 调度乱序)。
    /// 值 = (代次, 任务):任务完成后自摘;代次防旧任务误删新尾链
    /// (0.2.1:keyed 通道随实例 churn,空尾链条目不残留)。
    dispatch_tail: HashMap<TypeKey, (u64, tokio::task::JoinHandle<()>)>,
}

/// Typed event bus: four async dispatch modes, plus synchronous bail/waterfall.
///
/// 监听器经 `Ctx` 注册,自动归该 fiber 所有(D28)。
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Mutex<BusInner>>,
}

impl EventBus {
    fn ensure_bus(&self, ctx: &Ctx) -> Result<(), CordisError> {
        if Arc::ptr_eq(&self.inner, &ctx.events().inner) {
            Ok(())
        } else {
            Err(CordisError::Validation {
                issues: vec!["dispatch belongs to another event bus".into()],
            })
        }
    }
    pub fn on_pattern<E: Event>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl crate::PatternListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.on_pattern_opt(ctx, pattern, listener, EventOptions::default())
    }

    pub fn on_pattern_opt<E: Event>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl crate::PatternListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        let call: Arc<dyn ErasedCall> = Arc::new(crate::event::PatternAdapter(
            listener,
            std::marker::PhantomData,
        ));
        self.register_hook(
            ctx,
            TypeKey::of::<E>(),
            Some(pattern.into_prefixes()),
            call,
            opts,
            true,
            "event pattern",
            |inner| &mut inner.hooks,
        )
    }

    pub fn on_waterfall_pattern<E: Event>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl crate::PatternWaterfallListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.on_waterfall_pattern_opt(ctx, pattern, listener, EventOptions::default())
    }

    pub fn on_waterfall_pattern_opt<E: Event>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl crate::PatternWaterfallListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        let call: Arc<dyn ErasedWaterfallCall> = Arc::new(crate::event::PatternWaterfallAdapter(
            listener,
            std::marker::PhantomData,
        ));
        self.register_hook(
            ctx,
            TypeKey::of::<E>(),
            Some(pattern.into_prefixes()),
            call,
            opts,
            true,
            "waterfall pattern",
            |inner| &mut inner.wf_hooks,
        )
    }

    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner::default())),
        }
    }

    #[cfg(test)]
    pub(crate) fn table_counts(&self) -> (usize, usize, usize) {
        let inner = self.inner.lock().unwrap();
        (
            inner.hooks.len(),
            inner.wf_hooks.len(),
            inner.dispatch_tail.len(),
        )
    }

    #[cfg(test)]
    pub(crate) fn observer_count(&self) -> usize {
        self.inner.lock().unwrap().observers.len()
    }

    /// Observe a dispatch before business listeners are selected. The callback
    /// borrows the event for this synchronous invocation and cannot veto it.
    /// Observers are trusted code: they may inspect event payloads.
    pub fn observe_dispatch(
        &self,
        owner: &Ctx,
        observer: impl for<'a> Fn(&DispatchAttempt<'a>) + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        owner.registration_preflight()?;
        if !Arc::ptr_eq(&self.inner, &owner.events().inner) {
            return Err(CordisError::Validation {
                issues: vec!["dispatch observer belongs to another event bus".into()],
            });
        }
        let hook = Arc::new(DispatchObserver {
            call: Arc::new(observer),
            owner: owner.weak_fiber(),
        });
        let bus = self.clone();
        let shared = owner.shared().clone();
        owner.register_internal_effect_named("dispatch observer".into(), move |_, _, _| {
            bus.inner.lock().unwrap().observers.push(hook.clone());
            Ok(Effect::AsyncDisposer(Box::new(move || {
                {
                    // Serialize removal with observer selection and flight
                    // registration. Neither lock is held during callbacks.
                    let _admission = shared.admission.lock().unwrap();
                    let mut inner = bus.inner.lock().unwrap();
                    inner.observers.retain(|entry| !Arc::ptr_eq(entry, &hook));
                    if inner.observers.capacity() > 64
                        && inner.observers.len() * 4 < inner.observers.capacity()
                    {
                        inner.observers.shrink_to_fit();
                    }
                }
                Box::pin(async move {
                    if let Some(owner) = hook.owner.upgrade() {
                        owner.wait_events().await;
                    }
                    Ok(())
                })
            })))
        })
    }

    fn observe_attempt<E: Event>(
        &self,
        key: &TypeKey,
        mode: DispatchMode,
        ctx: &Ctx,
        event: &E,
    ) -> Result<(), CordisError> {
        // The first table read is the observation linearization point when no
        // observers exist. Avoid the admission lock and ancestry scan in that
        // common case; concurrent registration affects later attempts.
        if self.inner.lock().unwrap().observers.is_empty() {
            return Ok(());
        }
        let (observers, _flight) = {
            let _admission = ctx.shared().admission.lock().unwrap();
            if let Some(id) = key.instance_id() {
                ctx.registration_preflight()?;
                self.ensure_instance_ctx(ctx, id)?;
            } else if ctx.dispatch_preflight().is_err() {
                // Preserve the existing behavior of non-instance dispatches
                // from inactive contexts; they simply have no observation.
                return Ok(());
            }
            let mut ancestry = Vec::new();
            let mut current = ctx.weak_fiber().upgrade();
            while let Some(fiber) = current {
                current = fiber.parent_fiber.as_ref().and_then(Weak::upgrade);
                ancestry.push(fiber);
            }
            let mut owners = Vec::new();
            if let Some(emitter) = ancestry.first() {
                owners.push(emitter.clone());
            }
            if let Some(id) = key.instance_id() {
                if let Some(owner) = ctx.instance_owner(id) {
                    owners.push(owner);
                }
            }
            let observers = self
                .inner
                .lock()
                .unwrap()
                .observers
                .iter()
                .filter_map(|observer| {
                    let owner = observer.owner.upgrade()?;
                    if !owner.alive.load(Ordering::SeqCst)
                        || owner.closing.load(Ordering::SeqCst)
                        || !ancestry.iter().any(|fiber| Arc::ptr_eq(fiber, &owner))
                    {
                        return None;
                    }
                    owners.push(owner);
                    Some(observer.clone())
                })
                .collect::<Vec<_>>();
            (observers, EventFlight::from_owners(owners))
        };
        let attempt = DispatchAttempt {
            key,
            mode,
            emitter: ctx.plugin_id(),
            emitter_instance: ctx.instance(),
            event,
        };
        let sink = ctx.error_sink();
        for observer in observers {
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| (observer.call)(&attempt))) {
                let error = Arc::new(panic_error(panic));
                let _ = catch_unwind(AssertUnwindSafe(|| sink(error)));
            }
        }
        Ok(())
    }

    /// 注册监听器(默认追加在后)。
    pub fn on<E: Event>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        l: impl Listener<E>,
    ) -> Result<Disposer, CordisError> {
        self.add_hook(key.erased(), ctx, l, EventOptions::default(), false)
    }

    /// Register a listener visible only to the owning instance subtree.
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn on_instance<E: Event>(
        &self,
        ctx: &Ctx,
        id: InstanceId,
        listener: impl Listener<E>,
    ) -> Result<Disposer, CordisError> {
        ctx.registration_preflight()?;
        self.ensure_instance_ctx(ctx, id)?;
        self.add_hook(
            TypeKey::instance::<E>(id),
            ctx,
            listener,
            EventOptions::default(),
            false,
        )
    }

    fn ensure_instance_ctx(&self, ctx: &Ctx, id: InstanceId) -> Result<(), CordisError> {
        if !Arc::ptr_eq(&self.inner, &ctx.events().inner) {
            return Err(CordisError::InstanceOutOfScope { instance: id });
        }
        ctx.check_instance(&TypeKey::instance::<()>(id))
    }

    /// 注册监听器(带选项)。
    pub fn on_opt<E: Event>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        l: impl Listener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        self.add_hook(key.erased(), ctx, l, opts, false)
    }

    /// 注册一次性监听器:至多调用一次。
    pub fn once<E: Event>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        l: impl Listener<E>,
    ) -> Result<Disposer, CordisError> {
        self.add_hook(key.erased(), ctx, l, EventOptions::default(), true)
    }

    /// 注册带动态限定名的监听器(D33):同事件类型多通道互不串扰。
    /// name 与 `emit_keyed` 按字符串内容匹配。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn on_keyed<E: Event>(
        &self,
        ctx: &Ctx,
        name: impl Into<std::sync::Arc<str>>,
        l: impl Listener<E>,
    ) -> Result<Disposer, CordisError> {
        self.add_hook(
            TypeKey::keyed_dynamic::<E>(name),
            ctx,
            l,
            EventOptions::default(),
            false,
        )
    }

    /// 注册带动态限定名的监听器(带选项)。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn on_keyed_opt<E: Event>(
        &self,
        ctx: &Ctx,
        name: impl Into<std::sync::Arc<str>>,
        l: impl Listener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        self.add_hook(TypeKey::keyed_dynamic::<E>(name), ctx, l, opts, false)
    }

    /// 注册带动态限定名的一次性监听器。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn once_keyed<E: Event>(
        &self,
        ctx: &Ctx,
        name: impl Into<std::sync::Arc<str>>,
        l: impl Listener<E>,
    ) -> Result<Disposer, CordisError> {
        self.add_hook(
            TypeKey::keyed_dynamic::<E>(name),
            ctx,
            l,
            EventOptions::default(),
            true,
        )
    }

    /// 注册 waterfall 监听器(D17:独立注册面)。
    pub fn on_waterfall<E: Event>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        l: impl WaterfallListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.add_wf_hook(key.erased(), ctx, l, EventOptions::default(), false)
    }

    /// 注册 waterfall 监听器(带选项)。
    pub fn on_waterfall_opt<E: Event>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        l: impl WaterfallListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        self.add_wf_hook(key.erased(), ctx, l, opts, false)
    }

    /// 注册带动态限定名的 waterfall 监听器(D33)。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn on_waterfall_keyed<E: Event>(
        &self,
        ctx: &Ctx,
        name: impl Into<std::sync::Arc<str>>,
        l: impl WaterfallListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.add_wf_hook(
            TypeKey::keyed_dynamic::<E>(name),
            ctx,
            l,
            EventOptions::default(),
            false,
        )
    }

    // Keep every callback family on the same atomic effect registration path.
    #[allow(clippy::too_many_arguments)]
    fn register_hook<C: Send + Sync + 'static>(
        &self,
        ctx: &Ctx,
        key: TypeKey,
        prefixes: Option<Vec<Arc<str>>>,
        call: C,
        opts: EventOptions,
        drain: bool,
        label: &str,
        table: fn(&mut BusInner) -> &mut HookTable<C>,
    ) -> Result<Disposer, CordisError> {
        ctx.registration_preflight()?;
        if !Arc::ptr_eq(&self.inner, &ctx.events().inner) {
            return Err(CordisError::Validation {
                issues: vec!["listener belongs to another event bus".into()],
            });
        }
        ctx.check_instance(&key)?;
        if prefixes.as_ref().is_some_and(Vec::is_empty) {
            return Err(CordisError::Validation {
                issues: vec!["event pattern needs at least one prefix".into()],
            });
        }
        let pattern = prefixes.is_some();
        let owner = ctx.weak_fiber();
        let bus = self.clone();
        let access_ctx = ctx.clone();
        let shared = ctx.shared().clone();
        let drain = drain || pattern || key.instance_id().is_some();
        let label = match &prefixes {
            Some(prefixes) => format!("{label}: {} prefixes={prefixes:?}", key.describe()),
            None => format!("{label}: {}", key.describe()),
        };
        ctx.register_internal_effect_named(label, move |_, generation, _| {
            access_ctx.check_instance(&key)?;
            let hook = {
                let mut inner = bus.inner.lock().unwrap();
                inner.next_hook_id = inner
                    .next_hook_id
                    .checked_add(1)
                    .expect("event registration ids exhausted");
                let hook = Arc::new(Hook {
                    call,
                    once: opts.once,
                    prepend: opts.prepend,
                    pattern,
                    meta: Box::new(HookMeta {
                        id: inner.next_hook_id,
                        generation,
                        metrics: pattern.then(|| Box::new(HookMetrics::default())),
                        owner,
                    }),
                });
                table(&mut inner).insert(key.clone(), prefixes, hook.clone());
                hook
            };
            Ok(Effect::AsyncDisposer(Box::new(move || {
                {
                    let _admission = drain.then(|| shared.admission.lock().unwrap());
                    table(&mut bus.inner.lock().unwrap()).remove(&key, &hook);
                }
                Box::pin(async move {
                    if drain {
                        if let Some(owner) = hook.owner.upgrade() {
                            owner.wait_events().await;
                        }
                    }
                    Ok(())
                })
            })))
        })
    }

    fn add_hook<E: Event>(
        &self,
        key: TypeKey,
        ctx: &Ctx,
        l: impl Listener<E>,
        mut opts: EventOptions,
        once: bool,
    ) -> Result<Disposer, CordisError> {
        opts.once |= once;
        let call: Arc<dyn ErasedCall> = Arc::new(ListenerAdapter(l, std::marker::PhantomData));
        self.register_hook(
            ctx,
            key,
            None,
            call,
            opts,
            false,
            "event listener",
            |inner| &mut inner.hooks,
        )
    }

    fn add_wf_hook<E: Event>(
        &self,
        key: TypeKey,
        ctx: &Ctx,
        l: impl WaterfallListener<E>,
        mut opts: EventOptions,
        once: bool,
    ) -> Result<Disposer, CordisError> {
        opts.once |= once;
        let call: Arc<dyn ErasedWaterfallCall> =
            Arc::new(WaterfallAdapter(l, std::marker::PhantomData));
        self.register_hook(
            ctx,
            key,
            None,
            call,
            opts,
            false,
            "waterfall listener",
            |inner| &mut inner.wf_hooks,
        )
    }

    fn take_hooks(&self, key: &TypeKey) -> Vec<Arc<Hook<Arc<dyn ErasedCall>>>> {
        self.inner
            .lock()
            .unwrap()
            .hooks
            .take(key, key.instance_id().is_some())
    }

    fn take_instance_hooks(
        &self,
        ctx: &Ctx,
        id: InstanceId,
        key: &TypeKey,
    ) -> Result<(Vec<Arc<Hook<Arc<dyn ErasedCall>>>>, EventFlight), CordisError> {
        let _admission = ctx.shared().admission.lock().unwrap();
        ctx.registration_preflight()?;
        self.ensure_instance_ctx(ctx, id)?;
        let hooks = self.take_hooks(key);
        let flight = EventFlight::new(ctx, id, &hooks);
        Ok((hooks, flight))
    }

    fn take_wf_hooks(&self, key: &TypeKey) -> Vec<Arc<Hook<Arc<dyn ErasedWaterfallCall>>>> {
        self.inner
            .lock()
            .unwrap()
            .wf_hooks
            .take(key, key.instance_id().is_some())
    }

    #[inline]
    fn take_named_hooks<C>(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        table: fn(&mut BusInner) -> &mut HookTable<C>,
    ) -> (Vec<Arc<Hook<C>>>, Option<Arc<EventFlight>>) {
        // Keep exact dispatch's existing fast path and snapshot contract.
        {
            let mut inner = self.inner.lock().unwrap();
            if !table(&mut inner).has_patterns(key) {
                return (table(&mut inner).take(key, false), None);
            }
        }
        let _admission = ctx.shared().admission.lock().unwrap();
        let hooks = table(&mut self.inner.lock().unwrap()).take(key, false);
        let owners = hooks
            .iter()
            .filter(|hook| hook.pattern)
            .filter_map(|hook| hook.owner.upgrade())
            .collect();
        let flight = Arc::new(EventFlight::from_owners(owners));
        (hooks, Some(flight))
    }

    /// Current exact and grouped prefix registrations, in registration order.
    pub fn subscriptions(&self) -> Vec<EventSubscription> {
        let inner = self.inner.lock().unwrap();
        let mut out = Vec::new();
        inner.hooks.diagnostics(ListenerKind::Async, &mut out);
        inner
            .wf_hooks
            .diagnostics(ListenerKind::AsyncWaterfall, &mut out);
        inner.sync_hooks.diagnostics(ListenerKind::Sync, &mut out);
        inner
            .sync_wf_hooks
            .diagnostics(ListenerKind::SyncWaterfall, &mut out);
        out.sort_by_key(|entry| entry.id);
        out
    }

    /// emit:触发即忘(D16/D30)。**同事件键按发射序串行派发**(D31):
    /// 单次持锁内"取上一派发任务句柄 → spawn 新任务 → 存为尾"(原子,
    /// 防 remove/insert 两段锁在并发同键 emit 下分叉链);任务内先
    /// await 上一个,再按注册序逐个 await 监听器。监听器 panic 经
    /// CatchUnwind 捕获路由 ErrorSink,`prev.await` 正常返回,链不断;
    /// 监听器内重入 emit 同键事件仅排到链尾,不死锁。跨事件键不保证
    /// 顺序(已知边界,见 D31)。spawn 在临界区内只入队不同步执行,
    /// std Mutex 无重入,故 `take_hooks` 的锁必须已释放。
    pub fn emit<E: Event>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        e: Arc<E>,
    ) -> Result<(), CordisError> {
        self.emit_keyed_inner(key.erased(), ctx, e)
    }

    /// emit 的 keyed 通道(D33):同类型不同名互不串扰,同名共享尾链。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn emit_keyed<E: Event>(&self, ctx: &Ctx, name: impl Into<std::sync::Arc<str>>, e: Arc<E>) {
        let _ = self.emit_keyed_inner(TypeKey::keyed_dynamic::<E>(name), ctx, e);
    }

    /// Queue an event for one instance. A successful return means the event
    /// has been accepted; callback failures still go to the error sink.
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn emit_instance<E: Event>(
        &self,
        ctx: &Ctx,
        id: InstanceId,
        e: Arc<E>,
    ) -> Result<(), CordisError> {
        self.emit_keyed_inner(TypeKey::instance::<E>(id), ctx, e)
    }

    fn emit_keyed_inner<E: Event>(
        &self,
        key: TypeKey,
        ctx: &Ctx,
        e: Arc<E>,
    ) -> Result<(), CordisError> {
        self.ensure_bus(ctx)?;
        self.observe_attempt(&key, DispatchMode::Emit, ctx, e.as_ref())?;
        let _admission = key
            .instance_id()
            .map(|_| ctx.shared().admission.lock().unwrap());
        if let Some(id) = key.instance_id() {
            ctx.registration_preflight()?;
            self.ensure_instance_ctx(ctx, id)?;
        }
        let (hooks, pattern_flight) = if key.instance_id().is_some() {
            (self.take_hooks(&key), None)
        } else {
            self.take_named_hooks(ctx, &key, |inner| &mut inner.hooks)
        };
        if hooks.is_empty() {
            return Ok(()); // 不进链:无监听器不产生派发任务
        }
        let flight = key
            .instance_id()
            .map(|id| EventFlight::new(ctx, id, &hooks));
        let ctx2 = ctx.clone();
        let sink = ctx.error_sink();
        let handle = ctx.handle().clone();
        let bus = self.clone();
        let tail_key = key.clone();
        let mut inner = self.inner.lock().unwrap();
        let (gen, prev) = match inner.dispatch_tail.remove(&key) {
            Some((gen, tail)) => (gen + 1, Some(tail)),
            None => (0, None),
        };
        let tail = handle.spawn(async move {
            let _flight = flight;
            let _pattern_flight = pattern_flight;
            let _tail_cleanup = TailCleanup {
                bus,
                key: tail_key,
                generation: gen,
            };
            // 等同键上一次派发完成(链式保序)
            if let Some(prev) = prev {
                let _ = prev.await;
            }
            // 按注册序逐个 await(不并发 spawn,否则退回乱序)
            for hook in hooks {
                hook.record_call();
                let out = CatchUnwind::new(async {
                    hook.call
                        .call(&ctx2, &_tail_cleanup.key, &*e as &DynEvent)
                        .await
                })
                .await;
                match out {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => sink(Arc::new(err)),
                    Err(p) => sink(Arc::new(panic_error(p))),
                }
            }
        });
        inner.dispatch_tail.insert(key, (gen, tail));
        Ok(())
    }

    /// parallel:并发全等,聚合全部错误(JoinSet,D16)。
    pub fn parallel<'a, E: Event>(
        &self,
        ctx: &'a Ctx,
        key: &EventKey<E>,
        e: Arc<E>,
    ) -> impl std::future::Future<Output = Result<(), CordisError>> + Send + 'a {
        let bus = self.clone();
        let key = key.erased();
        async move { bus.parallel_keyed_inner(key, ctx, e).await }
    }

    /// parallel 的 keyed 通道(D33)。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub async fn parallel_keyed<E: Event>(
        &self,
        ctx: &Ctx,
        name: impl Into<std::sync::Arc<str>>,
        e: Arc<E>,
    ) -> Result<(), CordisError> {
        self.parallel_keyed_inner(TypeKey::keyed_dynamic::<E>(name), ctx, e)
            .await
    }

    /// Run instance listeners concurrently. The accepted dispatch continues
    /// to completion if the caller drops its waiting future.
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub async fn parallel_instance<E: Event>(
        &self,
        ctx: &Ctx,
        id: InstanceId,
        e: Arc<E>,
    ) -> Result<(), CordisError> {
        self.parallel(ctx, &EventKey::of().instance(id), e).await
    }

    async fn parallel_keyed_inner<E: Event>(
        &self,
        key: TypeKey,
        ctx: &Ctx,
        e: Arc<E>,
    ) -> Result<(), CordisError> {
        self.ensure_bus(ctx)?;
        self.observe_attempt(&key, DispatchMode::Parallel, ctx, e.as_ref())?;
        let (hooks, flight) = if let Some(id) = key.instance_id() {
            let (hooks, flight) = self.take_instance_hooks(ctx, id, &key)?;
            (hooks, Some(Arc::new(flight)))
        } else {
            self.take_named_hooks(ctx, &key, |inner| &mut inner.hooks)
        };
        if hooks.is_empty() {
            return Ok(());
        }
        let ctx2 = ctx.clone();
        let instance = key.instance_id().is_some();
        let run = async move {
            let _flight = flight;
            let mut set = tokio::task::JoinSet::new();
            for hook in hooks {
                let ctx3 = ctx2.clone();
                let e2 = e.clone();
                let key2 = key.clone();
                let flight2 = _flight.clone();
                set.spawn_on(
                    async move {
                        let _flight = flight2;
                        hook.record_call();
                        hook.call.call(&ctx3, &key2, &*e2 as &DynEvent).await
                    },
                    ctx2.handle(),
                );
            }
            let mut errors = Vec::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => errors.push(error),
                    Err(error) => errors.push(join_panic_error(error)),
                }
            }
            crate::error::aggregate_errors(errors).map_or(Ok(()), Err)
        };
        if instance {
            ctx.handle()
                .spawn(run)
                .await
                .unwrap_or_else(|error| Err(join_panic_error(error)))
        } else {
            run.await
        }
    }

    /// serial:顺序调用至首个短路值 `Ok(Some(v))`(TS serial 语义:
    /// 上一个监听器完成才调用下一个,按注册序短路)。
    /// 内联顺序 await,不 spawn(载荷可借用,`&E` 对齐 §二 草案);
    /// panic 经 CatchUnwind 边界转 `PluginFailed`(D30 精神)。
    pub fn serial<'a, E: Event>(
        &self,
        ctx: &'a Ctx,
        key: &EventKey<E>,
        e: &'a E,
    ) -> impl std::future::Future<Output = Result<Option<E::Value>, CordisError>> + Send + 'a {
        let bus = self.clone();
        let key = key.erased();
        async move { bus.serial_keyed_inner(key, ctx, e).await }
    }

    /// serial 的 keyed 通道(D33)。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub async fn serial_keyed<E: Event>(
        &self,
        ctx: &Ctx,
        name: impl Into<std::sync::Arc<str>>,
        e: &E,
    ) -> Result<Option<E::Value>, CordisError> {
        self.serial_keyed_inner(TypeKey::keyed_dynamic::<E>(name), ctx, e)
            .await
    }

    /// Call one instance's listeners in registration order until one bails.
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub async fn serial_instance<E: Event>(
        &self,
        ctx: &Ctx,
        id: InstanceId,
        e: &E,
    ) -> Result<Option<E::Value>, CordisError> {
        self.serial(ctx, &EventKey::of().instance(id), e).await
    }

    async fn serial_keyed_inner<E: Event>(
        &self,
        key: TypeKey,
        ctx: &Ctx,
        e: &E,
    ) -> Result<Option<E::Value>, CordisError> {
        self.ensure_bus(ctx)?;
        self.observe_attempt(&key, DispatchMode::Serial, ctx, e)?;
        let (hooks, _flight) = if let Some(id) = key.instance_id() {
            let (hooks, flight) = self.take_instance_hooks(ctx, id, &key)?;
            (hooks, Some(Arc::new(flight)))
        } else {
            self.take_named_hooks(ctx, &key, |inner| &mut inner.hooks)
        };
        for hook in hooks {
            hook.record_call();
            // The erased adapter calls the user inside its returned future;
            // one polling boundary catches both callback creation and polling.
            let outcome = CatchUnwind::new(hook.call.call(ctx, &key, e as &DynEvent)).await;
            match outcome {
                Ok(Ok(Some(boxed))) => {
                    return match boxed.downcast::<E::Value>() {
                        Ok(v) => Ok(Some(*v)),
                        Err(_) => Err(CordisError::PluginFailed(
                            "serial value type mismatch".into(),
                        )),
                    };
                }
                Ok(Ok(None)) => continue,
                Ok(Err(err)) => return Err(err),
                Err(p) => return Err(panic_error(p)),
            }
        }
        Ok(None)
    }

    /// waterfall:中间件续延(D17)。`terminal` 为调用方兜底续延;
    /// 监听器不调用 `next` 即 veto。内联 CPS 递归(见 §八:panic 向分发者传播)。
    pub fn waterfall<'a, E: Event, T: Terminal<E> + 'a>(
        &self,
        ctx: &'a Ctx,
        key: &EventKey<E>,
        e: &'a E,
        terminal: T,
    ) -> BoxFuture<'a, Result<E::Value, CordisError>> {
        self.waterfall_keyed_inner(key.erased(), ctx, e, terminal)
    }

    /// waterfall 的 keyed 通道(D33)。
    #[deprecated(
        since = "0.4.0",
        note = "use the corresponding method with EventKey instead"
    )]
    pub fn waterfall_keyed<'a, E: Event, T: Terminal<E> + 'a>(
        &self,
        ctx: &'a Ctx,
        name: impl Into<std::sync::Arc<str>>,
        e: &'a E,
        terminal: T,
    ) -> BoxFuture<'a, Result<E::Value, CordisError>> {
        self.waterfall_keyed_inner(TypeKey::keyed_dynamic::<E>(name), ctx, e, terminal)
    }

    fn waterfall_keyed_inner<'a, E: Event, T: Terminal<E> + 'a>(
        &self,
        key: TypeKey,
        ctx: &'a Ctx,
        e: &'a E,
        terminal: T,
    ) -> BoxFuture<'a, Result<E::Value, CordisError>> {
        let bus = self.clone();
        Box::pin(async move {
            bus.ensure_bus(ctx)?;
            bus.observe_attempt(&key, DispatchMode::Waterfall, ctx, e)?;
            let (chain, _flight) = if let Some(id) = key.instance_id() {
                let _admission = ctx.shared().admission.lock().unwrap();
                ctx.registration_preflight()?;
                bus.ensure_instance_ctx(ctx, id)?;
                let hooks = bus.take_wf_hooks(&key);
                let flight = EventFlight::new(ctx, id, &hooks);
                (hooks, Some(Arc::new(flight)))
            } else {
                bus.take_named_hooks(ctx, &key, |inner| &mut inner.wf_hooks)
            };
            let mut terminal: Box<dyn ErasedTerminal + 'a> =
                Box::new(TerminalAdapter(terminal, std::marker::PhantomData));
            let next = ErasedNext {
                chain: &chain,
                index: 0,
                ctx,
                key: &key,
                event: e as &DynEvent,
                terminal: terminal.as_mut(),
            };
            let boxed = next.invoke().await?;
            match boxed.downcast::<E::Value>() {
                Ok(v) => Ok(*v),
                Err(_) => Err(CordisError::PluginFailed(
                    "waterfall value type mismatch".into(),
                )),
            }
        })
    }
}

#[cfg(test)]
mod transient_tests;
