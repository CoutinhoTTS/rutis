//! Compatibility layer for mounting Cordis plugins in rutis applications.
//!
//! Everything here uses public rutis API only. Generated bindings cover typed
//! value methods and follow service replacement; the shared RPC layer also
//! supports owned callbacks and async results. See
//! `docs/design-protocol-plugin-mount.md` for the covered surface and the
//! boundary rules for Cordis plugins.

pub mod build;
#[cfg(unix)]
mod objects;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod projection;
#[cfg(unix)]
mod protocol;
#[cfg(unix)]
pub mod rpc;
#[cfg(unix)]
pub mod server;

#[cfg(unix)]
pub use objects::{arg, decode_value, ObjectRef};
#[cfg(unix)]
pub use process::{Host, HostDispatch, Process, ServiceEvents};
#[cfg(unix)]
pub use projection::Projection;
pub use serde;
pub use serde_json;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Transport(String),
    #[error("{name}: {message}")]
    Remote {
        name: String,
        message: String,
        graph: Option<serde_json::Value>,
    },
    #[error("synchronous wait cycle: {0}")]
    SyncWaitCycle(String),
    #[error("invalid binding value: {0}")]
    Value(String),
}

impl From<Error> for rutis::CordisError {
    fn from(error: Error) -> Self {
        Self::PluginFailed(Box::new(error))
    }
}

pub fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|error| Error::Value(error.to_string()))
}

/// Encode an optional argument: `None` is passed as JS `undefined`, not
/// `null`, so defaults and `=== undefined` checks behave as in native calls.
#[cfg(unix)]
pub fn optional<T: serde::Serialize>(value: Option<T>) -> Result<rpc::Value, Error> {
    match value {
        Some(value) => arg(&value),
        None => Ok(rpc::Value::Undefined),
    }
}
