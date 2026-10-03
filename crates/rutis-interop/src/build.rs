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
    events: Vec<String>,
    emits: Vec<String>,
    root: Option<std::path::PathBuf>,
}

impl Bindings {
    pub fn new(module: &str, node_package: impl AsRef<Path>) -> Self {
        Self {
            module: module.to_owned(),
            node_package: node_package.as_ref().to_owned(),
            members: Vec::new(),
            provided: Vec::new(),
            events: Vec::new(),
            emits: Vec::new(),
            root: None,
        }
    }

    /// Locate the runtime and the plugins relative to the npm project at
    /// `root`, so the binary can run against a copy of it elsewhere: see
    /// [`crate::npm_root`]. Without it the generated code names absolute
    /// paths on the build machine.
    pub fn relocatable(mut self, root: impl AsRef<Path>) -> Self {
        self.root = Some(root.as_ref().to_owned());
        self
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

    /// Forward a Cordis event to rutis listeners as a generated event type.
    /// Only notifications (events returning `void`) can be forwarded.
    pub fn event(mut self, name: &str) -> Self {
        self.events.push(name.to_owned());
        self
    }

    /// Emit a rutis event into the mounted Cordis Context, for example the
    /// change events of a service the host provides. An event is forwarded
    /// in one direction only.
    pub fn emit(mut self, name: &str) -> Self {
        self.emits.push(name.to_owned());
        self
    }

    pub fn generate(self) -> Result<(), Box<dyn std::error::Error>> {
        let single = matches!(self.members.as_slice(), [(None, _)]);
        let mut args: Vec<std::ffi::OsString> = self
            .provided
            .iter()
            .map(|service| format!("--provide={service}").into())
            .chain(
                self.events
                    .iter()
                    .map(|event| format!("--event={event}").into()),
            )
            .chain(
                self.emits
                    .iter()
                    .map(|event| format!("--emit={event}").into()),
            )
            .collect();
        // Relocatable paths stay lexical: a symlinked package (a `file:`
        // dependency) is addressed where it sits in the npm project.
        let locate = |path: &Path| -> std::io::Result<std::path::PathBuf> {
            match &self.root {
                Some(_) => {
                    path.metadata()?;
                    std::path::absolute(path)
                }
                None => path.canonicalize(),
            }
        };
        if let Some(root) = &self.root {
            let mut argument = std::ffi::OsString::from("--root=");
            argument.push(locate(root)?);
            args.push(argument);
        }
        for (name, plugin) in &self.members {
            let plugin = locate(plugin)?;
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
        let node_package = locate(&self.node_package)?;
        generate(args, &node_package, &self.module)
    }
}

fn generate(
    args: Vec<std::ffi::OsString>,
    node_package: &Path,
    module: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let generator = node_package.join("src/generate.mjs");
    println!("cargo:rerun-if-changed={}", generator.display());
    println!(
        "cargo:rerun-if-changed={}",
        node_package.join("package-lock.json").display()
    );
    let output = std::process::Command::new("node")
        .arg(&generator)
        .arg(node_package)
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

/// Generate every mount configured in the package's `Cargo.toml`:
///
/// ```toml
/// [package.metadata.rutis-interop]
/// npm = "."                      # npm project with the plugins installed
/// # runtime = "..."              # default: <npm>/node_modules/@arcships/rutis-interop
///
/// [package.metadata.rutis-interop.mounts.credentials]
/// plugin = "@deepseek-ai/dsh-credentials-local"   # or path = "src/plugin.ts"
/// version = "0.2.0-rc.1"         # optional: must match the installed package
/// events = ["credentials/record-updated"]         # Cordis -> rutis
/// emits = []                     # rutis -> Cordis
/// provide = []                   # services the rutis application provides
///
/// [package.metadata.rutis-interop.mounts.workspace]
/// group = [{ name = "storage", plugin = "@deepseek-ai/dsh-storage" }]
/// ```
///
/// Each mount becomes a module; `rutis_interop::include_mounts!()` declares
/// them all. The npm project is not installed by the build: a missing
/// package is an error that names the install command.
pub fn from_manifest() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let manifest_path = root.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest: toml::Table = std::fs::read_to_string(&manifest_path)?.parse()?;
    let config = manifest
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("rutis-interop"))
        .and_then(toml::Value::as_table)
        .ok_or("Cargo.toml has no [package.metadata.rutis-interop] section")?;
    let text = |table: &toml::Table, key: &str| {
        table
            .get(key)
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    let list = |table: &toml::Table, key: &str| -> Result<Vec<String>, String> {
        match table.get(key) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or(format!("{key} must list strings"))
                })
                .collect(),
            Some(_) => Err(format!("{key} must be an array of strings")),
        }
    };

    let npm = root.join(text(config, "npm").unwrap_or_else(|| ".".into()));
    let modules = npm.join("node_modules");
    let install = format!("npm --prefix {} ci", npm.display());
    if !npm.join("package.json").exists() {
        return Err(format!(
            "{} has no package.json for the Cordis plugins",
            npm.display()
        )
        .into());
    }
    println!(
        "cargo:rerun-if-changed={}",
        npm.join("package-lock.json").display()
    );
    if !modules.exists() {
        return Err(format!("the Cordis plugins are not installed: run `{install}`").into());
    }
    let runtime = match text(config, "runtime") {
        Some(runtime) => root.join(runtime),
        None => modules.join("@arcships/rutis-interop"),
    };
    check_runtime(&runtime, &install)?;

    let resolve = |entry: &toml::Table,
                   context: &str|
     -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        match (text(entry, "plugin"), text(entry, "path")) {
            (Some(package), None) => {
                let directory = modules.join(&package);
                if !directory.join("package.json").exists() {
                    return Err(format!("{context}: {package} is not installed: add it to {} and run `{install}`", npm.join("package.json").display()).into());
                }
                if let Some(expected) = text(entry, "version") {
                    let installed: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.join("package.json"))?)?;
                    let installed = installed["version"].as_str().unwrap_or_default();
                    if installed != expected {
                        return Err(format!("{context}: {package} {installed} is installed, {expected} is required: run `{install}`").into());
                    }
                }
                Ok(directory)
            }
            (None, Some(path)) => Ok(root.join(path)),
            _ => Err(format!("{context}: set exactly one of `plugin` (an npm package) or `path` (a TypeScript source)").into()),
        }
    };

    let mounts = config
        .get("mounts")
        .and_then(toml::Value::as_table)
        .ok_or("[package.metadata.rutis-interop] declares no mounts")?;
    let mut declarations = String::new();
    for (name, mount) in mounts {
        syn::parse_str::<syn::Ident>(name)
            .map_err(|_| format!("mount name {name} is not a Rust identifier"))?;
        let mount = mount
            .as_table()
            .ok_or(format!("mount {name} must be a table"))?;
        let mut bindings = Bindings::new(name, &runtime).relocatable(&npm);
        match mount.get("group") {
            Some(toml::Value::Array(members)) => {
                for member in members {
                    let member = member
                        .as_table()
                        .ok_or(format!("mount {name}: group members must be tables"))?;
                    let member_name = text(member, "name")
                        .ok_or(format!("mount {name}: every group member needs a name"))?;
                    bindings = bindings.member(
                        &member_name,
                        resolve(member, &format!("mount {name}.{member_name}"))?,
                    );
                }
            }
            Some(_) => return Err(format!("mount {name}: group must be an array of tables").into()),
            None => bindings = bindings.plugin(resolve(mount, &format!("mount {name}"))?),
        }
        for service in list(mount, "provide")? {
            bindings = bindings.provide(&service);
        }
        for event in list(mount, "events")? {
            bindings = bindings.event(&event);
        }
        for event in list(mount, "emits")? {
            bindings = bindings.emit(&event);
        }
        bindings
            .generate()
            .map_err(|error| format!("mount {name}: {error}"))?;
        declarations.push_str(&format!(
            "#[cfg(unix)]\n#[allow(clippy::all, dead_code, unused_imports)]\npub mod {name} {{ include!(concat!(env!(\"OUT_DIR\"), \"/{name}.rs\")); }}\n"
        ));
    }
    let output = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output.join("rutis_interop_mounts.rs"), declarations)?;
    Ok(())
}

/// The Node runtime must speak this crate's protocol version.
fn check_runtime(runtime: &Path, install: &str) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = runtime.join("package.json");
    if !manifest.exists() {
        return Err(format!("the @arcships/rutis-interop runtime is missing at {}: add it to the npm project and run `{install}`", runtime.display()).into());
    }
    println!("cargo:rerun-if-changed={}", manifest.display());
    let package: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
    let protocol = package["rutisProtocol"].as_u64();
    if protocol != Some(crate::PROTOCOL as u64) {
        return Err(format!(
            "{} speaks protocol {}, this rutis-interop speaks {}: install a matching @arcships/rutis-interop",
            runtime.display(),
            protocol.map_or("(unknown)".into(), |protocol| protocol.to_string()),
            crate::PROTOCOL
        )
        .into());
    }
    if !runtime.join("node_modules").exists() && !runtime.join("../../typescript").exists() {
        return Err(format!("the @arcships/rutis-interop runtime at {} has no dependencies installed: run `npm --prefix {} ci`", runtime.display(), runtime.display()).into());
    }
    Ok(())
}

#[cfg(test)]
mod manifest_tests {
    use super::from_manifest;
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn failure(root: &Path, manifest: &str) -> String {
        write(&root.join("Cargo.toml"), manifest);
        from_manifest().unwrap_err().to_string()
    }

    // Environment variables are process-wide, so the cases run in sequence.
    #[test]
    fn configuration_errors_name_the_cause_and_the_fix() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::env::set_var("CARGO_MANIFEST_DIR", root);
        std::env::set_var("OUT_DIR", root.join("out"));
        let config = r#"
            [package]
            name = "app"
            [package.metadata.rutis-interop]
            npm = "cordis"
            [package.metadata.rutis-interop.mounts.store]
            plugin = "store-plugin"
            version = "2.0.0"
        "#;

        assert!(failure(root, "[package]\nname = \"app\"")
            .contains("no [package.metadata.rutis-interop]"));

        write(&root.join("cordis/package.json"), "{}");
        let error = failure(root, config);
        assert!(
            error.contains("not installed") && error.contains("cordis ci"),
            "{error}"
        );

        let runtime = root.join("cordis/node_modules/@arcships/rutis-interop");
        write(&runtime.join("package.json"), r#"{ "rutisProtocol": 99 }"#);
        let error = failure(root, config);
        assert!(error.contains("speaks protocol 99"), "{error}");

        write(
            &runtime.join("package.json"),
            &format!(r#"{{ "rutisProtocol": {} }}"#, crate::PROTOCOL),
        );
        fs::create_dir_all(runtime.join("node_modules")).unwrap();
        let error = failure(root, config);
        assert!(error.contains("store-plugin is not installed"), "{error}");

        write(
            &root.join("cordis/node_modules/store-plugin/package.json"),
            r#"{ "version": "1.0.0" }"#,
        );
        let error = failure(root, config);
        assert!(
            error.contains("1.0.0 is installed, 2.0.0 is required"),
            "{error}"
        );
    }
}
