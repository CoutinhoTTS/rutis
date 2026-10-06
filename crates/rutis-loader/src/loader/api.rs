//! The public `Loader` methods.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rutis::{FiberView, PluginId};
use serde_json::Value;

use crate::edit::Edit;
use crate::patch::{Layer, Patch};
use crate::LoaderError;

use super::{
    Editable, EntryInfo, Inner, Isolate, Loader, LoaderChanged, NewEntry, PendingEditDropped,
    ReconcileReport, RowInfo,
};

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

    /// The application's layers, as last given to `reconcile`.
    pub fn layers(&self) -> Vec<Layer> {
        self.inner.state.lock().unwrap().layers.clone()
    }

    /// The overlay layers, in composition order.
    pub fn overlays(&self) -> Vec<Layer> {
        self.inner.state.lock().unwrap().overlays.clone()
    }

    /// The editable layer and the stored version the loader holds for it.
    pub fn editable(&self) -> Option<Editable> {
        let state = self.inner.state.lock().unwrap();
        state
            .editable
            .map(|i| Editable::new(state.layers[i].name.clone(), state.version.clone()))
    }

    /// Set (`Some`) or remove (`None`) the overlay layer `name` and reconcile.
    ///
    /// Overlays are composed after the application's layers and kept across
    /// `reconcile`; they are never persisted and their rows cannot be edited
    /// (they sit above the editable layer). A development channel loads
    /// plugins this way without touching the user's configuration.
    pub async fn set_overlay(
        &self,
        name: &str,
        patches: Option<Vec<Patch>>,
    ) -> Result<ReconcileReport, LoaderError> {
        let inner = &self.inner;
        let _op = inner.op.lock().await;
        inner.check_open()?;
        {
            let mut state = inner.state.lock().unwrap();
            if state.layers.iter().any(|l| l.name == name) {
                return Err(LoaderError::InvalidEntry(format!(
                    "{name:?} is an application layer"
                )));
            }
            let at = state.overlays.iter().position(|l| l.name == name);
            match (at, patches) {
                (Some(i), Some(patches)) => state.overlays[i].patches = patches,
                (None, Some(patches)) => state.overlays.push(Layer::new(name, patches)),
                (Some(i), None) => {
                    state.overlays.remove(i);
                }
                (None, None) => {}
            }
        }
        let report = inner.reconcile_inner().await;
        inner.emit(LoaderChanged::Overlay(name.to_owned()));
        Ok(report)
    }

    /// Edits applied but not yet persisted.
    pub fn pending(&self) -> Vec<Edit> {
        self.inner.state.lock().unwrap().pending.clone()
    }

    /// Retry persisting the pending queue.
    pub async fn flush(&self) -> Result<(), LoaderError> {
        let _op = self.inner.op.lock().await;
        self.inner.check_open()?;
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

    /// The row whose fiber has `instance` (the plugin's `ctx.instance()`).
    /// Answers from inside `apply`: the loader records a fiber before its
    /// first load runs.
    pub fn row(&self, instance: rutis::InstanceId) -> Option<RowInfo> {
        let state = self.inner.state.lock().unwrap();
        let (id, _) = state
            .running
            .iter()
            .find(|(_, r)| r.view.instance() == instance)?;
        let row = state.desired.row(id)?;
        Some(RowInfo {
            id: id.clone(),
            isolate: row.raw_scope.isolate.clone(),
            inject: row.raw_scope.inject.clone(),
        })
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
            (records, state.groups.get(&None).map(|g| g.ctx.clone()))
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

    /// Resolve a module name with this loader's resolvers, as a row naming
    /// it would be (a host serving its peer looks plugins up this way).
    pub async fn resolve(
        &self,
        name: &str,
    ) -> Result<std::sync::Arc<crate::Resolved>, LoaderError> {
        self.inner.resolver.resolve(name).await
    }

    /// The config a row's plugin runs with (read-only). For a running row
    /// this is what the kernel holds, which differs from the desired config
    /// when the last update was rejected (see [`EntryInfo::rejected`]).
    pub fn evaluated(&self, id: &str) -> Option<Result<Value, LoaderError>> {
        let state = self.inner.state.lock().unwrap();
        let row = state.desired.row(id)?;
        if let Some(running) = state.running.get(id) {
            if !running.group {
                return Some(Ok(running.config.clone()));
            }
        }
        let base = state
            .groups
            .get(&row.parent)
            .or(state.groups.get(&None))
            .map(|g| match &row.scope {
                Ok(scope) => scope.context(&g.ctx),
                Err(_) => g.ctx.clone(),
            });
        Some(self.inner.eval().value(&row.config, base.as_ref()))
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
        if !entry.inject.is_empty() {
            object.insert(
                "inject".into(),
                Value::Array(entry.inject.into_iter().map(Value::String).collect()),
            );
        }
        if !entry.isolate.is_empty() {
            object.insert("isolate".into(), isolate_value(&entry.isolate));
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

    /// Replace the row's `inject`; an empty list removes it.
    pub async fn set_inject(&self, id: &str, inject: Vec<String>) -> Result<(), LoaderError> {
        self.inner
            .edit(Edit::SetInject {
                id: id.to_owned(),
                inject: (!inject.is_empty()).then_some(inject),
            })
            .await
    }

    /// Replace the row's `isolate`; an empty map removes it.
    pub async fn set_isolate(
        &self,
        id: &str,
        isolate: BTreeMap<String, Isolate>,
    ) -> Result<(), LoaderError> {
        let value = isolate_value(&isolate);
        self.inner
            .edit(Edit::SetIsolate {
                id: id.to_owned(),
                isolate: (!isolate.is_empty()).then(|| value.as_object().unwrap().clone()),
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
    ///
    /// All or nothing: if the new module does not resolve, its dry run
    /// fails, or rows newly fail with it, the previous module stays (and is
    /// reloaded after a failed apply) and the error is returned.
    pub async fn reload(&self, id: &str) -> Result<ReconcileReport, LoaderError> {
        self.resolve_again(id, false).await
    }

    /// Resolve a row whose resolution was provisional (made while what can
    /// describe it was unreachable) again: as [`Loader::reload`], except that
    /// a failure replaces the provisional resolution, so the row shows it
    /// (`Unresolved`) instead of starting on the placeholder.
    #[cfg(any(all(unix, feature = "runtimes"), feature = "peer"))]
    pub(crate) async fn refresh(&self, id: &str) -> Result<ReconcileReport, LoaderError> {
        self.resolve_again(id, true).await
    }

    async fn resolve_again(
        &self,
        id: &str,
        provisional: bool,
    ) -> Result<ReconcileReport, LoaderError> {
        let inner = &self.inner;
        let _op = inner.op.lock().await;
        inner.check_open()?;
        let (name, previous, layers, before) = {
            let state = inner.state.lock().unwrap();
            let row = state
                .desired
                .row(id)
                .ok_or_else(|| LoaderError::UnknownEntry(id.to_owned()))?;
            let name = row.name.clone().unwrap_or_default();
            (
                name.clone(),
                state.resolved.get(&name).cloned(),
                state.composed_layers(),
                Inner::failures(&state),
            )
        };
        let fresh = inner.resolver.resolve(&name).await;
        let resolved = match fresh {
            Ok(resolved) => resolved,
            Err(error) => {
                // A row that never resolved shows the new reason; a running
                // module stays, unless it was only a placeholder.
                if provisional || !matches!(previous, Some(Ok(_))) {
                    inner
                        .state
                        .lock()
                        .unwrap()
                        .resolved
                        .insert(name, Err(error.clone()));
                    inner.reconcile_inner().await;
                }
                return Err(error);
            }
        };
        if let Some(Ok(previous)) = &previous {
            if Arc::ptr_eq(previous, &resolved) {
                return Ok(inner.reconcile_inner().await);
            }
        }
        inner.dry_run(&layers, id, Some(resolved.clone())).await?;
        inner
            .state
            .lock()
            .unwrap()
            .resolved
            .insert(name.clone(), Ok(resolved));
        let report = inner.reconcile_inner().await;
        if !report.new_failures.is_empty() {
            {
                let mut state = inner.state.lock().unwrap();
                match previous {
                    Some(previous) => state.resolved.insert(name, previous),
                    None => state.resolved.remove(&name),
                };
            }
            inner.reconcile_inner().await;
            let lingering: Vec<crate::Failure> = {
                let state = inner.state.lock().unwrap();
                Inner::failures(&state)
                    .into_iter()
                    .filter(|f| !before.contains(f))
                    .map(|(f, _)| f)
                    .collect()
            };
            return Err(if lingering.is_empty() {
                LoaderError::ApplyFailed {
                    failures: report.new_failures,
                }
            } else {
                LoaderError::RollbackFailed {
                    apply: report.new_failures,
                    rollback: lingering,
                }
            });
        }
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

fn isolate_value(isolate: &BTreeMap<String, Isolate>) -> Value {
    Value::Object(
        isolate
            .iter()
            .map(|(name, iso)| (name.clone(), iso.to_value()))
            .collect(),
    )
}
