//! All bindings come from `[package.metadata.rutis-interop]` in Cargo.toml.
//! When the baseline plugins are not installed, nothing is built.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(dsh_baseline)");
    let modules = std::path::Path::new("../../interop/baseline/node_modules");
    println!("cargo:rerun-if-changed={}", modules.display());
    if std::env::var("CARGO_CFG_TARGET_FAMILY").as_deref() != Ok("unix") || !modules.exists() {
        return;
    }
    rutis_interop::build::from_manifest().unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rustc-cfg=dsh_baseline");
}
