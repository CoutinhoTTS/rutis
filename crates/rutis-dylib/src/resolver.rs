//! Dylib plugins as rutis-loader rows.
//!
//! A row named `dylib:<dir>` loads the plugin bundle in `<dir>` (relative to
//! the resolver's root) through [`Loader`]. The same library resolves to the
//! same [`Resolved`], so re-resolving an unchanged bundle changes nothing;
//! after the bundle is replaced, `Loader::reload` on the row resolves the new
//! library and rutis-loader swaps it in place (or respawns when the plugin's
//! identity or dependencies changed).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Plugin, PluginFactory, TypeKey};
use rutis_loader::{LoaderError, Resolved, Resolver};
use rutis_sdk::serde_json::{json, Value};

use crate::{Loader, Module};

pub const PREFIX: &str = "dylib:";

/// Resolves `dylib:<dir>` names. Every other name is `NotFound`, so it fits
/// in a `rutis_loader::Chain`.
pub struct DylibResolver {
    loader: Arc<Loader>,
    root: PathBuf,
    resolved: Mutex<HashMap<String, Arc<Resolved>>>,
}

impl DylibResolver {
    /// # Safety
    ///
    /// Resolving a name loads its library into the process (see
    /// [`Loader::load`]): ELF initializers run as soon as it is opened. Only
    /// give rows naming trusted bundles to a loader using this resolver.
    pub unsafe fn new(loader: Arc<Loader>, root: impl Into<PathBuf>) -> Self {
        Self {
            loader,
            root: root.into(),
            resolved: Mutex::new(HashMap::new()),
        }
    }
}

impl Resolver for DylibResolver {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        Box::pin(async move {
            let Some(path) = name.strip_prefix(PREFIX) else {
                return Err(LoaderError::NotFound {
                    name: name.to_owned(),
                });
            };
            let dir = self.root.join(path);
            // SAFETY: the caller of `new` accepted loading these bundles.
            let module = unsafe { self.loader.load(&dir) }.map_err(|e| LoaderError::Resolve {
                name: name.to_owned(),
                message: e.to_string(),
            })?;
            let mut resolved = self.resolved.lock().unwrap();
            let entry = resolved
                .entry(module.library_sha256().to_owned())
                .or_insert_with(|| {
                    Arc::new(Resolved {
                        schema: module.schema().cloned(),
                        meta: json!({
                            "source": "dylib",
                            "id": module.id(),
                            "version": module.version(),
                            "librarySha256": module.library_sha256(),
                            "dir": dir.to_string_lossy(),
                        }),
                        factory: Arc::new(ModuleFactory { module }),
                    })
                });
            Ok(entry.clone())
        })
    }
}

/// A loaded module's factory over JSON config. Holding the `Arc<Module>`
/// keeps the retained library referenced while rows use it.
struct ModuleFactory {
    module: Arc<Module>,
}

impl PluginFactory<Value> for ModuleFactory {
    fn name(&self) -> &str {
        self.module.name()
    }

    fn injects(&self) -> &[TypeKey] {
        self.module.injects()
    }

    fn validate_config(&self, config: &Value) -> Result<(), CordisError> {
        self.module.factory().validate_config(config)
    }

    fn build(&self, config: &Value) -> Result<Box<dyn Plugin>, CordisError> {
        self.module.factory().build(config)
    }
}
