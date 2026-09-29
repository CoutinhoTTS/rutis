//! Typed rutis bindings for published dsh Cordis plugins, generated from
//! their declarations during the normal Cargo build. The mounts are listed
//! in Cargo.toml; each becomes a module exposing `Plugin`, `Config` and the
//! native service proxy types.
#![cfg(all(unix, dsh_baseline))]

rutis_interop::include_mounts!();
