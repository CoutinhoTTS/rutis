//! The loader: desired state, reconcile, editable layer and persistence.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Weak};

use rutis::{
    BoxFuture, CordisError, Ctx, Effect, Event, EventKey, FiberState, FiberView, Plugin,
    PluginFactory, PluginId, Snapshot, TypeKey,
};
use serde_json::Value;

use crate::edit::{apply_edit, Edit};
use crate::error::Failure;
use crate::patch::{apply_patches, truthy, Composed, Layer, Owner, PatchWarning};
use crate::persist::{NoPersist, Persist, Version};
use crate::resolver::{Resolved, Resolver};
use crate::{LoaderError, PersistError};

/// How many times a version conflict is resolved by replaying the pending
/// queue before giving up with [`LoaderError::Conflict`].
const CONFLICT_RETRIES: usize = 3;

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

#[derive(Default)]
struct Desired {
    rows: Vec<Row>,
    by_id: HashMap<String, usize>,
    warnings: Vec<PatchWarning>,
    issues: Vec<String>,
}

struct Row {
    id: String,
    parent: Option<String>,
    value: Value,
    name: Option<String>,
    group: bool,
    owner: Owner,
    overridden: BTreeMap<String, usize>,
    disabled: Result<bool, LoaderError>,
    config: Value,
    invalid: Option<LoaderError>,
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

fn is_expression(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.len() == 1 && map.get("__jsExpr").is_some_and(Value::is_string))
}

fn contains_expression(value: &Value) -> bool {
    match value {
        _ if is_expression(value) => true,
        Value::Array(items) => items.iter().any(contains_expression),
        Value::Object(map) => map.values().any(contains_expression),
        _ => false,
    }
}

impl Desired {
    fn from_composed(composed: Composed) -> Self {
        let mut desired = Desired {
            warnings: composed.warnings,
            ..Desired::default()
        };
        for flat in composed.flat {
            let Some(id) = flat.id.clone() else {
                desired
                    .issues
                    .push(format!("row without an id skipped: {}", flat.value));
                continue;
            };
            if desired.by_id.contains_key(&id) {
                desired
                    .issues
                    .push(format!("duplicate id {id:?}: the later row is skipped"));
                continue;
            }
            let value = flat.value;
            let group = value.get("group").is_some_and(truthy);
            let name = value.get("name").and_then(Value::as_str).map(str::to_owned);
            let disabled = match value.get("disabled") {
                Some(d) if is_expression(d) => Err(LoaderError::Expression(
                    "no expression evaluator is installed".into(),
                )),
                Some(d) => Ok(truthy(d)),
                None => Ok(false),
            };
            let config = if group {
                Value::Null
            } else {
                value.get("config").cloned().unwrap_or(Value::Null)
            };
            let invalid = if !group && name.is_none() {
                Some(LoaderError::InvalidEntry(format!("{id:?} has no name")))
            } else if ["inject", "isolate"]
                .iter()
                .any(|key| value.get(*key).is_some_and(|v| !v.is_null()))
            {
                Some(LoaderError::Unsupported(
                    "inject / isolate in the config need the service catalog".into(),
                ))
            } else if contains_expression(&config) {
                Some(LoaderError::Expression(
                    "no expression evaluator is installed".into(),
                ))
            } else {
                None
            };
            desired.by_id.insert(id.clone(), desired.rows.len());
            desired.rows.push(Row {
                id,
                parent: flat.parent,
                value,
                name,
                group,
                owner: flat.owner,
                overridden: flat.overridden,
                disabled,
                config,
                invalid,
            });
        }
        desired
    }

    fn row(&self, id: &str) -> Option<&Row> {
        self.by_id.get(id).map(|&i| &self.rows[i])
    }

    /// The row and every enclosing group are enabled and valid.
    fn wanted(&self, row: &Row) -> bool {
        if row.invalid.is_some() || !matches!(row.disabled, Ok(false)) {
            return false;
        }
        match &row.parent {
            None => true,
            Some(parent) => self.row(parent).is_some_and(|p| self.wanted(p)),
        }
    }
}

// ── plugin glue ─────────────────────────────────────────────────

/// One generation's module and evaluated config.
#[derive(Clone)]
struct EntryConfig {
    resolved: Arc<Resolved>,
    value: Value,
}

struct EntryFactory {
    name: String,
    injects: Vec<TypeKey>,
}

impl PluginFactory<EntryConfig> for EntryFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn validate_config(&self, config: &EntryConfig) -> Result<(), CordisError> {
        config.resolved.factory.validate_config(&config.value)
    }

    fn build(&self, config: &EntryConfig) -> Result<Box<dyn Plugin>, CordisError> {
        config.resolved.factory.build(&config.value)
    }
}

/// A group row: spawns its children in its own context, so disabling the
/// group unloads them through the kernel's cascade.
struct GroupPlugin {
    inner: Weak<Inner>,
    id: String,
}

impl Plugin for GroupPlugin {
    fn name(&self) -> &str {
        &self.id
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let Some(inner) = self.inner.upgrade() else {
                return Ok(Effect::Done);
            };
            inner.attach(Some(self.id.clone()), ctx);
            let weak = self.inner.clone();
            let id = self.id.clone();
            Ok(Effect::Disposer(Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.detach(Some(id));
                }
                Ok(())
            })))
        })
    }
}

/// Mounts a [`Loader`] and provides it as a service.
pub struct LoaderPlugin {
    loader: Loader,
}

impl LoaderPlugin {
    pub fn new(resolver: impl Resolver, options: LoaderOptions) -> Self {
        Self {
            loader: Loader {
                inner: Arc::new(Inner {
                    resolver: Arc::new(resolver),
                    persist: options.persist,
                    op: tokio::sync::Mutex::new(()),
                    state: Mutex::new(State::default()),
                }),
            },
        }
    }

    pub fn handle(&self) -> Loader {
        self.loader.clone()
    }
}

impl Plugin for LoaderPlugin {
    fn name(&self) -> &str {
        "rutis-loader"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            // The service is released with the fiber.
            ctx.provide(self.loader.clone())?;
            self.loader.inner.attach(None, ctx);
            let weak = Arc::downgrade(&self.loader.inner);
            Ok(Effect::Disposer(Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.detach(None);
                }
                Ok(())
            })))
        })
    }
}

// ── internals ───────────────────────────────────────────────────

impl Inner {
    /// Register a running group's context and spawn its wanted children.
    fn attach(self: &Arc<Self>, group: Option<String>, ctx: &Ctx) {
        let mut state = self.state.lock().unwrap();
        if group.is_none() {
            state.last_root = Some(ctx.clone());
        }
        state.groups.insert(group.clone(), ctx.clone());
        self.spawn_children(&mut state, &group);
    }

    /// Forget a group's context and the records below it; the kernel
    /// unloads the fibers themselves.
    fn detach(&self, group: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.groups.remove(&group);
        let mut gone: Vec<Option<String>> = vec![group];
        while let Some(parent) = gone.pop() {
            let children: Vec<String> = state
                .running
                .iter()
                .filter(|(_, r)| r.parent == parent)
                .map(|(id, _)| id.clone())
                .collect();
            for id in children {
                state.running.remove(&id);
                state.groups.remove(&Some(id.clone()));
                gone.push(Some(id));
            }
        }
    }

    fn spawn_children(self: &Arc<Self>, state: &mut State, group: &Option<String>) {
        let Some(ctx) = state.groups.get(group).cloned() else {
            return;
        };
        let candidates: Vec<usize> = state
            .desired
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| &row.parent == group)
            .map(|(i, _)| i)
            .collect();
        for index in candidates {
            let row = &state.desired.rows[index];
            if state.running.contains_key(&row.id) || !state.desired.wanted(row) {
                continue;
            }
            let id = row.id.clone();
            let name = row.name.clone().unwrap_or_default();
            if row.group {
                let view = ctx.plugin(GroupPlugin {
                    inner: Arc::downgrade(self),
                    id: id.clone(),
                });
                state.running.insert(
                    id,
                    Running {
                        parent: group.clone(),
                        view,
                        group: true,
                        name,
                        injects: Vec::new(),
                        resolved: None,
                        config: Value::Null,
                    },
                );
                continue;
            }
            let Some(Ok(resolved)) = state.resolved.get(&name).cloned() else {
                continue;
            };
            let config = row.config.clone();
            let injects = resolved.factory.injects().to_vec();
            let view = ctx.plugin_with(
                EntryFactory {
                    name: name.clone(),
                    injects: injects.clone(),
                },
                EntryConfig {
                    resolved: resolved.clone(),
                    value: config.clone(),
                },
            );
            state.running.insert(
                id,
                Running {
                    parent: group.clone(),
                    view,
                    group: false,
                    name,
                    injects,
                    resolved: Some(resolved),
                    config,
                },
            );
        }
    }

    fn root(&self) -> Option<Ctx> {
        let state = self.state.lock().unwrap();
        state.groups.get(&None).cloned().or(state.last_root.clone())
    }

    fn check_open(&self) -> Result<(), LoaderError> {
        match self.root() {
            Some(root) if root.diagnostics().shutting_down => Err(LoaderError::Closed),
            _ => Ok(()),
        }
    }

    fn emit<E: Event>(&self, event: E) {
        let root = self.state.lock().unwrap().groups.get(&None).cloned();
        if let Some(root) = root {
            let _ = root
                .events()
                .emit(&root, &EventKey::<E>::of(), Arc::new(event));
        }
    }

    fn failures(state: &State) -> Vec<(Failure, String)> {
        let mut out = Vec::new();
        for row in &state.desired.rows {
            let parent_wanted = match &row.parent {
                None => true,
                Some(p) => state
                    .desired
                    .row(p)
                    .is_some_and(|p| state.desired.wanted(p)),
            };
            if !parent_wanted {
                continue;
            }
            let error = if let Some(invalid) = &row.invalid {
                Some(invalid.to_string())
            } else if let Err(e) = &row.disabled {
                Some(e.to_string())
            } else if matches!(row.disabled, Ok(true)) {
                None
            } else if let Some(running) = state.running.get(&row.id) {
                let snapshot = running.view.state();
                (snapshot.state == FiberState::Failed).then(|| {
                    snapshot
                        .error
                        .map_or_else(|| "failed".to_owned(), |e| e.to_string())
                })
            } else if row.group {
                None
            } else {
                match row.name.as_ref().and_then(|n| state.resolved.get(n)) {
                    Some(Err(e)) => Some(e.to_string()),
                    _ => None,
                }
            };
            if let Some(error) = error {
                out.push((
                    Failure {
                        id: row.id.clone(),
                        error,
                    },
                    row.value.to_string(),
                ));
            }
        }
        out
    }

    /// Bring the running tree to the current layers and wait until settled.
    /// The caller holds the operation lock.
    async fn reconcile_inner(self: &Arc<Self>) -> ReconcileReport {
        let (before, names) = {
            let mut state = self.state.lock().unwrap();
            let before = Self::failures(&state);
            state.desired = Desired::from_composed(apply_patches(&state.layers));
            let names: Vec<String> = state
                .desired
                .rows
                .iter()
                .filter(|row| !row.group && state.desired.wanted(row))
                .filter_map(|row| row.name.clone())
                .filter(|name| !state.resolved.contains_key(name))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            (before, names)
        };
        for name in names {
            let resolved = self.resolver.resolve(&name).await;
            self.state.lock().unwrap().resolved.insert(name, resolved);
        }

        let mut disposals = Vec::new();
        let mut updates = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            let state = &mut *state;
            // Records to keep as they are, or to update in place.
            let mut keep: HashSet<String> = HashSet::new();
            for (id, running) in &state.running {
                let Some(row) = state.desired.row(id) else {
                    continue;
                };
                if row.parent != running.parent
                    || row.group != running.group
                    || !state.desired.wanted(row)
                {
                    continue;
                }
                if !row.group {
                    let name = row.name.clone().unwrap_or_default();
                    match state.resolved.get(&name) {
                        Some(Ok(resolved))
                            if resolved.factory.injects() == running.injects.as_slice() => {}
                        _ => continue,
                    }
                }
                keep.insert(id.clone());
            }
            // A record whose group goes away goes with it.
            loop {
                let orphans: Vec<String> = keep
                    .iter()
                    .filter(|id| {
                        state.running[*id]
                            .parent
                            .as_ref()
                            .is_some_and(|p| !keep.contains(p))
                    })
                    .cloned()
                    .collect();
                if orphans.is_empty() {
                    break;
                }
                for id in orphans {
                    keep.remove(&id);
                }
            }
            let dropped: Vec<String> = state
                .running
                .keys()
                .filter(|id| !keep.contains(*id))
                .cloned()
                .collect();
            for id in dropped {
                let running = state.running.remove(&id).unwrap();
                state.groups.remove(&Some(id));
                disposals.push(running.view.dispose());
            }
            for (id, running) in state.running.iter_mut() {
                if running.group {
                    continue;
                }
                let row = state.desired.row(id).unwrap();
                let name = row.name.clone().unwrap_or_default();
                let Some(Ok(resolved)) = state.resolved.get(&name) else {
                    continue;
                };
                let same_module = running
                    .resolved
                    .as_ref()
                    .is_some_and(|r| Arc::ptr_eq(r, resolved));
                if same_module && running.config == row.config {
                    continue;
                }
                running.resolved = Some(resolved.clone());
                running.config = row.config.clone();
                running.name = name;
                updates.push(running.view.update(EntryConfig {
                    resolved: resolved.clone(),
                    value: row.config.clone(),
                }));
            }
            let groups: Vec<Option<String>> = state.groups.keys().cloned().collect();
            for group in groups {
                self.spawn_children(state, &group);
            }
        }
        for disposal in disposals {
            let _ = disposal.await;
        }
        for update in updates {
            let _ = update.await;
        }
        self.settle().await;

        let state = self.state.lock().unwrap();
        let after = Self::failures(&state);
        let new_failures = after
            .iter()
            .filter(|f| !before.contains(f))
            .map(|(f, _)| f.clone())
            .collect();
        ReconcileReport {
            warnings: state.desired.warnings.clone(),
            issues: state.desired.issues.clone(),
            new_failures,
            failures: after.into_iter().map(|(f, _)| f).collect(),
        }
    }

    /// Wait until no running row is in transition. Groups spawn children
    /// while loading, so repeat until the set of records stops changing.
    async fn settle(&self) {
        loop {
            let views: Vec<FiberView> = {
                let state = self.state.lock().unwrap();
                state.running.values().map(|r| r.view.clone()).collect()
            };
            let before: HashSet<PluginId> = views.iter().map(|v| v.id).collect();
            for view in &views {
                let _ = view.await;
            }
            let after: HashSet<PluginId> = {
                let state = self.state.lock().unwrap();
                state.running.values().map(|r| r.view.id).collect()
            };
            if before == after {
                return;
            }
        }
    }

    /// Check a row would start: resolve, validate the config, build and
    /// validate the instance. Nothing is spawned.
    async fn dry_run(&self, layers: &[Layer], id: &str) -> Result<(), LoaderError> {
        let desired = Desired::from_composed(apply_patches(layers));
        let Some(row) = desired.row(id) else {
            return Ok(());
        };
        if let Some(invalid) = &row.invalid {
            return Err(invalid.clone());
        }
        if let Err(e) = &row.disabled {
            return Err(e.clone());
        }
        if row.group || !desired.wanted(row) {
            return Ok(());
        }
        let name = row.name.clone().unwrap_or_default();
        let resolved = self.resolver.resolve(&name).await?;
        let config = row.config.clone();
        let checked = catch_unwind(AssertUnwindSafe(|| {
            resolved.factory.validate_config(&config)?;
            resolved.factory.build(&config)?.validate()
        }));
        let rejected = |error: CordisError| LoaderError::Rejected {
            id: id.to_owned(),
            error: Arc::new(error),
        };
        match checked {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(rejected(error)),
            Err(_) => Err(rejected(CordisError::PluginFailed(
                "panicked during the dry run".into(),
            ))),
        }
    }

    /// Apply one edit in memory: rewrite the editable layer, dry-run,
    /// reconcile, and roll back if rows newly fail. Nothing is persisted.
    async fn commit(self: &Arc<Self>, edit: &Edit) -> Result<(), LoaderError> {
        let (layers, editable, before) = {
            let state = self.state.lock().unwrap();
            let editable = state.editable.ok_or(LoaderError::NoEditableLayer)?;
            (state.layers.clone(), editable, Self::failures(&state))
        };
        let patches = apply_edit(&layers, editable, edit)?;
        let mut next = layers.clone();
        next[editable].patches = patches;
        if !matches!(
            edit,
            Edit::Remove { .. } | Edit::SetDisabled { disabled: true, .. }
        ) {
            if let Some(id) = edit.id() {
                self.dry_run(&next, id).await?;
            }
        }
        // A rename changes the module: resolve it afresh.
        if let Edit::Rename { name, .. } = edit {
            self.state.lock().unwrap().resolved.remove(name);
        }
        self.state.lock().unwrap().layers = next;
        let report = self.reconcile_inner().await;
        if report.new_failures.is_empty() {
            return Ok(());
        }
        self.state.lock().unwrap().layers = layers;
        self.reconcile_inner().await;
        // Compare with the state before the edit, not before the rollback.
        let rollback: Vec<Failure> = {
            let state = self.state.lock().unwrap();
            Self::failures(&state)
                .into_iter()
                .filter(|f| !before.contains(f))
                .map(|(f, _)| f)
                .collect()
        };
        if rollback.is_empty() {
            Err(LoaderError::ApplyFailed {
                failures: report.new_failures,
            })
        } else {
            Err(LoaderError::RollbackFailed {
                apply: report.new_failures,
                rollback,
            })
        }
    }

    /// Persist the pending queue; on a version conflict, reload the layer,
    /// replay the queue on it and try again. `current` is the queue index
    /// of the edit the caller is waiting for.
    async fn persist_queue(
        self: &Arc<Self>,
        mut current: Option<usize>,
    ) -> Result<(), LoaderError> {
        let mut current_error: Option<LoaderError> = None;
        let finish = |error: Option<LoaderError>| error.map_or(Ok(()), Err);
        for attempt in 0..=CONFLICT_RETRIES {
            let (layer, version, edits, patches) = {
                let state = self.state.lock().unwrap();
                let Some(editable) = state.editable else {
                    return finish(current_error);
                };
                if state.pending.is_empty() {
                    return finish(current_error);
                }
                (
                    state.layers[editable].name.clone(),
                    state.version.clone(),
                    state.pending.clone(),
                    state.layers[editable].patches.clone(),
                )
            };
            match self.persist.save(&layer, &version, &edits, &patches).await {
                Ok(version) => {
                    let mut state = self.state.lock().unwrap();
                    state.version = version;
                    state.pending.clear();
                    return finish(current_error);
                }
                Err(PersistError::Failed(message)) => {
                    return Err(current_error.unwrap_or(LoaderError::PersistFailed(message)));
                }
                Err(PersistError::Conflict) if attempt == CONFLICT_RETRIES => {
                    return Err(current_error.unwrap_or(LoaderError::Conflict));
                }
                Err(PersistError::Conflict) => {
                    let (latest, version) = self
                        .persist
                        .load(&layer)
                        .await
                        .map_err(|e| LoaderError::PersistFailed(e.to_string()))?;
                    let queue = {
                        let mut state = self.state.lock().unwrap();
                        let editable = state.editable.unwrap();
                        state.layers[editable].patches = latest;
                        state.version = version;
                        std::mem::take(&mut state.pending)
                    };
                    self.reconcile_inner().await;
                    let mut replaced = None;
                    for (index, edit) in queue.into_iter().enumerate() {
                        match self.commit(&edit).await {
                            Ok(()) => {
                                let mut state = self.state.lock().unwrap();
                                if current == Some(index) {
                                    replaced = Some(state.pending.len());
                                }
                                state.pending.push(edit);
                            }
                            Err(error) if current == Some(index) => current_error = Some(error),
                            Err(error) => self.emit(PendingEditDropped { edit, error }),
                        }
                    }
                    current = replaced;
                }
            }
        }
        finish(current_error)
    }

    async fn edit(self: &Arc<Self>, edit: Edit) -> Result<(), LoaderError> {
        let _op = self.op.lock().await;
        self.check_open()?;
        self.commit(&edit).await?;
        let index = {
            let mut state = self.state.lock().unwrap();
            state.pending.push(edit.clone());
            state.pending.len() - 1
        };
        let result = self.persist_queue(Some(index)).await;
        self.emit(LoaderChanged::Edited(edit));
        result
    }

    fn info(state: &State, row: &Row) -> EntryInfo {
        let running = state.running.get(&row.id);
        let resolved = row
            .name
            .as_ref()
            .and_then(|n| state.resolved.get(n))
            .and_then(|r| r.as_ref().ok());
        let status = if let Some(invalid) = &row.invalid {
            EntryStatus::Unresolved(invalid.clone())
        } else if let Err(e) = &row.disabled {
            EntryStatus::Unresolved(e.clone())
        } else if matches!(row.disabled, Ok(true)) {
            EntryStatus::Disabled
        } else if let Some(running) = running {
            EntryStatus::Running(running.view.state())
        } else if let Some(Err(e)) = row.name.as_ref().and_then(|n| state.resolved.get(n)) {
            if !row.group && state.desired.wanted(row) {
                EntryStatus::Unresolved(e.clone())
            } else {
                EntryStatus::Inactive
            }
        } else {
            EntryStatus::Inactive
        };
        EntryInfo {
            id: row.id.clone(),
            options: row.value.clone(),
            parent: row.parent.clone(),
            owner: row.owner.clone(),
            overridden: row
                .overridden
                .iter()
                .map(|(field, &layer)| {
                    let name = state
                        .layers
                        .get(layer)
                        .map(|l| l.name.clone())
                        .unwrap_or_default();
                    (field.clone(), name)
                })
                .collect(),
            status,
            plugin: running.map(|r| r.view.id),
            view: running.map(|r| r.view.clone()),
            schema: resolved.and_then(|r| r.schema.clone()),
            meta: resolved.map(|r| r.meta.clone()).unwrap_or(Value::Null),
        }
    }
}

fn generate_id(taken: impl Fn(&str) -> bool) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mixed =
            (seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let id = format!("{:08x}", (mixed >> 32) as u32);
        if !taken(&id) {
            return id;
        }
    }
}

// ── public API ──────────────────────────────────────────────────

impl Loader {
    /// Replace the layers and bring the running tree to them. Queued,
    /// unsaved edits are replayed on the new editable layer and saved.
    pub async fn reconcile(
        &self,
        layers: Vec<Layer>,
        editable: Option<Editable>,
    ) -> Result<ReconcileReport, LoaderError> {
        let inner = &self.inner;
        let _op = inner.op.lock().await;
        inner.check_open()?;
        let replay = {
            let mut state = inner.state.lock().unwrap();
            let index = match &editable {
                None => None,
                Some(e) => Some(layers.iter().position(|l| l.name == e.layer).ok_or_else(
                    || LoaderError::InvalidEntry(format!("no layer named {:?}", e.layer)),
                )?),
            };
            state.layers = layers;
            state.editable = index;
            if let Some(e) = editable {
                state.version = e.version;
            }
            if index.is_some() {
                std::mem::take(&mut state.pending)
            } else {
                Vec::new()
            }
        };
        let mut report = inner.reconcile_inner().await;
        if !replay.is_empty() {
            for edit in replay {
                match inner.commit(&edit).await {
                    Ok(()) => inner.state.lock().unwrap().pending.push(edit),
                    Err(error) => inner.emit(PendingEditDropped { edit, error }),
                }
            }
            let _ = inner.persist_queue(None).await;
            let state = inner.state.lock().unwrap();
            report.failures = Inner::failures(&state)
                .into_iter()
                .map(|(f, _)| f)
                .collect();
        }
        inner.emit(LoaderChanged::Reconciled);
        Ok(report)
    }

    /// The current layers.
    pub fn layers(&self) -> Vec<Layer> {
        self.inner.state.lock().unwrap().layers.clone()
    }

    /// Edits applied but not yet persisted.
    pub fn pending(&self) -> Vec<Edit> {
        self.inner.state.lock().unwrap().pending.clone()
    }

    /// Retry persisting the pending queue.
    pub async fn flush(&self) -> Result<(), LoaderError> {
        let _op = self.inner.op.lock().await;
        self.inner.persist_queue(None).await
    }

    /// Every row in tree order.
    pub fn entries(&self) -> Vec<EntryInfo> {
        let state = self.inner.state.lock().unwrap();
        state
            .desired
            .rows
            .iter()
            .map(|row| Inner::info(&state, row))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<EntryInfo> {
        let state = self.inner.state.lock().unwrap();
        state.desired.row(id).map(|row| Inner::info(&state, row))
    }

    /// The row whose fiber is `plugin` or an ancestor of it.
    pub fn locate(&self, plugin: PluginId) -> Option<String> {
        let (records, root) = {
            let state = self.inner.state.lock().unwrap();
            let records: HashMap<PluginId, String> = state
                .running
                .iter()
                .map(|(id, r)| (r.view.id, id.clone()))
                .collect();
            (records, state.groups.get(&None).cloned())
        };
        if let Some(id) = records.get(&plugin) {
            return Some(id.clone());
        }
        let parents: HashMap<PluginId, Option<PluginId>> = root?
            .diagnostics()
            .plugins
            .into_iter()
            .map(|p| (p.id, p.parent))
            .collect();
        let mut current = parents.get(&plugin).copied().flatten();
        while let Some(id) = current {
            if let Some(entry) = records.get(&id) {
                return Some(entry.clone());
            }
            current = parents.get(&id).copied().flatten();
        }
        None
    }

    /// The config schema of a module, without loading it.
    pub async fn schema_of(&self, name: &str) -> Result<Option<Value>, LoaderError> {
        Ok(self.inner.resolver.resolve(name).await?.schema.clone())
    }

    /// The config a row's plugin receives (read-only).
    pub fn evaluated(&self, id: &str) -> Option<Result<Value, LoaderError>> {
        let state = self.inner.state.lock().unwrap();
        let row = state.desired.row(id)?;
        Some(if contains_expression(&row.config) {
            Err(LoaderError::Expression(
                "no expression evaluator is installed".into(),
            ))
        } else {
            Ok(row.config.clone())
        })
    }

    /// Add a row to the editable layer. Waits until the tree settles; a
    /// row waiting for dependencies (`Pending`) counts as settled.
    pub async fn create(
        &self,
        entry: NewEntry,
        parent: Option<&str>,
        position: Option<usize>,
    ) -> Result<(String, Option<FiberView>), LoaderError> {
        let id = match entry.id {
            Some(id) => id,
            None => {
                let state = self.inner.state.lock().unwrap();
                generate_id(|id| state.desired.by_id.contains_key(id))
            }
        };
        let mut object = serde_json::Map::new();
        object.insert("id".into(), Value::String(id.clone()));
        object.insert("name".into(), Value::String(entry.name));
        if entry.group {
            object.insert("group".into(), Value::Bool(true));
        }
        if entry.disabled {
            object.insert("disabled".into(), Value::Bool(true));
        }
        if !entry.config.is_null() || entry.group {
            let config = if entry.group && entry.config.is_null() {
                Value::Array(Vec::new())
            } else {
                entry.config
            };
            object.insert("config".into(), config);
        }
        self.inner
            .edit(Edit::Create {
                entry: Value::Object(object),
                parent: parent.map(str::to_owned),
                position,
            })
            .await?;
        let view = self
            .inner
            .state
            .lock()
            .unwrap()
            .running
            .get(&id)
            .map(|r| r.view.clone());
        Ok((id, view))
    }

    pub async fn update(&self, id: &str, config: Value) -> Result<(), LoaderError> {
        self.inner
            .edit(Edit::Update {
                id: id.to_owned(),
                config,
            })
            .await
    }

    pub async fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), LoaderError> {
        self.inner
            .edit(Edit::SetDisabled {
                id: id.to_owned(),
                disabled,
            })
            .await
    }

    pub async fn rename_module(&self, id: &str, name: &str) -> Result<(), LoaderError> {
        self.inner
            .edit(Edit::Rename {
                id: id.to_owned(),
                name: name.to_owned(),
            })
            .await
    }

    pub async fn move_to(
        &self,
        id: &str,
        parent: Option<&str>,
        position: Option<usize>,
    ) -> Result<(), LoaderError> {
        self.inner
            .edit(Edit::Move {
                id: id.to_owned(),
                parent: parent.map(str::to_owned),
                position,
            })
            .await
    }

    pub async fn remove(&self, id: &str) -> Result<(), LoaderError> {
        self.inner.edit(Edit::Remove { id: id.to_owned() }).await
    }

    /// Resolve a row's module again (a dylib upgrade, say) and apply it.
    /// Changes no layer and persists nothing.
    pub async fn reload(&self, id: &str) -> Result<ReconcileReport, LoaderError> {
        let inner = &self.inner;
        let _op = inner.op.lock().await;
        inner.check_open()?;
        {
            let mut state = inner.state.lock().unwrap();
            let name = state
                .desired
                .row(id)
                .ok_or_else(|| LoaderError::UnknownEntry(id.to_owned()))?
                .name
                .clone()
                .unwrap_or_default();
            state.resolved.remove(&name);
        }
        let report = inner.reconcile_inner().await;
        inner.emit(LoaderChanged::Reloaded(id.to_owned()));
        Ok(report)
    }

    /// Restart a row's fiber. Changes no layer and persists nothing.
    pub async fn restart(&self, id: &str) -> Result<(), LoaderError> {
        let view = self
            .inner
            .state
            .lock()
            .unwrap()
            .running
            .get(id)
            .map(|r| r.view.clone())
            .ok_or_else(|| LoaderError::UnknownEntry(id.to_owned()))?;
        view.restart().await.map_err(|error| LoaderError::Rejected {
            id: id.to_owned(),
            error,
        })
    }

    /// Wait until no row is in transition.
    pub async fn settled(&self) {
        self.inner.settle().await
    }
}
