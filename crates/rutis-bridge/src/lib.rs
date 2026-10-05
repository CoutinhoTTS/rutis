//! Connecting rutis to other processes and machines
//! (`docs/design-remote-plugins-2026-10-03.md`).
//!
//! Transport plugins (`rutis-transport-local`, `rutis-transport-memory`, …)
//! provide a [`Transport`] under [`transport_key`]; links depend on it and
//! never on a concrete transport crate. Identity, links and node bridge
//! plugins join this crate in later stages.

use rutis::{BoxFuture, TypeKey};
pub use rutis_channel::{Channel, ConnectError};

/// One kind of carrier, as its plugin provides it.
pub trait Transport: Send + Sync + 'static {
    /// `"local"`, `"memory"`, `"websocket"`, …: the key it is provided under.
    fn kind(&self) -> &str;

    /// Establish one channel to `address`, in the transport's own syntax.
    /// Each call reports one result and never retries: whoever dials owns
    /// the retry policy, decided by the [`ConnectError`] category.
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, ConnectError>>;
}

/// The key a transport of `kind` is provided under (`Transport#local`).
pub fn transport_key(kind: &str) -> TypeKey {
    TypeKey::keyed_dynamic::<dyn Transport>(kind.to_owned())
}
