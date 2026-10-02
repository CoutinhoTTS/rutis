#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin, PluginFactory, TypeKey};
use rutis_loader::{
    Builtins, Edit, Editable, EntryStatus, Layer, Loader, LoaderError, LoaderOptions, LoaderPlugin,
    Patch, Persist, PersistError, Resolved, Resolver, Version,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub type Log = Arc<Mutex<Vec<String>>>;

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct EchoConfig {
    #[serde(default)]
    pub label: String,
    /// Rejected by `validate_config`.
    #[serde(default)]
    pub invalid: bool,
    /// Fails in `apply`, after the dry run passed.
    #[serde(default)]
    pub fail_apply: bool,
}

/// A service the `provider` module provides and `consumer` depends on.
#[derive(Debug)]
pub struct Dep(pub String);

struct Echo {
    kind: &'static str,
    config: EchoConfig,
    log: Log,
    broken: Arc<AtomicBool>,
}

impl Plugin for Echo {
    fn name(&self) -> &str {
        self.kind
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let label = self.config.label.clone();
            if self.config.fail_apply || self.broken.load(Ordering::SeqCst) {
                self.log.lock().unwrap().push(format!("fail:{label}"));
                return Err(CordisError::PluginFailed(format!("{label} failed").into()));
            }
            match self.kind {
                "provider" => {
                    ctx.provide(Dep(label.clone()))?;
                }
                "consumer" => {
                    let dep = ctx.require::<Dep>()?;
                    self.log.lock().unwrap().push(format!("consume:{}", dep.0));
                }
                _ => {}
            }
            self.log.lock().unwrap().push(format!("apply:{label}"));
            let log = self.log.clone();
            Ok(Effect::Disposer(Box::new(move || {
                log.lock().unwrap().push(format!("cleanup:{label}"));
                Ok(())
            })))
        })
    }
}

struct EchoFactory {
    kind: &'static str,
    injects: Vec<TypeKey>,
    log: Log,
    broken: Arc<AtomicBool>,
}

impl PluginFactory<EchoConfig> for EchoFactory {
    fn name(&self) -> &str {
        self.kind
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn validate_config(&self, config: &EchoConfig) -> Result<(), CordisError> {
        if config.invalid {
            return Err(CordisError::Validation {
                issues: vec!["invalid".into()],
            });
        }
        Ok(())
    }

    fn build(&self, config: &EchoConfig) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Echo {
            kind: self.kind,
            config: config.clone(),
            log: self.log.clone(),
            broken: self.broken.clone(),
        }))
    }
}

/// Builtins `echo`, `echo2`, `provider`, `consumer` (needs `Dep`) and
/// `flaky` (fails while `broken` is set), plus `late`, which only resolves
/// while `late` is set.
pub struct Harness {
    pub log: Log,
    pub broken: Arc<AtomicBool>,
    pub late: Arc<AtomicBool>,
}

impl Harness {
    pub fn new() -> Self {
        Self {
            log: Log::default(),
            broken: Arc::new(AtomicBool::new(false)),
            late: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn resolver(&self) -> Switch {
        let mut builtins = Builtins::new();
        for (name, kind, injects) in [
            ("echo", "echo", vec![]),
            ("echo2", "echo", vec![]),
            ("provider", "provider", vec![]),
            ("consumer", "consumer", vec![TypeKey::of::<Dep>()]),
            ("late", "echo", vec![]),
        ] {
            builtins.register::<EchoConfig, _>(
                name,
                EchoFactory {
                    kind,
                    injects,
                    log: self.log.clone(),
                    broken: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        builtins.register::<EchoConfig, _>(
            "flaky",
            EchoFactory {
                kind: "flaky",
                injects: vec![],
                log: self.log.clone(),
                broken: self.broken.clone(),
            },
        );
        builtins.register_fn::<Value, _>("plain", |_| Ok(Box::new(Nothing)));
        Switch {
            builtins,
            late: self.late.clone(),
        }
    }

    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

struct Nothing;

impl Plugin for Nothing {
    fn name(&self) -> &str {
        "nothing"
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

pub struct Switch {
    builtins: Builtins,
    late: Arc<AtomicBool>,
}

impl Resolver for Switch {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        if name == "late" && !self.late.load(Ordering::SeqCst) {
            return Box::pin(async move {
                Err(LoaderError::NotFound {
                    name: name.to_owned(),
                })
            });
        }
        self.builtins.resolve(name)
    }
}

/// In-memory storage shared by several loaders, with version CAS.
#[derive(Clone, Default)]
pub struct MemStore {
    pub inner: Arc<Mutex<Stored>>,
}

#[derive(Default)]
pub struct Stored {
    pub patches: Vec<Patch>,
    pub version: u64,
    pub fail: bool,
    pub saves: Vec<Vec<Edit>>,
}

impl MemStore {
    pub fn version(&self) -> Version {
        Version(self.inner.lock().unwrap().version.to_string())
    }

    pub fn patches(&self) -> Vec<Patch> {
        self.inner.lock().unwrap().patches.clone()
    }

    pub fn set_fail(&self, fail: bool) {
        self.inner.lock().unwrap().fail = fail;
    }
}

impl Persist for MemStore {
    fn load<'a>(
        &'a self,
        _layer: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<Patch>, Version), LoaderError>> {
        Box::pin(async move {
            let stored = self.inner.lock().unwrap();
            Ok((stored.patches.clone(), Version(stored.version.to_string())))
        })
    }

    fn save<'a>(
        &'a self,
        _layer: &'a str,
        expected: &'a Version,
        edits: &'a [Edit],
        patches: &'a [Patch],
    ) -> BoxFuture<'a, Result<Version, PersistError>> {
        Box::pin(async move {
            let mut stored = self.inner.lock().unwrap();
            if stored.fail {
                return Err(PersistError::Failed("disk full".into()));
            }
            if expected.0 != stored.version.to_string() {
                return Err(PersistError::Conflict);
            }
            stored.patches = patches.to_vec();
            stored.version += 1;
            stored.saves.push(edits.to_vec());
            Ok(Version(stored.version.to_string()))
        })
    }
}

pub fn patches(value: Value) -> Vec<Patch> {
    serde_json::from_value(value).unwrap()
}

/// Mount a loader on a fresh root.
pub async fn mount(resolver: Switch, store: MemStore) -> (Ctx, Loader) {
    let root = Ctx::root().unwrap();
    let plugin = LoaderPlugin::new(
        resolver,
        LoaderOptions {
            persist: Arc::new(store),
        },
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    (root, loader)
}

/// Layers `base` (given) and an editable `user` layer read from `store`.
pub async fn reconcile(loader: &Loader, base: Vec<Patch>, store: &MemStore) {
    let report = loader
        .reconcile(
            vec![
                Layer::new("base", base),
                Layer::new("user", store.patches()),
            ],
            Some(Editable::new("user", store.version())),
        )
        .await
        .unwrap();
    assert!(report.new_failures.is_empty(), "{report:?}");
}

pub fn state(loader: &Loader, id: &str) -> Option<FiberState> {
    match loader.get(id)?.status {
        EntryStatus::Running(snapshot) => Some(snapshot.state),
        _ => None,
    }
}
