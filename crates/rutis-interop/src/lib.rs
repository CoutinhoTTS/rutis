//! Generated native bindings for cross-process Cordis and rutis plugins.
//!
//! Generated bindings cover typed value methods; the shared RPC layer also
//! supports owned callbacks and async results. Automatic callback bindings and
//! complete plugin interoperability remain under development.

pub mod build;
#[cfg(unix)]
mod process;
mod protocol;
#[cfg(unix)]
pub mod rpc;
#[cfg(unix)]
pub mod server;

#[cfg(unix)]
pub use process::Process;
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
