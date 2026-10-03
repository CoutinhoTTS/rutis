//! `cargo xtask pack-plugin`: build and package one trusted dylib plugin
//! against an immutable SDK.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const SHARED_CRATES: [&str; 4] = ["rutis", "tokio", "tokio-util", "serde_json"];
const ALLOCATOR_SYMBOLS: [&str; 3] = ["__rust_alloc", "__rust_dealloc", "__rust_realloc"];

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("inspect") {
        if let Err(error) = inspect(&args[1..]) {
            eprintln!("inspect: {error}");
            process::exit(1);
        }
        return;
    }
    if let Err(error) = run(args) {
        eprintln!("pack-plugin: {error}");
        process::exit(1);
    }
}

/// `cargo xtask inspect imports|exports|export-count <library> [--target <triple>]`:
/// what the test scripts would otherwise ask readelf, otool or dumpbin.
/// The target defaults to rustc's host.
fn inspect(args: &[String]) -> Result<(), String> {
    let usage =
        "usage: cargo xtask inspect imports|exports|export-count <library> [--target <triple>]";
    let (command, file) = match args {
        [command, file, ..] => (command.as_str(), Path::new(file)),
        _ => return Err(usage.into()),
    };
    let target = match &args[2..] {
        [] => command_output(Command::new("rustc").arg("-vV"))?
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .ok_or("rustc -vV printed no host")?
            .to_owned(),
        [flag, triple] if flag == "--target" => triple.clone(),
        _ => return Err(usage.into()),
    };
    let bytes = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    match command {
        "imports" => {
            for name in rutis_dylib_meta::needed_libraries(&bytes, &target)? {
                println!("{name}");
            }
        }
        "exports" => {
            for name in rutis_dylib_meta::exported_symbols(&bytes, &target)? {
                println!("{name}");
            }
        }
        "export-count" => {
            println!(
                "{}",
                rutis_dylib_meta::exported_symbols(&bytes, &target)?.len()
            )
        }
        _ => return Err(usage.into()),
    }
    Ok(())
}

#[derive(Default)]
struct Args {
    manifest_path: Option<PathBuf>,
    sdk_manifest: Option<PathBuf>,
    sdk_file: Option<PathBuf>,
    output: Option<PathBuf>,
    target_dir: Option<PathBuf>,
    features: String,
    anchor_package: Option<String>,
    anchor_features: String,
    prebuilt_library: Option<PathBuf>,
    interfaces: Vec<String>,
}

fn parse_args(raw: Vec<String>) -> Result<Args, String> {
    let mut raw = raw.into_iter();
    match raw.next().as_deref() {
        Some("pack-plugin") => {}
        Some(other) => return Err(format!("unknown command: {other}")),
        None => return Err("usage: cargo xtask pack-plugin --manifest-path … --sdk-manifest … --sdk-file … --output …".into()),
    }
    let mut args = Args::default();
    while let Some(flag) = raw.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (flag, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| raw.next())
                .ok_or(format!("{name} needs a value"))
        };
        match name.as_str() {
            "--manifest-path" => args.manifest_path = Some(value()?.into()),
            "--sdk-manifest" => args.sdk_manifest = Some(value()?.into()),
            "--sdk-file" => args.sdk_file = Some(value()?.into()),
            "--output" => args.output = Some(value()?.into()),
            "--target-dir" => args.target_dir = Some(value()?.into()),
            "--features" => args.features = value()?,
            "--anchor-package" => args.anchor_package = Some(value()?),
            "--anchor-features" => args.anchor_features = value()?,
            "--prebuilt-library" => args.prebuilt_library = Some(value()?.into()),
            "--interface" => args.interfaces.push(value()?),
            _ => return Err(format!("unknown argument: {name}")),
        }
    }
    Ok(args)
}

fn run(raw: Vec<String>) -> Result<(), String> {
    let args = parse_args(raw)?;
    let required =
        |value: &Option<PathBuf>, name: &str| value.clone().ok_or(format!("{name} is required"));
    let manifest = canonical(&required(&args.manifest_path, "--manifest-path")?)?;
    let release = read_toml(&required(&args.sdk_manifest, "--sdk-manifest")?)?;
    let sdk_file = required(&args.sdk_file, "--sdk-file")?;
    let output = required(&args.output, "--output")?;
    let sdk = table(&release, "sdk")?;
    let sdk_hash = string(sdk, "artifact_sha256")?;
    let sdk_target = string(sdk, "target")?;
    let sdk_bytes = fs::read(&sdk_file).map_err(|e| format!("{}: {e}", sdk_file.display()))?;
    if sha256_bytes(&sdk_bytes) != sdk_hash {
        return Err("SDK file differs from SDK release manifest".into());
    }
    let std_library = rutis_dylib_meta::std_reference(&sdk_bytes, sdk_target)?;
    let cargo_toml = read_toml(&manifest)?;
    let package = table(&cargo_toml, "package")?;
    check_shared_duplicates(&manifest)?;

    let library = match &args.prebuilt_library {
        Some(path) => canonical(path)?,
        None => {
            let target_dir = match &args.target_dir {
                Some(dir) => dir.clone(),
                None => manifest.parent().unwrap().join("target"),
            };
            fs::create_dir_all(&target_dir)
                .map_err(|e| format!("{}: {e}", target_dir.display()))?;
            let target_dir = canonical(&target_dir)?;
            let repo = canonical(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))?;
            let cargo_home = match env::var_os("CARGO_HOME") {
                Some(home) => PathBuf::from(home),
                None => home_dir()?.join(".cargo"),
            };
            let cargo_home = canonical(&cargo_home).unwrap_or(cargo_home);
            let flags = format!(
                "{} --remap-path-prefix={}=/src --remap-path-prefix={}=/target --remap-path-prefix={}=/cargo",
                env::var("RUSTFLAGS").unwrap_or_default(),
                repo.display(),
                target_dir.display(),
                cargo_home.display()
            );
            let release_build = release.get("build").and_then(toml::Value::as_table);
            let anchor_package = args.anchor_package.clone().or_else(|| {
                release_build
                    .and_then(|build| build.get("anchor_package"))
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            });
            let mut anchor_features = split_list(&args.anchor_features);
            if anchor_features.is_empty() {
                anchor_features = release_build
                    .and_then(|build| build.get("anchor_features"))
                    .and_then(toml::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            build(&Build {
                manifest: &manifest,
                cargo_toml: &cargo_toml,
                target_dir: &target_dir,
                sdk_hash,
                sdk_target,
                flags: &flags,
                features: &args.features,
                anchor_package: anchor_package.as_deref(),
                anchor_features: &anchor_features,
            })?
        }
    };

    let bytes = fs::read(&library).map_err(|e| format!("{}: {e}", library.display()))?;
    check_allocator(&bytes, sdk_target)?;
    let boot = rutis_dylib_meta::read_boot(&bytes, sdk_target)?;
    let sdk_library = rutis_dylib_meta::sdk_reference(sdk_target)?;
    let weak = rutis_dylib_meta::weak_rust_exports(&bytes, sdk_target)?;
    if !weak.is_empty() {
        return Err(format!(
            "plugin exports weak Rust definitions, which dyld may bind across plugin versions: {}",
            weak.join(", ")
        ));
    }
    let native_deps = rutis_dylib_meta::check_plugin_dependencies(
        &bytes,
        sdk_target,
        &rutis_dylib_meta::SharedLibraries {
            sdk: &sdk_library,
            std: &std_library,
        },
    )?;
    if boot.sdk_id != string(sdk, "id")? || boot.sdk_artifact != sdk_hash {
        return Err("plugin boot identity differs from the published SDK".into());
    }
    if boot.version != string(package, "version")? {
        return Err("plugin boot version differs from Cargo.toml".into());
    }
    let mut interfaces = BTreeMap::new();
    for item in &args.interfaces {
        let (name, requirement) = item
            .split_once('=')
            .ok_or(format!("--interface expects name=requirement, got {item}"))?;
        interfaces.insert(name.to_owned(), requirement.to_owned());
    }
    let library_sha = sha256_bytes(&bytes);
    let rustc = command_output(Command::new("rustc").arg("--version"))?
        .trim()
        .to_owned();
    if rustc != string(sdk, "rustc")? {
        return Err("plugin rustc differs from the published SDK".into());
    }
    let locked_sha = sha256_file(&lock_path(&manifest)?)?;

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::create_dir(&output).map_err(|e| format!("{}: {e}", output.display()))?;
    let output = canonical(&output)?;
    let name = rutis_dylib_meta::library_file_name(&boot.id, sdk_target)?;
    fs::write(output.join(&name), &bytes).map_err(|e| e.to_string())?;
    if sha256_file(&output.join(&name))? != library_sha {
        return Err("library changed while packaging".into());
    }
    let q = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let mut content = format!(
        "[plugin]\nid = {}\nversion = {}\nlibrary = {}\nlibrary_sha256 = {}\nnative_deps = {}\n\n[sdk]\nversion = {}\nid = {}\nartifact_sha256 = {}\n\n[interfaces]\n",
        q(&boot.id),
        q(&boot.version),
        q(&name),
        q(&library_sha),
        toml::Value::Array(native_deps.iter().map(|d| toml::Value::String(d.clone())).collect()),
        q(string(sdk, "version")?),
        q(&boot.sdk_id),
        q(&boot.sdk_artifact),
    );
    for (name, requirement) in &interfaces {
        content.push_str(&format!("{} = {}\n", q(name), q(requirement)));
    }
    content.push_str(&format!(
        "\n[build]\ntarget = {}\nrustc = {}\nlock_sha256 = {}\n",
        q(sdk_target),
        q(&rustc),
        q(&locked_sha)
    ));
    fs::write(output.join("plugin.toml"), content).map_err(|e| e.to_string())?;
    println!("{}", output.display());
    Ok(())
}

struct Build<'a> {
    manifest: &'a Path,
    cargo_toml: &'a toml::Value,
    target_dir: &'a Path,
    sdk_hash: &'a str,
    sdk_target: &'a str,
    flags: &'a str,
    features: &'a str,
    anchor_package: Option<&'a str>,
    anchor_features: &'a [String],
}

/// Two-stage build (SDK design §5.3): build once to confirm the SDK artifact
/// matches the published one, then again with its hash embedded.
fn build(b: &Build<'_>) -> Result<PathBuf, String> {
    let package = string(table(b.cargo_toml, "package")?, "name")?;
    let mut command = Command::new("cargo");
    command.args(["build", "--release", "--locked", "--manifest-path"]);
    command.arg(b.manifest);
    let mut selected = Vec::new();
    match b.anchor_package {
        Some(anchor) => {
            command.args(["-p", package, "-p", anchor]);
            selected.extend(
                split_list(b.features)
                    .iter()
                    .map(|f| format!("{package}/{f}")),
            );
            selected.extend(b.anchor_features.iter().map(|f| format!("{anchor}/{f}")));
        }
        None => selected.extend(split_list(b.features)),
    }
    if !selected.is_empty() {
        command.args(["--features", &selected.join(",")]);
    }
    command
        .env("CARGO_TARGET_DIR", b.target_dir)
        .env("RUSTFLAGS", b.flags)
        .env("RUTIS_SDK_LOCKFILE", lock_path(b.manifest)?)
        .env("RUTIS_SDK_ARTIFACT_SHA256", "0".repeat(64));
    let sdk_file = b
        .target_dir
        .join("release")
        .join(rutis_dylib_meta::library_file_name(
            "rutis_sdk",
            b.sdk_target,
        )?);
    run_command(&mut command)?;
    if sha256_file(&sdk_file)? != b.sdk_hash {
        return Err("independent SDK build differs from the published artifact; build the plugin in the SDK release pipeline".into());
    }
    command.env("RUTIS_SDK_ARTIFACT_SHA256", b.sdk_hash);
    run_command(&mut command)?;
    if sha256_file(&sdk_file)? != b.sdk_hash {
        return Err("SDK artifact changed during the second build".into());
    }
    let library = b
        .cargo_toml
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(package);
    Ok(b.target_dir
        .join("release")
        .join(rutis_dylib_meta::library_file_name(library, b.sdk_target)?))
}

fn check_shared_duplicates(manifest: &Path) -> Result<(), String> {
    let tree = command_output(
        Command::new("cargo")
            .args(["tree", "-d", "--locked", "--manifest-path"])
            .arg(manifest),
    )?;
    for name in SHARED_CRATES {
        let prefix = format!("{name} v");
        if tree.lines().any(|line| line.starts_with(&prefix)) {
            return Err(format!(
                "duplicate shared dependency {name}; align versions with the SDK"
            ));
        }
    }
    Ok(())
}

/// The allocator belongs to the SDK alone (SDK design §4.3). Newer rustc
/// versions mangle these names, so match the suffix.
fn check_allocator(bytes: &[u8], target: &str) -> Result<(), String> {
    for symbol in rutis_dylib_meta::exported_symbols(bytes, target)? {
        if ALLOCATOR_SYMBOLS.iter().any(|name| symbol.ends_with(name)) {
            return Err(format!(
                "plugin defines its own global allocator ({symbol})"
            ));
        }
    }
    Ok(())
}

fn lock_path(manifest: &Path) -> Result<PathBuf, String> {
    manifest
        .ancestors()
        .skip(1)
        .map(|dir| dir.join("Cargo.lock"))
        .find(|candidate| candidate.exists())
        .ok_or("Cargo.lock not found".into())
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn table<'a>(value: &'a toml::Value, key: &str) -> Result<&'a toml::Value, String> {
    value
        .get(key)
        .filter(|v| v.is_table())
        .ok_or(format!("missing [{key}]"))
}

fn string<'a>(value: &'a toml::Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(toml::Value::as_str)
        .ok_or(format!("missing {key}"))
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn home_dir() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set".into())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| sha256_bytes(&bytes))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn run_command(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("{command:?} failed with {status}"));
    }
    Ok(())
}

fn command_output(command: &mut Command) -> Result<String, String> {
    let output = command.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "{command:?} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}
