pub use rutis;
pub use serde_json;
pub use tokio;
pub use tokio_util;

#[cfg(feature = "serde")]
pub use serde;
#[cfg(feature = "misc")]
pub use {chrono, regex, tracing, tracing_subscriber, uuid};
#[cfg(feature = "web")]
pub use reqwest;

pub type ConfigValue = serde_json::Value;

#[global_allocator]
static SDK_ALLOCATOR: std::alloc::System = std::alloc::System;

/// A derived type, so serde's derive output is part of the SDK.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProbeConfig {
    pub name: String,
    pub retries: u32,
    pub tags: Vec<String>,
}
