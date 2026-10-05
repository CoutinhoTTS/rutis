//! Connecting rutis to other processes and machines
//! (`docs/design-remote-plugins-2026-10-03.md`).
//!
//! Transport plugins (`rutis-transport-local`, `rutis-transport-memory`,
//! `rutis-transport-websocket`, …) provide a [`Transport`] under
//! [`transport_key`]; links depend on it and never on a concrete transport
//! crate. An [`Identity`] holds credentials and the rules that map what a
//! far end presents to its endpoint id; transports apply them.

use std::sync::Arc;

use rutis::{BoxFuture, TypeKey};
pub use rutis_channel::{Channel, ConnectError, PeerId};

mod identity;
mod registration;

pub use identity::{fingerprint, identity_key, Credential, Identity, Presented, StaticIdentity};
pub use registration::{
    Deliver, Refusal, Registered, Registration, RegistrationError, Registrations, Ticket,
};

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
