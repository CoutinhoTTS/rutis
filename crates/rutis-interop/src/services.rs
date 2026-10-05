//! The service shapes the rest of rutis builds on, on every platform: a
//! rutis service a runtime or a peer can call ([`HostDispatch`]), and a
//! session with a runtime that something else owns ([`RuntimeSession`]).
//! Starting runtime processes is Unix only; these are not.

use std::sync::Arc;

use rutis::TypeKey;
use serde_json::Value;

use crate::rpc::{Connection, Dispatch, Reply, Value as RpcValue};
use crate::Error;

/// A rutis service provided to the mounted Cordis plugins: the Node side
/// registers a proxy under `name` whose calls arrive here.
pub trait HostDispatch: Send + Sync + 'static {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply;

    /// The methods as `{ method: "sync" | "async" }`, when the service knows
    /// them; otherwise whoever registers it with a runtime supplies them.
    fn methods(&self) -> Option<Value> {
        None
    }

    /// The runtime session whose plugin serves this service, when it is
    /// one ([`crate::RowService`]), as its [`Connection::tag`]: a row of that
    /// same session uses the plugin natively instead of through a proxy. A
    /// tag names one session of one runtime instance, wherever it runs.
    fn origin(&self) -> Option<&str> {
        None
    }
}

/// The key a host service named `name` is provided under, for example
/// `ctx.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe))`.
pub fn host_key(name: &str) -> TypeKey {
    TypeKey::keyed_dynamic::<dyn HostDispatch>(name.to_owned())
}

/// A session with a runtime that something else owns, such as a link to a
/// remote runtime. A [`crate::RuntimePlugin::remote`] runs its rows on it.
pub trait RuntimeSession: Send + Sync + 'static {
    /// The session, ready.
    fn connection(&self) -> Connection;

    /// Route the runtime's calls into rutis (`host:<name>`, `service`,
    /// `event`) to `dispatch` while the returned guard lives.
    fn route(
        &self,
        dispatch: Arc<dyn Dispatch>,
    ) -> Result<Box<dyn std::any::Any + Send + Sync>, Error>;
}

/// The key the runtime session named `name` is provided under
/// (`RuntimeSession#gpu`).
pub fn runtime_session_key(name: &str) -> rutis::TypeKey {
    rutis::TypeKey::keyed_dynamic::<dyn RuntimeSession>(name.to_owned())
}
