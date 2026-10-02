//! Runs dsh in a rutis host, with the model calls of dsh plugins served by
//! aimux-llm in this process.
//!
//! - [`web`]: the dsh web UI. The launcher (`dsh/launcher.ts`) starts a dsh
//!   profile in the mounted Context; `rutis-dsh up` runs it.
//! - [`agent`]: the dsh agent loop without a UI, for driving agents from Rust.
//!
//! Both mounts depend on the `aimux` service: register it with
//! [`provide_web_aimux`] / [`provide_agent_aimux`] before mounting.
#![cfg(all(unix, dsh_installed))]

rutis_interop::include_mounts!();

mod aimux;

use std::sync::Arc;

use aimux_llm::LlmService;
use rutis::{CordisError, Ctx, Disposer};

pub use aimux::AimuxBridge;

aimux::serve_aimux!(web);
aimux::serve_aimux!(agent);

/// Registers `service` as the `aimux` service of the [`web`] mount.
pub fn provide_web_aimux(ctx: &Ctx, service: Arc<dyn LlmService>) -> Result<Disposer, CordisError> {
    web::provide_aimux(ctx, AimuxBridge::new(service))
}

/// Registers `service` as the `aimux` service of the [`agent`] mount.
pub fn provide_agent_aimux(
    ctx: &Ctx,
    service: Arc<dyn LlmService>,
) -> Result<Disposer, CordisError> {
    agent::provide_aimux(ctx, AimuxBridge::new(service))
}
