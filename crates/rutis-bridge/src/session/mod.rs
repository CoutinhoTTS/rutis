//! The session protocol between rutis and another process: calls both
//! ways, references to objects and functions, async results and
//! cancellation, over any [`Channel`](crate::channel::Channel). Language
//! runtimes and links between nodes both run on it.
//!
//! A rutis service a session can call is a [`HostDispatch`], provided under
//! [`host_key`]`(name)`.

/// Wire protocol version of local runtime sessions. The Node runtime package
/// declares the version it speaks as `rutisProtocol` in its package.json;
/// builds check they match.
pub const PROTOCOL: u32 = 2;

/// The endpoint session format: endpoint ids in the handshake and in call
/// ids, capabilities, either side calling the other. Network sessions use
/// it (WebSocket subprotocol `rutis.3`); local runtime sessions keep
/// [`PROTOCOL`].
pub const ENDPOINT_PROTOCOL: u32 = 3;

mod objects;
mod protocol;
pub(crate) mod rpc;
mod services;
#[cfg(feature = "testing")]
pub mod testing;

pub use objects::{arg, decode_value, JsError, ObjectRef, RemoteFunction};
pub use rpc::*;
pub use services::{host_key, HostDispatch};

/// What [`Error`] says about a session that ended or a call that failed.
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
    /// The session could not be established: see [`Handshake`].
    #[error("{0}")]
    Handshake(Handshake),
}

/// Why a session handshake failed. A link stops retrying an incompatible
/// far end, and retries slowly one whose identity does not match.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Handshake {
    /// Another protocol version or format, or a malformed handshake.
    #[error("incompatible session: {0}")]
    Incompatible(String),
    /// The far end named an endpoint other than the one verified or expected.
    #[error("endpoint mismatch: {0}")]
    IdentityMismatch(String),
}

impl From<Error> for rutis::CordisError {
    fn from(error: Error) -> Self {
        Self::PluginFailed(Box::new(error))
    }
}

/// An error raised on the Rust side, as the other side sees it.
pub fn native_error(error: impl std::fmt::Display) -> Error {
    Error::Remote {
        name: "RustError".into(),
        message: error.to_string(),
        graph: None,
    }
}

pub fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|error| Error::Value(error.to_string()))
}

/// Deserialize a field that TypeScript declares both optional and nullable
/// (`key?: T | null`) as `Option<Option<T>>`: a missing field is `None`, an
/// explicit `null` is `Some(None)`. Use with `#[serde(default)]`.
pub fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// Encode an optional argument: `None` is passed as JS `undefined`, not
/// `null`, so defaults and `=== undefined` checks behave as in native calls.
pub fn optional<T: serde::Serialize>(value: Option<T>) -> Result<Value, Error> {
    match value {
        Some(value) => arg(&value),
        None => Ok(Value::Undefined),
    }
}
