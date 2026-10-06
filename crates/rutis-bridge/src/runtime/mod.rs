//! Language runtimes: processes that run plugins written in another
//! language (Node with Cordis, feature `node`; Python, feature `python`),
//! whose plugins rutis-loader manages as rows.
//!
//! A runtime's session comes from a link, wherever the runtime runs:
//! [`LocalRuntime`] starts one on this machine (the local transport starts
//! the process, a link carries its session); [`RuntimePlugin::remote`] uses
//! one elsewhere, through the session [`RuntimeAccessPlugin`] provides from
//! a link to it. Either way the rows are the same.

// Inside the crate, runtime code names session items through this module.
#[allow(unused_imports)]
pub(crate) use crate::session::*;

mod access;
#[cfg(unix)]
mod events;
#[cfg(unix)]
mod local;
#[cfg(unix)]
mod plugin;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod projection;
#[cfg(unix)]
mod rows;
#[cfg(unix)]
pub(crate) mod spawn;
#[cfg(all(unix, feature = "testing"))]
pub mod testing;
#[cfg(unix)]
pub(crate) mod unix;

pub use access::RuntimeAccessPlugin;
#[cfg(unix)]
pub use events::{EmitToCordis, EventSink, Events};
#[cfg(unix)]
pub use local::LocalRuntime;
#[cfg(unix)]
pub use plugin::{Runtime, RuntimeHandle, RuntimePlugin, RuntimeState};
#[cfg(unix)]
pub use process::{Host, HostLease, Launcher, Mount, Process, RowSchema, ServiceEvents};
#[cfg(unix)]
pub use projection::Projection;
#[cfg(unix)]
pub use rows::{row_projection, RowService};

use std::sync::Arc;

use crate::session::{Connection, Dispatch, Error};

/// A session with a runtime that something else owns, such as a link to a
/// remote runtime. A [`RuntimePlugin::remote`] runs its rows on it.
pub trait RuntimeSession: Send + Sync + 'static {
    /// The session, ready.
    fn connection(&self) -> Connection;

    /// Route the runtime's calls into rutis (`host:<name>`, `service`,
    /// `event`) to `dispatch` while the returned guard lives.
    fn route(
        &self,
        dispatch: Arc<dyn Dispatch>,
    ) -> Result<Box<dyn std::any::Any + Send + Sync>, Error>;
}

/// The key the runtime session named `name` is provided under
/// (`RuntimeSession#gpu`).
pub fn runtime_session_key(name: &str) -> rutis::TypeKey {
    rutis::TypeKey::keyed_dynamic::<dyn RuntimeSession>(name.to_owned())
}
