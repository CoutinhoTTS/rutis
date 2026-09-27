use std::marker::PhantomData;

use crate::bus::sync::{
    ErasedSyncCall, ErasedSyncNext, ErasedSyncTerminal, ErasedSyncWaterfallCall,
};
use crate::event::{DynEvent, ErasedValue, Event};
use crate::{CordisError, Ctx, EventKey, TypeKey};

/// Opt in to synchronous dispatch. Async dispatch remains available for this
/// type, through separate async registrations.
pub trait SyncEvent: Event {}

pub trait SyncListener<E: SyncEvent>: Send + Sync + 'static {
    fn call(&self, ctx: &Ctx, event: &E) -> Result<Option<E::Value>, CordisError>;
}
impl<E: SyncEvent, F> SyncListener<E> for F
where
    F: Fn(&Ctx, &E) -> Result<Option<E::Value>, CordisError> + Send + Sync + 'static,
{
    fn call(&self, ctx: &Ctx, event: &E) -> Result<Option<E::Value>, CordisError> {
        self(ctx, event)
    }
}

pub trait SyncWaterfallListener<E: SyncEvent>: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        event: &'a E,
        next: SyncNext<'a, E>,
    ) -> Result<E::Value, CordisError>;
}
impl<E: SyncEvent, F> SyncWaterfallListener<E> for F
where
    F: for<'a> Fn(&'a Ctx, &'a E, SyncNext<'a, E>) -> Result<E::Value, CordisError>
        + Send
        + Sync
        + 'static,
{
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        event: &'a E,
        next: SyncNext<'a, E>,
    ) -> Result<E::Value, CordisError> {
        self(ctx, event, next)
    }
}

pub trait SyncPatternListener<E: SyncEvent>: Send + Sync + 'static {
    fn call(&self, ctx: &Ctx, key: EventKey<E>, event: &E)
        -> Result<Option<E::Value>, CordisError>;
}
impl<E: SyncEvent, F> SyncPatternListener<E> for F
where
    F: Fn(&Ctx, EventKey<E>, &E) -> Result<Option<E::Value>, CordisError> + Send + Sync + 'static,
{
    fn call(
        &self,
        ctx: &Ctx,
        key: EventKey<E>,
        event: &E,
    ) -> Result<Option<E::Value>, CordisError> {
        self(ctx, key, event)
    }
}

pub trait SyncPatternWaterfallListener<E: SyncEvent>: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: EventKey<E>,
        event: &'a E,
        next: SyncNext<'a, E>,
    ) -> Result<E::Value, CordisError>;
}
impl<E: SyncEvent, F> SyncPatternWaterfallListener<E> for F
where
    F: for<'a> Fn(&'a Ctx, EventKey<E>, &'a E, SyncNext<'a, E>) -> Result<E::Value, CordisError>
        + Send
        + Sync
        + 'static,
{
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: EventKey<E>,
        event: &'a E,
        next: SyncNext<'a, E>,
    ) -> Result<E::Value, CordisError> {
        self(ctx, key, event, next)
    }
}

/// Single-use, borrowed continuation. It preserves the input event and passes
/// the downstream value back to its caller.
///
/// ```compile_fail
/// use rutis::{CordisError, Ctx, Event, SyncEvent, SyncNext};
/// struct Ping;
/// impl Event for Ping { const NAME: &'static str = "ping"; type Value = (); }
/// impl SyncEvent for Ping {}
/// fn twice(next: SyncNext<'_, Ping>) -> Result<(), CordisError> {
///     next.call()?;
///     next.call()
/// }
/// ```
///
/// ```compile_fail
/// use rutis::{Event, SyncEvent, SyncNext};
/// struct Ping;
/// impl Event for Ping { const NAME: &'static str = "ping"; type Value = (); }
/// impl SyncEvent for Ping {}
/// fn escape(next: SyncNext<'_, Ping>) -> SyncNext<'static, Ping> { next }
/// ```
pub struct SyncNext<'a, E: SyncEvent> {
    pub(crate) inner: ErasedSyncNext<'a>,
    pub(crate) marker: PhantomData<fn(E) -> E>,
}
impl<E: SyncEvent> SyncNext<'_, E> {
    pub fn call(self) -> Result<E::Value, CordisError> {
        value::<E>(self.inner.invoke()?)
    }
}

pub(crate) fn value<E: Event>(value: ErasedValue) -> Result<E::Value, CordisError> {
    value
        .downcast::<E::Value>()
        .map(|value| *value)
        .map_err(|_| CordisError::PluginFailed("synchronous event value type mismatch".into()))
}
fn event<E: Event>(event: &DynEvent) -> Result<&E, CordisError> {
    event
        .downcast_ref::<E>()
        .ok_or_else(|| CordisError::PluginFailed("synchronous event type mismatch".into()))
}

pub(crate) struct SyncAdapter<L, E>(pub L, pub PhantomData<fn(E) -> E>);
impl<E: SyncEvent, L: SyncListener<E>> ErasedSyncCall for SyncAdapter<L, E> {
    fn call(
        &self,
        ctx: &Ctx,
        _key: &TypeKey,
        e: &DynEvent,
    ) -> Result<Option<ErasedValue>, CordisError> {
        self.0
            .call(ctx, event::<E>(e)?)
            .map(|value| value.map(|v| Box::new(v) as ErasedValue))
    }
}
pub(crate) struct SyncPatternAdapter<L, E>(pub L, pub PhantomData<fn(E) -> E>);
impl<E: SyncEvent, L: SyncPatternListener<E>> ErasedSyncCall for SyncPatternAdapter<L, E> {
    fn call(
        &self,
        ctx: &Ctx,
        key: &TypeKey,
        e: &DynEvent,
    ) -> Result<Option<ErasedValue>, CordisError> {
        self.0
            .call(ctx, EventKey::from_erased(key), event::<E>(e)?)
            .map(|value| value.map(|v| Box::new(v) as ErasedValue))
    }
}
pub(crate) struct SyncWaterfallAdapter<L, E>(pub L, pub PhantomData<fn(E) -> E>);
impl<E: SyncEvent, L: SyncWaterfallListener<E>> ErasedSyncWaterfallCall
    for SyncWaterfallAdapter<L, E>
{
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        _key: &'a TypeKey,
        e: &'a DynEvent,
        next: ErasedSyncNext<'a>,
    ) -> Result<ErasedValue, CordisError> {
        self.0
            .call(
                ctx,
                event::<E>(e)?,
                SyncNext {
                    inner: next,
                    marker: PhantomData,
                },
            )
            .map(|v| Box::new(v) as ErasedValue)
    }
}
pub(crate) struct SyncPatternWaterfallAdapter<L, E>(pub L, pub PhantomData<fn(E) -> E>);
impl<E: SyncEvent, L: SyncPatternWaterfallListener<E>> ErasedSyncWaterfallCall
    for SyncPatternWaterfallAdapter<L, E>
{
    fn call<'a>(
        &'a self,
        ctx: &'a Ctx,
        key: &'a TypeKey,
        e: &'a DynEvent,
        next: ErasedSyncNext<'a>,
    ) -> Result<ErasedValue, CordisError> {
        self.0
            .call(
                ctx,
                EventKey::from_erased(key),
                event::<E>(e)?,
                SyncNext {
                    inner: next,
                    marker: PhantomData,
                },
            )
            .map(|v| Box::new(v) as ErasedValue)
    }
}

pub(crate) struct SyncTerminalAdapter<F, E>(pub Option<F>, pub PhantomData<fn(E) -> E>);
impl<E: SyncEvent, F: FnOnce(&Ctx, &E) -> Result<E::Value, CordisError>> ErasedSyncTerminal
    for SyncTerminalAdapter<F, E>
{
    fn call(&mut self, ctx: &Ctx, e: &DynEvent) -> Result<ErasedValue, CordisError> {
        self.0.take().expect("single-use synchronous terminal")(ctx, event::<E>(e)?)
            .map(|v| Box::new(v) as ErasedValue)
    }
}
