//! Forwards events between the Cordis and rutis event buses.
//!
//! Cordis → rutis:
//! The Node side registers one Cordis listener per forwarded event; its
//! arguments arrive here, are decoded into the generated event type and
//! emitted with rutis `parallel` from the mounting plugin's context. A
//! Cordis `emit` does not wait for it (fire and forget); `parallel` does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, Ctx, Event, EventKey};

use crate::rpc::{Reply, Value};
use crate::Error;

/// Receives forwarded Cordis events.
pub trait EventSink: Send + Sync + 'static {
    fn event(&self, name: &str, args: Vec<Value>) -> Reply;
}

type Route = Box<
    dyn Fn(&Ctx, Vec<Value>) -> Result<BoxFuture<'static, Result<(), Error>>, Error> + Send + Sync,
>;

#[derive(Default)]
struct State {
    target: Option<Ctx>,
    routes: HashMap<String, Arc<Route>>,
}

/// The events a mount forwards; attached to the mounting plugin's context.
#[derive(Default)]
pub struct Events {
    state: Mutex<State>,
}

impl Events {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Forward the Cordis event `name` as the rutis event `E`, built from
    /// the listener arguments by `decode`.
    pub fn forward<E: Event>(&self, name: &str, decode: fn(Vec<Value>) -> Result<E, Error>) {
        let route: Route = Box::new(move |ctx, args| {
            let event = Arc::new(decode(args)?);
            let ctx = ctx.clone();
            Ok(Box::pin(async move {
                ctx.events()
                    .parallel(&ctx, &EventKey::<E>::of(), event)
                    .await
                    .map_err(|error| Error::Value(error.to_string()))
            }))
        });
        self.state
            .lock()
            .unwrap()
            .routes
            .insert(name.to_owned(), Arc::new(route));
    }

    /// The Cordis event names to listen for.
    pub fn names(&self) -> Vec<String> {
        self.state.lock().unwrap().routes.keys().cloned().collect()
    }

    /// Emit forwarded events from the mounting plugin's context.
    pub fn attach(&self, ctx: &Ctx) {
        self.state.lock().unwrap().target = Some(ctx.clone());
    }

    /// Stop forwarding; later events are dropped.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.target = None;
        state.routes.clear();
    }
}

impl EventSink for Events {
    fn event(&self, name: &str, args: Vec<Value>) -> Reply {
        let (ctx, route) = {
            let state = self.state.lock().unwrap();
            match (&state.target, state.routes.get(name)) {
                (Some(ctx), Some(route)) => (ctx.clone(), route.clone()),
                // Before attach or after close there is no rutis emitter.
                _ => return Ok(Value::Undefined),
            }
        };
        let emitted = route(&ctx, args)?;
        Ok(Value::future(async move {
            emitted.await?;
            Ok(Value::Undefined)
        }))
    }
}

/// rutis → Cordis: a rutis listener that re-emits the event in the mounted
/// Cordis Context with `parallel`, so a rutis `parallel` waits for the
/// Cordis listeners while a rutis `emit` stays fire and forget. It belongs
/// to the mounting plugin and ends with it.
pub struct EmitToCordis<E> {
    process: Arc<crate::Process>,
    name: String,
    encode: fn(&E) -> Result<Vec<Value>, Error>,
}

impl<E: Event> EmitToCordis<E> {
    pub fn new(
        process: Arc<crate::Process>,
        name: &str,
        encode: fn(&E) -> Result<Vec<Value>, Error>,
    ) -> Self {
        Self {
            process,
            name: name.to_owned(),
            encode,
        }
    }
}

impl<E: Event> rutis::Listener<E> for EmitToCordis<E> {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a E,
    ) -> BoxFuture<'a, Result<Option<E::Value>, rutis::CordisError>> {
        Box::pin(async move {
            let args = (self.encode)(event)?;
            self.process.emit(&self.name, args).await?;
            Ok(None)
        })
    }
}
