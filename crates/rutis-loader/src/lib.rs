//! rutis-loader: data-driven plugin management for rutis.
//!
//! The input is ordered patch layers (the desired state); the loader drives
//! the running fiber tree towards them. Imperative edits change one editable
//! layer and are handed to a persistence hook. The loader reads and writes no
//! files. Design: `docs/design-rutis-loader-2026-10-02.md`.

mod catalog;
mod edit;
mod error;
#[cfg(all(unix, feature = "interop"))]
mod interop;
mod loader;
mod patch;
mod persist;
mod resolver;
mod volatile;

pub use catalog::{ExprScope, Expressions, ServiceCatalog};
pub use edit::{apply_edit, Edit};
pub use error::{Failure, LoaderError, PersistError};
#[cfg(all(unix, feature = "interop"))]
pub use interop::{resolve_entry, InteropResolver};
pub use loader::{
    Editable, EntryInfo, EntryStatus, Isolate, Loader, LoaderChanged, LoaderOptions, LoaderPlugin,
    NewEntry, PendingEditDropped, ReconcileReport, RowInfo,
};
pub use patch::{apply_patches, Composed, ComposedRow, Layer, Owner, Patch, PatchWarning};
pub use persist::{NoPersist, Persist, Version};
pub use resolver::{Builtins, Chain, Resolved, Resolver};
pub use volatile::{volatile_key, volatile_paths, VolatileUpdate};
