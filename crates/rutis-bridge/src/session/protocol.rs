use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::Error;

pub(crate) const VERSION: u32 = crate::session::PROTOCOL;

/// Who speaks at the far end, for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Function,
    Future,
    /// An object with methods and live properties, addressed by reference.
    Object,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum WireValue {
    Undefined,
    Data(Value),
    List(Vec<WireValue>),
    /// A plain object whose fields contain references.
    Record(std::collections::BTreeMap<String, WireValue>),
    /// An AbortSignal for the receiving call: it aborts when the caller
    /// cancels the call.
    Signal,
    Reference {
        id: u64,
        home: bool,
        kind: Kind,
        origin: Vec<String>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Failure {
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph: Option<Value>,
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        match error {
            Error::Remote {
                name,
                message,
                graph,
            } => Self {
                name,
                message,
                graph,
            },
            Error::SyncWaitCycle(message) => Self {
                name: "SyncWaitCycle".into(),
                message,
                graph: None,
            },
            error => Self {
                name: "BindingError".into(),
                message: error.to_string(),
                graph: None,
            },
        }
    }
}
impl From<Failure> for Error {
    fn from(error: Failure) -> Self {
        if error.name == "SyncWaitCycle" && error.graph.is_none() {
            Self::SyncWaitCycle(error.message)
        } else {
            Self::Remote {
                name: error.name,
                message: error.message,
                graph: error.graph,
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Frame {
    /// Opens a session. Version 2 (compat) carries only the version; the
    /// endpoint format (3) also names the endpoint, its implementation and
    /// its capabilities.
    Hello {
        version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        implementation: Option<Implementation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Vec<String>>,
    },
    Invoke {
        id: String,
        path: Vec<String>,
        target: String,
        method: String,
        args: WireValue,
    },
    /// Call a function reference, or a method of an object reference.
    Call {
        id: String,
        path: Vec<String>,
        reference: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        args: WireValue,
    },
    /// Read a property of an object reference (a live read).
    Get {
        id: String,
        path: Vec<String>,
        reference: u64,
        property: String,
    },
    Await {
        id: String,
        path: Vec<String>,
        reference: u64,
    },
    Return {
        id: String,
        value: WireValue,
    },
    Throw {
        id: String,
        error: Failure,
    },
    Release {
        reference: u64,
        count: u64,
    },
    /// The caller gave up on call `id`: abort its signal / stop awaiting.
    /// A reply may still arrive and is then discarded.
    Cancel {
        id: String,
    },
}
