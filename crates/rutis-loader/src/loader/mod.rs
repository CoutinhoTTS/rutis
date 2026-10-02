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

use crate::catalog::{Expressions, ServiceCatalog};
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
    /// Service names usable in `isolate`, `inject` and expressions.
    pub catalog: ServiceCatalog,
    /// Evaluates `{ "__jsExpr": .. }` nodes; without it such rows are
    /// `Unresolved`.
    pub expressions: Option<Arc<dyn Expressions>>,
}

impl Default for LoaderOptions {
    fn default() -> Self {
        Self {
            persist: Arc::new(NoPersist),
            catalog: ServiceCatalog::default(),
            expressions: None,
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

/// Which row a fiber belongs to, for plugins that act on their row's
/// settings themselves (see `Resolved::foreign_scope`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInfo {
    pub id: String,
    /// `isolate` as (service name, scope label): rows naming the same label
    /// share a scope; a private scope's label is unique to the row.
    pub isolate: Vec<(String, String)>,
    /// `inject`: extra service names the row waits for.
    pub inject: Vec<String>,
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
    /// Catalog names of extra services the row waits for.
    pub inject: Vec<String>,
    /// Catalog name → isolation.
    pub isolate: BTreeMap<String, Isolate>,
}

/// One `isolate` entry: a scope private to the row, or one shared by every
/// row naming the same label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Isolate {
    Private,
    Shared(String),
}

impl Isolate {
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Isolate::Private => Value::Bool(true),
            Isolate::Shared(label) => Value::String(label.clone()),
        }
    }
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
    /// The last config the loader tried to apply was refused by the plugin's
    /// validation. A running row keeps its previous config.
    pub rejected: Option<LoaderError>,
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
            .field("rejected", &self.rejected)
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
    /// An overlay layer was set or removed.
    Overlay(String),
    /// A row's plugin disposed itself; the row was disabled in the editable
    /// layer (`error` when that edit failed, e.g. no editable layer).
    SelfDisposed {
        id: String,
        error: Option<String>,
    },
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
    catalog: ServiceCatalog,
    expressions: Option<Arc<dyn Expressions>>,
    /// Serializes reconcile and edits.
    op: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The application's layers, as given to `reconcile`.
    layers: Vec<Layer>,
    /// Runtime layers composed after the application's (`set_overlay`):
    /// never persisted, kept across `reconcile`, not editable.
    overlays: Vec<Layer>,
    editable: Option<usize>,
    version: Version,
    pending: Vec<Edit>,
    desired: Desired,
    resolved: HashMap<String, Result<Arc<Resolved>, LoaderError>>,
    /// Running group contexts; `None` is the loader's own (the root).
    groups: HashMap<Option<String>, Group>,
    running: HashMap<String, Running>,
    /// Rows whose config the plugin refused: at spawn (the row does not
    /// run) or on update (the plugin keeps running its previous config).
    rejected: HashMap<String, LoaderError>,
    /// Source of spawn tokens; see [`Running::token`].
    next_token: u64,
    /// Last root context, kept to notice a host shutdown after unmount.
    last_root: Option<Ctx>,
}

/// A running group's context, tagged with the token of the spawn that owns
/// it so a late cleanup of an older instance cannot unregister a newer one.
struct Group {
    ctx: Ctx,
    token: u64,
}

struct Running {
    parent: Option<String>,
    /// Unique per spawn. A group registers its context under this token.
    token: u64,
    /// Token of the group context this row was spawned in.
    parent_token: u64,
    view: FiberView,
    group: bool,
    name: String,
    injects: Vec<TypeKey>,
    /// The module factory's name: its identity. A new one means respawning.
    factory_name: String,
    resolved: Option<Arc<Resolved>>,
    /// The evaluated config the kernel holds.
    config: Value,
    /// `isolate` names and labels and `inject` names it was spawned with.
    scope: (Vec<(String, String)>, Vec<String>),
    /// The row's context (parent with isolates), for re-evaluating config.
    ctx: Ctx,
}

impl State {
    /// The application's layers followed by the overlays.
    fn composed_layers(&self) -> Vec<Layer> {
        self.layers.iter().chain(&self.overlays).cloned().collect()
    }

    fn layer_name(&self, index: usize) -> Option<&str> {
        self.layers
            .iter()
            .chain(&self.overlays)
            .nth(index)
            .map(|l| l.name.as_str())
    }
}
