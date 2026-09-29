//! Application build integration; no per-service protocol declarations.

use std::path::Path;

mod rust;
pub use rust::rutis_plugin;

/// Generate Rust bindings for a Cordis plugin into `OUT_DIR/cordis.rs`.
pub fn cordis_plugin(
    plugin: impl AsRef<Path>,
    node_package: impl AsRef<Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    cordis_module(plugin, node_package, "cordis")
}

/// Generate Rust bindings for a Cordis plugin into `OUT_DIR/{module}.rs`.
/// `plugin` is either a TypeScript source file or an installed package
/// directory, which is analysed through its declared `types`.
pub fn cordis_module(
    plugin: impl AsRef<Path>,
    node_package: impl AsRef<Path>,
    module: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let plugin = plugin.as_ref().canonicalize()?;
    println!("cargo:rerun-if-changed={}", plugin.display());
    generate(vec![plugin.into_os_string()], node_package.as_ref(), module)
}

/// Generate one binding module for a group of Cordis plugins that are
/// mounted together, in the given order, in one Cordis Context: dependencies
/// between them resolve natively. Each `(name, plugin)` becomes a field of
/// the generated `Config`; the services of all members are exported.
pub fn cordis_group(
    module: &str,
    plugins: &[(&str, &Path)],
    node_package: impl AsRef<Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut members = Vec::new();
    for (name, plugin) in plugins {
        let plugin = plugin.canonicalize()?;
        println!("cargo:rerun-if-changed={}", plugin.display());
        let mut member = std::ffi::OsString::from(format!("{name}="));
        member.push(plugin);
        members.push(member);
    }
    generate(members, node_package.as_ref(), module)
}

fn generate(
    members: Vec<std::ffi::OsString>,
    node_package: &Path,
    module: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let node_package = node_package.canonicalize()?;
    let generator = node_package.join("src/generate.mjs");
    println!("cargo:rerun-if-changed={}", generator.display());
    println!(
        "cargo:rerun-if-changed={}",
        node_package.join("package-lock.json").display()
    );
    let output = std::process::Command::new("node")
        .arg(&generator)
        .arg(&node_package)
        .args(&members)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "binding generation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    #[derive(serde::Deserialize)]
    struct Generated {
        rust: String,
        inputs: Vec<String>,
        diagnostics: Vec<String>,
    }
    let generated: Generated = serde_json::from_slice(&output.stdout)?;
    for input in generated.inputs {
        println!("cargo:rerun-if-changed={input}");
    }
    // Members that cannot be bound yet are listed, not silently dropped.
    for diagnostic in generated.diagnostics {
        println!("cargo:warning={diagnostic}");
    }
    let output_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output_dir.join(format!("{module}.rs")), generated.rust)?;
    Ok(())
}
