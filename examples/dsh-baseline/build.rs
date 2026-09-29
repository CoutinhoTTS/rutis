//! Generate bindings for the baseline plugins when `interop/baseline` is
//! installed (`npm --prefix interop/baseline ci`); otherwise build nothing.

use std::path::Path;

const PLUGINS: &[(&str, &str)] = &[
    ("invariants", "dsh-invariants"),
    ("credentials", "dsh-credentials-local"),
    ("fs", "dsh-fs-local"),
    ("jobs", "dsh-jobs-local"),
    ("commands", "dsh-commands"),
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
    println!("cargo:rustc-cfg=dsh_baseline");
}
