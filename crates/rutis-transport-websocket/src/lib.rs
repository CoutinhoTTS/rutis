//! The WebSocket transport of rutis bridges
//! (`docs/design-protocol-channel-decoupling-2026-10-03.md`, WebSocket
//! binding).
//!
//! [`WebSocketPlugin`] provides `Transport#websocket`. It dials
//! `ws://` (loopback only) and `wss://` addresses once per call, and holds
//! the listeners links register on. Each connection carries one channel of
//! UTF-8 JSON text messages; the session protocol is its subprotocol.
//! Bearer tokens travel in the `Authorization` header, client certificates
//! in TLS; an [`Identity`](rutis_bridge::Identity) maps either to the far
//! end's endpoint id. Both sides ping every 10 s and drop a far end silent
//! for 30 s. The transport runs on its own threads, so channels progress
//! whatever the caller's executor is doing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};

use rustls::RootCertStore;
use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};
use rutis_bridge::{
    transport_key, Dial, Registered, Registration, RegistrationError, Registrations, Transport,
};
use rutis_channel::{Channel, Closer, ConnectError};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

mod config;
mod connection;
mod dial;
mod listen;
mod pipe;
mod tls;

pub use config::{Config, Limits, ListenerConfig, ServerTls, Trust};

/// Provides `Transport#websocket`, configured with listeners, trust and
/// limits. Unloading closes its listeners, registrations and channels.
/// Clones share the running transport ([`WebSocketPlugin::transport`]).
#[derive(Clone)]
pub struct WebSocketPlugin {
    config: Config,
    started: Arc<Mutex<Option<Arc<WebSocketTransport>>>>,
}

impl WebSocketPlugin {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            started: Arc::default(),
        })
    }

    /// The running transport, while the plugin is applied.
    pub fn transport(&self) -> Option<Arc<WebSocketTransport>> {
        self.started.lock().unwrap().clone()
    }
}

impl Plugin for WebSocketPlugin {
    fn name(&self) -> &str {
        "rutis-bridge/websocket"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let transport = WebSocketTransport::start(self.config.clone())
                .map_err(|error| CordisError::PluginFailed(error.into()))?;
            *self.started.lock().unwrap() = Some(transport.clone());
            let started = self.started.clone();
            let stopping = transport.clone();
            ctx.effect(move || {
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        started.lock().unwrap().take();
                        stopping.stop();
                        // Let the close frames go out before the threads stop.
                        stopping.closed(std::time::Duration::from_secs(1)).await;
                        Ok(())
                    })
                }))
            })?;
            ctx.provide_as::<dyn Transport>(transport_key("websocket"), transport)?;
            Ok(Effect::Done)
        })
    }
}

/// What listeners and connections share.
pub(crate) struct Shared {
    pub registrations: Registrations,
    pub limits: Limits,
    open: Mutex<Vec<Weak<dyn Closer>>>,
    /// Connection tasks still running.
    pub live: std::sync::atomic::AtomicUsize,
    pub ended: tokio::sync::Notify,
}

impl Shared {
    pub(crate) fn track(&self, channel: &Channel) {
        let mut open = self.open.lock().unwrap();
        open.retain(|closer| closer.strong_count() > 0);
        open.push(Arc::downgrade(&channel.closer));
    }
}

pub(crate) fn ws_config(limits: &Limits) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(limits.max_message))
        .max_frame_size(Some(limits.max_message))
}

/// The transport a [`WebSocketPlugin`] provides.
pub struct WebSocketTransport {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    handle: tokio::runtime::Handle,
    shared: Arc<Shared>,
    roots: Arc<RootCertStore>,
    listeners: Mutex<HashMap<String, listen::Listening>>,
}

impl WebSocketTransport {
    /// Start the transport's threads and bind its listeners.
    pub fn start(config: Config) -> Result<Arc<Self>, String> {
        config.validate()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rutis-websocket")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let handle = runtime.handle().clone();
        let shared = Arc::new(Shared {
            registrations: Registrations::new(),
            limits: config.limits.clone(),
            open: Mutex::default(),
            live: Default::default(),
            ended: Default::default(),
        });
        let roots = tls::roots(&config.trust)?;
        let mut listeners = HashMap::new();
        for listener in &config.listeners {
            let listening = listen::start(listener, shared.clone(), &handle);
            match listening {
                Ok(listening) => {
                    listeners.insert(listener.name.clone(), listening);
                }
                Err(error) => {
                    drop(listeners);
                    runtime.shutdown_background();
                    return Err(error);
                }
            }
        }
        Ok(Arc::new(Self {
            runtime: Mutex::new(Some(runtime)),
            handle,
            shared,
            roots,
            listeners: Mutex::new(listeners),
        }))
    }

    /// The address listener `name` is bound to (to learn a picked port).
    pub fn local_addr(&self, name: &str) -> Option<SocketAddr> {
        self.listeners
            .lock()
            .unwrap()
            .get(name)
            .map(|listening| listening.addr)
    }

    /// Wait until every connection task has finished, at most `limit`.
    pub async fn closed(&self, limit: std::time::Duration) {
        let all_ended = async {
            loop {
                let ended = self.shared.ended.notified();
                if self.shared.live.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    return;
                }
                ended.await;
            }
        };
        let _ = tokio::time::timeout(limit, all_ended).await;
    }

    /// Stop listening, revoke every registration and close every channel.
    pub fn stop(&self) {
        self.listeners.lock().unwrap().clear();
        self.shared.registrations.clear();
        for closer in std::mem::take(&mut *self.shared.open.lock().unwrap()) {
            if let Some(closer) = closer.upgrade() {
                closer.close("transport unloaded");
            }
        }
    }
}

impl Drop for WebSocketTransport {
    fn drop(&mut self) {
        self.stop();
        if let Some(runtime) = self.runtime.lock().unwrap().take() {
            runtime.shutdown_background();
        }
    }
}

/// Aborts the task whose result is awaited if the awaiting stops.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Transport for WebSocketTransport {
    fn kind(&self) -> &str {
        "websocket"
    }

    fn dial<'a>(&'a self, request: &'a Dial) -> BoxFuture<'a, Result<Channel, ConnectError>> {
        let request = request.clone();
        let roots = self.roots.clone();
        let limits = self.shared.limits.clone();
        let handle = self.handle.clone();
        let live = connection::Live::new(self.shared.clone());
        let task = self
            .handle
            .spawn(async move { dial::dial(&request, roots, &limits, &handle, live).await });
        let task = AbortOnDrop(task);
        Box::pin(async move {
            let mut task = task;
            let channel = (&mut task.0)
                .await
                .map_err(|error| ConnectError::Retryable {
                    reason: format!("dial stopped: {error}"),
                })??;
            self.shared.track(&channel);
            Ok(channel)
        })
    }

    fn register(&self, registration: Registration) -> Result<Registered, RegistrationError> {
        let local = self
            .listeners
            .lock()
            .unwrap()
            .get(&registration.listener)
            .map(|listening| listening.local.clone())
            .ok_or_else(|| {
                RegistrationError::NoListener(format!(
                    "no listener named {}",
                    registration.listener
                ))
            })?;
        self.shared.registrations.register(&local, registration)
    }
}
