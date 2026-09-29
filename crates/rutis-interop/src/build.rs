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
    Bindings::new(module, node_package)
        .plugin(plugin)
        .generate()
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
    plugins
        .iter()
        .fold(
            Bindings::new(module, node_package),
            |bindings, (name, plugin)| bindings.member(name, plugin),
        )
        .generate()
}

/// Bindings for one mount: a plugin or a named group, plus the services the
/// rutis application provides to it.
///
/// ```ignore
/// Bindings::new("persona", "../../interop/node")
///     .plugin(modules.join("dsh-persona"))
///     .provide("systemPrompt")
///     .generate()?;
/// ```
pub struct Bindings {
    module: String,
    node_package: std::path::PathBuf,
    members: Vec<(Option<String>, std::path::PathBuf)>,
    provided: Vec<String>,
}

impl Bindings {
    pub fn new(module: &str, node_package: impl AsRef<Path>) -> Self {
        Self {
            module: module.to_owned(),
            node_package: node_package.as_ref().to_owned(),
            members: Vec::new(),
            provided: Vec::new(),
        }
    }

    /// The single plugin of this mount; its configuration is `Config` itself.
    pub fn plugin(mut self, plugin: impl AsRef<Path>) -> Self {
        self.members.push((None, plugin.as_ref().to_owned()));
        self
    }

    /// A named member of a group, loaded in order; `name` is its `Config` field.
    pub fn member(mut self, name: &str, plugin: impl AsRef<Path>) -> Self {
        self.members
            .push((Some(name.to_owned()), plugin.as_ref().to_owned()));
        self
    }

    /// A Cordis service the rutis application provides to the plugins. The
    /// generated module gets a trait to implement and a `provide_*` helper;
    /// the mount waits for the service natively.
    pub fn provide(mut self, service: &str) -> Self {
        self.provided.push(service.to_owned());
        self
    }

    pub fn generate(self) -> Result<(), Box<dyn std::error::Error>> {
        let single = matches!(self.members.as_slice(), [(None, _)]);
        let mut args: Vec<std::ffi::OsString> = self
            .provided
            .iter()
            .map(|service| format!("--provide={service}").into())
            .collect();
        for (name, plugin) in &self.members {
            let plugin = plugin.canonicalize()?;
            println!("cargo:rerun-if-changed={}", plugin.display());
            match name {
                None if single => args.push(plugin.into_os_string()),
                None => return Err("group members need names; use Bindings::member".into()),
                Some(name) => {
                    let mut member = std::ffi::OsString::from(format!("{name}="));
                    member.push(plugin);
                    args.push(member);
                }
            }
        }
        generate(args, &self.node_package, &self.module)
    }
}

fn generate(
    args: Vec<std::ffi::OsString>,
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
        .args(&args)
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
