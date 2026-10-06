//! A rutis service the far end of a session can call ([`HostDispatch`]).

use rutis::TypeKey;
use serde_json::Value;

use crate::session::rpc::{Reply, Value as RpcValue};

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
    /// one ([`RowService`](crate::runtime::RowService)), as its
    /// [`Connection::tag`](crate::session::Connection::tag): a row of that
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
