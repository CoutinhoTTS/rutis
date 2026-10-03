//! Channels for rutis protocols: ordered, reliable, message-bounded duplex
//! links that carry messages without knowing what they mean.
//!
//! A [`Channel`] comes in three parts, so a session can send under its own
//! lock, receive on its own reader thread, and close from anywhere. All of
//! them block: a session may wait for a reply on any thread, including the
//! only thread of a current-thread runtime, so a channel must make progress
//! without the caller's executor. Implementations built on an async stack
//! run it on a thread of their own.
//!
//! Design: `docs/design-protocol-channel-decoupling-2026-10-03.md`.

mod memory;
#[cfg(unix)]
mod unix;

pub use memory::pair;

use std::sync::Arc;

/// One duplex channel.
pub struct Channel {
    /// Used by one sender at a time (a session sends under its own lock).
    pub sender: Box<dyn Sender>,
    /// Owned by the reader.
    pub receiver: Box<dyn Receiver>,
    /// Callable from any thread at any time.
    pub closer: Arc<dyn Closer>,
    pub info: ChannelInfo,
}

pub trait Sender: Send {
    /// Send one message, blocking while the channel applies backpressure.
    fn send(&mut self, message: Vec<u8>) -> Result<(), ChannelError>;
}

pub trait Receiver: Send {
    /// Block until the next message. `Ok(None)`: the peer ended the channel.
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError>;
}

pub trait Closer: Send + Sync {
    /// End the channel. Idempotent; wakes threads blocked in `send` or
    /// `recv`. Channels that can carry it give `reason` to the peer.
    fn close(&self, reason: &str);
}

/// What a channel is, for diagnostics and authorization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChannelInfo {
    /// The kind of transport, such as `"unix"` or `"memory"`.
    pub transport: &'static str,
    /// The peer identity a connector verified; `None` for local channels.
    pub peer: Option<String>,
    /// A name for error messages; empty for none.
    pub label: String,
}

impl ChannelInfo {
    pub fn new(transport: &'static str) -> Self {
        Self {
            transport,
            peer: None,
            label: String::new(),
        }
    }

    pub fn with_peer(mut self, peer: impl Into<String>) -> Self {
        self.peer = Some(peer.into());
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelError {
    /// The channel has ended: closed by either side, or the peer went away.
    #[error("{reason}")]
    Closed { reason: String },
    /// A message is larger than the channel accepts.
    #[error("message of {size} bytes exceeds the limit of {limit} bytes")]
    TooLarge { limit: usize, size: usize },
}

impl ChannelError {
    pub fn closed(reason: impl Into<String>) -> Self {
        Self::Closed {
            reason: reason.into(),
        }
    }
}

impl Channel {
    /// Report the end of the channel as `reason()`, called once when the
    /// receiver sees the end: the exit status of a child process, say. An
    /// end caused by a local close reports it too.
    pub fn with_end_reason(mut self, reason: impl FnOnce() -> String + Send + 'static) -> Self {
        self.receiver = Box::new(EndReason {
            inner: self.receiver,
            reason: Some(Box::new(reason)),
            ended: None,
        });
        self
    }
}

type Reason = Box<dyn FnOnce() -> String + Send>;

struct EndReason {
    inner: Box<dyn Receiver>,
    reason: Option<Reason>,
    ended: Option<String>,
}

impl Receiver for EndReason {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        if let Some(reason) = &self.ended {
            return Err(ChannelError::closed(reason.clone()));
        }
        match self.inner.recv() {
            Ok(Some(message)) => Ok(Some(message)),
            Ok(None) | Err(ChannelError::Closed { .. }) => {
                let reason = self
                    .reason
                    .take()
                    .map_or_else(|| "channel closed".to_owned(), |reason| reason());
                self.ended = Some(reason.clone());
                Err(ChannelError::closed(reason))
            }
            Err(error) => Err(error),
        }
    }
}
