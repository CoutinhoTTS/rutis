//! The channel a rutis interop session runs on
//! (`docs/design-protocol-channel-decoupling-2026-10-03.md`).
//!
//! A [`Channel`] is one ordered, reliable, duplex stream of messages: every
//! message sent arrives once, in order, with its boundaries kept. It moves
//! opaque bytes and knows nothing of protocol frames or their encoding.
//! Implementations, with their framing, limits and liveness, live in the
//! transport crates (`rutis-transport-*`); this crate holds only the
//! contract and the connection errors.

use std::sync::Arc;

/// One established channel. The sender and receiver are used from one
/// thread each; the closer may be called from anywhere.
pub struct Channel {
    pub sender: Box<dyn Sender>,
    pub receiver: Box<dyn Receiver>,
    pub closer: Arc<dyn Closer>,
    pub info: ChannelInfo,
}

pub trait Sender: Send {
    /// Send one message; blocks under backpressure.
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError>;
}

pub trait Receiver: Send {
    /// Block until the next message; `Ok(None)` when the far end finished
    /// normally.
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError>;
}

pub trait Closer: Send + Sync {
    /// Idempotent; wakes threads blocked in `send` and `recv`.
    fn close(&self, reason: &str);
}

/// What the connector established about a channel. Identity is delivered
/// here, never inside the message stream.
#[derive(Debug, Clone, Default)]
pub struct ChannelInfo {
    /// `"unix"`, `"fd"`, `"memory"`, `"websocket"`, …
    pub transport: &'static str,
    /// The far end's endpoint, as confirmed by the connector; for a local
    /// child, as named by whoever started it.
    pub peer: Option<PeerId>,
    /// For diagnostics, for example `"peer mac"`; may be empty.
    pub label: String,
}

/// An endpoint id: lowercase letters, digits and `-`, unique in a
/// deployment. The far end need not be a full framework node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(String);

impl PeerId {
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidPeerId> {
        let id = id.into();
        let valid = !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidPeerId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid endpoint id {0:?}: use lowercase letters, digits and `-`")]
pub struct InvalidPeerId(pub String);

/// How an established channel ended. The reason is for diagnostics only:
/// no code branches on it. Why a transport ends a channel (a lost peer, a
/// message over its size limit, a failed heartbeat) is the transport's own
/// business.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("{reason}")]
    Closed { reason: String },
}

/// Why a channel could not be established. The category decides what a
/// link does next; `reason` is for diagnostics and carries no credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    /// Temporary: refused, timed out, unavailable. Retry with backoff.
    #[error("{reason}")]
    Retryable { reason: String },
    /// Credentials, certificate, identity mapping or registration refused.
    /// Retry slowly and keep reporting.
    #[error("{reason}")]
    AuthRejected { reason: String },
    /// Subprotocol or a required transport capability does not match. Stop
    /// retrying until the configuration changes.
    #[error("{reason}")]
    Incompatible { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_ids_are_lowercase_letters_digits_and_dashes() {
        assert!(PeerId::new("mac-2").is_ok());
        assert!(PeerId::new("").is_err());
        assert!(PeerId::new("Mac").is_err());
        assert!(PeerId::new("a/b").is_err());
        assert!(PeerId::new("a:1").is_err());
    }
}
