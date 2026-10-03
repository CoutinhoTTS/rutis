//! JavaScript (Cordis) plugins as loader rows, through rutis-interop.
//!
//! All rows share one Node process and one Cordis Context, so they resolve
//! each other's services natively, as in dsh. A row names an npm package
//! (resolved from the anchor `package.json`, `exports` honored), a subpath
//! of one, or a file (`file://`, absolute). Each row is loaded and disposed
//! on its own; its `isolate` and `inject` name Cordis services, so the
//! resolver handles them itself (`Resolved::foreign_scope`) and forwards
//! them. Its schemastery `Config` becomes the row's JSON Schema
//! (`meta.volatile` → `x-volatile`), and a volatile-only change is handed
//! to Cordis, which commits it in place and emits `loader/volatile-update`
//! to the plugin, as dsh's loader does.
//!
//! Services do not cross between the JavaScript rows and Rust plugins here;
//! a Rust service the rows need is given as a host ([`with_hosts`]).
//!
//! [`with_hosts`]: InteropResolver::with_hosts

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Effect, Listener, Plugin, PluginFactory};
use rutis_interop::{Host, Mount, Process};
use serde_json::{json, Value};

use crate::{volatile_key, Loader, LoaderError, Resolved, Resolver, VolatileUpdate};

pub struct InteropResolver {
    node_package: PathBuf,
    anchor: PathBuf,
    hosts: Mutex<Option<Vec<Host>>>,
    process: tokio::sync::OnceCell<Arc<Process>>,
    resolved: Mutex<HashMap<String, Arc<Resolved>>>,
}

impl InteropResolver {
    /// `node_package`: the rutis-interop npm runtime (`interop/node`, or a
    /// deployed `@arcships/rutis-interop`). `anchor`: the `package.json`
    /// plugins and Cordis resolve from.
    pub fn new(node_package: impl Into<PathBuf>, anchor: impl Into<PathBuf>) -> Self {
        Self {
            node_package: node_package.into(),
            anchor: anchor.into(),
            hosts: Mutex::new(Some(Vec::new())),
            process: tokio::sync::OnceCell::new(),
            resolved: Mutex::new(HashMap::new()),
        }
    }

    /// Rust services the rows may inject, registered before any row loads.
    pub fn with_hosts(self, hosts: Vec<Host>) -> Self {
        *self.hosts.lock().unwrap() = Some(hosts);
        self
    }

    async fn process(&self) -> Result<Arc<Process>, LoaderError> {
        self.process
            .get_or_try_init(|| async {
                let hosts = self.hosts.lock().unwrap().take().unwrap_or_default();
                Process::mount(
                    &self.node_package,
                    Mount {
                        hosts,
                        anchor: Some(&self.anchor),
                        ..Mount::default()
                    },
                )
                .await
                .map_err(|e| LoaderError::Resolve {
                    name: "<cordis runtime>".into(),
                    message: e.to_string(),
                })
            })
            .await
            .cloned()
    }
}

impl Resolver for InteropResolver {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        Box::pin(async move {
            let Some(entry) = resolve_entry(&self.anchor, name) else {
                return Err(LoaderError::NotFound {
                    name: name.to_owned(),
                });
            };
            if let Some(found) = self.resolved.lock().unwrap().get(name) {
                return Ok(found.clone());
            }
            let process = self.process().await?;
            let failed = |e: rutis_interop::Error| LoaderError::Resolve {
                name: name.to_owned(),
                message: e.to_string(),
            };
            let schema = process.row_schema(&entry).await.map_err(failed)?;
            let resolved = Arc::new(Resolved {
                factory: Arc::new(JsFactory {
                    name: name.to_owned(),
                    entry: entry.clone(),
                    process,
                }),
                schema,
                meta: json!({ "source": "interop", "entry": entry }),
                foreign_scope: true,
            });
            self.resolved
                .lock()
                .unwrap()
                .insert(name.to_owned(), resolved.clone());
            Ok(resolved)
        })
    }
}

struct JsFactory {
    name: String,
    entry: PathBuf,
    process: Arc<Process>,
}

impl PluginFactory<Value> for JsFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn build(&self, config: &Value) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(JsRow {
            name: self.name.clone(),
            entry: self.entry.clone(),
            config: config.clone(),
            process: self.process.clone(),
        }))
    }
}

/// One generation of a JavaScript row: loaded on apply, disposed on cleanup.
struct JsRow {
    name: String,
    entry: PathBuf,
    config: Value,
    process: Arc<Process>,
}

impl Plugin for JsRow {
    fn name(&self) -> &str {
        &self.name
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            // The fiber identity keys the row on the Cordis side: unique, and
            // new for every generation.
            let key = ctx.instance().to_string();
            let row = ctx
                .get::<Loader>()
                .and_then(|loader| loader.row(ctx.instance()));
            let (isolate, inject) = row.map(|row| (row.isolate, row.inject)).unwrap_or_default();
            self.process
                .load_row(&key, &self.entry, self.config.clone(), &isolate, &inject)
                .await
                .map_err(|e| CordisError::PluginFailed(e.to_string().into()))?;
            // Volatile-only changes go to Cordis, which commits them in place.
            ctx.events().on(
                ctx,
                &volatile_key(ctx),
                Forward {
                    key: key.clone(),
                    process: self.process.clone(),
                },
            )?;
            let process = self.process.clone();
            Ok(Effect::AsyncDisposer(Box::new(move || {
                Box::pin(async move {
                    process
                        .unload_row(&key)
                        .await
                        .map_err(|e| CordisError::PluginFailed(e.to_string().into()))
                })
            })))
        })
    }
}

struct Forward {
    key: String,
    process: Arc<Process>,
}

impl Listener<VolatileUpdate> for Forward {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        update: &'a VolatileUpdate,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.process
                .update_row(&self.key, update.config.clone())
                .await
                .map_err(|e| CordisError::PluginFailed(e.to_string().into()))?;
            Ok(None)
        })
    }
}

// ── Node's package resolution ───────────────────────────────────

fn package_dir(anchor: &Path, package: &str) -> Option<PathBuf> {
    let start = anchor.parent()?;
    start
        .ancestors()
        .filter(|dir| dir.file_name().is_none_or(|n| n != "node_modules"))
        .map(|dir| dir.join("node_modules").join(package))
        .find(|candidate| candidate.join("package.json").exists())
}

/// The `exports` target for `subpath` under the `node`/`import`/`default`
/// conditions.
fn export_target(exports: &Value, subpath: &str) -> Option<String> {
    fn pick(value: &Value) -> Option<String> {
        match value {
            Value::String(target) => Some(target.clone()),
            Value::Object(conditions) => ["node", "import", "default"]
                .iter()
                .find_map(|c| conditions.get(*c).and_then(pick)),
            Value::Array(options) => options.iter().find_map(pick),
            _ => None,
        }
    }
    match exports {
        Value::String(_) | Value::Array(_) if subpath == "." => pick(exports),
        Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => {
            map.get(subpath).and_then(pick)
        }
        Value::Object(_) if subpath == "." => pick(exports),
        _ => None,
    }
}

/// The module file a row name loads, or `None` when it is not a resolvable
/// JavaScript plugin name.
pub fn resolve_entry(anchor: &Path, name: &str) -> Option<PathBuf> {
    if name.starts_with("file:") {
        // A URL: percent-decoded, query and fragment dropped, as Node's
        // fileURLToPath; a remote host is not a local file.
        return url::Url::parse(name).ok()?.to_file_path().ok();
    }
    if name.starts_with('/') {
        return Some(PathBuf::from(name));
    }
    let mut parts = name.splitn(if name.starts_with('@') { 3 } else { 2 }, '/');
    let package = if name.starts_with('@') {
        format!("{}/{}", parts.next()?, parts.next()?)
    } else {
        parts.next()?.to_owned()
    };
    if package.is_empty() || package.contains(':') {
        return None;
    }
    let subpath = match parts.next() {
        Some(rest) => format!("./{rest}"),
        None => ".".to_owned(),
    };
    let dir = package_dir(anchor, &package)?;
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
    let relative = match manifest.get("exports") {
        Some(exports) => export_target(exports, &subpath)?,
        None if subpath == "." => manifest
            .get("module")
            .or_else(|| manifest.get("main"))
            .and_then(Value::as_str)
            .unwrap_or("index.js")
            .to_owned(),
        None => subpath,
    };
    Some(dir.join(relative.trim_start_matches("./")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_like_node() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("app/package.json");
        let modules = dir.path().join("node_modules");
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(anchor.clone(), "{}");
        write(
            modules.join("@s/a/package.json"),
            r#"{ "exports": { ".": { "require": "./c.cjs", "import": "./lib/index.js" }, "./tools": "./lib/tools.js" } }"#,
        );
        write(modules.join("b/package.json"), r#"{ "main": "main.js" }"#);
        write(
            modules.join("c/package.json"),
            r#"{ "exports": "./only.mjs" }"#,
        );
        assert_eq!(
            resolve_entry(&anchor, "@s/a"),
            Some(modules.join("@s/a/lib/index.js"))
        );
        assert_eq!(
            resolve_entry(&anchor, "@s/a/tools"),
            Some(modules.join("@s/a/lib/tools.js"))
        );
        assert_eq!(resolve_entry(&anchor, "@s/a/hidden"), None, "not exported");
        assert_eq!(resolve_entry(&anchor, "b"), Some(modules.join("b/main.js")));
        assert_eq!(
            resolve_entry(&anchor, "c"),
            Some(modules.join("c/only.mjs"))
        );
        assert_eq!(resolve_entry(&anchor, "missing"), None);
        assert_eq!(resolve_entry(&anchor, "dylib:x"), None);
        assert_eq!(
            resolve_entry(&anchor, "/abs/p.mjs"),
            Some(PathBuf::from("/abs/p.mjs"))
        );
    }

    #[test]
    fn file_urls_are_decoded() {
        let cases = [
            ("file:///abs/p.mjs", "/abs/p.mjs"),
            ("file:///my%20plugins/p.mjs", "/my plugins/p.mjs"),
            ("file:///%E6%8F%92%E4%BB%B6/p.mjs", "/插件/p.mjs"),
            ("file:///a%23b/p.mjs", "/a#b/p.mjs"),
            ("file:///100%25/p.mjs", "/100%/p.mjs"),
            ("file:///p.mjs#fragment", "/p.mjs"),
            ("file:///p.mjs?v=2", "/p.mjs"),
            ("file://localhost/p.mjs", "/p.mjs"),
        ];
        let anchor = Path::new("/nowhere/package.json");
        for (url, path) in cases {
            assert_eq!(
                resolve_entry(anchor, url),
                Some(PathBuf::from(path)),
                "{url}"
            );
        }
        assert_eq!(resolve_entry(anchor, "file://server/share/p.mjs"), None);
    }
}
