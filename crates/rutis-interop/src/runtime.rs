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
use tokio::task::JoinHandle;

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
            // Node may hang before it connects; dispose, restart or a
            // withdrawn host cancels this generation, and dropping the mount
            // kills the half-started process.
            let mounted = tokio::select! {
                mounted = Process::mount(
                    &self.node_package,
                    Mount {
                        hosts,
                        anchor: Some(&self.anchor),
                        ..Mount::default()
                    },
                ) => Some(mounted),
                _ = ctx.cancelled() => None,
            };
            // Cancelled (dispose, restart, a withdrawn host) while starting:
            // the dropped start, or the process it produced, is killed, and
            // the generation ends with nothing registered, so the kernel
            // carries on with the unload (back to Pending on a lost host)
            // instead of marking a failure.
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
            let disposer = ctx.provide(CordisRuntime {
                process: process.clone(),
            })?;
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
