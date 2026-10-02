//! dsh's profile configuration, driven by rutis-loader.
//!
//! dsh composes a profile from patch layers (dsh-app-boot): every bundle's
//! patch files, the user's `cordis.patch.yml`, `~/.dsh/cordis.patch.yml`,
//! `--patch` overlays and the telemetry switch. This module reads them into
//! [`rutis_loader::Layer`]s, stores the user layer, and evaluates `!!js`.
//! Design: `docs/design-rutis-loader-2026-10-02.md` §十二.

pub mod expr;
pub mod layers;
pub mod npm_semver;
pub mod paths;
pub mod yaml;

pub use layers::{load, Profile, ProfileContext, ProfileError, SkippedBundle};
