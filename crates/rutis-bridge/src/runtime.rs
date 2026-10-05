//! Runtime access: a peer that is a language runtime, as the
//! `RuntimeSession` a [`RuntimePlugin::remote`](rutis_interop::RuntimePlugin::remote)
//! runs its rows on.
//!
//! The runtime's calls into rutis (`host:<name>`, `service`, `event`) are
//! families on the peer, routed to whatever the runtime plugin serves them
//! with. The session stays the link's: the runtime plugin neither opens nor
//! closes it.

use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Connection, Dispatch, Reply, Value};
use rutis_interop::{runtime_session_key, Error, RuntimeSession};

use crate::{peer_key, Peer};

/// The families a runtime calls rutis with.
const FAMILIES: [&str; 3] = ["host", "service", "event"];

/// Provides `RuntimeSession#<runtime>` on `Peer#<peer>`.
pub struct RuntimeAccessPlugin {
    label: String,
    runtime: String,
    injects: [TypeKey; 1],
}

impl RuntimeAccessPlugin {
    /// The runtime instance `runtime` is the far end `peer`. The link to it
    /// should require the `runtime` contract.
    pub fn new(peer: PeerId, runtime: &str) -> Self {
        Self {
            label: format!("rutis-bridge/runtime#{runtime}"),
            runtime: runtime.to_owned(),
            injects: [peer_key(&peer)],
        }
    }
}

impl Plugin for RuntimeAccessPlugin {
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
            ctx.provide_as::<dyn RuntimeSession>(
                runtime_session_key(&self.runtime),
                Arc::new(PeerSession(peer)),
            )?;
            Ok(Effect::Done)
        })
    }
}

struct PeerSession(Arc<Peer>);

struct Routed(Arc<dyn Dispatch>);
impl crate::Handler for Routed {
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: Value) -> Reply {
        self.0.invoke(peer, target, method, args)
    }
}

impl RuntimeSession for PeerSession {
    fn connection(&self) -> Connection {
        self.0.connection().clone()
    }

    fn route(
        &self,
        dispatch: Arc<dyn Dispatch>,
    ) -> Result<Box<dyn std::any::Any + Send + Sync>, Error> {
        let handler: Arc<dyn crate::Handler> = Arc::new(Routed(dispatch));
        let offered = FAMILIES
            .iter()
            .map(|family| self.0.register(family, handler.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(offered))
    }
}
