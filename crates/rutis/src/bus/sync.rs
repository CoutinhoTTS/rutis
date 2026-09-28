use std::cell::RefCell;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use super::{BusInner, DispatchMode, EventBus, EventFlight, Hook, HookTable};
use crate::event::sync::{
    value, SyncAdapter, SyncPatternAdapter, SyncPatternWaterfallAdapter, SyncTerminalAdapter,
    SyncWaterfallAdapter,
};
use crate::event::{DynEvent, ErasedValue};
use crate::{
    CordisError, Ctx, Disposer, EventKey, EventOptions, EventPattern, SyncEvent, SyncListener,
    SyncPatternListener, SyncPatternWaterfallListener, SyncWaterfallListener, TypeKey,
};

pub(crate) trait ErasedSyncCall: Send + Sync + 'static {
    fn call(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        event: &DynEvent,
    ) -> Result<Option<ErasedValue>, CordisError>;
}
pub(crate) trait ErasedSyncWaterfallCall: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: &'a TypeKey,
        event: &'a DynEvent,
        next: ErasedSyncNext<'a>,
    ) -> Result<ErasedValue, CordisError>;
}
pub trait ErasedSyncTerminal {
    fn call(&mut self, ctx: &Ctx, event: &DynEvent) -> Result<ErasedValue, CordisError>;
}

pub struct ErasedSyncNext<'a> {
    chain: &'a [Arc<Hook<Arc<dyn ErasedSyncWaterfallCall>>>],
    ctx: &'a Ctx,
    key: &'a TypeKey,
    event: &'a DynEvent,
    terminal: &'a mut (dyn ErasedSyncTerminal + 'a),
}
impl<'a> ErasedSyncNext<'a> {
    /// A borrowed continuation; it can capture non-Send stack state.
    pub fn new(
        ctx: &'a Ctx,
        key: &'a TypeKey,
        event: &'a DynEvent,
        terminal: &'a mut (dyn ErasedSyncTerminal + 'a),
    ) -> Self {
        Self {
            chain: &[],
            ctx,
            key,
            event,
            terminal,
        }
    }

    pub fn call(self) -> Result<ErasedValue, CordisError> {
        self.invoke()
    }

    pub(crate) fn invoke(self) -> Result<ErasedValue, CordisError> {
        let Self {
            chain,
            ctx,
            key,
            event,
            terminal,
        } = self;
        if let Some((hook, rest)) = chain.split_first() {
            hook.record_call();
            user_call(ctx, || {
                hook.call.call(
                    ctx,
                    key,
                    event,
                    Self {
                        chain: rest,
                        ctx,
                        key,
                        event,
                        terminal,
                    },
                )
            })
        } else {
            user_call(ctx, || terminal.call(ctx, event))
        }
    }
}

pub(super) fn user_call<T>(
    ctx: &Ctx,
    call: impl FnOnce() -> Result<T, CordisError>,
) -> Result<T, CordisError> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(panic) => {
            let error = Arc::new(crate::error::panic_error(panic));
            let _ = catch_unwind(AssertUnwindSafe(|| ctx.error_sink()(error.clone())));
            Err(CordisError::SyncEventPanicked(error))
        }
    }
}

thread_local! {
    static ACTIVE: RefCell<Vec<(usize, TypeKey)>> = const { RefCell::new(Vec::new()) };
}
struct ReentryGuard;
impl ReentryGuard {
    fn enter(bus: &EventBus, key: &TypeKey) -> Result<Self, CordisError> {
        // Dispatch borrows the bus until this guard drops, so its Arc allocation
        // remains alive and this address cannot be reused while it is active.
        let identity = Arc::as_ptr(&bus.inner) as usize;
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active
                .iter()
                .any(|entry| entry.0 == identity && entry.1 == *key)
            {
                return Err(CordisError::ReentrantEvent { key: key.clone() });
            }
            active.push((identity, key.clone()));
            Ok(Self)
        })
    }
}
impl Drop for ReentryGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.borrow_mut().pop();
        });
    }
}

impl EventBus {
    pub fn on_sync<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        listener: impl SyncListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.on_sync_opt(ctx, key, listener, EventOptions::default())
    }
    pub fn on_sync_opt<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        listener: impl SyncListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        let call: Arc<dyn ErasedSyncCall> = Arc::new(SyncAdapter(listener, PhantomData));
        self.register_hook(
            ctx,
            key.erased(),
            None,
            call,
            opts,
            true,
            "sync event listener",
            |inner| &mut inner.sync_hooks,
        )
    }
    pub fn on_sync_pattern<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl SyncPatternListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.on_sync_pattern_opt(ctx, pattern, listener, EventOptions::default())
    }
    pub fn on_sync_pattern_opt<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl SyncPatternListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        let call: Arc<dyn ErasedSyncCall> = Arc::new(SyncPatternAdapter(listener, PhantomData));
        self.register_hook(
            ctx,
            TypeKey::of::<E>(),
            Some(pattern.into_prefixes()),
            call,
            opts,
            true,
            "sync event pattern",
            |inner| &mut inner.sync_hooks,
        )
    }
    pub fn on_waterfall_sync<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        listener: impl SyncWaterfallListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.on_waterfall_sync_opt(ctx, key, listener, EventOptions::default())
    }
    pub fn on_waterfall_sync_opt<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        listener: impl SyncWaterfallListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        let call: Arc<dyn ErasedSyncWaterfallCall> =
            Arc::new(SyncWaterfallAdapter(listener, PhantomData));
        self.register_hook(
            ctx,
            key.erased(),
            None,
            call,
            opts,
            true,
            "sync waterfall listener",
            |inner| &mut inner.sync_wf_hooks,
        )
    }
    pub fn on_waterfall_sync_pattern<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl SyncPatternWaterfallListener<E>,
    ) -> Result<Disposer, CordisError> {
        self.on_waterfall_sync_pattern_opt(ctx, pattern, listener, EventOptions::default())
    }
    pub fn on_waterfall_sync_pattern_opt<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        pattern: EventPattern<E>,
        listener: impl SyncPatternWaterfallListener<E>,
        opts: EventOptions,
    ) -> Result<Disposer, CordisError> {
        let call: Arc<dyn ErasedSyncWaterfallCall> =
            Arc::new(SyncPatternWaterfallAdapter(listener, PhantomData));
        self.register_hook(
            ctx,
            TypeKey::of::<E>(),
            Some(pattern.into_prefixes()),
            call,
            opts,
            true,
            "sync waterfall pattern",
            |inner| &mut inner.sync_wf_hooks,
        )
    }

    fn sync_preflight(&self, ctx: &Ctx, key: &TypeKey) -> Result<(), CordisError> {
        ctx.registration_preflight()?;
        self.ensure_bus(ctx)?;
        ctx.check_instance(key)
    }

    fn sync_snapshot<C: super::queue::QueueCallback>(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        table: fn(&mut BusInner) -> &mut HookTable<C>,
    ) -> Result<(Vec<Arc<Hook<C>>>, EventFlight), CordisError> {
        if let Some(hooks) = self.queue_snapshot(ctx, key) {
            return hooks;
        }
        let _admission = ctx.shared().admission.lock().unwrap();
        self.sync_preflight(ctx, key)?;
        let hooks = table(&mut self.inner.lock().unwrap()).take(key, true);
        let mut owners = Vec::new();
        if let Some(emitter) = ctx.weak_fiber().upgrade() {
            owners.push(emitter);
        }
        if let Some(id) = key.instance_id() {
            if let Some(owner) = ctx.instance_owner(id) {
                owners.push(owner);
            }
        }
        owners.extend(hooks.iter().filter_map(|hook| hook.owner.upgrade()));
        Ok((hooks, EventFlight::from_owners(owners)))
    }

    /// Call synchronous listeners in registration/prepend order until `Some`.
    /// Same-bus, same-key recursion is rejected, including from observers.
    pub fn bail_sync<E: SyncEvent>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        event: &E,
    ) -> Result<Option<E::Value>, CordisError> {
        let key = key.erased();
        self.sync_preflight(ctx, &key)?;
        let _reentry = ReentryGuard::enter(self, &key)?;
        self.observe_attempt(&key, DispatchMode::BailSync, ctx, event)?;
        let (hooks, _flight) = self.sync_snapshot(ctx, &key, |inner| &mut inner.sync_hooks)?;
        for hook in &hooks {
            hook.record_call();
            if let Some(result) = user_call(ctx, || hook.call.call(ctx, &key, event))? {
                return value::<E>(result).map(Some);
            }
        }
        Ok(None)
    }

    /// Run borrowed synchronous middleware. The terminal can capture stack
    /// variables and a caller-held MutexGuard; it needs neither Send nor 'static.
    /// Submit side effects only after the complete chain returns and validates.
    pub fn waterfall_sync<E: SyncEvent, F>(
        &self,
        ctx: &Ctx,
        key: &EventKey<E>,
        event: &E,
        terminal: F,
    ) -> Result<E::Value, CordisError>
    where
        F: FnOnce(&Ctx, &E) -> Result<E::Value, CordisError>,
    {
        let key = key.erased();
        self.sync_preflight(ctx, &key)?;
        let _reentry = ReentryGuard::enter(self, &key)?;
        self.observe_attempt(&key, DispatchMode::WaterfallSync, ctx, event)?;
        let (chain, _flight) = self.sync_snapshot(ctx, &key, |inner| &mut inner.sync_wf_hooks)?;
        if chain.is_empty() {
            return user_call(ctx, || terminal(ctx, event));
        }
        let mut terminal = SyncTerminalAdapter(Some(terminal), PhantomData::<fn(E) -> E>);
        value::<E>(
            ErasedSyncNext {
                chain: &chain,
                ctx,
                key: &key,
                event,
                terminal: &mut terminal,
            }
            .invoke()?,
        )
    }
}
