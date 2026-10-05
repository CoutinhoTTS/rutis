//! A language runtime as a rutis plugin: one process, whose lifetime is the
//! plugin's. The Node runtime runs an empty Cordis Context
//! ([`RuntimePlugin::node`], feature `node`); the Python runtime runs leaf
//! plugins ([`RuntimePlugin::python`], feature `python`). Both speak the
//! same protocol and row contract, so nothing below depends on the language.
//!
//! Plugins loaded into it one by one (`Process::load_row`) depend on the
//! [`Runtime`] service, so they wait for the runtime natively and stop
//! when it goes away. The runtime itself depends on nothing: each plugin
//! leases the host services it uses (`Process::lease_host`), so the waiting
//! falls on that plugin, and runtimes never wait for each other.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, Plugin, TypeKey};
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::{HostDispatch, Mount, Process};

/// What the rows of a runtime need from it: describing plugins, exporting
/// their services (`rows.v2`) and leasing host services (`hosts`). Static
/// mounts use neither, so only [`RuntimePlugin`] checks them.
const ROW_FEATURES: [&str; 2] = ["rows.v2", "hosts"];

/// The service a running runtime provides.
pub struct Runtime {
    process: Arc<Process>,
    hosts: Arc<HashMap<String, Value>>,
}

impl Runtime {
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// The key the runtime named `name` provides this service under
    /// ([`RuntimePlugin::named`]; `"node"` by default).
    pub fn key(name: &str) -> TypeKey {
        TypeKey::keyed_dynamic::<Runtime>(name.to_owned())
    }

    /// The methods declared with [`RuntimePlugin::host`] for `name`.
    pub fn host_methods(&self, name: &str) -> Option<Value> {
        self.hosts.get(name).cloned()
    }
}

/// The key a host service named `name` is provided under, for example
/// `ctx.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe))`.
pub fn host_key(name: &str) -> TypeKey {
    TypeKey::keyed_dynamic::<dyn HostDispatch>(name.to_owned())
}

/// What the runtime is doing, as seen through a [`RuntimeHandle`].
#[derive(Clone)]
pub enum RuntimeState {
    /// No generation is running: not applied yet, or disposed.
    Idle,
    /// A generation is starting the process.
    Starting,
    Ready(Arc<Process>),
    /// The last generation failed to start, or its process ended on its own.
    Down(String),
}

/// Observes a runtime from code that is not a plugin (a loader resolver).
#[derive(Clone)]
pub struct RuntimeHandle {
    name: String,
    anchor: PathBuf,
    remote: bool,
    state: watch::Receiver<RuntimeState>,
}

impl RuntimeHandle {
    /// The runtime's name ([`RuntimePlugin::named`]).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `package.json` plugins and Cordis resolve from (for a Python
    /// runtime, the project directory).
    pub fn anchor(&self) -> &Path {
        &self.anchor
    }

    pub fn state(&self) -> RuntimeState {
        self.state.borrow().clone()
    }

    /// Whether the runtime runs elsewhere ([`RuntimePlugin::remote`]): its
    /// plugins are found where it runs, not under [`RuntimeHandle::anchor`].
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// Whether the running runtime reported `feature` (`rows.v2`, `hosts`,
    /// `leaf`, …); `false` while none runs.
    pub fn supports(&self, feature: &str) -> bool {
        match &*self.state.borrow() {
            RuntimeState::Ready(process) => process.supports(feature),
            _ => false,
        }
    }

    /// The running process. Waits while a generation is starting; `None`
    /// when no generation is running.
    pub async fn ready(&self) -> Option<Arc<Process>> {
        let mut state = self.state.clone();
        let settled = state
            .wait_for(|state| !matches!(state, RuntimeState::Starting))
            .await
            .ok()?;
        match &*settled {
            RuntimeState::Ready(process) => Some(process.clone()),
            _ => None,
        }
    }
}

/// Where a runtime's session comes from.
#[derive(Clone)]
enum Source {
    /// A process this plugin starts.
    Spawn,
    /// `RuntimeSession#<name>`, which something else (a link) provides.
    Session([TypeKey; 1]),
}

/// The runtime plugin. Mount it before the plugins that load into it.
///
/// One runtime is one process of one language: the Node runtime runs a
/// Cordis Context ([`RuntimePlugin::node`]); a Python runtime runs leaf
/// plugins ([`RuntimePlugin::python`]). Both speak the same protocol
/// and the same `rows.*` / `hosts.*` contract. An application gets only the
/// runtimes it mounts, and compiles only the languages it enables.
///
/// When the process ends on its own, the plugin withdraws its service
/// and stays Active: dependent plugins stop and wait, as for any provider
/// that goes away. Restarting it (`FiberView::restart`) is the
/// application's decision.
pub struct RuntimePlugin {
    runtime: String,
    label: String,
    /// Where its session comes from.
    source: Source,
    node_package: PathBuf,
    anchor: PathBuf,
    launcher: Option<crate::Launcher>,
    hosts: Arc<HashMap<String, Value>>,
    state: Arc<watch::Sender<RuntimeState>>,
}

impl RuntimePlugin {
    /// The Node runtime, named `"node"`. `node_package`: the rutis-interop
    /// npm runtime (`interop/node`, or a deployed `@arcships/rutis-interop`).
    /// `anchor`: the `package.json` plugins and Cordis resolve from.
    #[cfg(feature = "node")]
    pub fn node(node_package: impl Into<PathBuf>, anchor: impl Into<PathBuf>) -> Self {
        Self {
            runtime: "node".into(),
            label: "node-runtime".into(),
            source: Source::Spawn,
            node_package: node_package.into(),
            anchor: anchor.into(),
            launcher: None,
            hosts: Arc::default(),
            state: Arc::new(watch::channel(RuntimeState::Idle).0),
        }
    }

    /// The Node runtime, as [`RuntimePlugin::node`].
    #[cfg(feature = "node")]
    #[deprecated(since = "0.3.0", note = "use RuntimePlugin::node")]
    pub fn new(node_package: impl Into<PathBuf>, anchor: impl Into<PathBuf>) -> Self {
        Self::node(node_package, anchor)
    }

    /// Declare the methods `{ method: "sync" | "async" }` of the rutis
    /// service at [`host_key`]`(name)`, for when it does not report them
    /// itself ([`HostDispatch::methods`]). The runtime does not wait for it:
    /// the plugins that use it do.
    pub fn host(mut self, name: &str, methods: Value) -> Self {
        Arc::make_mut(&mut self.hosts).insert(name.to_owned(), methods);
        self
    }

    /// A Python runtime named `"py"`: `python3 -m rutis_runtime`, with the
    /// SDK directory `sdk` (`interop/python`) on `PYTHONPATH`, importing
    /// plugin modules from `project`. Python 3.12 or later.
    #[cfg(feature = "python")]
    pub fn python(sdk: impl Into<PathBuf>, project: impl Into<PathBuf>) -> Self {
        let (sdk, project) = (sdk.into(), project.into());
        // Ahead of whatever the application already puts on the path.
        let mut path = std::ffi::OsString::from(&sdk);
        path.push(":");
        path.push(&project);
        if let Some(inherited) = std::env::var_os("PYTHONPATH").filter(|p| !p.is_empty()) {
            path.push(":");
            path.push(inherited);
        }
        let launcher = crate::Launcher::new("python3")
            .arg("-m")
            .arg("rutis_runtime")
            .env("PYTHONPATH", path)
            .env("PYTHONUNBUFFERED", "1")
            // A plugin imported again after an edit must not come from a
            // bytecode file written in the same second as the old source.
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .cwd(&project)
            .inherit_fd();
        Self {
            runtime: "py".into(),
            label: "python-runtime".into(),
            source: Source::Spawn,
            node_package: sdk,
            anchor: project,
            launcher: Some(launcher),
            hosts: Arc::default(),
            state: Arc::new(watch::channel(RuntimeState::Idle).0),
        }
    }

    /// Run the Python runtime with this interpreter instead of `python3`.
    #[cfg(feature = "python")]
    pub fn interpreter(mut self, program: impl Into<std::ffi::OsString>) -> Self {
        if let Some(launcher) = &mut self.launcher {
            launcher.program = program.into();
        }
        self
    }

    /// Start the runtime process with `launcher` (another language, or
    /// another way to start one). The launcher receives its channel (`fd:3`
    /// or a socket path, see [`crate::Launcher`]) and the anchor as its last
    /// two arguments.
    pub fn launcher(
        name: impl Into<String>,
        launcher: crate::Launcher,
        anchor: impl Into<PathBuf>,
    ) -> Self {
        let runtime: String = name.into();
        Self {
            label: format!("{runtime}-runtime"),
            source: Source::Spawn,
            runtime,
            node_package: launcher.cwd.clone().unwrap_or_default(),
            anchor: anchor.into(),
            launcher: Some(launcher),
            hosts: Arc::default(),
            state: Arc::new(watch::channel(RuntimeState::Idle).0),
        }
    }

    /// A runtime this side does not start: its session is
    /// `RuntimeSession#<name>`, provided by whatever reaches it (a link to a
    /// remote runtime, through the bridge's runtime access plugin). It waits
    /// for that session and stops when it goes; a new session is a new
    /// generation of the runtime, whose rows are loaded again. Row names are
    /// resolved where the runtime runs.
    pub fn remote(name: impl Into<String>) -> Self {
        let runtime: String = name.into();
        Self {
            label: format!("{runtime}-runtime"),
            source: Source::Session([crate::runtime_session_key(&runtime)]),
            runtime,
            node_package: PathBuf::new(),
            anchor: PathBuf::new(),
            launcher: None,
            hosts: Arc::default(),
            state: Arc::new(watch::channel(RuntimeState::Idle).0),
        }
    }

    /// Name the runtime: its service is keyed by the name
    /// ([`Runtime::key`]), so runtimes of several languages, or
    /// several of one language, live side by side.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.runtime = name.into();
        self
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            name: self.runtime.clone(),
            anchor: self.anchor.clone(),
            remote: matches!(self.source, Source::Session(_)),
            state: self.state.subscribe(),
        }
    }
}

impl Plugin for RuntimePlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        match &self.source {
            Source::Spawn => &[],
            Source::Session(session) => session,
        }
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            self.state.send_replace(RuntimeState::Starting);
            // Node may hang before it connects; dispose or restart cancels
            // this generation, and dropping the mount kills the half-started
            // process.
            let start = async {
                match &self.source {
                    Source::Spawn => {
                        Process::mount(
                            &self.node_package,
                            Mount {
                                anchor: Some(&self.anchor),
                                launcher: self.launcher.as_ref(),
                                ..Mount::default()
                            },
                        )
                        .await
                    }
                    Source::Session([key]) => {
                        let session = ctx
                            .get_as::<dyn crate::RuntimeSession>(key.clone())
                            .ok_or_else(|| {
                                crate::Error::Value(format!(
                                    "the session of the {} runtime is gone",
                                    self.runtime
                                ))
                            })?;
                        Process::over(session, Mount::default()).await
                    }
                }
            };
            let mounted = tokio::select! {
                mounted = start => Some(mounted),
                _ = ctx.cancelled() => None,
            };
            // Cancelled (dispose, restart) while starting: the dropped start,
            // or the process it produced, is killed, and the generation ends
            // with nothing registered, so the kernel carries on with the
            // unload instead of marking a failure.
            if ctx.cancellation_token().is_cancelled() {
                self.state.send_replace(RuntimeState::Idle);
                return Ok(Effect::Done);
            }
            let process = match mounted {
                Some(Ok(process)) => process,
                Some(Err(error)) => {
                    self.state
                        .send_replace(RuntimeState::Down(error.to_string()));
                    return Err(error.into());
                }
                None => unreachable!("only cancellation ends the start early"),
            };
            // Rows need both, so a runtime without them fails here, once,
            // rather than every row failing on its own later. The process is
            // dropped, which ends it.
            let missing: Vec<&str> = ROW_FEATURES
                .iter()
                .copied()
                .filter(|feature| !process.supports(feature))
                .collect();
            if !missing.is_empty() {
                let error = crate::Error::Value(format!(
                    "the {} runtime lacks {}: @arcships/rutis-interop 0.3.0 or later \
                     (or a runtime speaking its row contract) is required",
                    self.runtime,
                    missing.join(" and ")
                ));
                self.state
                    .send_replace(RuntimeState::Down(error.to_string()));
                return Err(error.into());
            }

            // Registered before the service, so cleanup withdraws the service
            // (dependent plugins unload first) before the process ends.
            let service: Arc<Mutex<Option<Disposer>>> = Arc::default();
            let watcher: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::default();
            let owner = process.clone();
            let state = self.state.clone();
            let stop_watching = watcher.clone();
            let registered = ctx.effect(move || {
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        if let Some(watcher) = stop_watching.lock().unwrap().take() {
                            watcher.abort();
                        }
                        let ended = matches!(*state.borrow(), RuntimeState::Down(_));
                        state.send_replace(RuntimeState::Idle);
                        if ended {
                            // Already gone; its cause was reported by the watcher.
                            return Ok(());
                        }
                        owner.dispose().await.map_err(Into::into)
                    })
                }))
            });
            if let Err(error) = registered {
                // Nothing owns the process: dropping it kills it.
                self.state.send_replace(RuntimeState::Idle);
                return match ctx.cancellation_token().is_cancelled() {
                    true => Ok(Effect::Done),
                    false => Err(error),
                };
            }
            let disposer = ctx.provide_as(
                Runtime::key(&self.runtime),
                Arc::new(Runtime {
                    process: process.clone(),
                    hosts: self.hosts.clone(),
                }),
            )?;
            *service.lock().unwrap() = Some(disposer);
            // The watcher starts last, so every failure above leaves no task
            // holding the process. A process that already ended is seen at
            // once: `closed` resolves for a session that has ended.
            self.state
                .send_replace(RuntimeState::Ready(process.clone()));
            *watcher.lock().unwrap() = Some(tokio::spawn(watch_exit(
                process,
                self.state.clone(),
                service,
            )));
            Ok(Effect::Done)
        })
    }
}

/// Waits for the process to end on its own, then withdraws the service.
async fn watch_exit(
    process: Arc<Process>,
    state: Arc<watch::Sender<RuntimeState>>,
    service: Arc<Mutex<Option<Disposer>>>,
) {
    process.closed().await;
    let status = process.exit_status().map_or_else(
        || "runtime disconnected".to_owned(),
        |status| format!("runtime process {status}"),
    );
    state.send_replace(RuntimeState::Down(status));
    withdraw(&service).await;
}

async fn withdraw(service: &Mutex<Option<Disposer>>) {
    let disposer = service.lock().unwrap().take();
    if let Some(disposer) = disposer {
        if let Err(error) = disposer.dispose().await {
            eprintln!("rutis-interop: cannot withdraw the runtime: {error}");
        }
    }
}

/// The former name of [`RuntimePlugin`].
#[deprecated(since = "0.3.0", note = "renamed to RuntimePlugin")]
pub type CordisRuntimePlugin = RuntimePlugin;

/// The former name of [`Runtime`].
#[deprecated(since = "0.3.0", note = "renamed to Runtime")]
pub type CordisRuntime = Runtime;
