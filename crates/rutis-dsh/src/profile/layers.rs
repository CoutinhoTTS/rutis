//! dsh's patch layers for a profile, as dsh-app-boot's
//! `loadProfileDirectory` + `readProfilePatches` compose them:
//!
//! 1. each bundle of `dsh.profile.bundles`, its `dsh.bundle.patch` files in
//!    order — an unreadable or incompatible bundle is skipped, with a reason;
//! 2. the user layer `<profile>/cordis.patch.yml` (optional; editable);
//! 3. the home layer `$DSH_HOME/cordis.patch.yml` (optional);
//! 4. each `--patch` overlay (required);
//! 5. the telemetry switch: `DSH_TELEMETRY_DISABLED` set and a
//!    `session-telemetry-otel` row composed → that row disabled.
//!
//! Inserted rows whose `name` is a path are anchored to the patch file, as
//! `anchorInsertedPluginNames` does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rutis_loader::{apply_patches, Editable, Layer, Patch, Version};
use semver::Version as SemVer;
use serde_json::{json, Value};

use super::{npm_semver, paths, yaml};

pub const USER_LAYER: &str = "user";
pub const HOME_LAYER: &str = "home";
pub const TELEMETRY_LAYER: &str = "telemetry";
pub const PROFILE_PATCH_FILENAME: &str = "cordis.patch.yml";
const TELEMETRY_ROW_ID: &str = "session-telemetry-otel";

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// What dsh calls the profile context: where things are and what the
/// launcher adds.
#[derive(Debug, Clone)]
pub struct ProfileContext {
    pub name: String,
    pub dir: PathBuf,
    /// `package.json` of the installation the bundles resolve from first.
    pub install_anchor: PathBuf,
    /// The dsh home (`$DSH_HOME`, else `~/.dsh`).
    pub home: PathBuf,
    /// `--patch` overlay files, in order.
    pub overlays: Vec<PathBuf>,
    /// The raw `DSH_TELEMETRY_DISABLED`.
    pub telemetry_disabled: Option<String>,
    /// The dsh runtime version bundles are checked against; `None` reads it
    /// from `@deepseek-ai/dsh-app-boot` next to the install anchor.
    pub runtime_version: Option<String>,
}

impl ProfileContext {
    /// `$DSH_HOME/profiles/<name>`, with the live environment.
    pub fn named(name: &str, install_anchor: impl Into<PathBuf>) -> Self {
        let home = paths::dsh_home();
        Self {
            name: name.to_owned(),
            dir: home.join("profiles").join(name),
            install_anchor: install_anchor.into(),
            home,
            overlays: Vec::new(),
            telemetry_disabled: std::env::var("DSH_TELEMETRY_DISABLED").ok(),
            runtime_version: None,
        }
    }

    pub fn user_layer_path(&self) -> PathBuf {
        self.dir.join(PROFILE_PATCH_FILENAME)
    }
}

#[derive(Debug, Clone)]
pub struct SkippedBundle {
    pub package: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub layers: Vec<Layer>,
    /// The user layer, at the version just read.
    pub editable: Editable,
    pub skipped: Vec<SkippedBundle>,
    /// Every file read; watch these to reload.
    pub files: Vec<PathBuf>,
}

/// FNV-1a over the bytes; the version of a stored layer.
pub fn version_of(bytes: Option<&[u8]>) -> Version {
    match bytes {
        None => Version("absent".into()),
        Some(bytes) => {
            let mut hash: u64 = 0xcbf29ce484222325;
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            Version(format!("{hash:016x}"))
        }
    }
}

fn read(path: &Path) -> Result<String, ProfileError> {
    std::fs::read_to_string(path).map_err(|source| ProfileError::Read {
        path: path.to_owned(),
        source,
    })
}

fn invalid(path: &Path, message: impl Into<String>) -> ProfileError {
    ProfileError::Invalid {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn read_json(path: &Path) -> Result<Value, ProfileError> {
    serde_json::from_str(&read(path)?).map_err(|e| invalid(path, e.to_string()))
}

/// `parsePatchList`: a top-level array of mappings, names anchored.
pub fn parse_patch_list(path: &Path, source: &str) -> Result<Vec<Patch>, ProfileError> {
    let value = yaml::parse(source).map_err(|e| invalid(path, e.to_string()))?;
    let Value::Array(items) = value else {
        return Err(invalid(
            path,
            "must be a top-level YAML array of loader patch entries",
        ));
    };
    let mut patches = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        if !item.is_object() {
            return Err(invalid(
                path,
                format!(
                    "entry {} must be a mapping (a loader patch entry)",
                    index + 1
                ),
            ));
        }
        let mut patch: Patch = serde_json::from_value(item)
            .map_err(|e| invalid(path, format!("entry {}: {e}", index + 1)))?;
        anchor_patch(&mut patch, path);
        patches.push(patch);
    }
    Ok(patches)
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn is_path_name(name: &str) -> bool {
    name.starts_with('/') || name.starts_with("./") || name.starts_with("../")
}

fn file_url(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'@'
            | b'+'
            | b','
            | b'='
            | b':'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b';'
            | b'['
            | b']'
            | b'|'
            | b'^' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn base_dir(file: &Path) -> String {
    paths::absolute(file.parent().unwrap_or(Path::new("/")))
        .to_string_lossy()
        .into_owned()
}

fn anchor_rows(rows: &mut [Value], base: &str) {
    for row in rows {
        if let Some(Value::String(name)) = row.get_mut("name") {
            if is_path_name(name) {
                *name = file_url(&paths::resolve(base, &[name.as_str()]));
            }
        }
        if row.get("group").is_some_and(js_truthy) {
            if let Some(Value::Array(children)) = row.get_mut("config") {
                anchor_rows(children, base);
            }
        }
    }
}

/// Make the path names of rows a patch inserts absolute `file:` URLs.
pub fn anchor_patch(patch: &mut Patch, file: &Path) {
    if let Some(rows) = &mut patch.insert {
        anchor_rows(rows, &base_dir(file));
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn relative(from: &str, to: &str) -> String {
    let from: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

fn unanchor_rows(rows: &mut [Value], base: &str) {
    for row in rows {
        if let Some(Value::String(name)) = row.get_mut("name") {
            if let Some(path) = name.strip_prefix("file://") {
                *name = relative(base, &percent_decode(path));
            }
        }
        if row.get("group").is_some_and(js_truthy) {
            if let Some(Value::Array(children)) = row.get_mut("config") {
                unanchor_rows(children, base);
            }
        }
    }
}

/// The inverse of [`anchor_patch`] for writing a patch back to `file`:
/// `file:` names become paths relative to the file's directory.
pub fn unanchor_patch(patch: &mut Patch, file: &Path) {
    if let Some(rows) = &mut patch.insert {
        unanchor_rows(rows, &base_dir(file));
    }
}

/// Node's lookup of `package` from `anchor` (a file): `node_modules` in
/// each ancestor directory.
pub fn package_dir(anchor: &Path, package: &str) -> Option<PathBuf> {
    let start = paths::absolute(anchor.parent()?);
    start
        .ancestors()
        .filter(|dir| dir.file_name().is_none_or(|n| n != "node_modules"))
        .map(|dir| dir.join("node_modules").join(package))
        .find(|candidate| candidate.join("package.json").exists())
}

fn runtime_version(context: &ProfileContext) -> Option<String> {
    if let Some(version) = &context.runtime_version {
        return Some(version.clone());
    }
    let dir = package_dir(&context.install_anchor, "@deepseek-ai/dsh-app-boot")?;
    let manifest = read_json(&dir.join("package.json")).ok()?;
    manifest["version"].as_str().map(str::to_owned)
}

/// Profile `compatibility.json`: `name@version` → allowed runtime versions.
/// Unreadable records authorize nothing, like dsh.
fn exemptions(dir: &Path) -> BTreeMap<String, Vec<String>> {
    let Ok(Value::Object(map)) = read_json(&dir.join("compatibility.json")) else {
        return BTreeMap::new();
    };
    map.into_iter()
        .filter_map(|(key, versions)| {
            let versions: Vec<String> = versions
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<_>>()?;
            Some((key, versions))
        })
        .collect()
}

/// `evaluatePluginCompatibility`: `Some(reason)` when a dsh peer does not
/// accept the runtime and no exemption covers it.
fn incompatibility(
    manifest: &Value,
    runtime: &str,
    exemptions: &BTreeMap<String, Vec<String>>,
) -> Result<Option<String>, String> {
    let Some(peers) = manifest.get("peerDependencies") else {
        return Ok(None);
    };
    let peers = peers
        .as_object()
        .ok_or("Plugin manifest peerDependencies must be an object")?;
    let version =
        SemVer::parse(runtime).map_err(|_| format!("Invalid dsh runtime version: {runtime:?}"))?;
    let mut bad = BTreeMap::new();
    for (name, range) in peers {
        let range = range.as_str().ok_or_else(|| {
            format!("Plugin manifest peerDependencies[{name:?}] must be a string")
        })?;
        if name != "@deepseek-ai/dsh" && !name.starts_with("@deepseek-ai/dsh-") {
            continue;
        }
        let requirement = if matches!(range, "workspace:^" | "workspace:~" | "workspace:*") {
            runtime
        } else {
            range
        };
        if requirement.trim().is_empty() || !npm_semver::satisfies(&version, requirement) {
            bad.insert(name.clone(), range.to_owned());
        }
    }
    if bad.is_empty() {
        return Ok(None);
    }
    let name = manifest["name"].as_str().filter(|s| !s.trim().is_empty());
    let pkg_version = manifest["version"]
        .as_str()
        .filter(|s| !s.trim().is_empty());
    let (Some(name), Some(pkg_version)) = (name, pkg_version) else {
        return Err("Plugin manifest name and version must be non-empty strings when dsh peers are incompatible".into());
    };
    let key = format!("{name}@{pkg_version}");
    if exemptions
        .get(&key)
        .is_some_and(|v| v.iter().any(|r| r == runtime))
    {
        return Ok(None);
    }
    Ok(Some(format!(
        "Plugin {key} is incompatible with dsh {runtime}: peerDependencies {}",
        serde_json::to_string(&bad).unwrap_or_default()
    )))
}

fn bundle_layer(
    context: &ProfileContext,
    package: &str,
    runtime: Option<&str>,
    exemptions: &BTreeMap<String, Vec<String>>,
    files: &mut Vec<PathBuf>,
) -> Result<Layer, String> {
    let profile_anchor = context.dir.join("package.json");
    let dir = package_dir(&context.install_anchor, package)
        .or_else(|| package_dir(&profile_anchor, package))
        .ok_or_else(|| {
            format!(
                "cannot resolve profile bundle {package:?} from the dsh installation or {}",
                context.dir.display()
            )
        })?;
    let manifest_path = dir.join("package.json");
    let manifest = read_json(&manifest_path).map_err(|e| e.to_string())?;
    let bundle = manifest.pointer("/dsh/bundle").ok_or_else(|| {
        format!("profile bundle {package:?} declares no dsh.bundle in its package.json")
    })?;
    if let Some(runtime) = runtime {
        if let Some(reason) = incompatibility(&manifest, runtime, exemptions)? {
            return Err(reason);
        }
    }
    let declared: Vec<String> = match bundle.get("patch") {
        Some(Value::String(file)) => vec![file.clone()],
        Some(Value::Array(list)) if list.iter().all(Value::is_string) => list
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect(),
        _ => return Err("dsh.bundle.patch must be a file path or a list of file paths".into()),
    };
    let mut patches = Vec::new();
    for file in declared {
        let path = dir.join(file);
        let source = read(&path).map_err(|e| e.to_string())?;
        patches.extend(parse_patch_list(&path, &source).map_err(|e| e.to_string())?);
        files.push(path);
    }
    Ok(Layer::new(package, patches))
}

/// Read every layer of a profile.
pub fn load(context: &ProfileContext) -> Result<Profile, ProfileError> {
    let manifest_path = context.dir.join("package.json");
    let manifest = read_json(&manifest_path)?;
    if !manifest.is_object() {
        return Err(invalid(&manifest_path, "must hold a JSON object"));
    }
    let bundles: Vec<String> = match manifest.pointer("/dsh/profile/bundles") {
        None => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    invalid(
                        &manifest_path,
                        "dsh.profile.bundles must list package names",
                    )
                })
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(invalid(
                &manifest_path,
                "dsh.profile.bundles must be a list",
            ))
        }
    };
    let mut files = vec![manifest_path.clone()];
    let mut layers = Vec::new();
    let mut skipped = Vec::new();
    let runtime = if bundles.is_empty() {
        None
    } else {
        runtime_version(context)
    };
    let exemptions = if bundles.is_empty() {
        BTreeMap::new()
    } else {
        exemptions(&context.dir)
    };
    for package in bundles {
        match bundle_layer(
            context,
            &package,
            runtime.as_deref(),
            &exemptions,
            &mut files,
        ) {
            Ok(layer) => layers.push(layer),
            Err(reason) => skipped.push(SkippedBundle { package, reason }),
        }
    }

    let user_path = context.user_layer_path();
    files.push(user_path.clone());
    let (user, version) = match std::fs::read(&user_path) {
        Ok(bytes) => {
            let source = String::from_utf8_lossy(&bytes);
            (
                parse_patch_list(&user_path, &source)?,
                version_of(Some(&bytes)),
            )
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), version_of(None)),
        Err(source) => {
            return Err(ProfileError::Read {
                path: user_path,
                source,
            })
        }
    };
    layers.push(Layer::new(USER_LAYER, user));

    let home_path = context.home.join(PROFILE_PATCH_FILENAME);
    files.push(home_path.clone());
    let home = match std::fs::read_to_string(&home_path) {
        Ok(source) => parse_patch_list(&home_path, &source)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(ProfileError::Read {
                path: home_path,
                source,
            })
        }
    };
    layers.push(Layer::new(HOME_LAYER, home));

    for overlay in &context.overlays {
        let path = paths::absolute(overlay);
        let source = read(&path)?;
        layers.push(Layer::new(
            format!("patch:{}", path.display()),
            parse_patch_list(&path, &source)?,
        ));
        files.push(path);
    }

    let disabled = context.telemetry_disabled.as_deref().unwrap_or("");
    let has_row = || {
        apply_patches(&layers)
            .flat
            .iter()
            .any(|row| row.id.as_deref() == Some(TELEMETRY_ROW_ID))
    };
    let telemetry = if !disabled.is_empty() && has_row() {
        vec![serde_json::from_value(json!({ "id": TELEMETRY_ROW_ID, "disabled": true })).unwrap()]
    } else {
        Vec::new()
    };
    layers.push(Layer::new(TELEMETRY_LAYER, telemetry));

    Ok(Profile {
        layers,
        editable: Editable::new(USER_LAYER, version),
        skipped,
        files,
    })
}
