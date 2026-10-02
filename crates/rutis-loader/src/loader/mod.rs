//! The loader: desired state, reconcile, editable layer and persistence.
//!
//! - `api`: the public `Loader` methods;
//! - `desired`: the composed tree as rows the loader can act on;
//! - `plugins`: the kernel glue (entry factory, group and loader plugins);
//! - `reconcile`: driving fibers towards the desired tree;
//! - `commit`: imperative edits, rollback and the persistence queue.

mod api;
mod commit;
mod desired;
mod plugins;
mod reconcile;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use rutis::{Ctx, Event, FiberView, PluginId, Snapshot, TypeKey};
use serde_json::Value;

use crate::edit::Edit;
use crate::error::Failure;
use crate::patch::{Layer, Owner, PatchWarning};
use crate::persist::{NoPersist, Persist, Version};
use crate::resolver::{Resolved, Resolver};
use crate::LoaderError;

use desired::Desired;
pub use plugins::LoaderPlugin;

pub struct LoaderOptions {
    pub persist: Arc<dyn Persist>,
}

impl Default for LoaderOptions {
    fn default() -> Self {
        Self {
            persist: Arc::new(NoPersist),
        }
    }
}

/// The layer that imperative edits change, and the stored version it was
/// read at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editable {
    pub layer: String,
    pub version: Version,
}

impl Editable {
    pub fn new(layer: impl Into<String>, version: Version) -> Self {
        Self {
            layer: layer.into(),
            version,
        }
    }
}

/// A new row for [`Loader::create`].
#[derive(Debug, Clone, Default)]
pub struct NewEntry {
    /// Generated when absent.
    pub id: Option<String>,
    pub name: String,
    pub config: Value,
    pub group: bool,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub enum EntryStatus {
    /// The row itself is disabled.
    Disabled,
    /// An enclosing group is not running, or the loader is not mounted.
    Inactive,
    /// The row cannot run: invalid, unknown module, or unsupported content.
    Unresolved(LoaderError),
    Running(Snapshot),
}

#[derive(Clone)]
pub struct EntryInfo {
    pub id: String,
    /// The row as composed (raw; expressions are not evaluated).
    pub options: Value,
    pub parent: Option<String>,
    pub owner: Owner,
    /// Field → name of the last layer that replaced it.
    pub overridden: BTreeMap<String, String>,
    pub status: EntryStatus,
    pub plugin: Option<PluginId>,
    pub view: Option<FiberView>,
    pub schema: Option<Value>,
    pub meta: Value,
}

impl std::fmt::Debug for EntryInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryInfo")
            .field("id", &self.id)
            .field("options", &self.options)
            .field("parent", &self.parent)
            .field("owner", &self.owner)
            .field("overridden", &self.overridden)
            .field("status", &self.status)
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub warnings: Vec<PatchWarning>,
    /// Rows skipped while reading the tree (missing or duplicate ids).
    pub issues: Vec<String>,
    /// Rows failing after this reconcile that were not failing before.
    pub new_failures: Vec<Failure>,
    /// Every row failing after this reconcile.
    pub failures: Vec<Failure>,
}

/// Emitted on the root bus after a reconcile or an edit completes.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LoaderChanged {
    Reconciled,
    Edited(Edit),
    Reloaded(String),
}

impl Event for LoaderChanged {
    const NAME: &'static str = "rutis-loader::changed";
    type Value = ();
}

/// A queued, unsaved edit no longer applied when replayed on newer content.
#[derive(Debug, Clone)]
pub struct PendingEditDropped {
    pub edit: Edit,
    pub error: LoaderError,
}

impl Event for PendingEditDropped {
    const NAME: &'static str = "rutis-loader::pending-edit-dropped";
    type Value = ();
}

/// Handle to the loader; cheap to clone. Mount it with [`LoaderPlugin`].
#[derive(Clone)]
pub struct Loader {
    inner: Arc<Inner>,
}

struct Inner {
    resolver: Arc<dyn Resolver>,
    persist: Arc<dyn Persist>,
    /// Serializes reconcile and edits.
    op: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    layers: Vec<Layer>,
    editable: Option<usize>,
    version: Version,
    pending: Vec<Edit>,
    desired: Desired,
    resolved: HashMap<String, Result<Arc<Resolved>, LoaderError>>,
    /// Running group contexts; `None` is the loader's own (the root).
    groups: HashMap<Option<String>, Ctx>,
    running: HashMap<String, Running>,
    /// Last root context, kept to notice a host shutdown after unmount.
    last_root: Option<Ctx>,
}

struct Running {
    parent: Option<String>,
    view: FiberView,
    group: bool,
    name: String,
    injects: Vec<TypeKey>,
    resolved: Option<Arc<Resolved>>,
    config: Value,
}
