//! Optional in-process loader for trusted, first-party Rust dylib plugins.
//! A verified, immutable bundle launcher must run the host before this API is
//! used: a mismatched SDK can execute during Rust startup, before `Loader::new`.

#[cfg(not(target_os = "linux"))]
compile_error!("rutis-dylib currently supports Linux only; use the static host on this target");
#[cfg(panic = "abort")]
compile_error!("rutis-dylib requires panic = unwind");

use rutis::{CordisError, Ctx, FiberView, Plugin, PluginFactory, TypeKey};
use rutis_sdk::{ConfigValue, PluginMeta, BOOT_MAGIC, BOOT_SIZE, SDK_ID};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, thiserror::Error)]
#[error("plugin {plugin} {version}: {step}: {reason}")]
pub struct LoadError {
    pub plugin: String,
    pub version: String,
    pub step: &'static str,
    pub reason: String,
}

fn error(plugin: &str, version: &str, step: &'static str, reason: impl Into<String>) -> LoadError {
    LoadError {
        plugin: plugin.into(),
        version: version.into(),
        step,
        reason: reason.into(),
    }
}

#[derive(Deserialize)]
struct Manifest {
    plugin: PluginManifest,
    sdk: SdkManifest,
    #[serde(default)]
    interfaces: HashMap<String, String>,
    build: BuildManifest,
}

#[derive(Deserialize)]
struct PluginManifest {
    id: String,
    version: String,
    library: String,
    library_sha256: String,
}
#[derive(Deserialize)]
struct SdkManifest {
    version: String,
    id: String,
    artifact_sha256: String,
}
#[derive(Deserialize)]
struct BuildManifest {
    target: String,
    rustc: String,
    lock_sha256: String,
}

#[derive(Debug, Clone)]
pub struct ModuleDiagnostic {
    pub id: String,
    pub version: String,
    pub library_sha256: String,
    pub mapped_bytes: u64,
    pub loaded_at: SystemTime,
    /// Includes the loader's own reference, plus caller and fiber references.
    pub strong_references: usize,
    pub usable: bool,
}

struct Retained {
    id: String,
    version: String,
    hash: String,
    bytes: u64,
    loaded_at: SystemTime,
    module: Option<Arc<Module>>,
    _handle: NonNull<libc::c_void>,
}

// dlopen handles are process-global, remain mapped forever, and may be used on
// any thread. We never call dlclose, including when a rejected load is dropped.
unsafe impl Send for Retained {}

/// Host-side loader. Construct it after the independent bundle launcher has
/// verified the host, SDK and dynamic libstd files, and before creating root.
pub struct Loader {
    sdk_artifact_sha256: String,
    cache: PathBuf,
    interfaces: HashMap<String, semver::Version>,
    max_versions: usize,
    retained: Mutex<Vec<Retained>>,
}

impl Loader {
    pub fn new(
        sdk_artifact_sha256: impl Into<String>,
        cache: impl Into<PathBuf>,
        interfaces: HashMap<String, semver::Version>,
        max_versions: usize,
    ) -> Result<Self, LoadError> {
        let sdk_artifact_sha256 = sdk_artifact_sha256.into();
        if !rutis_sdk::is_sha256(&sdk_artifact_sha256) {
            return Err(error(
                "<host>",
                "",
                "startup",
                "invalid embedded SDK artifact SHA-256",
            ));
        }
        let mut buf = [0_u8; 128];
        let len = unsafe { rutis_sdk::rutis_sdk_boot_id(buf.as_mut_ptr(), buf.len()) };
        if len != SDK_ID.len() || &buf[..len] != SDK_ID.as_bytes() {
            return Err(error(
                "<host>",
                "",
                "startup",
                "loaded SDK identity differs from host",
            ));
        }
        let actual = loaded_sdk_path().map_err(|e| error("<host>", "", "startup", e))?;
        let actual_hash = sha_file(&actual).map_err(|e| error("<host>", "", "startup", e))?;
        if actual_hash != sdk_artifact_sha256 {
            return Err(error("<host>", "", "startup", format!("loaded SDK artifact mismatch at {}: expected {sdk_artifact_sha256}, got {actual_hash}", actual.display())));
        }
        Ok(Self {
            sdk_artifact_sha256,
            cache: cache.into(),
            interfaces,
            max_versions,
            retained: Mutex::new(Vec::new()),
        })
    }

    /// Loads trusted code. The caller is responsible for accepting that ELF
    /// initializers run inside the host as soon as `dlopen` is called.
    pub unsafe fn load(&self, dir: impl AsRef<Path>) -> Result<Arc<Module>, LoadError> {
        let manifest_path = dir.as_ref().join("plugin.toml");
        let source = fs::read_to_string(&manifest_path)
            .map_err(|e| error("<unknown>", "", "manifest", e.to_string()))?;
        let manifest: Manifest = toml::from_str(&source)
            .map_err(|e| error("<unknown>", "", "manifest", e.to_string()))?;
        let id = &manifest.plugin.id;
        let version = &manifest.plugin.version;
        let fail = |step, reason| error(id, version, step, reason);
        if id.is_empty() || version.is_empty() {
            return Err(fail(
                "manifest",
                "plugin id and version must be nonempty".into(),
            ));
        }
        if manifest.build.target != rutis_sdk::SDK_TARGET {
            return Err(fail(
                "manifest",
                format!("target mismatch: {}", manifest.build.target),
            ));
        }
        if manifest.build.rustc != rutis_sdk::SDK_RUSTC_VERSION {
            return Err(fail(
                "manifest",
                format!("rustc mismatch: {}", manifest.build.rustc),
            ));
        }
        if manifest.sdk.id != SDK_ID || manifest.sdk.artifact_sha256 != self.sdk_artifact_sha256 {
            return Err(fail(
                "manifest",
                format!(
                    "SDK mismatch: manifest id={} artifact={}, host id={} artifact={}",
                    manifest.sdk.id, manifest.sdk.artifact_sha256, SDK_ID, self.sdk_artifact_sha256
                ),
            ));
        }
        if manifest.sdk.version != rutis_sdk::SDK_VERSION {
            return Err(fail(
                "manifest",
                format!("SDK version mismatch: {}", manifest.sdk.version),
            ));
        }
        if !rutis_sdk::is_sha256(&manifest.plugin.library_sha256)
            || !rutis_sdk::is_sha256(&manifest.build.lock_sha256)
        {
            return Err(fail("manifest", "invalid SHA-256 field".into()));
        }
        for (name, requirement) in &manifest.interfaces {
            let req = semver::VersionReq::parse(requirement)
                .map_err(|e| fail("interfaces", e.to_string()))?;
            let actual = self
                .interfaces
                .get(name)
                .ok_or_else(|| fail("interfaces", format!("missing interface {name}")))?;
            if !req.matches(actual) {
                return Err(fail(
                    "interfaces",
                    format!("{name} {actual} does not meet {req}"),
                ));
            }
        }
        let library = Path::new(&manifest.plugin.library);
        if library.components().count() != 1 || library.file_name().is_none() {
            return Err(fail("manifest", "library must be a filename".into()));
        }
        let source_library = dir.as_ref().join(library);
        let bytes = fs::read(&source_library).map_err(|e| fail("binary", e.to_string()))?;
        let actual_hash = sha_bytes(&bytes);
        if actual_hash != manifest.plugin.library_sha256 {
            return Err(fail(
                "binary",
                format!("library SHA-256 mismatch: {actual_hash}"),
            ));
        }
        let boot = parse_boot(&bytes).map_err(|e| fail("boot metadata", e))?;
        if boot.sdk_id != SDK_ID
            || boot.sdk_artifact != self.sdk_artifact_sha256
            || boot.sdk_id != manifest.sdk.id
            || boot.sdk_artifact != manifest.sdk.artifact_sha256
            || boot.id != *id
            || boot.version != *version
        {
            return Err(fail(
                "boot metadata",
                "binary identity differs from manifest or host".into(),
            ));
        }
        let mut retained = self.retained.lock().unwrap();
        let existing_slot = retained
            .iter()
            .position(|entry| entry.id == *id && entry.hash == actual_hash);
        let (handle, slot) = if let Some(slot) = existing_slot {
            if let Some(module) = &retained[slot].module {
                return Ok(module.clone());
            }
            // A previous entry/metadata attempt failed after dlopen. Retry on
            // the retained mapping without consuming another version slot.
            (retained[slot]._handle, slot)
        } else {
            if retained.iter().filter(|entry| entry.id == *id).count() >= self.max_versions {
                return Err(fail(
                    "retention",
                    format!(
                        "version limit {} reached; restart the host to reclaim code",
                        self.max_versions
                    ),
                ));
            }
            let cached = self.cache.join(&actual_hash).join(library);
            ensure_cached(&cached, &bytes, &actual_hash).map_err(|e| fail("cache", e))?;
            let handle = open_library(&cached).map_err(|e| fail("dlopen", e))?;
            // Every successful dlopen remains mapped, even if entry fails.
            retained.push(Retained {
                id: id.clone(),
                version: version.clone(),
                hash: actual_hash.clone(),
                bytes: bytes.len() as u64,
                loaded_at: SystemTime::now(),
                module: None,
                _handle: handle,
            });
            (handle, retained.len() - 1)
        };
        let meta_fn: unsafe fn() -> PluginMeta =
            symbol(handle, b"rutis_plugin_meta\0").map_err(|e| fail("metadata", e))?;
        let meta = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| meta_fn()))
            .map_err(|_| fail("metadata", "metadata function panicked".into()))?;
        if meta.sdk_id != SDK_ID
            || meta.sdk_artifact_sha256 != self.sdk_artifact_sha256
            || meta.sdk_id != boot.sdk_id
            || meta.sdk_artifact_sha256 != boot.sdk_artifact
            || meta.id != id
            || meta.version != version
        {
            return Err(fail(
                "metadata",
                "runtime identity differs from boot metadata or manifest".into(),
            ));
        }
        let entry: unsafe fn() -> Result<Box<dyn PluginFactory<ConfigValue>>, CordisError> =
            symbol(handle, b"rutis_plugin_entry\0").map_err(|e| fail("entry", e))?;
        let factory = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry()))
            .map_err(|_| fail("entry", "factory entry panicked".into()))?
            .map_err(|e| fail("entry", e.to_string()))?;
        let (name, injects) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (factory.name().to_string(), factory.injects().to_vec())
        }))
        .map_err(|_| fail("entry", "factory metadata panicked".into()))?;
        // Optional: plugins built before the symbol existed have no schema.
        let schema = match optional_symbol::<unsafe fn() -> Option<String>>(
            handle,
            b"rutis_plugin_config_schema\0",
        ) {
            None => None,
            Some(schema_fn) => {
                let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| schema_fn()))
                    .map_err(|_| fail("schema", "schema function panicked".into()))?;
                match text {
                    None => None,
                    Some(text) => Some(
                        rutis_sdk::serde_json::from_str(&text)
                            .map_err(|e| fail("schema", format!("invalid JSON: {e}")))?,
                    ),
                }
            }
        };
        let module = Arc::new(Module {
            id: id.clone(),
            version: version.clone(),
            library_sha256: actual_hash,
            name,
            injects,
            schema,
            factory,
            _handle: handle,
        });
        retained[slot].module = Some(module.clone());
        Ok(module)
    }

    pub fn diagnostics(&self) -> Vec<ModuleDiagnostic> {
        self.retained
            .lock()
            .unwrap()
            .iter()
            .map(|entry| ModuleDiagnostic {
                id: entry.id.clone(),
                version: entry.version.clone(),
                library_sha256: entry.hash.clone(),
                mapped_bytes: entry.bytes,
                loaded_at: entry.loaded_at,
                strong_references: entry.module.as_ref().map(Arc::strong_count).unwrap_or(0),
                usable: entry.module.is_some(),
            })
            .collect()
    }

    pub fn spawn(
        &self,
        ctx: &Ctx,
        module: &Arc<Module>,
        value: ConfigValue,
    ) -> Result<FiberView, CordisError> {
        let factory = DylibFactory {
            id: module.id.clone(),
            name: module.name.clone(),
            injects: module.injects.clone(),
        };
        factory.validate_config(&DylibConfig::new(module.clone(), value.clone()))?;
        Ok(ctx.plugin_with(factory, DylibConfig::new(module.clone(), value)))
    }

    pub async fn swap(
        &self,
        view: &FiberView,
        module: &Arc<Module>,
        value: ConfigValue,
    ) -> Result<(), Arc<CordisError>> {
        let current = view.current_config::<DylibConfig>().ok_or_else(|| {
            Arc::new(CordisError::Validation {
                issues: vec!["fiber is not a dylib plugin".into()],
            })
        })?;
        let expected = DylibFactory {
            id: current.module.id.clone(),
            name: current.module.name.clone(),
            injects: current.module.injects.clone(),
        };
        expected.check_module(module).map_err(Arc::new)?;
        view.update(DylibConfig::new(module.clone(), value)).await
    }
}

pub struct Module {
    id: String,
    version: String,
    library_sha256: String,
    name: String,
    injects: Vec<TypeKey>,
    schema: Option<rutis_sdk::serde_json::Value>,
    factory: Box<dyn PluginFactory<ConfigValue>>,
    _handle: NonNull<libc::c_void>,
}
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn library_sha256(&self) -> &str {
        &self.library_sha256
    }
    /// The factory's display name.
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    /// The config's JSON Schema, when the plugin exported one.
    pub fn schema(&self) -> Option<&rutis_sdk::serde_json::Value> {
        self.schema.as_ref()
    }
    /// The plugin's factory over JSON config.
    pub fn factory(&self) -> &dyn PluginFactory<ConfigValue> {
        self.factory.as_ref()
    }
}

#[derive(Clone)]
pub struct DylibConfig {
    module: Arc<Module>,
    value: ConfigValue,
}
impl DylibConfig {
    pub fn new(module: Arc<Module>, value: ConfigValue) -> Self {
        Self { module, value }
    }
    pub fn module(&self) -> &Arc<Module> {
        &self.module
    }
}

struct DylibFactory {
    id: String,
    name: String,
    injects: Vec<TypeKey>,
}
impl DylibFactory {
    fn check_module(&self, module: &Module) -> Result<(), CordisError> {
        if self.id != module.id || self.name != module.name || self.injects != module.injects {
            return Err(CordisError::Validation { issues: vec!["plugin identity or dependency declaration changed; dispose and spawn a new fiber".into()] });
        }
        Ok(())
    }
}
impl PluginFactory<DylibConfig> for DylibFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn validate_config(&self, config: &DylibConfig) -> Result<(), CordisError> {
        self.check_module(&config.module)?;
        config.module.factory.validate_config(&config.value)
    }
    fn build(&self, config: &DylibConfig) -> Result<Box<dyn Plugin>, CordisError> {
        self.check_module(&config.module)?;
        config.module.factory.build(&config.value)
    }
}

struct BootMeta {
    sdk_id: String,
    sdk_artifact: String,
    id: String,
    version: String,
}
fn parse_boot(bytes: &[u8]) -> Result<BootMeta, String> {
    // Rust embeds another copy of the static in its .rustc metadata. A raw
    // magic search therefore cannot identify the blob that the linker maps.
    let boot = elf_section(bytes, b".note.rutis.meta")?;
    if boot.len() != BOOT_SIZE || !boot.starts_with(BOOT_MAGIC) {
        return Err("invalid plugin boot section".into());
    }
    let mut pos = BOOT_MAGIC.len();
    let mut next = || -> Result<String, String> {
        let len_bytes = boot
            .get(pos..pos + 2)
            .ok_or("truncated boot field length")?;
        let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
        pos += 2;
        let value = boot.get(pos..pos + len).ok_or("truncated boot field")?;
        pos += len;
        std::str::from_utf8(value)
            .map(str::to_string)
            .map_err(|e| e.to_string())
    };
    Ok(BootMeta {
        sdk_id: next()?,
        sdk_artifact: next()?,
        id: next()?,
        version: next()?,
    })
}

fn elf_section<'a>(bytes: &'a [u8], wanted: &[u8]) -> Result<&'a [u8], String> {
    if bytes.get(..6) != Some(b"\x7fELF\x02\x01") {
        return Err("expected little-endian ELF64".into());
    }
    let u16_at = |pos: usize| -> Result<usize, String> {
        let raw: [u8; 2] = bytes
            .get(pos..pos + 2)
            .ok_or("truncated ELF header")?
            .try_into()
            .unwrap();
        Ok(u16::from_le_bytes(raw) as usize)
    };
    let u32_at = |pos: usize| -> Result<usize, String> {
        let raw: [u8; 4] = bytes
            .get(pos..pos + 4)
            .ok_or("truncated ELF section")?
            .try_into()
            .unwrap();
        Ok(u32::from_le_bytes(raw) as usize)
    };
    let u64_at = |pos: usize| -> Result<usize, String> {
        let raw: [u8; 8] = bytes
            .get(pos..pos + 8)
            .ok_or("truncated ELF section")?
            .try_into()
            .unwrap();
        usize::try_from(u64::from_le_bytes(raw)).map_err(|_| "ELF offset too large".into())
    };
    let offset = u64_at(40)?;
    let size = u16_at(58)?;
    let count = u16_at(60)?;
    let names = u16_at(62)?;
    if size < 64 || count == 0 || names >= count {
        return Err("unsupported ELF section table".into());
    }
    let section = |index: usize| -> Result<(usize, usize, usize), String> {
        let base = offset
            .checked_add(index.checked_mul(size).ok_or("ELF section overflow")?)
            .ok_or("ELF section overflow")?;
        let _ = bytes
            .get(base..base + 64)
            .ok_or("truncated ELF section table")?;
        Ok((u32_at(base)?, u64_at(base + 24)?, u64_at(base + 32)?))
    };
    let (_, name_offset, name_size) = section(names)?;
    let names = bytes
        .get(
            name_offset
                ..name_offset
                    .checked_add(name_size)
                    .ok_or("ELF name table overflow")?,
        )
        .ok_or("truncated ELF name table")?;
    let mut found = None;
    for index in 0..count {
        let (name, data_offset, data_size) = section(index)?;
        let Some(name_bytes) = names.get(name..) else {
            return Err("bad ELF section name".into());
        };
        let Some(end) = name_bytes.iter().position(|b| *b == 0) else {
            return Err("unterminated ELF section name".into());
        };
        if &name_bytes[..end] == wanted {
            if found.is_some() {
                return Err("duplicate plugin boot section".into());
            }
            found = Some(
                bytes
                    .get(
                        data_offset
                            ..data_offset
                                .checked_add(data_size)
                                .ok_or("ELF section overflow")?,
                    )
                    .ok_or("truncated plugin boot section")?,
            );
        }
    }
    found.ok_or_else(|| "plugin boot section missing".into())
}

fn sha_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn sha_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(sha_bytes(&bytes))
}

fn ensure_cached(path: &Path, bytes: &[u8], hash: &str) -> Result<(), String> {
    if sha_file(path).as_deref() == Ok(hash) {
        return Ok(());
    }
    let dir = path.parent().ok_or("cache path has no parent")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut staged = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    staged.write_all(bytes).map_err(|e| e.to_string())?;
    staged.as_file().sync_all().map_err(|e| e.to_string())?;
    staged.persist(path).map_err(|e| e.to_string())?;
    if sha_file(path).as_deref() != Ok(hash) {
        return Err("cached library SHA-256 mismatch after atomic replacement".into());
    }
    Ok(())
}

fn loaded_sdk_path() -> Result<PathBuf, String> {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::uninit();
    if unsafe {
        libc::dladdr(
            rutis_sdk::rutis_sdk_boot_id as *const libc::c_void,
            info.as_mut_ptr(),
        )
    } == 0
    {
        return Err("dladdr failed for SDK boot function".into());
    }
    let info = unsafe { info.assume_init() };
    if info.dli_fname.is_null() {
        return Err("dladdr returned no SDK path".into());
    }
    let path = unsafe { CStr::from_ptr(info.dli_fname) };
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

unsafe fn open_library(path: &Path) -> Result<NonNull<libc::c_void>, String> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    NonNull::new(libc::dlopen(
        path.as_ptr(),
        libc::RTLD_NOW | libc::RTLD_LOCAL,
    ))
    .ok_or_else(|| {
        CStr::from_ptr(libc::dlerror())
            .to_string_lossy()
            .into_owned()
    })
}

unsafe fn optional_symbol<T>(handle: NonNull<libc::c_void>, name: &[u8]) -> Option<T> {
    let ptr = libc::dlsym(handle.as_ptr(), name.as_ptr().cast());
    if ptr.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&ptr))
    }
}

unsafe fn symbol<T>(handle: NonNull<libc::c_void>, name: &[u8]) -> Result<T, String> {
    let ptr = libc::dlsym(handle.as_ptr(), name.as_ptr().cast());
    if ptr.is_null() {
        return Err(CStr::from_ptr(libc::dlerror())
            .to_string_lossy()
            .into_owned());
    }
    Ok(std::mem::transmute_copy(&ptr))
}
