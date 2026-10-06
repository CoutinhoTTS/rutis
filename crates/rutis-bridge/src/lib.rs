//! Connect rutis to other processes, languages and machines.
//!
//! - [`runtime`]: plugins written in Node or Python, run by language
//!   runtimes on this machine ([`runtime::LocalRuntime`]) or elsewhere
//!   ([`runtime::RuntimePlugin::remote`]); rutis-loader manages them as
//!   rows.
//! - Links between rutis nodes ([`LinkPlugin`]) and what runs on them:
//!   services exported and imported, plugins hosted for a peer, events
//!   forwarded ([`PeerPlugin`] composes them).
//! - [`transport`]: what links run over (local processes and Unix sockets,
//!   in-process channels, WebSocket with feature `websocket`). A transport
//!   provides a [`Transport`] under [`transport_key`]; links depend on that
//!   and never on a concrete transport. An [`Identity`] holds credentials
//!   and the rules that map what a far end presents to its endpoint id.
//! - [`session`]: the protocol all of these speak, over any
//!   [`channel::Channel`].
//! - [`cordis`] (feature `cordis`): Cordis plugins mounted in a Rust
//!   application with Rust bindings generated at build time.
//!
//! Design: `docs/design-protocol-channel-decoupling-2026-10-03.md`,
//! `docs/design-remote-plugins-2026-10-03.md`.

use std::sync::Arc;

use rutis::{BoxFuture, TypeKey};

pub mod channel;
#[cfg(feature = "cordis")]
pub mod cordis;
pub mod runtime;
pub mod session;
#[cfg(feature = "testing")]
pub mod testing;
pub mod transport;

mod compose;
mod events;
mod host;
mod identity;
mod link;
mod peer;
mod registration;
mod services;

pub use channel::{Channel, ConnectError, PeerId};
pub use compose::{Features, PeerHandle, PeerPlugin};
pub use events::{node_event, EventsPlugin, NodeEvent};
pub use host::{Described, HostPlugin, Installed, PluginCatalog, ServiceKeys, StaticCatalog};
pub use identity::{
    fingerprint, identity_key, Credential, Identity, IdentityPlugin, Presented, StaticIdentity,
};
pub use link::{protocol, Connect, Failure, LinkConfig, LinkPlugin, LinkState, Retry};
pub use peer::{family, peer_key, Handler, Offered, Offers, Peer};
pub use registration::{
    Deliver, Refusal, Registered, Registration, RegistrationError, Registrations, Ticket,
};
pub use services::{ExportPlugin, ImportPlugin};

/// One kind of carrier, as its plugin provides it.
pub trait Transport: Send + Sync + 'static {
    /// `"local"`, `"memory"`, `"websocket"`, …: the key it is provided under.
    fn kind(&self) -> &str;

    /// Establish one channel. Each call reports one result and never
    /// retries: whoever dials owns the retry policy, decided by the
    /// [`ConnectError`] category.
    fn dial<'a>(&'a self, dial: &'a Dial) -> BoxFuture<'a, Result<Channel, ConnectError>>;

    /// Accept the channels of one far end on a listener this transport
    /// holds. The registration lasts until the returned handle is dropped.
    /// Transports without listeners refuse.
    fn register(&self, registration: Registration) -> Result<Registered, RegistrationError> {
        let _ = registration;
        Err(RegistrationError::NoListener(format!(
            "the {} transport does not listen",
            self.kind()
        )))
    }
}

/// What to dial.
#[derive(Clone, Default)]
pub struct Dial {
    /// In the transport's own syntax (`unix:/path`, `wss://host/rutis`, …).
    pub address: String,
    /// The endpoint expected at the far end; bound to the channel once the
    /// transport has verified it (for TLS, the server certificate).
    pub peer: Option<PeerId>,
    /// Credentials to present.
    pub identity: Option<Arc<dyn Identity>>,
    /// The session protocol to speak (`rutis.2`), for transports that
    /// negotiate one (a WebSocket subprotocol); a mismatch is
    /// [`ConnectError::Incompatible`].
    pub protocol: String,
}

impl Dial {
    pub fn address(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            ..Self::default()
        }
    }

    pub fn peer(mut self, peer: PeerId) -> Self {
        self.peer = Some(peer);
        self
    }

    pub fn identity(mut self, identity: Arc<dyn Identity>) -> Self {
        self.identity = Some(identity);
        self
    }

    pub fn protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocol = protocol.into();
        self
    }
}

/// The key a transport of `kind` is provided under (`Transport#local`).
pub fn transport_key(kind: &str) -> TypeKey {
    TypeKey::keyed_dynamic::<dyn Transport>(kind.to_owned())
}
