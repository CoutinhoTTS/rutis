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
    match args.first().map(String::as_str) {
        Some("inspect") => {
            if let Err(error) = inspect(&args[1..]) {
                eprintln!("inspect: {error}");
                process::exit(1);
            }
        }
        Some("pack-sdk-bundle") => {
            if let Err(error) = pack_sdk_bundle(&args[1..]) {
                eprintln!("pack-sdk-bundle: {error}");
                process::exit(1);
            }
        }
        Some("pack-plugin") => {
            if let Err(error) = run(args) {
                eprintln!("pack-plugin: {error}");
                process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            process::exit(1);
        }
        None => {
            eprintln!("usage: cargo xtask pack-plugin … | pack-sdk-bundle … | inspect …");
            process::exit(1);
        }
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
    bundle: Option<PathBuf>,
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
            "--bundle" => args.bundle = Some(value()?.into()),
            "--interface" => args.interfaces.push(value()?),
            _ => return Err(format!("unknown argument: {name}")),
        }
    }
    Ok(args)
}

fn run(raw: Vec<String>) -> Result<(), String> {
    let args = parse_args(raw)?;
    if args.bundle.is_some() {
        if args.sdk_manifest.is_some() || args.sdk_file.is_some() {
            return Err("--bundle cannot be combined with --sdk-manifest/--sdk-file".into());
        }
        if args.prebuilt_library.is_some() {
            return Err("--bundle cannot be combined with --prebuilt-library".into());
        }
        return pack_plugin_bundle(&args);
    }
    let required =
        |value: &Option<PathBuf>, name: &str| value.clone().ok_or(format!("{name} is required"));
    let manifest = canonical(&required(&args.manifest_path, "--manifest-path")?)?;
    let release = read_toml(&required(&args.sdk_manifest, "--sdk-manifest")?)?;
    let sdk_file = required(&args.sdk_file, "--sdk-file")?;
    let sdk = table(&release, "sdk")?;
    let sdk_hash = string(sdk, "artifact_sha256")?;
    let sdk_target = string(sdk, "target")?;
    let sdk_bytes = fs::read(&sdk_file).map_err(|e| format!("{}: {e}", sdk_file.display()))?;
    if sha256_bytes(&sdk_bytes) != sdk_hash {
        return Err("SDK file differs from SDK release manifest".into());
    }
    let std_library = rutis_dylib_meta::std_reference(&sdk_bytes, sdk_target)?;
    let cargo_toml = read_toml(&manifest)?;
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

    finish_pack(&args, &library, &manifest, sdk, sdk_target, &std_library)
}

/// Everything after the plugin binary exists: binary checks, identity
/// cross-checks and the output directory with plugin.toml.
fn finish_pack(
    args: &Args,
    library: &Path,
    manifest: &Path,
    sdk: &toml::Value,
    sdk_target: &str,
    std_library: &str,
) -> Result<(), String> {
    let output = args.output.clone().ok_or("--output is required")?;
    let sdk_hash = string(sdk, "artifact_sha256")?;
    let bytes = fs::read(library).map_err(|e| format!("{}: {e}", library.display()))?;
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
            std: std_library,
        },
    )?;
    if boot.sdk_id != string(sdk, "id")? || boot.sdk_artifact != sdk_hash {
        return Err("plugin boot identity differs from the published SDK".into());
    }
    let cargo_toml = read_toml(manifest)?;
    let package = table(&cargo_toml, "package")?;
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
    let locked_sha = sha256_file(&lock_path(manifest)?)?;

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

/// An sdk-bundle as produced by `pack-sdk-bundle`
/// (design-sdk-build-package §三).
struct SdkBundle {
    lib: PathBuf,
    deps: PathBuf,
}

/// Read and fully verify an sdk-bundle: the manifest format, the `[sdk]`
/// identity against sdk.toml, and every listed file's hash. A bundle that
/// lost a file or was tampered with fails here, before anything is built.
fn read_bundle(dir: &Path) -> Result<(SdkBundle, toml::Value), String> {
    let bundle_toml = read_toml(&dir.join("bundle.toml"))?;
    if bundle_toml
        .get("format_version")
        .and_then(toml::Value::as_integer)
        != Some(1)
    {
        return Err("unsupported bundle format; use a pack-plugin that matches this bundle".into());
    }
    let identity = table(&bundle_toml, "sdk")?;
    let release = read_toml(&dir.join("sdk.toml"))?;
    let sdk = table(&release, "sdk")?;
    for key in ["version", "id", "artifact_sha256"] {
        if string(identity, key)? != string(sdk, key)? {
            return Err(format!("bundle.toml and sdk.toml disagree on {key}"));
        }
    }
    let files = bundle_toml
        .get("files")
        .and_then(toml::Value::as_table)
        .ok_or("bundle.toml has no [files] manifest")?;
    if files.is_empty() {
        return Err("bundle.toml lists no files".into());
    }
    for (rel, hash) in files {
        let expected = hash
            .as_str()
            .ok_or(format!("files.{rel} is not a string"))?;
        let actual =
            sha256_file(&dir.join(rel)).map_err(|_| format!("the bundle is missing {rel}"))?;
        if actual != expected {
            return Err(format!(
                "{rel} differs from the bundle manifest; the bundle is incomplete or was modified"
            ));
        }
    }
    Ok((
        SdkBundle {
            lib: dir.join("lib"),
            deps: dir.join("deps"),
        },
        release,
    ))
}

/// A plugin packed with `--bundle` must take every shared crate from the
/// SDK's re-exports. A direct dependency would build a second, incompatible
/// copy in the plugin graph (the anchor mode requires the rutis-sdk
/// dependency because it builds the SDK in the same graph; --bundle does
/// not build it). The check parses the manifest instead of relying on a
/// rustc error: with the injected --extern in RUSTFLAGS, a source rutis-sdk
/// build dies in its build.rs classifying an unknown RUSTFLAGS argument.
fn check_bundle_manifest(manifest: &Path) -> Result<(), String> {
    let cargo_toml = read_toml(manifest)?;
    let mut offenders = Vec::new();
    fn scan(section: Option<&toml::Value>, where_: &str, offenders: &mut Vec<String>) {
        if let Some(deps) = section.and_then(toml::Value::as_table) {
            for name in deps.keys() {
                if name == "rutis-sdk" || SHARED_CRATES.contains(&name.as_str()) {
                    offenders.push(format!("{name} ({where_})"));
                }
            }
        }
    }
    scan(
        cargo_toml.get("dependencies"),
        "dependencies",
        &mut offenders,
    );
    if let Some(targets) = cargo_toml.get("target").and_then(toml::Value::as_table) {
        for cfg in targets.values() {
            scan(
                cfg.get("dependencies"),
                "target dependencies",
                &mut offenders,
            );
        }
    }
    if !offenders.is_empty() {
        return Err(format!(
            "the plugin declares {} as a direct dependency; with --bundle, shared crates come from the rutis_sdk re-exports (rutis_sdk::rutis, ::tokio, ::serde_json) — remove the direct dependency",
            offenders.join(", ")
        ));
    }
    Ok(())
}

/// `pack-plugin --bundle <dir>`: build a plugin against the prebuilt SDK in
/// an sdk-bundle. No host sources, no SDK rebuild, no feature-graph
/// unification (design-sdk-build-package §四).
fn pack_plugin_bundle(args: &Args) -> Result<(), String> {
    let bundle_dir = canonical(args.bundle.as_ref().ok_or("--bundle needs a directory")?)?;
    let (bundle, release) = read_bundle(&bundle_dir)?;
    let sdk = table(&release, "sdk")?;
    let sdk_hash = string(sdk, "artifact_sha256")?;
    let sdk_target = string(sdk, "target")?;

    // The compiler is checked before anything else: a mismatched rustc fails
    // deep inside metadata loading (E0514) with a far less helpful error.
    let rustc = command_output(Command::new("rustc").arg("--version"))?
        .trim()
        .to_owned();
    if rustc != string(sdk, "rustc")? {
        return Err(format!(
            "this rustc is {rustc}; the bundle's SDK was built with {}; use the bundle's rust-toolchain.toml",
            string(sdk, "rustc")?
        ));
    }
    let host = command_output(Command::new("rustc").arg("-vV"))?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("rustc -vV printed no host")?
        .to_owned();
    if host != sdk_target {
        return Err(format!(
            "host target {host} differs from the bundle's {sdk_target}"
        ));
    }

    let manifest = canonical(
        args.manifest_path
            .as_ref()
            .ok_or("--manifest-path is required")?,
    )?;
    // The manifest is checked before the lock file: a blacklisted direct
    // dependency fails fast, without cargo touching the network or the graph.
    check_bundle_manifest(&manifest)?;
    // A standalone workspace may not have a lock file yet; generate it so
    // the duplicate check and the build can run --locked.
    if lock_path(&manifest).is_err() {
        run_command(
            Command::new("cargo")
                .args(["generate-lockfile", "--manifest-path"])
                .arg(&manifest),
        )?;
    }
    check_shared_duplicates(&manifest)?;

    // The packer owns the environment. An ambient RUSTFLAGS would replace
    // the injected flags entirely (cargo never appends the variable to the
    // config), which fails later with a misleading E0463.
    for var in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
        if let Some(value) = env::var_os(var) {
            if !value.is_empty() {
                return Err(format!(
                    "{var} is set; it would replace the injected SDK flags — unset it and retry"
                ));
            }
        }
    }

    let cargo_toml = read_toml(&manifest)?;
    let package = string(table(&cargo_toml, "package")?, "name")?;
    let lib_name = cargo_toml
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(package)
        .to_owned();
    let target_dir = match &args.target_dir {
        Some(dir) => dir.clone(),
        None => manifest.parent().unwrap().join("target"),
    };
    fs::create_dir_all(&target_dir).map_err(|e| format!("{}: {e}", target_dir.display()))?;
    let target_dir = canonical(&target_dir)?;
    let sdk_file = bundle.lib.join(rutis_dylib_meta::library_file_name(
        "rutis_sdk",
        sdk_target,
    )?);

    let mut command = Command::new("cargo");
    command
        .args(["build", "--release", "--locked", "--manifest-path"])
        .arg(&manifest);
    if !args.features.is_empty() {
        command.args(["--features", &args.features]);
    }
    command
        .env("CARGO_TARGET_DIR", &target_dir)
        .env(
            "RUSTFLAGS",
            format!(
                "--extern rutis_sdk={} -L dependency={}",
                sdk_file.display(),
                bundle.deps.display()
            ),
        )
        .env("RUTIS_SDK_ARTIFACT_SHA256", sdk_hash);
    let output = command.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A private dependency overlapping the SDK closure fails inside
        // rustc; say what to do instead of the raw error.
        if stderr.contains("colliding StableCrateId") || stderr.contains("E0463") {
            return Err(format!(
                "{}\nnote: the plugin graph overlaps the SDK's dependency closure; shared crates (rutis, tokio, tokio-util, serde_json) come from the rutis_sdk re-exports — remove the direct dependency, or the private dependency that pulls one in",
                stderr.trim()
            ));
        }
        return Err(format!("cargo build failed:\n{}", stderr.trim()));
    }
    let library = target_dir
        .join("release")
        .join(rutis_dylib_meta::library_file_name(&lib_name, sdk_target)?);

    let sdk_bytes = fs::read(&sdk_file).map_err(|e| format!("{}: {e}", sdk_file.display()))?;
    if sha256_bytes(&sdk_bytes) != sdk_hash {
        return Err("SDK file differs from sdk.toml".into());
    }
    let std_library = rutis_dylib_meta::std_reference(&sdk_bytes, sdk_target)?;
    finish_pack(args, &library, &manifest, sdk, sdk_target, &std_library)
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

/// Absolute without requiring the path to exist (the output directory is
/// created later). Relative --extern/-L paths resolve against rustc's
/// working directory — the package root — not against the caller's.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| format!("current dir: {e}"))
    }
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

/// `cargo xtask pack-sdk-bundle --bundle-dir <runtime bundle> --output <dir>
/// [--deps-dir <dir>]`: produce an sdk-bundle from a built runtime bundle
/// (design-sdk-build-package §三). The closure is collected by crate name
/// from sdk.toml's packages list (every same-name variant; rustc picks by
/// disambiguator), then shrunk by removing one variant at a time while a
/// probe plugin still builds.
fn pack_sdk_bundle(args: &[String]) -> Result<(), String> {
    let mut bundle_dir: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut deps_dir: Option<PathBuf> = None;
    let mut raw = args.iter();
    while let Some(flag) = raw.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (flag.clone(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| raw.next().cloned())
                .ok_or(format!("{name} needs a value"))
        };
        match name.as_str() {
            "--bundle-dir" => bundle_dir = Some(value()?.into()),
            "--output" => output = Some(value()?.into()),
            "--deps-dir" => deps_dir = Some(value()?.into()),
            _ => return Err(format!("unknown argument: {name}")),
        }
    }
    let bundle_dir = bundle_dir
        .ok_or("usage: cargo xtask pack-sdk-bundle --bundle-dir <runtime bundle> --output <dir>")?;
    let output = output.ok_or("--output is required")?;
    let bundle_dir = canonical(&bundle_dir)?;
    let output = absolute(&output)?;
    if output.exists() {
        return Err(format!("{} already exists", output.display()));
    }

    let repo = canonical(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))?;
    let deps_dir = match deps_dir {
        Some(dir) => canonical(&dir)?,
        None => canonical(&repo.join("target/release/deps"))?,
    };
    let release = read_toml(&bundle_dir.join("sdk.toml"))?;
    let sdk = table(&release, "sdk")?;
    let sdk_target = string(sdk, "target")?;
    let sdk_name = rutis_dylib_meta::library_file_name("rutis_sdk", sdk_target)?;
    let sdk_file = bundle_dir.join(&sdk_name);
    let sdk_hash = sha256_file(&sdk_file)?;

    fs::create_dir_all(output.join("lib"))
        .map_err(|e| format!("{}: {e}", output.join("lib").display()))?;
    fs::create_dir_all(output.join("deps"))
        .map_err(|e| format!("{}: {e}", output.join("deps").display()))?;
    fs::copy(&sdk_file, output.join("lib").join(&sdk_name))
        .map_err(|e| format!("copy SDK: {e}"))?;
    // Windows links against the import library; the runtime bundle leaves it
    // out, the sdk-bundle needs it next to the DLL.
    if sdk_target.contains("windows") {
        let import_lib = repo.join("target/release/rutis_sdk.dll.lib");
        fs::copy(&import_lib, output.join("lib").join("rutis_sdk.dll.lib"))
            .map_err(|e| format!("copy import library: {e}"))?;
    }

    // Closure candidates: every artifact of every package in the SDK subtree.
    let packages: Vec<String> = sdk
        .get("packages")
        .and_then(toml::Value::as_array)
        .ok_or("sdk.toml lists no packages")?
        .iter()
        .filter_map(|item| item.as_str())
        .filter_map(|item| item.split(' ').next())
        .map(|name| name.replace('-', "_"))
        .collect();
    if packages.is_empty() {
        return Err("sdk.toml lists no packages".into());
    }
    let mut variants: Vec<String> = Vec::new();
    for name in &packages {
        let mut found = 0;
        for entry in fs::read_dir(&deps_dir).map_err(|e| format!("{}: {e}", deps_dir.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let file = entry.file_name();
            let file = file.to_string_lossy();
            // lib<crate>-<hash>.<ext>: an rlib, or a proc-macro artifact
            // (.so on Linux, .dylib on macOS, .dll on Windows). The hash
            // part is required: the SDK dylib itself has no suffix.
            let stem = format!("lib{name}-");
            let after = file.strip_prefix(&stem).unwrap_or("");
            let has_hash = after.split_once('.').is_some_and(|(hash, _)| {
                !hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit())
            });
            let is_artifact = has_hash
                && matches!(
                    file.rsplit('.').next(),
                    Some("rlib") | Some("so") | Some("dylib") | Some("dll")
                );
            if is_artifact {
                fs::copy(entry.path(), output.join("deps").join(entry.file_name()))
                    .map_err(|e| format!("copy closure: {e}"))?;
                variants.push(file.into_owned());
                found += 1;
            }
        }
        if found == 0 {
            eprintln!("note: no artifacts for {name} in {}", deps_dir.display());
        }
    }
    variants.sort();

    // Shrink: one variant at a time, while the probe still builds against
    // the bundle. The probe touches every surface the SDK re-exports.
    let report = shrink_closure(&output, &sdk_name, sdk_target, &sdk_hash)?;
    let kept = fs::read_dir(output.join("deps"))
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .count();
    eprintln!(
        "closure: collected {}, shrunk to {} artifacts ({} removed)",
        variants.len(),
        kept,
        report.removed
    );

    for name in ["sdk.toml", "Cargo.lock", "rust-toolchain.toml"] {
        let source = bundle_dir.join(name);
        let source = if source.exists() {
            source
        } else {
            repo.join(name)
        };
        fs::copy(&source, output.join(name)).map_err(|e| format!("copy {name}: {e}"))?;
    }
    write_cargo_config(&output, &sdk_name)?;
    write_guide(&output, &sdk_name)?;

    // bundle.toml lists every file except itself and the editable
    // cargo-config.toml template.
    let q = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let mut content = String::from("format_version = 1\n\n[sdk]\n");
    for key in ["version", "id", "artifact_sha256"] {
        content.push_str(&format!("{key} = {}\n", q(string(sdk, key)?)));
    }
    content.push_str("\n[files]\n");
    let mut files: Vec<(String, String)> = Vec::new();
    for dir in ["deps", "lib"] {
        for entry in fs::read_dir(output.join(dir)).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let rel = format!("{}/{}", dir, entry.file_name().to_string_lossy());
            files.push((rel, sha256_file(&entry.path())?));
        }
    }
    for name in ["sdk.toml", "Cargo.lock", "rust-toolchain.toml", "GUIDE.md"] {
        files.push((name.to_owned(), sha256_file(&output.join(name))?));
    }
    files.sort();
    for (rel, hash) in &files {
        content.push_str(&format!("{} = {}\n", q(rel), q(hash)));
    }
    fs::write(output.join("bundle.toml"), content).map_err(|e| e.to_string())?;
    println!("{}", output.display());
    Ok(())
}

struct ShrinkReport {
    removed: usize,
}

/// Remove one closure variant at a time while a probe plugin still builds;
/// a variant whose removal breaks the build is the one rustc selected.
/// Cross builds skip shrinking: the probe would not link on this host.
fn shrink_closure(
    bundle: &Path,
    sdk_name: &str,
    sdk_target: &str,
    sdk_hash: &str,
) -> Result<ShrinkReport, String> {
    let probe_root = bundle
        .parent()
        .ok_or("bundle has no parent")?
        .join("sdk-bundle-probe");
    let probe = probe_root.join("plugin");
    fs::create_dir_all(probe.join("src"))
        .map_err(|e| format!("{}: {e}", probe.join("src").display()))?;
    fs::write(
        probe.join("Cargo.toml"),
        "[package]\nname = \"sdk-bundle-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\ncrate-type = [\"dylib\"]\ntest = false\ndoctest = false\n\n# The probe lives under the repo's target/; keep it out of the workspace.\n[workspace]\n",
    )
    .map_err(|e| e.to_string())?;
    let repo = canonical(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))?;
    if let Ok(toolchain) = fs::read_to_string(repo.join("rust-toolchain.toml")) {
        fs::write(probe.join("rust-toolchain.toml"), toolchain).map_err(|e| e.to_string())?;
    }
    fs::write(probe.join("src/lib.rs"), PROBE_SOURCE).map_err(|e| e.to_string())?;

    let deps = bundle.join("deps");
    let target_dir = probe_root.join("target");
    let build = |verbose: bool| -> Result<bool, String> {
        // Cargo does not track -L contents; force the plugin crate to
        // recompile each round by bumping its source mtime.
        let lib = probe.join("src/lib.rs");
        if let Ok(file) = fs::File::options().write(true).open(&lib) {
            file.set_modified(std::time::SystemTime::now()).ok();
        }
        let mut command = Command::new("cargo");
        command
            .args(["build", "--release", "--manifest-path"])
            .arg(probe.join("Cargo.toml"));
        command
            .env("CARGO_TARGET_DIR", &target_dir)
            .env(
                "RUSTFLAGS",
                format!(
                    "--extern rutis_sdk={} -L dependency={}",
                    bundle.join("lib").join(sdk_name).display(),
                    deps.display()
                ),
            )
            .env("RUTIS_SDK_ARTIFACT_SHA256", sdk_hash);
        let out = command.output().map_err(|e| e.to_string())?;
        if !out.status.success() && verbose {
            eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(out.status.success())
    };
    if !build(true)? {
        return Err(
            "the probe plugin does not build against the bundle; the closure is incomplete".into(),
        );
    }
    let host = command_output(Command::new("rustc").arg("-vV"))?
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("rustc -vV printed no host")?
        .to_owned();
    if host != sdk_target {
        return Ok(ShrinkReport { removed: 0 });
    }

    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in fs::read_dir(&deps).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let file = entry.file_name().into_string().unwrap_or_default();
        let prefix = file.split('-').next().unwrap_or_default().to_owned();
        groups.entry(prefix).or_default().push(file);
    }
    let mut removed = 0;
    for (_prefix, files) in groups {
        if files.len() == 1 {
            continue;
        }
        for file in files {
            let path = deps.join(&file);
            let bak = deps.join(format!("{file}.bak"));
            fs::rename(&path, &bak).map_err(|e| e.to_string())?;
            if build(false)? {
                fs::remove_file(&bak).map_err(|e| e.to_string())?;
                removed += 1;
            } else {
                fs::rename(&bak, &path).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(ShrinkReport { removed })
}

/// Touches every shared crate the SDK re-exports, so that shrinking keeps
/// the full metadata chain: rutis types, tokio runtime, serde_json values.
/// tokio-util has no re-export; it stays in the closure through the rutis
/// types the probe instantiates.
const PROBE_SOURCE: &str = r#"
use rutis_sdk::rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin, PluginFactory, Snapshot,
};
use rutis_sdk::{ConfigValue, serde_json, tokio};

struct ProbeFactory;
struct Probe;

impl PluginFactory<ConfigValue> for ProbeFactory {
    fn name(&self) -> &str {
        "sdk-bundle-probe"
    }
    fn build(&self, _: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Probe))
    }
}

impl Plugin for Probe {
    fn name(&self) -> &str {
        "sdk-bundle-probe"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let _config = serde_json::json!({ "probe": true });
            tokio::spawn(async {})
                .await
                .map_err(|e| CordisError::PluginFailed(e.into()))?;
            ctx.provide("probe".to_string())?;
            ctx.provide(Snapshot {
                generation: 1,
                state: FiberState::Active,
                error: None,
            })?;
            Ok(Effect::Done)
        })
    }
}

rutis_sdk::export_plugin! { id: "sdk-bundle-probe", factory: ProbeFactory }
"#;

fn write_cargo_config(bundle: &Path, sdk_name: &str) -> Result<(), String> {
    let mut content = format!(
        "# Copy this file to the plugin workspace as .cargo/config.toml and\n\
         # adjust the two paths below to where you unpacked the bundle.\n\
         # The RUSTFLAGS environment variable fully replaces these flags:\n\
         # if it is set, the injection silently stops working. Unset it.\n\
         [build]\nrustflags = [\n  \"--extern\", \"rutis_sdk={0}\",\n  \"-L\", \"dependency={1}\",\n]\n",
        bundle.join("lib").join(sdk_name).display(),
        bundle.join("deps").display()
    );
    content.push_str(
        "\n# macOS: keep the plugin's deployment target equal to the SDK's.\n\
         # [env]\n# MACOSX_DEPLOYMENT_TARGET = \"13.0\"\n",
    );
    fs::write(bundle.join("cargo-config.toml"), content).map_err(|e| e.to_string())
}

fn write_guide(bundle: &Path, sdk_name: &str) -> Result<(), String> {
    let content = format!(
        "# Building a rutis dylib plugin with this SDK bundle\n\
         \n\
         Requirements: the Rust toolchain pinned by `rust-toolchain.toml`\n\
         (copy it into your plugin workspace), and a plugin crate with\n\
         `crate-type = [\"dylib\"]` and **no** `rutis-sdk`, `rutis`, `tokio`,\n\
         `tokio-util` or `serde_json` dependency. Shared crates come from the\n\
         SDK re-exports: `rutis_sdk::rutis`, `rutis_sdk::tokio`,\n\
         `rutis_sdk::serde_json`.\n\
         \n\
         For editor and `cargo check` support, copy `cargo-config.toml` to\n\
         your workspace as `.cargo/config.toml` and fix the paths. The\n\
         `--extern`-injected crate is not in cargo's crate graph, so\n\
         rust-analyzer may not resolve `rutis_sdk::` paths; its flycheck\n\
         still runs `cargo check`.\n\
         \n\
         Package the plugin (from the rutis repository):\n\
         \n\
         ```sh\n\
         cargo xtask pack-plugin --bundle <this directory> \\\n\
           --manifest-path <plugin>/Cargo.toml --output <plugin-dist>\n\
         ```\n\
         \n\
         The packer verifies the whole bundle against `bundle.toml`, checks\n\
         the rustc version, injects the SDK, and rejects direct dependencies\n\
         on shared crates before building.\n\
         \n\
         Rules (violations fail at pack time or at load time):\n\
         \n\
         - no custom global allocator, `panic = \"unwind\"`;\n\
         - no `RUNPATH`/`RPATH`; native library dependencies must match\n\
           `plugin.toml`'s `native_deps`;\n\
         - types crossing the plugin boundary must come from the SDK;\n\
         - an indirectly linked crate with the same name as one in the SDK\n\
           does not share runtime state with the host (e.g. its own `tokio`\n\
           never sees the host runtime) — use the re-exports;\n\
         - a private dependency that pulls a shared crate into your graph\n\
           collides with the SDK closure and fails to compile: remove it or\n\
           align it with the SDK re-exports.\n\
         \n\
         When the SDK is upgraded, rebuild every plugin with the new bundle;\n\
         old plugins are rejected by the host (L1/L2 identity mismatch).\n\
         The runtime bundle this sdk-bundle belongs to ships `{sdk_name}`\n\
         with the identity recorded in `sdk.toml`.\n"
    );
    fs::write(bundle.join("GUIDE.md"), content).map_err(|e| e.to_string())
}
