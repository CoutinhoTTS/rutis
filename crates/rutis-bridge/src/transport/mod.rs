//! What links run over. Each transport is a plugin that provides a
//! [`Transport`](crate::Transport) under [`transport_key`](crate::transport_key):
//!
//! - [`local`]: Unix sockets, and processes it starts on a channel it owns;
//! - [`memory`]: channels within one process, for tests and in-process links;
//! - [`websocket`] (feature `websocket`): links between machines.

pub mod local;
pub mod memory;
#[cfg(feature = "websocket")]
pub mod websocket;
