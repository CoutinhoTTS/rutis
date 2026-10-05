//! The local transport: channels to processes on the same machine.
//!
//! [`LocalPlugin`] provides `Transport#local`. It dials Unix sockets
//! (`unix:<path>`, or a bare path), framing messages by newline, and starts
//! runtime processes: `spawn:<name>` starts the process registered as
//! `name` ([`LocalTransport::spawner`]) on an inherited socket and connects
//! it; the channel owns the process. Unloading the plugin closes every
//! channel it opened, and so ends the processes it started.
//!
//! [`LocalRuntime`] is a local language runtime on top: the process, a link
//! to it, and the runtime plugin running its rows.

use std::collections::HashMap;
use std::path::PathBuf;
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
mod runtime;
#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use runtime::LocalRuntime;

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
    spawners: Mutex<HashMap<String, Spawner>>,
}

/// A runtime process `spawn:<name>` starts.
#[derive(Clone, Debug)]
pub struct Spawner {
    /// How to start it; `None` runs the Node runtime of `node_package`.
    pub launcher: Option<rutis_interop::Launcher>,
    pub node_package: PathBuf,
    /// Its first plugin, or its anchor: its last argument.
    pub first: PathBuf,
    /// The endpoint the process is: whoever starts a process names it.
    pub peer: rutis_channel::PeerId,
}

impl LocalTransport {
    /// Let `spawn:<name>` start `spawner`.
    pub fn spawner(&self, name: &str, spawner: Spawner) {
        self.spawners
            .lock()
            .unwrap()
            .insert(name.to_owned(), spawner);
    }

    fn track(&self, channel: &Channel) {
        let mut open = self.open.lock().unwrap();
        open.retain(|closer| closer.strong_count() > 0);
        open.push(Arc::downgrade(&channel.closer));
    }

    pub(crate) fn close_all(&self) {
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
            self.track(&channel);
            Ok(channel)
        })
    }
}

impl LocalTransport {
    #[cfg(unix)]
    async fn spawn(&self, name: &str) -> Result<Channel, ConnectError> {
        let spawner = self
            .spawners
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| ConnectError::Incompatible {
                reason: format!("nothing to spawn as {name}"),
            })?;
        let mut channel = rutis_interop::spawn::start(
            spawner.launcher.as_ref(),
            &spawner.node_package,
            &spawner.first,
        )
        .await
        .map_err(|error| match error {
            // The process could not be started at all: its configuration.
            rutis_interop::Error::Value(reason) => ConnectError::Incompatible { reason },
            error => ConnectError::Retryable {
                reason: error.to_string(),
            },
        })?;
        channel.info.peer = Some(spawner.peer);
        Ok(channel)
    }

    #[cfg(not(unix))]
    async fn spawn(&self, _name: &str) -> Result<Channel, ConnectError> {
        Err(ConnectError::Incompatible {
            reason: "runtime processes start on Unix only".into(),
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
