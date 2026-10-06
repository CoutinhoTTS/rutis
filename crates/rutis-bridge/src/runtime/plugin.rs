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

use crate::runtime::{Mount, Process};

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

    /// Resolves at the next change of [`RuntimeHandle::state`].
    pub async fn changed(&self) {
        let mut state = self.state.clone();
        state.borrow_and_update();
        let _ = state.changed().await;
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

/// The runtime plugin: the rows of one language runtime, on the session
/// `RuntimeSession#<name>` that something else provides. Mount it before
/// the plugins that load into it; [`LocalRuntime`](crate::runtime::LocalRuntime)
/// mounts it for a runtime on this machine.
///
/// One runtime is one process of one language: the Node runtime runs a
/// Cordis Context, a Python runtime runs leaf plugins. Both speak the same
/// protocol and the same `rows.*` / `hosts.*` contract.
///
/// When the session ends, the plugin withdraws its service: dependent
/// plugins stop and wait, as for any provider that goes away.
#[derive(Clone)]
pub struct RuntimePlugin {
    runtime: String,
    label: String,
    /// `RuntimeSession#<name>`.
    session: [TypeKey; 1],
    /// The runtime runs elsewhere and resolves its plugins there.
    remote: bool,
    anchor: PathBuf,
    hosts: Arc<HashMap<String, Value>>,
    state: Arc<watch::Sender<RuntimeState>>,
}

impl RuntimePlugin {
    fn with(runtime: String, anchor: PathBuf, remote: bool) -> Self {
        Self {
            label: format!("{runtime}-runtime"),
            session: [crate::runtime::runtime_session_key(&runtime)],
            remote,
            runtime,
            anchor,
            hosts: Arc::default(),
            state: Arc::new(watch::channel(RuntimeState::Idle).0),
        }
    }

    /// A runtime that runs elsewhere: its session is `RuntimeSession#<name>`,
    /// provided by a link to it ([`RuntimeAccessPlugin`](crate::runtime::RuntimeAccessPlugin)).
    /// It waits for that session and stops when it goes; a new session is a
    /// new generation of the runtime, whose rows are loaded again. Row names
    /// are resolved where the runtime runs.
    pub fn remote(name: impl Into<String>) -> Self {
        Self::with(name.into(), PathBuf::new(), true)
    }

    /// A runtime on this machine whose session something else provides
    /// (`RuntimeSession#<name>`), as [`LocalRuntime`](crate::runtime::LocalRuntime)
    /// composes it. Its plugins resolve from `anchor`.
    pub fn session(name: impl Into<String>, anchor: impl Into<PathBuf>) -> Self {
        Self::with(name.into(), anchor.into(), false)
    }

    /// Declare the methods `{ method: "sync" | "async" }` of the rutis
    /// service at [`host_key`]`(name)`, for when it does not report them
    /// itself ([`HostDispatch::methods`]). The runtime does not wait for it:
    /// the plugins that use it do.
    pub fn host(mut self, name: &str, methods: Value) -> Self {
        Arc::make_mut(&mut self.hosts).insert(name.to_owned(), methods);
        self
    }

    /// Name the runtime: its service is keyed by the name
    /// ([`Runtime::key`]), so runtimes of several languages, or
    /// several of one language, live side by side.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.runtime = name.into();
        self.label = format!("{}-runtime", self.runtime);
        self.session = [crate::runtime::runtime_session_key(&self.runtime)];
        self
    }

    /// For a composition that starts this runtime's session (a local
    /// runtime): the runtime is starting, before this plugin can run.
    pub fn starting(&self) {
        self.state.send_replace(RuntimeState::Starting);
    }

    /// For a composition that starts this runtime's session: the start was
    /// abandoned (a restart or dispose while starting).
    pub fn stopped(&self) {
        self.state.send_replace(RuntimeState::Idle);
    }

    /// For a composition that starts this runtime's session: it could not,
    /// or it ended, and why.
    pub fn failed(&self, reason: impl Into<String>) {
        self.state.send_replace(RuntimeState::Down(reason.into()));
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            name: self.runtime.clone(),
            anchor: self.anchor.clone(),
            remote: self.remote,
            state: self.state.subscribe(),
        }
    }
}

impl Plugin for RuntimePlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.session
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            self.state.send_replace(RuntimeState::Starting);
            // The runtime may hang before it answers; dispose or restart
            // cancels this generation.
            let start = async {
                let session = ctx
                    .get_as::<dyn crate::runtime::RuntimeSession>(self.session[0].clone())
                    .ok_or_else(|| {
                        crate::runtime::Error::Value(format!(
                            "the session of the {} runtime is gone",
                            self.runtime
                        ))
                    })?;
                Process::over(session, Mount::default()).await
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
                let error = crate::runtime::Error::Value(format!(
                    "the {} runtime lacks {}: @arcships/rutis-runtime 0.3.0 or later \
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
                        if ended {
                            // Already gone, and its cause stays visible: the
                            // plugin may stop only because its session went.
                            return Ok(());
                        }
                        state.send_replace(RuntimeState::Idle);
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
    // A process started here says how it ended; a session through a link
    // ends with its channel's reason (for a local runtime, the same).
    let status = match (process.exit_status(), process.connection().close_reason()) {
        (Some(status), _) => format!("runtime process {status}"),
        (None, Some(reason)) => format!("runtime ended: {reason}"),
        (None, None) => "runtime disconnected".to_owned(),
    };
    state.send_replace(RuntimeState::Down(status));
    withdraw(&service).await;
}

async fn withdraw(service: &Mutex<Option<Disposer>>) {
    let disposer = service.lock().unwrap().take();
    if let Some(disposer) = disposer {
        if let Err(error) = disposer.dispose().await {
            eprintln!("rutis: cannot withdraw the runtime: {error}");
        }
    }
}
