use rutis::BoxFuture;

use crate::{Edit, LoaderError, Patch, PersistError};

/// Opaque version of a stored layer, compared for equality only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Version(pub String);

/// Where the editable layer is stored. The loader never reads or writes
/// files itself.
pub trait Persist: Send + Sync + 'static {
    /// The latest stored content of `layer` and its version; used to redo
    /// queued edits after a conflict.
    fn load<'a>(
        &'a self,
        layer: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<Patch>, Version), LoaderError>>;

    /// Store `patches` only if the stored version still equals `expected`,
    /// returning the new version, or [`PersistError::Conflict`].
    ///
    /// `edits` are all edits since `expected`, in order; applying them to
    /// that content yields `patches`. An implementation may use them for a
    /// partial update (to keep comments, say) but must then check that the
    /// result equals `patches`, and fall back to writing `patches` whole.
    fn save<'a>(
        &'a self,
        layer: &'a str,
        expected: &'a Version,
        edits: &'a [Edit],
        patches: &'a [Patch],
    ) -> BoxFuture<'a, Result<Version, PersistError>>;
}

/// Stores nothing; the version never changes.
pub struct NoPersist;

impl Persist for NoPersist {
    fn load<'a>(
        &'a self,
        _layer: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<Patch>, Version), LoaderError>> {
        Box::pin(async { Err(LoaderError::Unsupported("NoPersist cannot load".into())) })
    }

    fn save<'a>(
        &'a self,
        _layer: &'a str,
        expected: &'a Version,
        _edits: &'a [Edit],
        _patches: &'a [Patch],
    ) -> BoxFuture<'a, Result<Version, PersistError>> {
        Box::pin(async move { Ok(expected.clone()) })
    }
}
