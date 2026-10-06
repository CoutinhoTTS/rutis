//! `events`: forwards notification events between nodes, by name, each
//! name in one direction per link.
//!
//! ```text
//! events.forward { name, args }   → once the far end's listeners finished
//! ```
//!
//! Events cross as [`NodeEvent`]s on dynamic channels
//! (`EventKey::<NodeEvent>::dynamic(name)`), their arguments as JSON. The
//! receiving node emits them with `parallel`, so the sender's forward ends
//! when the listeners there did.

use std::collections::HashSet;
use std::sync::Arc;

use crate::channel::PeerId;
use crate::session::rpc::{Connection, Reply, Value};
use crate::session::Error;
use rutis::{BoxFuture, CordisError, Ctx, Effect, Event, EventKey, Listener, Plugin, TypeKey};
use serde_json::{json, Value as Json};

use crate::{peer_key, Offered, Peer};

/// An event that crosses nodes: its arguments, as JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeEvent {
    pub args: Json,
}

impl Event for NodeEvent {
    const NAME: &'static str = "rutis-bridge/event";
    type Value = ();
}

/// The channel of the node event `name`.
pub fn node_event(name: &str) -> EventKey<NodeEvent> {
    EventKey::dynamic(name.to_owned())
}

/// Forwards `outbound` events to `Peer#<peer>` and emits the `inbound`
/// events it forwards here.
pub struct EventsPlugin {
    label: String,
    injects: [TypeKey; 1],
    outbound: Vec<String>,
    inbound: HashSet<String>,
}

impl EventsPlugin {
    /// A name may go one way only on a link.
    pub fn new(
        peer: PeerId,
        outbound: impl IntoIterator<Item = impl Into<String>>,
        inbound: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, String> {
        let outbound: Vec<String> = outbound.into_iter().map(Into::into).collect();
        let inbound: HashSet<String> = inbound.into_iter().map(Into::into).collect();
        if let Some(both) = outbound.iter().find(|name| inbound.contains(*name)) {
            return Err(format!(
                "event {both} cannot be forwarded both ways on the link to {peer}"
            ));
        }
        Ok(Self {
            label: format!("rutis-bridge/events#{peer}"),
            injects: [peer_key(&peer)],
            outbound,
            inbound,
        })
    }
}

struct Forward {
    name: String,
    peer: Arc<Peer>,
}

impl Listener<NodeEvent> for Forward {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a NodeEvent,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            let forwarded = self
                .peer
                .connection()
                .invoke_async(
                    "",
                    "events.forward",
                    json!([{ "name": self.name, "args": event.args }]).into(),
                )
                .await;
            // The far end answers once its listeners finished.
            crate::session::rpc::settle(
                forwarded.map_err(|error| CordisError::PluginFailed(Box::new(error)))?,
            )
            .await
            .map_err(|error| CordisError::PluginFailed(Box::new(error)))?;
            Ok(None)
        })
    }
}

struct Inbound {
    ctx: Ctx,
    names: HashSet<String>,
}

impl crate::Handler for Inbound {
    fn invoke(&self, _: &Connection, _: &str, method: &str, args: Value) -> Reply {
        if method != "events.forward" {
            return Err(Error::Value(format!("{method} is not offered here")));
        }
        let fields = args
            .list()?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Value("events.forward needs the event".into()))?
            .json()?;
        let name: String = crate::session::decode(fields["name"].clone())?;
        if !self.names.contains(&name) {
            return Err(Error::Value(format!("event {name} is not forwarded here")));
        }
        let event = Arc::new(NodeEvent {
            args: fields["args"].clone(),
        });
        let ctx = self.ctx.clone();
        Ok(Value::future(async move {
            ctx.events()
                .parallel(&ctx, &node_event(&name), event)
                .await
                .map_err(|error| Error::Value(error.to_string()))?;
            Ok(Value::Undefined)
        }))
    }
}

impl Plugin for EventsPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let peer = ctx.get_as::<Peer>(self.injects[0].clone()).ok_or_else(|| {
                CordisError::PluginFailed(format!("{}: the peer is gone", self.label).into())
            })?;
            for name in &self.outbound {
                ctx.events().on(
                    ctx,
                    &node_event(name),
                    Forward {
                        name: name.clone(),
                        peer: peer.clone(),
                    },
                )?;
            }
            if !self.inbound.is_empty() {
                let offered: Offered = peer
                    .register(
                        "events",
                        Arc::new(Inbound {
                            ctx: ctx.clone(),
                            names: self.inbound.clone(),
                        }),
                    )
                    .map_err(|error| CordisError::PluginFailed(error.to_string().into()))?;
                ctx.effect(move || {
                    Effect::Disposer(Box::new(move || {
                        drop(offered);
                        Ok(())
                    }))
                })?;
            }
            Ok(Effect::Done)
        })
    }
}
