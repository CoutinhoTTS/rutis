//! The local transport: channels to processes on the same machine.
//!
//! [`LocalPlugin`] provides `Transport#local`. It dials Unix sockets
//! (`unix:<path>`, or a bare path), framing messages by newline. Starting
//! runtime processes on an inherited fd joins it in a later stage. Unloading
//! the plugin closes every channel it opened.

use std::sync::{Arc, Mutex, Weak};

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};
use rutis_bridge::{transport_key, Transport};
use rutis_channel::{Channel, ConnectError};

#[cfg_attr(not(unix), allow(dead_code))]
mod lines;
#[cfg(unix)]
mod unix;

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
}

impl LocalTransport {
    fn track(&self, channel: &Channel) {
        let mut open = self.open.lock().unwrap();
        open.retain(|closer| closer.strong_count() > 0);
        open.push(Arc::downgrade(&channel.closer));
    }

    fn close_all(&self) {
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

    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, ConnectError>> {
        Box::pin(async move {
            let channel = dial(address).await?;
            self.track(&channel);
            Ok(channel)
        })
    }
}

#[cfg(unix)]
async fn dial(address: &str) -> Result<Channel, ConnectError> {
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
async fn dial(_address: &str) -> Result<Channel, ConnectError> {
    Err(ConnectError::Incompatible {
        reason: "the local transport supports Unix sockets only on this platform".into(),
    })
}
