//! External queue storage; native dispatch algorithms remain in `EventBus`.

use std::any::TypeId;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use super::sync::{ErasedSyncCall, ErasedSyncNext, ErasedSyncWaterfallCall};
use super::*;

/// A trusted queue adapter configured before mounting plugins.
///
/// `handles` must be stable for this bus's lifetime and covers all channels of
/// an event type, including patterns and instance keys. Unhandled types retain
/// native storage. All methods run outside framework locks and may return errors.
///
/// Registration must commit one entry or return an error without retaining it.
/// The returned effect removes that entry. `select` returns matching entries in
/// the queue's order, without invoking callbacks or consuming `once`. Rutis
/// validates and claims that snapshot before running its native algorithm.
/// Adapters must serialize registration, selection and removal in their queue.
/// This interface contains no transport or language-specific types.
pub trait EventQueue: Send + Sync + 'static {
    fn handles(&self, event: TypeId) -> bool;
    fn register(&self, registration: EventRegistration) -> Result<Effect, CordisError>;
    fn select(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        kind: ListenerKind,
    ) -> Result<Vec<EventRegistration>, CordisError>;
}

#[derive(Clone)]
pub(super) enum Callback {
    Async(Arc<Hook<Arc<dyn ErasedCall>>>),
    AsyncWaterfall(Arc<Hook<Arc<dyn ErasedWaterfallCall>>>),
    Sync(Arc<Hook<Arc<dyn ErasedSyncCall>>>),
    SyncWaterfall(Arc<Hook<Arc<dyn ErasedSyncWaterfallCall>>>),
}

/// A registration retained by an external queue. It cannot call user code until
/// selected. Clones share removal and once state; retaining one does not keep
/// an unloaded plugin callable.
#[derive(Clone)]
pub struct EventRegistration(Arc<Registration>);

struct Registration {
    info: EventSubscription,
    callback: Callback,
    bus: Weak<Mutex<BusInner>>,
    owner: Weak<FiberInner>,
    active: AtomicBool,
    claimed: AtomicBool,
}

/// One accepted callback. Its lifetime retains the native dispatch flight where
/// required (sync, pattern and instance listeners). Drop it after the dispatch;
/// retaining it also retains that flight. Exact async snapshots keep their
/// existing unload behavior. Each handle may be invoked once. It invokes one
/// callback, not a new dispatch: a foreign dispatcher owns its dispatch-level
/// reentry policy, just as `EventBus` owns that policy for native dispatch.
pub struct EventListener {
    callback: Callback,
    ctx: Ctx,
    key: TypeKey,
    invoked: bool,
}

fn invalid(message: &str) -> CordisError {
    CordisError::Validation {
        issues: vec![message.into()],
    }
}

impl EventRegistration {
    /// Registration-time metadata. For current selection/invocation counters,
    /// use `EventBus::subscriptions`.
    pub fn subscription(&self) -> &EventSubscription {
        &self.0.info
    }

    /// Match a concrete channel, including grouped prefixes. This does not
    /// claim once, inspect lifecycle state or execute a callback.
    pub fn matches(&self, key: &TypeKey, kind: ListenerKind) -> bool {
        let info = &self.0.info;
        info.kind == kind
            && if info.prefixes.is_empty() {
                info.key == *key
            } else {
                info.key.type_id() == key.type_id()
                    && key.instance_id().is_none()
                    && key.name().is_some_and(|name| {
                        info.prefixes.iter().any(|p| name.starts_with(p.as_ref()))
                    })
            }
    }

    /// Claim a listener selected by a foreign dispatcher. For native dispatch,
    /// `EventBus` claims the entire returned snapshot before any invocation.
    /// A foreign dispatcher chooses its own selection/once point, then calls
    /// this method before invoking the proxy. An already removed/claimed entry
    /// returns `None` and must be skipped.
    pub fn select(&self, ctx: &Ctx, key: &TypeKey) -> Result<Option<EventListener>, CordisError> {
        let _admission = ctx.shared().admission.lock().unwrap();
        self.validate(ctx, key, self.0.info.kind)?;
        self.claim(ctx, key)
    }

    fn validate(&self, ctx: &Ctx, key: &TypeKey, kind: ListenerKind) -> Result<(), CordisError> {
        if !Weak::ptr_eq(&self.0.bus, &Arc::downgrade(&ctx.events().inner)) {
            return Err(invalid(
                "event queue returned a registration from another bus",
            ));
        }
        if !self.matches(key, kind) {
            return Err(invalid("event queue returned a mismatched registration"));
        }
        Ok(())
    }

    fn claim(&self, ctx: &Ctx, key: &TypeKey) -> Result<Option<EventListener>, CordisError> {
        let sync = matches!(
            self.0.info.kind,
            ListenerKind::Sync | ListenerKind::SyncWaterfall
        );
        if sync || key.instance_id().is_some() {
            ctx.registration_preflight()?;
            ctx.check_instance(key)?;
        }
        if !self.0.active.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let live = sync || !self.0.info.prefixes.is_empty() || key.instance_id().is_some();
        let callback = match &self.0.callback {
            Callback::Async(hook) => self.accept(ctx, key, hook, live).map(Callback::Async),
            Callback::AsyncWaterfall(hook) => self
                .accept(ctx, key, hook, live)
                .map(Callback::AsyncWaterfall),
            Callback::Sync(hook) => self.accept(ctx, key, hook, live).map(Callback::Sync),
            Callback::SyncWaterfall(hook) => self
                .accept(ctx, key, hook, live)
                .map(Callback::SyncWaterfall),
        };
        Ok(callback.map(|callback| EventListener {
            callback,
            ctx: ctx.clone(),
            key: key.clone(),
            invoked: false,
        }))
    }

    fn accept<C: Clone>(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        hook: &Arc<Hook<C>>,
        live: bool,
    ) -> Option<Arc<Hook<C>>> {
        if live && !hook.live() {
            return None;
        }
        if hook.once && self.0.claimed.swap(true, Ordering::SeqCst) {
            return None;
        }
        if let Some(metrics) = &hook.metrics {
            metrics.selected.fetch_add(1, Ordering::Relaxed);
        }
        let flight = live.then(|| {
            let mut owners = self.0.owner.upgrade().into_iter().collect::<Vec<_>>();
            if key.instance_id().is_some()
                || matches!(
                    self.0.info.kind,
                    ListenerKind::Sync | ListenerKind::SyncWaterfall
                )
            {
                owners.extend(ctx.weak_fiber().upgrade());
                owners.extend(key.instance_id().and_then(|id| ctx.instance_owner(id)));
            }
            Arc::new(EventFlight::from_owners(owners))
        });
        Some(Arc::new(Hook {
            call: hook.call.clone(),
            once: hook.once,
            prepend: hook.prepend,
            pattern: hook.pattern,
            meta: Box::new(HookMeta {
                id: hook.id,
                generation: hook.generation,
                owner: hook.owner.clone(),
                metrics: hook.metrics.clone(),
                _flight: flight,
            }),
        }))
    }
}

impl EventListener {
    fn begin(&mut self) -> Result<(), CordisError> {
        if std::mem::replace(&mut self.invoked, true) {
            return Err(invalid("selected event listener has already been invoked"));
        }
        Ok(())
    }

    pub fn call<'a>(
        &'a mut self,
        event: &'a DynEvent,
    ) -> BoxFuture<'a, Result<Option<ErasedValue>, CordisError>> {
        Box::pin(async move {
            self.begin()?;
            let Callback::Async(hook) = &self.callback else {
                return Err(invalid("expected async event listener"));
            };
            hook.record_call();
            match CatchUnwind::new(hook.call.call(&self.ctx, &self.key, event)).await {
                Ok(result) => result,
                Err(panic) => Err(panic_error(panic)),
            }
        })
    }

    pub fn call_sync(&mut self, event: &DynEvent) -> Result<Option<ErasedValue>, CordisError> {
        self.begin()?;
        let Callback::Sync(hook) = &self.callback else {
            return Err(invalid("expected synchronous event listener"));
        };
        hook.record_call();
        super::sync::user_call(&self.ctx, || hook.call.call(&self.ctx, &self.key, event))
    }

    pub fn call_waterfall<'a>(
        &'a mut self,
        event: &'a DynEvent,
        next: ErasedNext<'a>,
    ) -> BoxFuture<'a, Result<ErasedValue, CordisError>> {
        Box::pin(async move {
            self.begin()?;
            let Callback::AsyncWaterfall(hook) = &self.callback else {
                return Err(invalid("expected async waterfall listener"));
            };
            hook.record_call();
            hook.call.call(&self.ctx, &self.key, event, next).await
        })
    }

    pub fn call_waterfall_sync<'a>(
        &'a mut self,
        event: &'a DynEvent,
        next: ErasedSyncNext<'a>,
    ) -> Result<ErasedValue, CordisError> {
        self.begin()?;
        let Callback::SyncWaterfall(hook) = &self.callback else {
            return Err(invalid("expected synchronous waterfall listener"));
        };
        hook.record_call();
        super::sync::user_call(&self.ctx, || {
            hook.call.call(&self.ctx, &self.key, event, next)
        })
    }
}

pub(super) trait QueueCallback: Clone + Send + Sync + 'static {
    const KIND: ListenerKind;
    fn registration(hook: Arc<Hook<Self>>) -> Callback;
    fn selected(listener: EventListener) -> Arc<Hook<Self>>;
}

macro_rules! callback {
    ($trait:ident, $kind:ident) => {
        impl QueueCallback for Arc<dyn $trait> {
            const KIND: ListenerKind = ListenerKind::$kind;
            fn registration(hook: Arc<Hook<Self>>) -> Callback {
                Callback::$kind(hook)
            }
            fn selected(listener: EventListener) -> Arc<Hook<Self>> {
                let Callback::$kind(hook) = listener.callback else {
                    unreachable!("validated callback family")
                };
                hook
            }
        }
    };
}
callback!(ErasedCall, Async);
callback!(ErasedWaterfallCall, AsyncWaterfall);
callback!(ErasedSyncCall, Sync);
callback!(ErasedSyncWaterfallCall, SyncWaterfall);

impl EventBus {
    pub(crate) fn with_queue(queue: Arc<dyn EventQueue>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner::default())),
            queue: Some(queue),
        }
    }

    pub(super) fn queue_for(&self, key: &TypeKey) -> Option<&Arc<dyn EventQueue>> {
        self.queue
            .as_ref()
            .filter(|queue| queue.handles(key.type_id()))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn register_queued<C: QueueCallback>(
        &self,
        queue: Arc<dyn EventQueue>,
        ctx: &Ctx,
        key: TypeKey,
        prefixes: Option<Vec<Arc<str>>>,
        call: C,
        opts: EventOptions,
        drain: bool,
        label: String,
    ) -> Result<Disposer, CordisError> {
        let bus = self.clone();
        let owner = ctx.weak_fiber();
        let shared = ctx.shared().clone();
        ctx.try_effect_named(label, || {
            let fiber = owner
                .upgrade()
                .expect("effect factory owns its fiber lease");
            let generation = fiber.snapshot_rx.borrow().generation;
            let pattern = prefixes.is_some();
            let registration = {
                let mut inner = bus.inner.lock().unwrap();
                inner.next_hook_id = inner
                    .next_hook_id
                    .checked_add(1)
                    .expect("event registration ids exhausted");
                let id = inner.next_hook_id;
                let hook = Arc::new(Hook {
                    call,
                    once: opts.once,
                    prepend: opts.prepend,
                    pattern,
                    meta: Box::new(HookMeta {
                        id,
                        generation,
                        owner: owner.clone(),
                        metrics: pattern.then(|| Arc::new(HookMetrics::default())),
                        _flight: None,
                    }),
                });
                let registration = EventRegistration(Arc::new(Registration {
                    info: EventSubscription {
                        id,
                        key,
                        prefixes: prefixes.unwrap_or_default(),
                        owner: fiber.id,
                        kind: C::KIND,
                        once: opts.once,
                        prepend: opts.prepend,
                        selected: pattern.then_some(0),
                        invoked: pattern.then_some(0),
                    },
                    callback: C::registration(hook),
                    bus: Arc::downgrade(&bus.inner),
                    owner: owner.clone(),
                    active: AtomicBool::new(true),
                    claimed: AtomicBool::new(false),
                }));
                inner.queued.insert(id, registration.clone());
                registration
            };
            let cleanup =
                match catch_unwind(AssertUnwindSafe(|| queue.register(registration.clone()))) {
                    Ok(Ok(cleanup)) => cleanup,
                    result => {
                        registration.0.active.store(false, Ordering::SeqCst);
                        bus.inner
                            .lock()
                            .unwrap()
                            .queued
                            .remove(&registration.0.info.id);
                        return Err(match result {
                            Ok(Err(error)) => error,
                            Err(panic) => panic_error(panic),
                            _ => unreachable!(),
                        });
                    }
                };
            // Stop selection, begin external cleanup, then wait for accepted
            // callbacks. Cleanup can be what releases a waiting callback.
            Ok(Effect::Many(vec![
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        if drain {
                            if let Some(owner) = owner.upgrade() {
                                owner.wait_events().await;
                            }
                        }
                        Ok(())
                    })
                })),
                cleanup,
                Effect::Disposer(Box::new(move || {
                    let _admission = shared.admission.lock().unwrap();
                    registration.0.active.store(false, Ordering::SeqCst);
                    let mut inner = bus.inner.lock().unwrap();
                    inner.queued.remove(&registration.0.info.id);
                    shrink_if_sparse(&mut inner.queued);
                    Ok(())
                })),
            ]))
        })
    }

    pub(super) fn queue_snapshot<C: QueueCallback>(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
    ) -> Option<Result<(Vec<Arc<Hook<C>>>, EventFlight), CordisError>> {
        let queue = self.queue_for(key)?;
        Some((|| {
            let preflight = || {
                if key.instance_id().is_some()
                    || matches!(C::KIND, ListenerKind::Sync | ListenerKind::SyncWaterfall)
                {
                    ctx.registration_preflight()?;
                    ctx.check_instance(key)?;
                }
                Ok::<_, CordisError>(())
            };
            preflight()?;
            // IPC, reentry and adapter code must never run under admission/bus locks.
            let entries = match catch_unwind(AssertUnwindSafe(|| queue.select(ctx, key, C::KIND))) {
                Ok(result) => result?,
                Err(panic) => return Err(panic_error(panic)),
            };
            let _admission = ctx.shared().admission.lock().unwrap();
            preflight()?;
            let mut ids = HashSet::new();
            for entry in &entries {
                entry.validate(ctx, key, C::KIND)?;
                if !ids.insert(entry.0.info.id) {
                    return Err(invalid("event queue returned a duplicate registration"));
                }
            }
            let mut out = Vec::new();
            for entry in entries {
                if let Some(listener) = entry.claim(ctx, key)? {
                    out.push(C::selected(listener));
                }
            }
            // Even an empty synchronous waterfall retains its emitter and
            // instance while the borrowed terminal runs.
            let mut owners = Vec::new();
            if key.instance_id().is_some()
                || matches!(C::KIND, ListenerKind::Sync | ListenerKind::SyncWaterfall)
            {
                owners.extend(ctx.weak_fiber().upgrade());
                owners.extend(key.instance_id().and_then(|id| ctx.instance_owner(id)));
            }
            Ok((out, EventFlight::from_owners(owners)))
        })())
    }

    pub(super) fn queued_subscriptions(&self) -> Vec<EventSubscription> {
        if self.queue.is_none() {
            return Vec::new();
        }
        self.inner
            .lock()
            .unwrap()
            .queued
            .values()
            .filter(|entry| {
                entry.0.active.load(Ordering::SeqCst) && !entry.0.claimed.load(Ordering::SeqCst)
            })
            .map(|entry| {
                let mut info = entry.0.info.clone();
                let metrics = match &entry.0.callback {
                    Callback::Async(h) => &h.metrics,
                    Callback::AsyncWaterfall(h) => &h.metrics,
                    Callback::Sync(h) => &h.metrics,
                    Callback::SyncWaterfall(h) => &h.metrics,
                };
                info.selected = metrics.as_ref().map(|m| m.selected.load(Ordering::Relaxed));
                info.invoked = metrics.as_ref().map(|m| m.invoked.load(Ordering::Relaxed));
                info
            })
            .collect()
    }
}
