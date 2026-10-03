use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Link arguments belong to this artifact rather than to RUSTFLAGS: a
    // plugin build must reproduce the SDK byte for byte, and RUSTFLAGS would
    // also reach the plugin itself, which must not carry a run path.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
    // Cargo does not expose the invoking workspace lockfile to a dependency's
    // build script. Release tooling passes the actual resolver lock explicitly;
    // ordinary downstream builds can still compile without one.
    let lock = env::var_os("RUTIS_SDK_LOCKFILE").map(PathBuf::from);
    if let Some(lock) = &lock {
        println!("cargo:rerun-if-changed={}", lock.display());
    }
    for key in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_CFG_PANIC",
        "CARGO_CFG_DEBUG_ASSERTIONS",
        "OPT_LEVEL",
        "PROFILE",
        "TARGET",
        "RUSTC",
        "RUTIS_SDK_LOCKFILE",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("rustc -vV failed");
    assert!(version.status.success(), "rustc -vV failed");
    let rustc_full = String::from_utf8(version.stdout).unwrap();
    let rustc_short = rustc_full.lines().next().unwrap().to_string();
    let target = env::var("TARGET").unwrap();
    let flags = canonical_rustflags();
    let mut features = env::vars()
        .filter_map(|(key, value)| {
            key.strip_prefix("CARGO_FEATURE_")
                .map(|name| (name.to_owned(), value))
        })
        .collect::<Vec<_>>();
    features.sort();
    let packages = lock.as_ref().map(locked_sdk_tree).unwrap_or_default();
    let mut digest = Sha256::new();
    for field in [
        env!("CARGO_PKG_VERSION").to_string(),
        rustc_full,
        target.clone(),
        env::var("CARGO_CFG_PANIC").unwrap_or_default(),
        env::var("CARGO_CFG_DEBUG_ASSERTIONS").unwrap_or_default(),
        env::var("PROFILE").unwrap_or_default(),
        flags.join("\n"),
        packages.join("\n"),
    ] {
        digest.update(field.len().to_le_bytes());
        digest.update(field.as_bytes());
    }
    for (name, value) in features {
        digest.update(name.len().to_le_bytes());
        digest.update(name.as_bytes());
        digest.update(value.len().to_le_bytes());
        digest.update(value.as_bytes());
    }
    let id = format!("{:x}", digest.finalize());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("identity.rs");
    let package_literals = packages
        .iter()
        .map(|package| format!("{package:?}"))
        .collect::<Vec<_>>()
        .join(",");
    fs::write(out, format!("pub const SDK_ID: &str = {id:?};\npub const SDK_RUSTC_VERSION: &str = {rustc_short:?};\npub const SDK_TARGET: &str = {target:?};\npub const SDK_PACKAGES: &[&str] = &[{package_literals}];\n")).unwrap();
}

fn canonical_rustflags() -> Vec<String> {
    let raw = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let args: Vec<&str> = raw.split('\u{1f}').filter(|s| !s.is_empty()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if matches!(
            arg,
            "-A" | "-W"
                | "-D"
                | "-F"
                | "--allow"
                | "--warn"
                | "--deny"
                | "--forbid"
                | "--cap-lints"
                | "--force-warn"
        ) {
            i += 2;
            assert!(i <= args.len(), "missing lint value for {arg}");
            continue;
        }
        if [
            "-A",
            "-W",
            "-D",
            "-F",
            "--allow=",
            "--warn=",
            "--deny=",
            "--forbid=",
            "--cap-lints=",
            "--force-warn=",
        ]
        .iter()
        .any(|prefix| arg.starts_with(prefix))
        {
            i += 1;
            continue;
        }
        let (kind, value) = if matches!(
            arg,
            "-C" | "--cfg" | "-Z" | "-L" | "-l" | "--remap-path-prefix"
        ) {
            i += 1;
            (
                arg,
                *args
                    .get(i)
                    .unwrap_or_else(|| panic!("missing value for {arg}")),
            )
        } else if let Some(value) = arg.strip_prefix("-C") {
            ("-C", value)
        } else if let Some(value) = arg.strip_prefix("-Z") {
            ("-Z", value)
        } else if let Some(value) = arg.strip_prefix("--cfg=") {
            ("--cfg", value)
        } else if let Some(value) = arg.strip_prefix("--remap-path-prefix=") {
            ("--remap-path-prefix", value)
        } else if let Some(value) = arg.strip_prefix("-L") {
            ("-L", value)
        } else if let Some(value) = arg.strip_prefix("-l") {
            ("-l", value)
        } else {
            panic!("unclassified RUSTFLAGS argument: {arg}");
        };
        match kind {
            "--cfg" | "-Z" => out.push(format!("{kind}{value}")),
            "-C" => {
                let name = value.split('=').next().unwrap();
                match name {
                    "target-cpu" | "target-feature" | "panic" | "debug-assertions"
                    | "overflow-checks" => out.push(format!(
                        "-C{name}={}",
                        value.split_once('=').map(|(_, v)| v).unwrap_or("")
                    )),
                    "link-arg" | "link-args" | "linker" | "debuginfo" | "opt-level"
                    | "incremental" | "codegen-units" | "metadata" | "extra-filename"
                    | "embed-bitcode" | "strip" => {}
                    _ => panic!("unclassified RUSTFLAGS codegen option: {name}"),
                }
            }
            "-L" | "-l" | "--remap-path-prefix" => {}
            _ => unreachable!(),
        }
        i += 1;
    }
    out.sort();
    out
}

#[derive(Default)]
struct LockedPackage {
    name: String,
    version: String,
    source: String,
    checksum: String,
    deps: Vec<String>,
}

fn locked_sdk_tree(path: &PathBuf) -> Vec<String> {
    let lock = fs::read_to_string(path).expect("read Cargo.lock");
    let mut by_name: BTreeMap<String, Vec<LockedPackage>> = BTreeMap::new();
    let mut current: Option<LockedPackage> = None;
    let mut in_deps = false;
    for line in lock.lines().map(str::trim) {
        if line == "[[package]]" {
            if let Some(package) = current.take() {
                by_name
                    .entry(package.name.clone())
                    .or_default()
                    .push(package);
            }
            current = Some(LockedPackage::default());
            in_deps = false;
        } else if let Some(package) = current.as_mut() {
            if line == "dependencies = [" {
                in_deps = true;
            } else if in_deps {
                if line == "]" {
                    in_deps = false;
                } else if let Some(value) = line.strip_prefix('"').and_then(|s| s.split('"').next())
                {
                    package
                        .deps
                        .push(value.split_whitespace().next().unwrap().to_string());
                }
            } else {
                let value = |key: &str| {
                    line.strip_prefix(key)
                        .and_then(|s| s.strip_prefix(" = \""))
                        .and_then(|s| s.strip_suffix('"'))
                };
                if let Some(v) = value("name") {
                    package.name = v.into();
                }
                if let Some(v) = value("version") {
                    package.version = v.into();
                }
                if let Some(v) = value("source") {
                    package.source = v.into();
                }
                if let Some(v) = value("checksum") {
                    package.checksum = v.into();
                }
            }
        }
    }
    if let Some(package) = current {
        by_name
            .entry(package.name.clone())
            .or_default()
            .push(package);
    }
    let mut pending = vec!["rutis-sdk".to_string()];
    let mut seen = BTreeSet::new();
    let mut lines = Vec::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        for package in by_name
            .get(&name)
            .unwrap_or_else(|| panic!("{name} missing from Cargo.lock"))
        {
            lines.push(format!(
                "{} {} {} {}",
                package.name, package.version, package.source, package.checksum
            ));
            pending.extend(package.deps.iter().cloned());
        }
    }
    lines.sort();
    lines
}
