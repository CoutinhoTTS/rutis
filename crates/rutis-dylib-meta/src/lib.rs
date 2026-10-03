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

    #[test]
    fn names_libraries_per_platform() {
        let name = |target| library_file_name("rutis-sdk", target).unwrap();
        assert_eq!(name("x86_64-unknown-linux-gnu"), "librutis_sdk.so");
        assert_eq!(name("aarch64-apple-darwin"), "librutis_sdk.dylib");
        assert_eq!(name("x86_64-pc-windows-msvc"), "rutis_sdk.dll");
    }
}
