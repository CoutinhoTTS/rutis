// Standalone design probe, not an EventBus implementation.
// Run from the repository root:
// rustc --edition=2021 docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-probe
// /tmp/rutis-events-probe
// These three commands must fail to compile:
// rustc --edition=2021 --cfg mismatched_payload docs/probes/event-keys-sync-dispatch.rs
// rustc --edition=2021 --cfg double_next docs/probes/event-keys-sync-dispatch.rs
// rustc --edition=2021 --cfg escape_next docs/probes/event-keys-sync-dispatch.rs
use std::any::TypeId;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

trait Event: Send + Sync + 'static {
    type Value: Send + 'static;
}
trait SyncEvent: Event {}

enum Name {
    Default,
    Static(&'static str),
    Dynamic(Arc<str>),
}

struct EventKey<E: Event> {
    name: Name,
    marker: PhantomData<fn(E) -> E>,
}

impl<E: Event> EventKey<E> {
    const fn of() -> Self {
        Self {
            name: Name::Default,
            marker: PhantomData,
        }
    }
    const fn named(name: &'static str) -> Self {
        Self {
            name: Name::Static(name),
            marker: PhantomData,
        }
    }
    fn dynamic(name: impl Into<Arc<str>>) -> Self {
        Self {
            name: Name::Dynamic(name.into()),
            marker: PhantomData,
        }
    }
    fn erase(&self) -> (TypeId, Option<&str>) {
        let name = match &self.name {
            Name::Default => None,
            Name::Static(name) => Some(*name),
            Name::Dynamic(name) => Some(name.as_ref()),
        };
        (TypeId::of::<E>(), name)
    }
}

fn emit_async<E: Event>(_key: &EventKey<E>, _event: &E) {}

struct Ctx;
type Outcome<E> = Result<<E as Event>::Value, &'static str>;

trait SyncWaterfallListener<E: SyncEvent>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, event: &'a E, next: SyncNext<'a, E>) -> Outcome<E>;
}

struct SyncNext<'a, E: SyncEvent> {
    chain: &'a [&'a dyn SyncWaterfallListener<E>],
    ctx: &'a Ctx,
    event: &'a E,
    terminal: &'a mut dyn FnMut(&Ctx, &E) -> Outcome<E>,
}

impl<E: SyncEvent> SyncNext<'_, E> {
    fn call(self) -> Outcome<E> {
        let Self {
            chain,
            ctx,
            event,
            terminal,
        } = self;
        match chain.split_first() {
            Some((hook, rest)) => hook.call(
                ctx,
                event,
                SyncNext {
                    chain: rest,
                    ctx,
                    event,
                    terminal,
                },
            ),
            None => terminal(ctx, event),
        }
    }
}

fn waterfall_sync<E: SyncEvent, F>(
    chain: &[&dyn SyncWaterfallListener<E>],
    ctx: &Ctx,
    event: &E,
    terminal: F,
) -> Outcome<E>
where
    F: FnOnce(&Ctx, &E) -> Outcome<E>,
{
    let mut terminal = Some(terminal);
    let mut adapter =
        |ctx: &Ctx, event: &E| terminal.take().expect("terminal called once")(ctx, event);
    SyncNext {
        chain,
        ctx,
        event,
        terminal: &mut adapter,
    }
    .call()
}

struct Ping(u64);
impl Event for Ping {
    type Value = u64;
}
impl SyncEvent for Ping {}
struct Other;
impl Event for Other {
    type Value = u64;
}

struct Add(u64);
impl SyncWaterfallListener<Ping> for Add {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
        next: SyncNext<'a, Ping>,
    ) -> Outcome<Ping> {
        Ok(next.call()? + self.0)
    }
}
struct Multiply(u64);
impl SyncWaterfallListener<Ping> for Multiply {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
        next: SyncNext<'a, Ping>,
    ) -> Outcome<Ping> {
        Ok(next.call()? * self.0)
    }
}
struct Veto;
impl SyncWaterfallListener<Ping> for Veto {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
        _next: SyncNext<'a, Ping>,
    ) -> Outcome<Ping> {
        Ok(99)
    }
}

#[cfg(double_next)]
struct Repeat;
#[cfg(double_next)]
impl SyncWaterfallListener<Ping> for Repeat {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _event: &'a Ping,
        next: SyncNext<'a, Ping>,
    ) -> Outcome<Ping> {
        next.call()?;
        next.call()
    }
}

#[cfg(escape_next)]
fn escape(next: SyncNext<'_, Ping>) -> SyncNext<'static, Ping> {
    next
}

fn main() {
    const KEY: EventKey<Ping> = EventKey::named("room/1");
    assert_eq!(KEY.erase(), EventKey::<Ping>::dynamic("room/1").erase());
    assert_ne!(KEY.erase(), EventKey::<Other>::named("room/1").erase());
    assert_ne!(
        EventKey::<Ping>::of().erase(),
        EventKey::<Ping>::named("Ping").erase()
    );
    // A marker is additive, and a freely constructed name is not spell-checked.
    emit_async(&KEY, &Ping(1));
    emit_async(&EventKey::<Ping>::named("rooom/typo"), &Ping(1));
    #[cfg(mismatched_payload)]
    emit_async(&KEY, &Other);

    let ctx = Ctx;
    let add = Add(10);
    let multiply = Multiply(2);
    let event = Ping(1);
    let waterfall = waterfall_sync(&[&add, &multiply], &ctx, &event, |_, e| Ok(e.0)).unwrap();
    let transform = (event.0 + 10) * 2;
    assert_eq!((transform, waterfall), (22, 12));

    let locked = Mutex::new(5u64);
    let mut guard = locked.lock().unwrap();
    let mut terminals = 0;
    let value = waterfall_sync(&[&add], &ctx, &event, |_, e| {
        terminals += 1;
        *guard += e.0;
        Ok(*guard)
    })
    .unwrap();
    assert_eq!((value, terminals, *guard), (16, 1, 6));
    let value = waterfall_sync(&[&Veto], &ctx, &event, |_, _| {
        terminals += 1;
        Ok(0)
    })
    .unwrap();
    assert_eq!((value, terminals), (99, 1));

    println!("typed key identity: passed");
    println!("marker permits async APIs and misspelled same-type names: confirmed");
    println!("borrowed MutexGuard and borrowed terminal captures: passed");
    println!("single-use terminal and veto: passed");
    println!("sequential transform = {transform}, fixed-input waterfall = {waterfall}");
}
