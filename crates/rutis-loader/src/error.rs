use std::sync::Arc;

use rutis::CordisError;

/// A row that ended up `Failed` or `Unresolved`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub id: String,
    pub error: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.id, self.error)
    }
}

fn list(failures: &[Failure]) -> String {
    failures
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum LoaderError {
    /// No resolver knows this module name.
    #[error("no plugin named {name:?}")]
    NotFound { name: String },
    /// A resolver knows the name but could not load it.
    #[error("cannot load {name:?}: {message}")]
    Resolve { name: String, message: String },
    /// The dry run (config validation, build, instance validation) failed.
    #[error("entry {id:?} rejected: {error}")]
    Rejected { id: String, error: Arc<CordisError> },
    #[error("no entry {0:?}")]
    UnknownEntry(String),
    #[error("invalid entry: {0}")]
    InvalidEntry(String),
    /// The edit cannot be expressed as a patch in the editable layer.
    #[error("entry {id:?} is not owned by the editable layer: {reason}")]
    NotOwned { id: String, reason: String },
    /// A layer above the editable one replaces the same field.
    #[error("{field} of {id:?} is overridden by layer {layer:?}")]
    OverriddenByLayer {
        id: String,
        field: String,
        layer: String,
    },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no editable layer")]
    NoEditableLayer,
    /// The edit validated but rows failed to start; it was rolled back.
    #[error("apply failed, rolled back: {}", list(.failures))]
    ApplyFailed { failures: Vec<Failure> },
    /// The rollback after `ApplyFailed` could not restore the old rows either.
    #[error("apply failed ({}) and rollback failed ({})", list(.apply), list(.rollback))]
    RollbackFailed {
        apply: Vec<Failure>,
        rollback: Vec<Failure>,
    },
    /// The stored layer kept changing under concurrent writers.
    #[error("persisted layer changed concurrently")]
    Conflict,
    /// The edit is live but could not be persisted; it stays queued.
    #[error("edit applied but not persisted: {0}")]
    PersistFailed(String),
    #[error("expression not supported: {0}")]
    Expression(String),
    #[error("loader is closed")]
    Closed,
}

/// What a [`crate::Persist`] implementation reports.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PersistError {
    /// The stored version is not the expected one.
    #[error("version conflict")]
    Conflict,
    #[error("{0}")]
    Failed(String),
}
