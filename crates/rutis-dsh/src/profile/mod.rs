//! dsh's profile configuration, driven by rutis-loader.
//!
//! dsh composes a profile from patch layers (dsh-app-boot): every bundle's
//! patch files, the user's `cordis.patch.yml`, `$DSH_HOME/cordis.patch.yml`,
//! `--patch` overlays and the telemetry switch. This module reads them into
//! [`rutis_loader::Layer`]s ([`load`]), stores the user layer
//! ([`UserLayerStore`]), evaluates `!!js` ([`expr::JsSubset`]), expands
//! nested includes and reloads on change ([`watch::watch`]).
//! Design: `docs/design-rutis-loader-2026-10-02.md` §十二.

pub mod expr;
pub mod include;
pub mod layers;
pub mod lock;
pub mod npm_semver;
pub mod paths;
pub mod persist;
pub mod watch;
pub mod yaml;

use std::sync::Arc;

use rutis_loader::{Expressions, LoaderOptions, ServiceCatalog};
use serde::Serialize;

pub use layers::{load, Profile, ProfileContext, ProfileError, SkipKind, SkippedBundle};
pub use persist::UserLayerStore;

/// The `profileContext` service dsh's rows read (`ctx.get('profileContext')`).
/// Provide it on the loader's root context; it is readable from expressions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileContextService {
    pub name: String,
    pub dir: String,
    pub patch_path: String,
    pub install_anchor: String,
    pub cwd: String,
    pub home: String,
    pub started_bundles: Vec<String>,
}

impl ProfileContextService {
    pub fn new(context: &ProfileContext, profile: &Profile) -> Self {
        let text = |p: &std::path::Path| p.to_string_lossy().into_owned();
        Self {
            name: context.name.clone(),
            dir: text(&context.dir),
            patch_path: text(&context.user_layer_path()),
            install_anchor: text(&context.install_anchor),
            cwd: std::env::current_dir()
                .map(|p| text(&p))
                .unwrap_or_default(),
            home: text(&context.home),
            started_bundles: profile
                .layers
                .iter()
                .map(|l| l.name.clone())
                .filter(|n| {
                    !n.starts_with("patch:")
                        && !["user", "home", "telemetry", "includes"].contains(&n.as_str())
                })
                .collect(),
        }
    }
}

/// A catalog with dsh's host services; add the app's own (`webStartup`,
/// `headlessStartup`, ...) before mounting.
pub fn catalog() -> ServiceCatalog {
    let mut catalog = ServiceCatalog::new();
    catalog.readable::<ProfileContextService>("profileContext");
    catalog
}

/// Loader options for a dsh profile: the user layer store, dsh's `!!js`
/// evaluator over the live process, and [`catalog`].
pub fn loader_options(context: &ProfileContext) -> LoaderOptions {
    LoaderOptions {
        persist: Arc::new(UserLayerStore::new(context)),
        catalog: catalog(),
        expressions: Some(
            Arc::new(expr::JsSubset::new(expr::Environment::current())) as Arc<dyn Expressions>
        ),
    }
}

/// `package.json` of the npm project this binary's dsh installation is
/// (`RUTIS_CORDIS_ROOT` when set), where bundles resolve from first.
pub fn install_anchor() -> std::path::PathBuf {
    rutis_bridge::cordis::npm_root(concat!(env!("CARGO_MANIFEST_DIR"), "/dsh")).join("package.json")
}

/// The composed rows of `profile` as the entry-list YAML (`!!js` kept), the
/// output of `rutis-dsh dump-config`.
pub fn dump(profile: &Profile) -> String {
    let rows = rutis_loader::apply_patches(&profile.layers).rows;
    yaml::Document::parse("")
        .expect("an empty document")
        .render_fresh(&rows)
}
