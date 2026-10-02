//! Driving the running fibers towards the desired tree.

use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use rutis::{CordisError, Ctx, Event, EventKey, FiberState, FiberView, PluginId, TypeKey};
use serde_json::Value;

use crate::error::Failure;
use crate::patch::{apply_patches, Layer};
use crate::resolver::Resolved;
use crate::volatile::{volatile_change, volatile_paths, VolatileUpdate};
use crate::LoaderError;

use super::desired::{Desired, Eval, Row, RowScope};
use super::plugins::{EntryConfig, EntryFactory, GroupPlugin};
use super::{EntryInfo, EntryStatus, Group, Inner, LoaderChanged, ReconcileReport, Running, State};

/// The module's own injects followed by the row's `inject`, deduplicated.
fn combined_injects(resolved: &Resolved, scope: &RowScope) -> Vec<TypeKey> {
    let mut keys = resolved.factory.injects().to_vec();
    for key in scope.inject_keys() {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    keys
}

/// The catalog scope a row runs with: none for a resolver that handles
/// scope itself, else the row's resolved scope (`None` when it failed).
fn effective_scope(resolved: &Resolved, row: &Row) -> Option<RowScope> {
    if resolved.foreign_scope {
        Some(RowScope::default())
    } else {
        row.scope.as_ref().ok().cloned()
    }
}

impl Inner {
    pub(super) fn eval(&self) -> Eval<'_> {
        Eval {
            expressions: self.expressions.as_deref(),
            catalog: &self.catalog,
        }
    }

    /// Compose `layers` and read the rows, evaluating `disabled` with the
    /// root context.
    pub(super) fn build_desired(&self, layers: &[Layer], root: Option<&Ctx>) -> Desired {
        Desired::from_composed(apply_patches(layers), &self.eval(), root)
    }

    pub(super) fn next_token(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.next_token += 1;
        state.next_token
    }

    /// Register a running group's context and spawn its wanted children.
    /// A group instance that is no longer the current record of its row
    /// (an older spawn still loading) registers nothing.
    pub(super) fn attach(self: &Arc<Self>, group: Option<String>, token: u64, ctx: &Ctx) {
        let mut state = self.state.lock().unwrap();
        match &group {
            None => state.last_root = Some(ctx.clone()),
            Some(id) => {
                if state.running.get(id).map(|r| r.token) != Some(token) {
                    return;
                }
            }
        }
        state.groups.insert(
            group.clone(),
            Group {
                ctx: ctx.clone(),
                token,
            },
        );
        self.spawn_children(&mut state, &group);
    }

    /// Forget a group's context and the records spawned in it; the kernel
    /// unloads the fibers themselves. Only the instance that registered
    /// the context (same token) can remove it, so a late cleanup of an old
    /// instance leaves a newer one alone.
    pub(super) fn detach(&self, group: Option<String>, token: u64) {
        let mut state = self.state.lock().unwrap();
        if state.groups.get(&group).map(|g| g.token) != Some(token) {
            return;
        }
        state.groups.remove(&group);
        let mut gone: Vec<(Option<String>, u64)> = vec![(group, token)];
        while let Some((parent, parent_token)) = gone.pop() {
            let children: Vec<String> = state
                .running
                .iter()
                .filter(|(_, r)| r.parent == parent && r.parent_token == parent_token)
                .map(|(id, _)| id.clone())
                .collect();
            for id in children {
                let child = state.running.remove(&id).unwrap();
                let key = Some(id.clone());
                if state.groups.get(&key).map(|g| g.token) == Some(child.token) {
                    state.groups.remove(&key);
                }
                gone.push((key, child.token));
            }
        }
    }

    pub(super) fn spawn_children(self: &Arc<Self>, state: &mut State, group: &Option<String>) {
        let Some((ctx, parent_token)) = state.groups.get(group).map(|g| (g.ctx.clone(), g.token))
        else {
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
            let scope = row.raw_scope.signature();
            // A resolver that handles isolate/inject itself (foreign scope)
            // gets the parent context as is; others need the catalog's keys.
            let resolved = if row.group {
                None
            } else {
                match state.resolved.get(&name).cloned() {
                    Some(Ok(resolved)) => Some(resolved),
                    _ => continue,
                }
            };
            let foreign = resolved.as_ref().is_some_and(|r| r.foreign_scope);
            let rust_scope = if foreign {
                RowScope::default()
            } else {
                match &row.scope {
                    Ok(scope) => scope.clone(),
                    Err(error) => {
                        state.rejected.insert(id, error.clone());
                        continue;
                    }
                }
            };
            let row_ctx = rust_scope.context(&ctx);
            let extra: Vec<TypeKey> = rust_scope.inject_keys().cloned().collect();
            state.next_token += 1;
            let token = state.next_token;
            if row.group {
                let view = row_ctx.plugin(GroupPlugin {
                    inner: Arc::downgrade(self),
                    id: id.clone(),
                    injects: extra,
                    token,
                });
                self.monitor(id.clone(), view.clone());
                state.running.insert(
                    id,
                    Running {
                        parent: group.clone(),
                        token,
                        parent_token,
                        view,
                        group: true,
                        name,
                        injects: Vec::new(),
                        factory_name: String::new(),
                        resolved: None,
                        config: Value::Null,
                        scope,
                        ctx: row_ctx,
                    },
                );
                continue;
            }
            let resolved = resolved.expect("a leaf row resolved above");
            let config = match self.eval().value(&row.config, Some(&row_ctx)) {
                Ok(config) => config,
                Err(error) => {
                    state.rejected.insert(id, error);
                    continue;
                }
            };
            // The kernel's first load only builds and validates the instance;
            // check the config here as `update` and the dry run do.
            let checked = catch_unwind(AssertUnwindSafe(|| {
                resolved.factory.validate_config(&config)
            }))
            .unwrap_or_else(|_| Err(CordisError::PluginFailed("validate_config panicked".into())));
            if let Err(error) = checked {
                state.rejected.insert(
                    id.clone(),
                    LoaderError::Rejected {
                        id,
                        error: Arc::new(error),
                    },
                );
                continue;
            }
            state.rejected.remove(&id);
            let injects = combined_injects(&resolved, &rust_scope);
            let view = row_ctx.plugin_with(
                EntryFactory {
                    name: name.clone(),
                    injects: injects.clone(),
                },
                EntryConfig {
                    resolved: resolved.clone(),
                    value: config.clone(),
                },
            );
            self.monitor(id.clone(), view.clone());
            state.running.insert(
                id,
                Running {
                    parent: group.clone(),
                    token,
                    parent_token,
                    view,
                    group: false,
                    name,
                    injects,
                    factory_name: resolved.factory.name().to_owned(),
                    resolved: Some(resolved),
                    config,
                    scope,
                    ctx: row_ctx,
                },
            );
        }
    }

    /// Watch a spawned fiber; if it ends while still this row's record, the
    /// plugin disposed itself: drop the record and disable the row, as
    /// cordis's loader does. Disposals the loader starts (or a group's
    /// cascade) remove the record first, so they are not mistaken for it.
    fn monitor(self: &Arc<Self>, id: String, view: FiberView) {
        let weak = Arc::downgrade(self);
        let mut watch = view.watch();
        tokio::spawn(async move {
            loop {
                if watch.borrow().state == FiberState::Disposed {
                    break;
                }
                if watch.changed().await.is_err() {
                    return;
                }
            }
            let Some(inner) = weak.upgrade() else {
                return;
            };
            {
                let mut state = inner.state.lock().unwrap();
                match state.running.get(&id) {
                    Some(running) if running.view.id == view.id => {
                        let running = state.running.remove(&id).unwrap();
                        let key = Some(id.clone());
                        if state.groups.get(&key).map(|g| g.token) == Some(running.token) {
                            state.groups.remove(&key);
                        }
                    }
                    _ => return,
                }
            }
            let loader = super::Loader {
                inner: inner.clone(),
            };
            let error = loader
                .set_disabled(&id, true)
                .await
                .err()
                .map(|e| e.to_string());
            inner.emit(LoaderChanged::SelfDisposed { id, error });
        });
    }

    pub(super) fn root(&self) -> Option<Ctx> {
        let state = self.state.lock().unwrap();
        state
            .groups
            .get(&None)
            .map(|g| g.ctx.clone())
            .or(state.last_root.clone())
    }

    pub(super) fn check_open(&self) -> Result<(), LoaderError> {
        match self.root() {
            Some(root) if root.diagnostics().shutting_down => Err(LoaderError::Closed),
            _ => Ok(()),
        }
    }

    pub(super) fn emit<E: Event>(&self, event: E) {
        let root = self
            .state
            .lock()
            .unwrap()
            .groups
            .get(&None)
            .map(|g| g.ctx.clone());
        if let Some(root) = root {
            let _ = root
                .events()
                .emit(&root, &EventKey::<E>::of(), Arc::new(event));
        }
    }

    pub(super) fn failures(state: &State) -> Vec<(Failure, String)> {
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
            } else if let Some(rejected) = state.rejected.get(&row.id) {
                Some(rejected.to_string())
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
    pub(super) async fn reconcile_inner(self: &Arc<Self>) -> ReconcileReport {
        let (before, names) = {
            let mut state = self.state.lock().unwrap();
            let before = Self::failures(&state);
            let root = state.groups.get(&None).map(|g| g.ctx.clone());
            state.desired = self.build_desired(&state.composed_layers(), root.as_ref());
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

        // Old instances go first: a respawned provider must not meet its
        // predecessor's service, and a group's cleanup must not race the
        // registration of its successor.
        let disposals = {
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
                    || row.raw_scope.signature() != running.scope
                {
                    continue;
                }
                if !row.group {
                    let name = row.name.clone().unwrap_or_default();
                    match state.resolved.get(&name) {
                        Some(Ok(resolved))
                            if effective_scope(resolved, row).is_some_and(|s| {
                                combined_injects(resolved, &s) == running.injects
                            }) && resolved.factory.name() == running.factory_name => {}
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
            let mut disposals = Vec::new();
            for id in dropped {
                let running = state.running.remove(&id).unwrap();
                let key = Some(id);
                if state.groups.get(&key).map(|g| g.token) == Some(running.token) {
                    state.groups.remove(&key);
                }
                disposals.push(running.view.dispose());
            }
            let wanted: HashSet<String> = state
                .desired
                .rows
                .iter()
                .filter(|row| state.desired.wanted(row))
                .map(|row| row.id.clone())
                .collect();
            state.rejected.retain(|id, _| wanted.contains(id));
            disposals
        };
        for disposal in disposals {
            let _ = disposal.await;
        }

        let mut updates = Vec::new();
        let mut notifications = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            let state = &mut *state;
            let mut settled = Vec::new();
            let mut unevaluable = Vec::new();
            let mut volatile = Vec::new();
            for (id, running) in state.running.iter() {
                if running.group {
                    continue;
                }
                let row = state.desired.row(id).unwrap();
                let name = row.name.clone().unwrap_or_default();
                let Some(Ok(resolved)) = state.resolved.get(&name) else {
                    continue;
                };
                // Expressions are evaluated where the plugin runs.
                let desired = match self.eval().value(&row.config, Some(&running.ctx)) {
                    Ok(value) => value,
                    Err(error) => {
                        unevaluable.push((id.clone(), error));
                        continue;
                    }
                };
                let same_module = running
                    .resolved
                    .as_ref()
                    .is_some_and(|r| Arc::ptr_eq(r, resolved));
                if same_module && running.config == desired {
                    // Already running what is wanted: an earlier rejection
                    // no longer applies.
                    settled.push(id.clone());
                    continue;
                }
                if same_module && running.view.state().state == FiberState::Active {
                    let paths = resolved
                        .schema
                        .as_ref()
                        .map(volatile_paths)
                        .unwrap_or_default();
                    if let Some(changed) = volatile_change(&running.config, &desired, &paths) {
                        volatile.push((id.clone(), resolved.clone(), desired, changed));
                        continue;
                    }
                }
                let config = EntryConfig {
                    resolved: resolved.clone(),
                    value: desired,
                };
                updates.push((
                    id.clone(),
                    running.view.clone(),
                    running.view.update(config),
                    name,
                ));
            }
            for id in settled {
                state.rejected.remove(&id);
            }
            // The plugin keeps its previous config.
            for (id, error) in unevaluable {
                state.rejected.insert(id, error);
            }
            // Volatile-only changes: store without restarting, then tell the
            // plugin. A refused store falls back to an ordinary update.
            for (id, resolved, desired, paths) in volatile {
                let Some(running) = state.running.get_mut(&id) else {
                    continue;
                };
                let config = EntryConfig {
                    resolved: resolved.clone(),
                    value: desired.clone(),
                };
                if running.view.set_config(config.clone()).is_ok() {
                    running.config = desired.clone();
                    state.rejected.remove(&id);
                    let running = &state.running[&id];
                    notifications.push((
                        running.ctx.clone(),
                        crate::volatile::key_for(running.view.instance()),
                        VolatileUpdate {
                            paths,
                            config: desired,
                        },
                    ));
                } else {
                    let name = running.name.clone();
                    updates.push((id, running.view.clone(), running.view.update(config), name));
                }
            }
            let groups: Vec<Option<String>> = state.groups.keys().cloned().collect();
            for group in groups {
                self.spawn_children(state, &group);
            }
        }
        for (ctx, key, update) in notifications {
            let _ = ctx.events().emit(&ctx, &key, Arc::new(update));
        }
        for (id, view, update, name) in updates {
            let result = update.await;
            // Record what the kernel actually holds: a rejected update keeps
            // the previous config; one that failed in apply stored the new.
            let current = view.current_config::<EntryConfig>();
            let mut state = self.state.lock().unwrap();
            let Some(running) = state.running.get_mut(&id) else {
                continue;
            };
            if running.view.id != view.id {
                continue;
            }
            if let Some(current) = current {
                running.resolved = Some(current.resolved.clone());
                running.config = current.value.clone();
                running.name = name;
            }
            match result {
                Err(error) if view.state().state != FiberState::Failed => {
                    state
                        .rejected
                        .insert(id.clone(), LoaderError::Rejected { id, error });
                }
                _ => {
                    state.rejected.remove(&id);
                }
            }
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
    pub(super) async fn settle(&self) {
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

    pub(super) fn info(state: &State, row: &Row) -> EntryInfo {
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
        } else if let Some(rejected) = state.rejected.get(&row.id) {
            EntryStatus::Unresolved(rejected.clone())
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
                    let name = state.layer_name(layer).unwrap_or_default().to_owned();
                    (field.clone(), name)
                })
                .collect(),
            status,
            rejected: state.rejected.get(&row.id).cloned(),
            plugin: running.map(|r| r.view.id),
            view: running.map(|r| r.view.clone()),
            schema: resolved.and_then(|r| r.schema.clone()),
            meta: resolved.map(|r| r.meta.clone()).unwrap_or(Value::Null),
        }
    }
}
