//! Application build integration; no per-service protocol declarations.

use std::path::Path;

mod rust;
pub use rust::rutis_plugin;

pub fn cordis_plugin(
    plugin: impl AsRef<Path>,
    node_package: impl AsRef<Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let plugin = plugin.as_ref().canonicalize()?;
    let node_package = node_package.as_ref().canonicalize()?;
    let generator = node_package.join("src/generate.mjs");
    println!("cargo:rerun-if-changed={}", plugin.display());
    println!("cargo:rerun-if-changed={}", generator.display());
    println!(
        "cargo:rerun-if-changed={}",
        node_package.join("package-lock.json").display()
    );
    let output = std::process::Command::new("node")
        .arg(&generator)
        .arg(&plugin)
        .arg(&node_package)
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
    }
    let generated: Generated = serde_json::from_slice(&output.stdout)?;
    for input in generated.inputs {
        println!("cargo:rerun-if-changed={input}");
    }
    let output_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output_dir.join("cordis.rs"), generated.rust)?;
    Ok(())
}
