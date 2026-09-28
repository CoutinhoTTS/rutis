use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Function,
    Future,
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
    Hello {
        version: u32,
    },
    Invoke {
        id: String,
        path: Vec<String>,
        target: String,
        method: String,
        args: WireValue,
    },
    Call {
        id: String,
        path: Vec<String>,
        reference: u64,
        args: WireValue,
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
}
