//! Reads what a rutis dylib plugin says about itself without loading it.
//!
//! `dlopen` runs a library's initializers before it returns, so everything the
//! loader must check before trusting a plugin is read from the file here. The
//! crate has no rutis dependencies: the packaging tool uses it too and must not
//! link the SDK dylib or the dynamic libstd.

use object::{
    Architecture, BinaryFormat, FileKind, Object, ObjectKind, ObjectSection, ObjectSymbol,
};

/// Must equal `rutis_sdk::BOOT_MAGIC`; `rutis-dylib` asserts this at compile time.
pub const BOOT_MAGIC: &[u8] = b"RUTIS_PLUGIN_BOOT_V1\0";
/// Must equal `rutis_sdk::BOOT_SIZE`; `rutis-dylib` asserts this at compile time.
pub const BOOT_SIZE: usize = 512;

/// Section that `export_plugin!` places the boot blob in, per object format.
pub const ELF_BOOT_SECTION: &str = ".note.rutis.meta";
pub const MACHO_BOOT_SEGMENT: &str = "__DATA";
pub const MACHO_BOOT_SECTION: &str = "__rutis_meta";
/// PE image section names are limited to 8 bytes.
pub const PE_BOOT_SECTION: &str = ".rutism";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootMeta {
    pub sdk_id: String,
    pub sdk_artifact: String,
    pub id: String,
    pub version: String,
}

/// Checks that `bytes` is a shared library for `target` and returns its boot blob.
pub fn read_boot(bytes: &[u8], target: &str) -> Result<BootMeta, String> {
    let file = open(bytes, target)?;
    parse_boot(boot_section(&file)?)
}

/// Names of the symbols the library exports.
pub fn exported_symbols(bytes: &[u8], target: &str) -> Result<Vec<String>, String> {
    let file = open(bytes, target)?;
    let mut names = Vec::new();
    if file.format() == BinaryFormat::Elf {
        // ELF exports are the defined, global dynamic symbols.
        for symbol in file.dynamic_symbols() {
            if symbol.is_definition() && symbol.is_global() {
                names.push(symbol.name().map_err(|e| e.to_string())?.to_owned());
            }
        }
    } else {
        for export in file.exports().map_err(|e| e.to_string())? {
            let export = export.map_err(|e| e.to_string())?;
            if let object::NameOrOrdinal::Name(name) = export.name() {
                names.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    Ok(names)
}

/// The Rust libraries a plugin must share with the host, by the file names
/// its dynamic section records.
pub struct SharedLibraries<'a> {
    /// The SDK, e.g. `librutis_sdk.so`.
    pub sdk: &'a str,
    /// The exact dynamic libstd the host runs with, e.g. `libstd-<hash>.so`.
    pub std: &'a str,
}

/// Checks a plugin's dynamic dependencies before it is loaded and returns
/// its native ones (everything that is not the SDK or libstd), sorted.
///
/// The SDK and libstd must be the host's copies; another Rust dylib would
/// bring a second std or SDK. Native libraries are allowed, but only by a
/// form the platform loader resolves outside the plugin cache.
pub fn check_plugin_dependencies(
    bytes: &[u8],
    target: &str,
    shared: &SharedLibraries<'_>,
) -> Result<Vec<String>, String> {
    let file = open(bytes, target)?;
    match file.format() {
        BinaryFormat::Elf => check_elf_dependencies(bytes, shared),
        BinaryFormat::MachO => check_macho_dependencies(bytes, shared),
        format => Err(format!(
            "dependency check is not implemented for {format:?}"
        )),
    }
}

/// File names of the libraries `bytes` depends on, as recorded in the file.
pub fn needed_libraries(bytes: &[u8], target: &str) -> Result<Vec<String>, String> {
    let file = open(bytes, target)?;
    match file.format() {
        BinaryFormat::Elf => Ok(elf_dynamic(bytes)?
            .into_iter()
            .filter(|(tag, _)| *tag == DT_NEEDED)
            .map(|(_, value)| value)
            .collect()),
        BinaryFormat::MachO => Ok(macho_info(bytes)?
            .dylibs
            .into_iter()
            .map(|(_, name)| name)
            .collect()),
        format => Err(format!(
            "dependency listing is not implemented for {format:?}"
        )),
    }
}

use object::elf::{
    DynamicTag, DT_AUDIT, DT_AUXILIARY, DT_DEPAUDIT, DT_FILTER, DT_NEEDED, DT_RPATH, DT_RUNPATH,
};

/// The string-valued dynamic entries that decide what gets loaded.
fn elf_dynamic(bytes: &[u8]) -> Result<Vec<(DynamicTag, String)>, String> {
    let file = object::read::elf::ElfFile64::<object::LittleEndian>::parse(bytes)
        .map_err(|e| e.to_string())?;
    let table = file
        .elf_section_table()
        .dynamic_table(file.endian(), bytes)
        .map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    for entry in &table {
        if [
            DT_NEEDED,
            DT_RPATH,
            DT_RUNPATH,
            DT_AUXILIARY,
            DT_FILTER,
            DT_AUDIT,
            DT_DEPAUDIT,
        ]
        .contains(&entry.tag)
        {
            let value = table.string(entry).map_err(|e| e.to_string())?;
            entries.push((entry.tag, String::from_utf8_lossy(value).into_owned()));
        }
    }
    Ok(entries)
}

/// What dyld will do with a Mach-O image before any of its code runs.
struct MachOInfo {
    flags: u32,
    /// (load command, install name) of every dependency.
    dylibs: Vec<(u32, String)>,
    rpaths: Vec<String>,
    dyld_environment: bool,
    /// Load commands dyld must understand that this check does not know.
    unknown_required: Vec<u32>,
    /// Undefined symbols looked up in every loaded image (flat lookup).
    flat_lookups: Vec<String>,
    platform: Option<u32>,
}

fn macho_info(bytes: &[u8]) -> Result<MachOInfo, String> {
    use object::macho;
    use object::read::macho::{LoadCommandVariant, MachHeader, MachOFile64, Nlist};
    let file = MachOFile64::<object::LittleEndian>::parse(bytes).map_err(|e| e.to_string())?;
    let endian = file.endian();
    let text = |raw: &[u8]| String::from_utf8_lossy(raw).into_owned();
    let mut info = MachOInfo {
        flags: file.macho_header().flags(endian).0,
        dylibs: Vec::new(),
        rpaths: Vec::new(),
        dyld_environment: false,
        unknown_required: Vec::new(),
        flat_lookups: Vec::new(),
        platform: None,
    };
    let mut commands = file.macho_load_commands().map_err(|e| e.to_string())?;
    while let Some(command) = commands.next().map_err(|e| e.to_string())? {
        let cmd = command.cmd().0;
        match command.variant().map_err(|e| e.to_string())? {
            LoadCommandVariant::Dylib(dylib) => {
                let name = command
                    .string(endian, dylib.dylib.name)
                    .map_err(|e| e.to_string())?;
                info.dylibs.push((cmd, text(name)));
            }
            LoadCommandVariant::Rpath(rpath) => {
                let path = command
                    .string(endian, rpath.path)
                    .map_err(|e| e.to_string())?;
                info.rpaths.push(text(path));
            }
            LoadCommandVariant::DyldEnvironment(_) => info.dyld_environment = true,
            LoadCommandVariant::BuildVersion(build, _) => {
                info.platform = Some(build.platform.get(endian).0)
            }
            LoadCommandVariant::Other if cmd & macho::LC_REQ_DYLD != 0 => {
                info.unknown_required.push(cmd)
            }
            _ => {}
        }
    }
    let symbols = file.macho_symbol_table();
    for nlist in symbols.iter() {
        let n_type = nlist.n_type().0;
        let undefined_external = n_type & macho::N_STAB == 0
            && n_type & macho::N_TYPE == macho::N_UNDF.0
            && n_type & macho::N_EXT.0 != 0;
        // GET_LIBRARY_ORDINAL(n_desc)
        let ordinal = (nlist.n_desc(endian).0 >> 8) & 0xff;
        if undefined_external && ordinal == macho::DYNAMIC_LOOKUP_ORDINAL.0 as u16 {
            let name = nlist
                .name(endian, symbols.strings())
                .map_err(|e| e.to_string())?;
            info.flat_lookups.push(text(name));
        }
    }
    Ok(info)
}

fn check_macho_dependencies(
    bytes: &[u8],
    shared: &SharedLibraries<'_>,
) -> Result<Vec<String>, String> {
    use object::macho;
    let info = macho_info(bytes)?;
    if info.flags & macho::MH_TWOLEVEL.0 == 0 || info.flags & macho::MH_FORCE_FLAT.0 != 0 {
        return Err("plugin does not use the two-level namespace; its symbols could bind to another plugin".into());
    }
    if let Some(symbol) = info.flat_lookups.first() {
        return Err(format!(
            "plugin looks up {symbol} in every loaded image (-undefined dynamic_lookup); it could bind to another plugin"
        ));
    }
    if let Some(rpath) = info.rpaths.first() {
        return Err(format!("plugin carries a run path ({rpath}); plugins must not"));
    }
    if info.dyld_environment {
        return Err("plugin sets dyld environment variables (LC_DYLD_ENVIRONMENT)".into());
    }
    if let Some(cmd) = info.unknown_required.first() {
        return Err(format!("plugin has a load command dyld requires that this check does not know: {cmd:#x}"));
    }
    let (mut sdk, mut std) = (false, false);
    let mut native = Vec::new();
    for (_, name) in info.dylibs {
        let file = name.rsplit('/').next().unwrap_or(&name);
        if name == shared.sdk {
            sdk = true;
        } else if name == shared.std {
            std = true;
        } else if file.starts_with("libstd-") || file.starts_with("librutis_sdk") {
            return Err(format!(
                "plugin depends on {name}, not the host's {} and {}; rebuild it against this SDK",
                shared.sdk, shared.std
            ));
        } else if name.starts_with('@') {
            // @rpath would resolve in the host's bundle; @loader_path and
            // @executable_path in the plugin cache or next to the host.
            return Err(format!("dependency {name} is relative to a search path; native libraries need an absolute path"));
        } else if !name.starts_with('/') || name.split('/').any(|part| part == "..") {
            return Err(format!("dependency {name} is not a plain absolute path"));
        } else {
            native.push(name);
        }
    }
    if !sdk || !std {
        return Err(format!(
            "plugin must link {} and {} dynamically; it would otherwise carry its own copy",
            shared.sdk, shared.std
        ));
    }
    native.sort();
    native.dedup();
    Ok(native)
}

fn check_elf_dependencies(
    bytes: &[u8],
    shared: &SharedLibraries<'_>,
) -> Result<Vec<String>, String> {
    let (mut sdk, mut std) = (false, false);
    let mut native = Vec::new();
    for (tag, value) in elf_dynamic(bytes)? {
        match tag {
            DT_RPATH | DT_RUNPATH => {
                return Err(format!("plugin carries a run path ({value}); plugins must not"))
            }
            tag if tag != DT_NEEDED => {
                return Err(format!(
                    "plugin uses a filter or audit library ({value}); not allowed"
                ))
            }
            _ if value.contains('/') => {
                return Err(format!("dependency {value} is a path; only file names are allowed"))
            }
            _ if value == shared.sdk => sdk = true,
            _ if value == shared.std => std = true,
            _ if value.starts_with("libstd-") || value.starts_with("librutis_sdk") => {
                return Err(format!(
                    "plugin depends on {value}, not the host's {} and {}; rebuild it against this SDK",
                    shared.sdk, shared.std
                ))
            }
            _ => native.push(value),
        }
    }
    if !sdk || !std {
        return Err(format!(
            "plugin must link {} and {} dynamically; it would otherwise carry its own copy",
            shared.sdk, shared.std
        ));
    }
    native.sort();
    native.dedup();
    Ok(native)
}

/// How a plugin built for `target` refers to the SDK in its dependencies.
pub fn sdk_reference(target: &str) -> Result<String, String> {
    let file = library_file_name("rutis_sdk", target)?;
    Ok(match Target::parse(target)?.format {
        // The SDK's install name (set by its build script).
        BinaryFormat::MachO => format!("@rpath/{file}"),
        _ => file,
    })
}

/// How plugins must refer to the dynamic libstd that the SDK in `sdk_bytes`
/// links: the same reference the SDK itself uses.
pub fn std_reference(sdk_bytes: &[u8], target: &str) -> Result<String, String> {
    needed_libraries(sdk_bytes, target)?
        .into_iter()
        .find(|name| {
            let file = name.rsplit('/').next().unwrap_or(name);
            file.starts_with("libstd-") || file.starts_with("std-")
        })
        .ok_or("the SDK does not link a dynamic libstd".into())
}

/// Exported weak definitions with Rust symbol names. dyld coalesces weak
/// definitions across images, so one plugin version could bind to another's.
pub fn weak_rust_exports(bytes: &[u8], target: &str) -> Result<Vec<String>, String> {
    let file = open(bytes, target)?;
    if file.format() != BinaryFormat::MachO {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for export in file.exports().map_err(|e| e.to_string())? {
        let export = export.map_err(|e| e.to_string())?;
        if let object::NameOrOrdinal::Name(name) = export.name() {
            // Mach-O adds a leading underscore: _ZN… becomes __ZN….
            if export.is_weak() && (name.starts_with(b"__ZN") || name.starts_with(b"__R")) {
                names.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    Ok(names)
}

/// File name of a shared library called `name` on `target`.
pub fn library_file_name(name: &str, target: &str) -> Result<String, String> {
    let name = name.replace('-', "_");
    Ok(match Target::parse(target)?.format {
        BinaryFormat::MachO => format!("lib{name}.dylib"),
        BinaryFormat::Pe => format!("{name}.dll"),
        _ => format!("lib{name}.so"),
    })
}

struct Target {
    format: BinaryFormat,
    architecture: Architecture,
}

impl Target {
    fn parse(triple: &str) -> Result<Self, String> {
        let architecture = match triple.split('-').next() {
            Some("x86_64") => Architecture::X86_64,
            Some("aarch64") => Architecture::Aarch64,
            _ => return Err(format!("unsupported target architecture: {triple}")),
        };
        let format = if triple.contains("-linux-") {
            BinaryFormat::Elf
        } else if triple.contains("-apple-") {
            BinaryFormat::MachO
        } else if triple.contains("-windows-") {
            BinaryFormat::Pe
        } else {
            return Err(format!("unsupported target: {triple}"));
        };
        Ok(Self {
            format,
            architecture,
        })
    }
}

fn open<'a>(bytes: &'a [u8], target: &str) -> Result<object::File<'a>, String> {
    let expected = Target::parse(target)?;
    match FileKind::parse(bytes).map_err(|e| format!("not an object file: {e}"))? {
        FileKind::MachOFat32 | FileKind::MachOFat64 => {
            return Err(
                "universal (fat) Mach-O is not supported; ship one architecture per target".into(),
            )
        }
        _ => {}
    }
    let file = object::File::parse(bytes).map_err(|e| format!("not an object file: {e}"))?;
    if file.format() != expected.format {
        return Err(format!(
            "expected {:?} for {target}, found {:?}",
            expected.format,
            file.format()
        ));
    }
    if !file.is_64() || !file.is_little_endian() {
        return Err("expected a 64-bit little-endian library".into());
    }
    if file.architecture() != expected.architecture {
        return Err(format!(
            "architecture mismatch: expected {:?} for {target}, found {:?}",
            expected.architecture,
            file.architecture()
        ));
    }
    check_kind(&file)?;
    if file.format() == BinaryFormat::MachO {
        // iOS-simulator arm64 code has the same CPU type as macOS arm64.
        if let Some(platform) = macho_info(bytes)?.platform {
            if platform != object::macho::PLATFORM_MACOS.0 {
                return Err(format!("built for Mach-O platform {platform}, not macOS"));
            }
        }
    }
    Ok(file)
}

#[cfg(not(test))]
fn check_kind(file: &object::File<'_>) -> Result<(), String> {
    // Mach-O bundles report Unknown here; only MH_DYLIB is Dynamic.
    if file.kind() != ObjectKind::Dynamic {
        return Err(format!("not a shared library ({:?})", file.kind()));
    }
    Ok(())
}

// Unit tests can only write relocatable objects; everything else is shared.
#[cfg(test)]
fn check_kind(file: &object::File<'_>) -> Result<(), String> {
    match file.kind() {
        ObjectKind::Dynamic | ObjectKind::Relocatable => Ok(()),
        kind => Err(format!("not a shared library ({kind:?})")),
    }
}

fn boot_section<'a>(file: &object::File<'a>) -> Result<&'a [u8], String> {
    // Rust embeds another copy of the static in its .rustc metadata. A raw
    // magic search therefore cannot identify the blob that the linker maps.
    let mut found = Vec::new();
    for section in file.sections() {
        let name = section.name_bytes().map_err(|e| e.to_string())?;
        let matches = match file.format() {
            BinaryFormat::Elf => name == ELF_BOOT_SECTION.as_bytes(),
            BinaryFormat::MachO => {
                name == MACHO_BOOT_SECTION.as_bytes()
                    && section.segment_name_bytes().map_err(|e| e.to_string())?
                        == Some(MACHO_BOOT_SEGMENT.as_bytes())
            }
            BinaryFormat::Pe => name == PE_BOOT_SECTION.as_bytes(),
            _ => false,
        };
        if matches {
            found.push(section.data().map_err(|e| e.to_string())?);
        }
    }
    match found.as_slice() {
        [boot] => Ok(boot),
        [] => Err("plugin boot section not found".into()),
        _ => Err(format!("{} plugin boot sections; expected one", found.len())),
    }
}

fn parse_boot(boot: &[u8]) -> Result<BootMeta, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use object::write;
    use object::{Endianness, SectionKind};

    fn blob(fields: &[&str]) -> Vec<u8> {
        let mut out = BOOT_MAGIC.to_vec();
        for field in fields {
            out.extend_from_slice(&(field.len() as u16).to_le_bytes());
            out.extend_from_slice(field.as_bytes());
        }
        out.resize(BOOT_SIZE, 0);
        out
    }

    fn object_with(
        format: BinaryFormat,
        architecture: Architecture,
        sections: &[(&str, &str, &[u8])],
    ) -> Vec<u8> {
        let mut obj = write::Object::new(format, architecture, Endianness::Little);
        for (segment, name, data) in sections {
            let id = obj.add_section(
                segment.as_bytes().to_vec(),
                name.as_bytes().to_vec(),
                SectionKind::Data,
            );
            obj.append_section_data(id, data, 1);
        }
        obj.write().unwrap()
    }

    const FIELDS: [&str; 4] = ["sdk", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "greeter", "1.2.3"];

    fn expected() -> BootMeta {
        BootMeta {
            sdk_id: "sdk".into(),
            sdk_artifact: "a".repeat(64),
            id: "greeter".into(),
            version: "1.2.3".into(),
        }
    }

    #[test]
    fn reads_the_elf_boot_section() {
        let bytes = object_with(
            BinaryFormat::Elf,
            Architecture::X86_64,
            &[("", ELF_BOOT_SECTION, &blob(&FIELDS))],
        );
        assert_eq!(
            read_boot(&bytes, "x86_64-unknown-linux-gnu").unwrap(),
            expected()
        );
    }

    #[test]
    fn reads_the_macho_boot_section_only_in_its_segment() {
        let good = object_with(
            BinaryFormat::MachO,
            Architecture::Aarch64,
            &[(MACHO_BOOT_SEGMENT, MACHO_BOOT_SECTION, &blob(&FIELDS))],
        );
        assert_eq!(read_boot(&good, "aarch64-apple-darwin").unwrap(), expected());
        let wrong_segment = object_with(
            BinaryFormat::MachO,
            Architecture::Aarch64,
            &[("__TEXT", MACHO_BOOT_SECTION, &blob(&FIELDS))],
        );
        assert!(read_boot(&wrong_segment, "aarch64-apple-darwin")
            .unwrap_err()
            .contains("not found"));
    }

    #[test]
    fn rejects_a_missing_duplicate_or_malformed_section() {
        let target = "x86_64-unknown-linux-gnu";
        let none = object_with(BinaryFormat::Elf, Architecture::X86_64, &[]);
        assert!(read_boot(&none, target).unwrap_err().contains("not found"));
        let boot = blob(&FIELDS);
        let two = object_with(
            BinaryFormat::Elf,
            Architecture::X86_64,
            &[("", ELF_BOOT_SECTION, &boot), ("", ELF_BOOT_SECTION, &boot)],
        );
        assert!(read_boot(&two, target).unwrap_err().contains("2 plugin boot"));
        let short = object_with(
            BinaryFormat::Elf,
            Architecture::X86_64,
            &[("", ELF_BOOT_SECTION, &boot[..100])],
        );
        assert!(read_boot(&short, target).unwrap_err().contains("invalid"));
        let mut truncated = BOOT_MAGIC.to_vec();
        truncated.extend_from_slice(&600u16.to_le_bytes());
        truncated.resize(BOOT_SIZE, 0);
        let truncated = object_with(
            BinaryFormat::Elf,
            Architecture::X86_64,
            &[("", ELF_BOOT_SECTION, &truncated)],
        );
        assert!(read_boot(&truncated, target)
            .unwrap_err()
            .contains("truncated"));
    }

    #[test]
    fn rejects_the_wrong_format_or_architecture() {
        let elf = object_with(
            BinaryFormat::Elf,
            Architecture::X86_64,
            &[("", ELF_BOOT_SECTION, &blob(&FIELDS))],
        );
        assert!(read_boot(&elf, "aarch64-unknown-linux-gnu")
            .unwrap_err()
            .contains("architecture mismatch"));
        assert!(read_boot(&elf, "aarch64-apple-darwin")
            .unwrap_err()
            .contains("expected MachO"));
        assert!(read_boot(b"not an object", "x86_64-unknown-linux-gnu").is_err());
        assert!(read_boot(&elf, "riscv64gc-unknown-linux-gnu")
            .unwrap_err()
            .contains("unsupported"));
    }

    #[test]
    fn rejects_universal_macho() {
        // FAT_MAGIC, zero architectures, padded past the 16-byte magic probe.
        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe];
        fat.resize(64, 0);
        assert!(read_boot(&fat, "aarch64-apple-darwin")
            .unwrap_err()
            .contains("universal"));
    }

    /// A minimal ELF shared object whose dynamic section holds `entries`.
    fn elf_with_dynamic(entries: &[(DynamicTag, &str)]) -> Vec<u8> {
        use object::elf;
        let header = write::elf::FileHeader {
            os_abi: elf::ELFOSABI_SYSV,
            abi_version: 0,
            e_type: elf::ET_DYN,
            e_machine: elf::EM_X86_64,
            e_entry: 0,
            e_flags: elf::FileFlags(0),
        };
        let mut bytes = Vec::new();
        {
            let mut w = write::elf::Writer::new(Endianness::Little, true, &mut bytes);
            let ids: Vec<_> = entries
                .iter()
                .map(|(_, value)| w.add_dynamic_string(value.as_bytes()))
                .collect();
            w.reserve_file_header();
            w.reserve_dynstr().unwrap();
            w.reserve_dynamic(entries.len() + 1);
            w.reserve_null_section_index();
            w.reserve_dynstr_section_index();
            w.reserve_dynamic_section_index();
            w.reserve_shstrtab_section_index();
            w.reserve_shstrtab().unwrap();
            w.reserve_section_headers();
            w.write_file_header(&header).unwrap();
            w.write_dynstr();
            w.write_align_dynamic();
            for ((tag, _), id) in entries.iter().zip(ids) {
                w.write_dynamic_string(*tag, id).unwrap();
            }
            w.write_dynamic(elf::DT_NULL, 0).unwrap();
            w.write_shstrtab();
            w.write_null_section_header();
            w.write_dynstr_section_header(0);
            w.write_dynamic_section_header(0);
            w.write_shstrtab_section_header();
        }
        bytes
    }

    const SHARED: SharedLibraries<'static> = SharedLibraries {
        sdk: "librutis_sdk.so",
        std: "libstd-0123.so",
    };
    const LINUX: &str = "x86_64-unknown-linux-gnu";

    #[test]
    fn plugin_dependencies_return_the_native_libraries() {
        let bytes = elf_with_dynamic(&[
            (DT_NEEDED, "libz.so.1"),
            (DT_NEEDED, "librutis_sdk.so"),
            (DT_NEEDED, "libstd-0123.so"),
            (DT_NEEDED, "libc.so.6"),
        ]);
        assert_eq!(
            check_plugin_dependencies(&bytes, LINUX, &SHARED).unwrap(),
            ["libc.so.6", "libz.so.1"]
        );
        assert_eq!(
            needed_libraries(&bytes, LINUX).unwrap(),
            ["libz.so.1", "librutis_sdk.so", "libstd-0123.so", "libc.so.6"]
        );
    }

    #[test]
    fn plugin_dependencies_reject_what_could_load_another_copy() {
        let base = [
            (DT_NEEDED, "librutis_sdk.so"),
            (DT_NEEDED, "libstd-0123.so"),
        ];
        let reject = |extra: (DynamicTag, &str), expected: &str| {
            let mut entries = base.to_vec();
            entries.push(extra);
            let error =
                check_plugin_dependencies(&elf_with_dynamic(&entries), LINUX, &SHARED).unwrap_err();
            assert!(error.contains(expected), "{error}");
        };
        reject((DT_RUNPATH, "$ORIGIN"), "run path");
        reject((DT_RPATH, "/opt/lib"), "run path");
        reject((DT_AUDIT, "libaudit.so"), "audit");
        reject((DT_NEEDED, "/opt/lib/libz.so.1"), "is a path");
        reject((DT_NEEDED, "libstd-9999.so"), "rebuild it against this SDK");
        reject((DT_NEEDED, "librutis_sdk-old.so"), "rebuild it against this SDK");
        let no_std = elf_with_dynamic(&[(DT_NEEDED, "librutis_sdk.so")]);
        assert!(check_plugin_dependencies(&no_std, LINUX, &SHARED)
            .unwrap_err()
            .contains("dynamically"));
    }

    #[test]
    fn names_libraries_per_platform() {
        let name = |target| library_file_name("rutis-sdk", target).unwrap();
        assert_eq!(name("x86_64-unknown-linux-gnu"), "librutis_sdk.so");
        assert_eq!(name("aarch64-apple-darwin"), "librutis_sdk.dylib");
        assert_eq!(name("x86_64-pc-windows-msvc"), "rutis_sdk.dll");
    }
}
