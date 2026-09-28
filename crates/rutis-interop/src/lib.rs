//! Generated native bindings for cross-process Cordis plugins.
//!
//! The first implementation covers typed value methods. It is not yet a
//! complete implementation of the plugin interoperability requirements.

pub mod build;
#[cfg(unix)]
mod process;

#[cfg(unix)]
pub use process::Process;
pub use serde;
pub use serde_json;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Transport(String),
    #[error("{name}: {message}")]
    Remote { name: String, message: String },
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
