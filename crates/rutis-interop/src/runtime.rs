//! A Cordis runtime as a rutis plugin: one Node process and one empty Cordis
//! Context, whose lifetime is the plugin's.
//!
//! Plugins loaded into it one by one (`Process::load_row`) inject the
//! [`CordisRuntime`] service, so they wait for the runtime natively and stop
//! when it goes away. The host services the Cordis plugins may use are rutis
//! services the runtime injects, so it waits for them too.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, Plugin, TypeKey};
use serde_json::Value;
use tokio::sync::watch;

use crate::{Host, HostDispatch, Mount, Process};

/// The service a running Cordis runtime provides.
pub struct CordisRuntime {
    process: Arc<Process>,
}

impl CordisRuntime {
    pub fn process(&self) -> &Arc<Process> {
        &self.process
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
    /// No generation is running: not applied yet, waiting for host
    /// services, or disposed.
    Idle,
    /// A generation is starting the Node process.
    Starting,
    Ready(Arc<Process>),
    /// The last generation failed to start, or its process ended on its own.
    Down(String),
}

/// Observes a runtime from code that is not a plugin (a loader resolver).
#[derive(Clone)]
pub struct RuntimeHandle {
    anchor: PathBuf,
    state: watch::Receiver<RuntimeState>,
}

impl RuntimeHandle {
    /// The `package.json` plugins and Cordis resolve from.
    pub fn anchor(&self) -> &Path {
        &self.anchor
    }

    pub fn state(&self) -> RuntimeState {
        self.state.borrow().clone()
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

/// The runtime plugin. Mount it before the plugins that load into it.
///
/// When the Node process ends on its own, the plugin withdraws its service
/// and stays Active: dependent plugins stop and wait, as for any provider
/// that goes away. Restarting it (`FiberView::restart`) is the
/// application's decision.
pub struct CordisRuntimePlugin {
    node_package: PathBuf,
    anchor: PathBuf,
    hosts: Vec<(String, Value)>,
    injects: Vec<TypeKey>,
    state: Arc<watch::Sender<RuntimeState>>,
}

impl CordisRuntimePlugin {
    /// `node_package`: the rutis-interop npm runtime (`interop/node`, or a
    /// deployed `@arcships/rutis-interop`). `anchor`: the `package.json`
    /// plugins and Cordis resolve from.
    pub fn new(node_package: impl Into<PathBuf>, anchor: impl Into<PathBuf>) -> Self {
        Self {
            node_package: node_package.into(),
            anchor: anchor.into(),
            hosts: Vec::new(),
            injects: Vec::new(),
            state: Arc::new(watch::channel(RuntimeState::Idle).0),
        }
    }

    /// Offer the rutis service at [`host_key`]`(name)` to the Cordis plugins
    /// as `name`, with methods `{ method: "sync" | "async" }`. The runtime
    /// waits for it, and restarts when it is replaced.
    pub fn host(mut self, name: &str, methods: Value) -> Self {
        self.injects.push(host_key(name));
        self.hosts.push((name.to_owned(), methods));
        self
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            anchor: self.anchor.clone(),
            state: self.state.subscribe(),
        }
    }
}

impl Plugin for CordisRuntimePlugin {
    fn name(&self) -> &str {
        "cordis-runtime"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let hosts = self
                .hosts
                .iter()
                .map(|(name, methods)| {
                    Ok(Host {
                        name: name.clone(),
                        methods: methods.clone(),
                        dispatch: ctx.require_as::<dyn HostDispatch>(host_key(name))?,
                    })
                })
                .collect::<Result<Vec<_>, CordisError>>()?;
            self.state.send_replace(RuntimeState::Starting);
            let mounted = Process::mount(
                &self.node_package,
                Mount {
                    hosts,
                    anchor: Some(&self.anchor),
                    ..Mount::default()
                },
            )
            .await;
            let process = match mounted {
                Ok(process) => process,
                Err(error) => {
                    self.state
                        .send_replace(RuntimeState::Down(error.to_string()));
                    return Err(error.into());
                }
            };

            // Registered before the service, so cleanup withdraws the service
            // (dependent plugins unload first) before the process ends.
            let service: Arc<Mutex<Option<Disposer>>> = Arc::default();
            let watcher = tokio::spawn(watch_exit(
                process.clone(),
                self.state.clone(),
                service.clone(),
            ));
            let owner = process.clone();
            let state = self.state.clone();
            ctx.effect(move || {
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        watcher.abort();
                        let ended = matches!(*state.borrow(), RuntimeState::Down(_));
                        state.send_replace(RuntimeState::Idle);
                        if ended {
                            // Already gone; its cause was reported by the watcher.
                            return Ok(());
                        }
                        owner.dispose().await.map_err(Into::into)
                    })
                }))
            })?;
            let disposer = ctx.provide(CordisRuntime {
                process: process.clone(),
            })?;
            *service.lock().unwrap() = Some(disposer);
            // Ready unless the watcher saw the process end meanwhile; then
            // the service it could not withdraw yet goes now.
            let ready = self.state.send_if_modified(|state| {
                if matches!(state, RuntimeState::Starting) {
                    *state = RuntimeState::Ready(process.clone());
                    true
                } else {
                    false
                }
            });
            if !ready {
                withdraw(&service).await;
            }
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
        || "Cordis runtime disconnected".to_owned(),
        |status| format!("Cordis process {status}"),
    );
    state.send_replace(RuntimeState::Down(status));
    withdraw(&service).await;
}

async fn withdraw(service: &Mutex<Option<Disposer>>) {
    let disposer = service.lock().unwrap().take();
    if let Some(disposer) = disposer {
        if let Err(error) = disposer.dispose().await {
            eprintln!("rutis-interop: cannot withdraw the Cordis runtime: {error}");
        }
    }
}
