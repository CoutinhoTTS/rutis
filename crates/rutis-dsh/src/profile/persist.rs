//! The user layer `<profile>/cordis.patch.yml` as rutis-loader storage.

use std::path::PathBuf;
use std::time::Duration;

use rutis::BoxFuture;
use rutis_loader::{Edit, LoaderError, Patch, Persist, PersistError, Version};
use serde_json::Value;

use super::layers::{parse_patch_list, unanchor_patch, version_of, ProfileContext};
use super::lock::{with_file_lock, write_atomic, DEFAULT_WAIT};
use super::yaml::Document;

/// What dsh writes into a new profile's user layer.
pub const USER_LAYER_TEMPLATE: &str =
    "# Your patch layer for this dsh profile, applied after every bundle layer:
# a top-level YAML array of loader patch entries (id-targeted config
# overrides, disables, and insert lists; `!!js` expressions allowed).
[]
";

/// Stores the user layer. Saves hold the profile's writer lock (the one
/// dsh's config editor and plugin manager take: `<profile>/package.json`),
/// check the version, and rewrite only the patches that changed, so the
/// comments on the others stay.
#[derive(Clone)]
pub struct UserLayerStore {
    file: PathBuf,
    lock_target: PathBuf,
    wait: Duration,
}

impl UserLayerStore {
    pub fn new(context: &ProfileContext) -> Self {
        Self {
            file: context.user_layer_path(),
            lock_target: context.dir.join("package.json"),
            wait: DEFAULT_WAIT,
        }
    }

    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    fn read(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.file) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn save_locked(&self, expected: &Version, patches: &[Patch]) -> Result<Version, PersistError> {
        let failed = |e: &dyn std::fmt::Display| {
            PersistError::Failed(format!("{}: {e}", self.file.display()))
        };
        let current = self.read().map_err(|e| failed(&e))?;
        if version_of(current.as_deref().map(str::as_bytes)) != *expected {
            return Err(PersistError::Conflict);
        }
        let doc = Document::parse(current.as_deref().unwrap_or(USER_LAYER_TEMPLATE))
            .map_err(|e| failed(&e))?;
        let values: Vec<Value> = patches
            .iter()
            .map(|patch| {
                let mut patch = patch.clone();
                unanchor_patch(&mut patch, &self.file);
                serde_json::to_value(patch).unwrap_or(Value::Null)
            })
            .collect();
        // Items compare as patches, so key order and spelling do not matter.
        let normalize = |value: &Value| {
            serde_json::from_value::<Patch>(value.clone())
                .ok()
                .and_then(|p| serde_json::to_value(p).ok())
                .unwrap_or_else(|| value.clone())
        };
        let reads_back =
            |text: &str| parse_patch_list(&self.file, text).is_ok_and(|read| read == patches);
        let mut rendered = doc.render(&values, normalize);
        if !reads_back(&rendered) {
            rendered = doc.render_fresh(&values);
            if !reads_back(&rendered) {
                return Err(failed(
                    &"the rendered layer does not read back as the patches",
                ));
            }
        }
        write_atomic(&self.file, &rendered).map_err(|e| failed(&e))?;
        Ok(version_of(Some(rendered.as_bytes())))
    }
}

impl Persist for UserLayerStore {
    fn load<'a>(
        &'a self,
        _layer: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<Patch>, Version), LoaderError>> {
        Box::pin(async move {
            let text = self.read().map_err(|e| LoaderError::Resolve {
                name: self.file.display().to_string(),
                message: e.to_string(),
            })?;
            let version = version_of(text.as_deref().map(str::as_bytes));
            let patches = match &text {
                None => Vec::new(),
                Some(text) => {
                    parse_patch_list(&self.file, text).map_err(|e| LoaderError::Resolve {
                        name: self.file.display().to_string(),
                        message: e.to_string(),
                    })?
                }
            };
            Ok((patches, version))
        })
    }

    fn save<'a>(
        &'a self,
        _layer: &'a str,
        expected: &'a Version,
        _edits: &'a [Edit],
        patches: &'a [Patch],
    ) -> BoxFuture<'a, Result<Version, PersistError>> {
        let this = self.clone();
        let expected = expected.clone();
        let patches = patches.to_vec();
        Box::pin(async move {
            // Blocking file work: waiting for the lock, read, render, rename.
            tokio::task::spawn_blocking(move || {
                with_file_lock(&this.lock_target, this.wait, || {
                    this.save_locked(&expected, &patches)
                })
                .map_err(|e| PersistError::Failed(e.to_string()))?
            })
            .await
            .map_err(|e| PersistError::Failed(e.to_string()))?
        })
    }
}
