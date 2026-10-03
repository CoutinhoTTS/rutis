//! A development channel into a running rutis host
//! (`docs/design-host-dev-mode-2026-09-25.md` §三–§五), built on
//! rutis-loader: local Unix socket, JSON lines, protocol version 1.
//!
//! Requests are `{"id": <any>, "cmd": "<command>", ...}`; each gets one
//! `{"id", "ok": true, "result"}` or `{"id", "ok": false, "error"}` line.
//!
//! | command | arguments | effect |
//! |---|---|---|
//! | `hello` | | protocol and host identity |
//! | `describe` | | fibers, service bindings, event backlogs, loader rows |
//! | `status` | | loader rows, dev rows marked |
//! | `watch` | | then a stream of `{"event": ..}` lines (fiber, service, loader) until the client closes |
//! | `load` | `name`, `id?`, `config?`, `parent?` | add a row to the channel's overlay layer |
//! | `swap` | `id` | `Loader::reload`: all or nothing, the old version keeps running on failure |
//! | `unload-dev` | `id` | remove a row the channel loaded |
//!
//! Loaded rows live in an overlay layer (`Loader::set_overlay`): never
//! persisted, kept across the application's reconciles, not editable as user
//! configuration. The socket is created `0600` and only on the path given;
//! start the channel only in development hosts. Mutating commands are
//! reported to the audit hook.

#[cfg(unix)]
mod describe;
#[cfg(unix)]
mod server;

#[cfg(unix)]
pub use server::{AuditRecord, DevChannel, DevOptions, PROTOCOL_VERSION};
