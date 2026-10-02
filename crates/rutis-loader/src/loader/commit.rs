//! Imperative edits: rewrite the editable layer, dry run, reconcile, roll
//! back, and persist through the pending queue.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use rutis::CordisError;

use crate::edit::{apply_edit, Edit};
use crate::error::Failure;
use crate::patch::{apply_patches, Layer};
use crate::{LoaderError, PersistError};

use super::desired::Desired;
use super::{Inner, LoaderChanged, PendingEditDropped};

/// How many times a version conflict is resolved by replaying the pending
/// queue before giving up with [`LoaderError::Conflict`].
const CONFLICT_RETRIES: usize = 3;

impl Inner {
    /// Check a row would start: resolve, validate the config, build and
    /// validate the instance. Nothing is spawned.
    pub(super) async fn dry_run(&self, layers: &[Layer], id: &str) -> Result<(), LoaderError> {
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
    pub(super) async fn commit(self: &Arc<Self>, edit: &Edit) -> Result<(), LoaderError> {
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
    pub(super) async fn persist_queue(
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

    pub(super) async fn edit(self: &Arc<Self>, edit: Edit) -> Result<(), LoaderError> {
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
}
