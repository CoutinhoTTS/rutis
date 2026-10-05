//! The local transport: channels to processes on the same machine.
//!
//! [`LocalPlugin`] provides `Transport#local`. It dials Unix sockets
//! (`unix:<path>`, or a bare path), framing messages by newline, and starts
//! processes: `spawn:<name>` starts the process registered as `name`
//! ([`LocalTransport::spawner`], a [`Spawn`]) on an inherited socket and
//! connects it; the channel owns the process. Unloading the plugin closes
//! every channel it opened, and so ends the processes it started.
//!
//! It knows nothing of what runs in the process: a language runtime started
//! this way is composed on top (`rutis-runtime-local`).

#[cfg(unix)]
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};
use rutis_bridge::{transport_key, Dial, Transport};
use rutis_channel::{Channel, ConnectError};

mod lines;

/// Frame a connected byte stream (two handles of one socket, and a closer
/// that wakes both) as a channel, one message per line: what this
/// transport's Unix channels are.
pub fn framed(
    read: impl std::io::Read + Send + 'static,
    write: impl std::io::Write + Send + 'static,
    closer: Arc<dyn rutis_channel::Closer>,
) -> Channel {
    lines::channel(
        read,
        write,
        closer,
        rutis_channel::ChannelInfo {
            transport: "unix",
            peer: None,
            label: String::new(),
        },
    )
}
#[cfg(unix)]
mod spawn;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use spawn::{Handover, Spawn, CHANNEL_FD};

/// Provides `Transport#local`.
#[derive(Default)]
pub struct LocalPlugin;

impl LocalPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for LocalPlugin {
    fn name(&self) -> &str {
        "rutis-bridge/local"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let transport = Arc::new(LocalTransport::default());
            let open = transport.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    open.close_all();
                    Ok(())
                }))
            })?;
            ctx.provide_as::<dyn Transport>(transport_key("local"), transport)?;
            Ok(Effect::Done)
        })
    }
}

/// The transport a [`LocalPlugin`] provides.
#[derive(Default)]
pub struct LocalTransport {
    open: Mutex<Vec<Weak<dyn rutis_channel::Closer>>>,
    #[cfg(unix)]
    spawners: Mutex<HashMap<String, Spawn>>,
    trace: Mutex<Option<rutis_channel::trace::Sink>>,
}

impl LocalTransport {
    /// Let `spawn:<name>` start `spawn`.
    #[cfg(unix)]
    pub fn spawner(&self, name: &str, spawn: Spawn) {
        self.spawners.lock().unwrap().insert(name.to_owned(), spawn);
    }

    /// Report every message crossing the channels it opens from now on
    /// (direction and length, never content) to `sink`.
    pub fn trace(&self, sink: rutis_channel::trace::Sink) {
        *self.trace.lock().unwrap() = Some(sink);
    }

    fn track(&self, channel: &Channel) {
        let mut open = self.open.lock().unwrap();
        open.retain(|closer| closer.strong_count() > 0);
        open.push(Arc::downgrade(&channel.closer));
    }

    /// Close every channel it opened, ending the processes it started.
    pub fn close_all(&self) {
        for closer in std::mem::take(&mut *self.open.lock().unwrap()) {
            if let Some(closer) = closer.upgrade() {
                closer.close("transport unloaded");
            }
        }
    }
}

impl Transport for LocalTransport {
    fn kind(&self) -> &str {
        "local"
    }

    fn dial<'a>(&'a self, dial: &'a Dial) -> BoxFuture<'a, Result<Channel, ConnectError>> {
        Box::pin(async move {
            let address = dial.address.as_str();
            let channel = match address.strip_prefix("spawn:") {
                Some(name) => self.spawn(name).await?,
                None => connect(address).await?,
            };
            let channel = match self.trace.lock().unwrap().clone() {
                Some(sink) => rutis_channel::trace::trace(channel, sink),
                None => channel,
            };
            self.track(&channel);
            Ok(channel)
        })
    }
}

impl LocalTransport {
    #[cfg(unix)]
    async fn spawn(&self, name: &str) -> Result<Channel, ConnectError> {
        let spawn = self
            .spawners
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| ConnectError::Incompatible {
                reason: format!("nothing to spawn as {name}"),
            })?;
        spawn::start(&spawn).await
    }

    #[cfg(not(unix))]
    async fn spawn(&self, _name: &str) -> Result<Channel, ConnectError> {
        Err(ConnectError::Incompatible {
            reason: "processes start on Unix only".into(),
        })
    }
}

#[cfg(unix)]
async fn connect(address: &str) -> Result<Channel, ConnectError> {
    let path = match address.split_once(':') {
        Some(("unix", path)) => path,
        Some((scheme, _)) if !scheme.contains('/') => {
            return Err(ConnectError::Incompatible {
                reason: format!("the local transport cannot dial {scheme}: addresses"),
            })
        }
        _ => address,
    };
    unix::dial(path).await
}

#[cfg(not(unix))]
async fn connect(_address: &str) -> Result<Channel, ConnectError> {
    Err(ConnectError::Incompatible {
        reason: "the local transport supports Unix sockets only on this platform".into(),
    })
}
