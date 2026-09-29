//! Generate bindings for the baseline plugins when `interop/baseline` is
//! installed (`npm --prefix interop/baseline ci`); otherwise build nothing.

use std::path::Path;

const PLUGINS: &[(&str, &str)] = &[
    ("invariants", "dsh-invariants"),
    ("fs", "dsh-fs-local"),
    ("jobs", "dsh-jobs-local"),
    ("commands", "dsh-commands"),
];

/// dsh-workspace needs storage and session persistence, which other plugins
/// provide: mount them together so the dependencies resolve natively.
const WORKSPACE: &[(&str, &str)] = &[
    ("storage", "dsh-storage"),
    ("storage_json", "dsh-storage-json"),
    ("storage_domain", "dsh-storage-domain"),
    ("sessions", "dsh-session-persistence-jsonl"),
    ("workspace", "dsh-workspace"),
];

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dsh_baseline)");
    let modules = Path::new("../../interop/baseline/node_modules/@deepseek-ai");
    println!("cargo:rerun-if-changed={}", modules.display());
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") || !modules.exists() {
        return;
    }
    for (module, package) in PLUGINS {
        rutis_interop::build::cordis_module(modules.join(package), "../../interop/node", module)
            .unwrap_or_else(|error| panic!("generate bindings for {package}: {error}"));
    }
    let workspace: Vec<_> = WORKSPACE
        .iter()
        .map(|(name, package)| (*name, modules.join(package)))
        .collect();
    let members: Vec<_> = workspace
        .iter()
        .map(|(name, path)| (*name, path.as_path()))
        .collect();
    rutis_interop::build::cordis_group("workspace", &members, "../../interop/node")
        .unwrap_or_else(|error| panic!("generate bindings for the workspace group: {error}"));
    // Credential changes are forwarded to rutis listeners as events.
    rutis_interop::build::Bindings::new("credentials", "../../interop/node")
        .plugin(modules.join("dsh-credentials-local"))
        .event("credentials/reference-updated")
        .event("credentials/record-updated")
        .generate()
        .unwrap_or_else(|error| panic!("generate bindings for dsh-credentials-local: {error}"));
    // dsh-persona contributes prompt sections to `systemPrompt`, which the
    // rutis application provides.
    rutis_interop::build::Bindings::new("persona", "../../interop/node")
        .plugin(modules.join("dsh-persona"))
        .provide("systemPrompt")
        // The host announces prompt changes the way the Cordis registry does.
        .emit("system-prompt/change")
        .generate()
        .unwrap_or_else(|error| panic!("generate bindings for dsh-persona: {error}"));
    println!("cargo:rustc-cfg=dsh_baseline");
}
