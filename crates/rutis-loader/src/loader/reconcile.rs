//! Driving the running fibers towards the desired tree.

use std::collections::HashSet;
use std::sync::Arc;

use rutis::{Ctx, Event, EventKey, FiberState, FiberView, PluginId};
use serde_json::Value;

use crate::error::Failure;
use crate::patch::apply_patches;
use crate::LoaderError;

use super::desired::{Desired, Row};
use super::plugins::{EntryConfig, EntryFactory, GroupPlugin};
use super::{EntryInfo, EntryStatus, Inner, ReconcileReport, Running, State};

impl Inner {
    /// Register a running group's context and spawn its wanted children.
    pub(super) fn attach(self: &Arc<Self>, group: Option<String>, ctx: &Ctx) {
        let mut state = self.state.lock().unwrap();
        if group.is_none() {
            state.last_root = Some(ctx.clone());
        }
        state.groups.insert(group.clone(), ctx.clone());
        self.spawn_children(&mut state, &group);
    }

    /// Forget a group's context and the records below it; the kernel
    /// unloads the fibers themselves.
    pub(super) fn detach(&self, group: Option<String>) {
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

    pub(super) fn spawn_children(self: &Arc<Self>, state: &mut State, group: &Option<String>) {
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

    pub(super) fn root(&self) -> Option<Ctx> {
        let state = self.state.lock().unwrap();
        state.groups.get(&None).cloned().or(state.last_root.clone())
    }

    pub(super) fn check_open(&self) -> Result<(), LoaderError> {
        match self.root() {
            Some(root) if root.diagnostics().shutting_down => Err(LoaderError::Closed),
            _ => Ok(()),
        }
    }

    pub(super) fn emit<E: Event>(&self, event: E) {
        let root = self.state.lock().unwrap().groups.get(&None).cloned();
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
