//! Runs dsh in a rutis host, with the model calls of dsh plugins served by
//! aimux-llm in this process.
//!
//! - [`web`]: the dsh web UI. The launcher (`dsh/launcher.ts`) starts a dsh
//!   profile in the mounted Context; `rutis-dsh up` runs it.
//! - [`agent`]: the dsh agent loop without a UI, for driving agents from Rust.
//!
//! Both mounts depend on the `aimux` service: register it with
//! [`provide_web_aimux`] / [`provide_agent_aimux`] before mounting. They are
//! built only when the npm project is installed (Unix).
//!
//! - [`profile`]: dsh's profile configuration on rutis-loader — the patch
//!   layers, the `!!js` expressions, and the user layer's storage. It needs
//!   no Node.

pub mod profile;

#[cfg(all(unix, dsh_installed))]
rutis_interop::include_mounts!();

#[cfg(all(unix, dsh_installed))]
mod aimux;

#[cfg(all(unix, dsh_installed))]
use std::sync::Arc;

#[cfg(all(unix, dsh_installed))]
use aimux_llm::LlmService;
#[cfg(all(unix, dsh_installed))]
use rutis::{CordisError, Ctx, Disposer};

#[cfg(all(unix, dsh_installed))]
pub use aimux::AimuxBridge;

#[cfg(all(unix, dsh_installed))]
aimux::serve_aimux!(web);
#[cfg(all(unix, dsh_installed))]
aimux::serve_aimux!(agent);

/// Registers `service` as the `aimux` service of the [`web`] mount.
#[cfg(all(unix, dsh_installed))]
pub fn provide_web_aimux(ctx: &Ctx, service: Arc<dyn LlmService>) -> Result<Disposer, CordisError> {
    web::provide_aimux(ctx, AimuxBridge::new(service))
}

/// Registers `service` as the `aimux` service of the [`agent`] mount.
#[cfg(all(unix, dsh_installed))]
pub fn provide_agent_aimux(
    ctx: &Ctx,
    service: Arc<dyn LlmService>,
) -> Result<Disposer, CordisError> {
    agent::provide_aimux(ctx, AimuxBridge::new(service))
}
