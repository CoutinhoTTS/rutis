//! Reload a profile when its files change (dsh-hmr's profile reload).
//!
//! The watcher polls the files the last load read (manifest, bundle patch
//! files, user, home and overlay layers, included files) and, when any
//! changed, reads the profile again and reconciles the loader. Reconcile
//! reports new failures but never rolls back: the change came from outside.
//! A file that cannot be read or parsed keeps the running tree.

use std::path::PathBuf;
use std::time::Duration;

use rutis_loader::{Loader, ReconcileReport};

use super::layers::{load, version_of, ProfileContext};

/// What a reload produced.
#[derive(Debug)]
pub enum Reload {
    Reconciled(ReconcileReport),
    /// The profile could not be read, or the loader refused; nothing changed.
    Failed(String),
}

/// Stops watching when dropped.
pub struct ProfileWatcher {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ProfileWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn fingerprint(files: &[PathBuf]) -> Vec<(PathBuf, String)> {
    files
        .iter()
        .map(|file| {
            let version = version_of(std::fs::read(file).ok().as_deref());
            (file.clone(), version.0)
        })
        .collect()
}

/// Poll `context`'s files every `interval` and reconcile `loader` on change.
/// `files` is what the initial load read (`Profile::files`).
pub fn watch(
    loader: Loader,
    context: ProfileContext,
    files: Vec<PathBuf>,
    interval: Duration,
    on_reload: impl Fn(Reload) + Send + Sync + 'static,
) -> ProfileWatcher {
    // The baseline is taken now, not when the task first runs: a change
    // made right after this call must count as a change.
    let mut seen = fingerprint(&files);
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let now = fingerprint(&seen.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>());
            if now == seen {
                continue;
            }
            seen = now;
            let profile = match load(&context) {
                Ok(profile) => profile,
                Err(error) => {
                    on_reload(Reload::Failed(error.to_string()));
                    continue;
                }
            };
            // Files may have come or gone (a bundle added, an include).
            seen = fingerprint(&profile.files);
            match loader
                .reconcile(profile.layers, Some(profile.editable))
                .await
            {
                Ok(report) => on_reload(Reload::Reconciled(report)),
                Err(error) => on_reload(Reload::Failed(error.to_string())),
            }
        }
    });
    ProfileWatcher { task }
}
