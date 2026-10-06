//! rutis-loader: data-driven plugin management for rutis.
//!
//! The input is ordered patch layers (the desired state); the loader drives
//! the running fiber tree towards them. Imperative edits change one editable
//! layer and are handed to a persistence hook. The loader reads and writes no
//! files. Design: `docs/design-rutis-loader-2026-10-02.md`.

mod catalog;
mod edit;
mod error;
mod loader;
mod patch;
#[cfg(feature = "peer")]
mod peer;
mod persist;
mod resolver;
#[cfg(all(unix, feature = "runtimes"))]
mod runtime;
mod volatile;

pub use catalog::{ExprScope, Expressions, ServiceCatalog};
pub use edit::{apply_edit, Edit};
pub use error::{Failure, LoaderError, PersistError};
pub use loader::{
    Editable, EntryInfo, EntryStatus, Isolate, Loader, LoaderChanged, LoaderOptions, LoaderPlugin,
    NewEntry, PendingEditDropped, ReconcileReport, RowInfo,
};
pub use patch::{apply_patches, Composed, ComposedRow, Layer, Owner, Patch, PatchWarning};
#[cfg(feature = "peer")]
pub use peer::{
    node_schema, register_peer_node, LoaderCatalog, PeerResolver, PeerRows, PeerRowsPlugin,
};
pub use persist::{NoPersist, Persist, Version};
pub use resolver::{Builtins, Chain, Resolved, Resolver};
#[cfg(all(unix, feature = "node"))]
pub use runtime::resolve_entry;
#[cfg(all(unix, feature = "runtimes"))]
pub use runtime::{RuntimeResolver, RuntimeRows, RuntimeRowsPlugin};
pub use volatile::{volatile_key, volatile_paths, VolatileUpdate};
