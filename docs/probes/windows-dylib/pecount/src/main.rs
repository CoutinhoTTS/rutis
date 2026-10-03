//! Small PE inspector for the Windows dylib probe.
//!
//! pecount exports <dll>            export directory counts and top crates
//! pecount imports <pe>             imported DLL names
//! pecount sections <pe>            section names and sizes
//! pecount blob <pe> <name>         count sections named <name>, check the probe pattern

use object::read::pe::PeFile64;
use object::{LittleEndian as LE, Object, ObjectSection};
use std::collections::{BTreeMap, BTreeSet};

/// Same generator as runtime/greeter: magic, then a fixed byte pattern.
pub fn expected_blob() -> [u8; 512] {
    let mut out = [0u8; 512];
    let magic = b"RUTIS_PROBE_BOOT";
    let mut i = 0;
    while i < 512 {
        out[i] = if i < magic.len() { magic[i] } else { ((i * 31 + 7) % 251) as u8 };
        i += 1;
    }
    out
}

fn crate_of(name: &str) -> String {
    // Legacy mangling on MSVC: _ZN<len><ident>...; v0: _R...; else: unmangled.
    let rest = name.strip_prefix("_ZN").or_else(|| name.strip_prefix("__ZN"));
    if let Some(rest) = rest {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(len) = digits.parse::<usize>() {
            let start = digits.len();
            if let Some(ident) = rest.get(start..start + len) {
                // Trait impls start with `_$LT$`; keep them as a bucket.
                return if ident.starts_with("_$LT$") { "<impl>".into() } else { ident.into() };
            }
        }
        return "<legacy?>".into();
    }
    if let Some(rest) = name.strip_prefix("_R") {
        // v0 mangling: the first crate root `C[s<base62>_]<len><ident>`.
        let b = rest.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'C' {
                let mut j = i + 1;
                if j < b.len() && b[j] == b's' {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_alphanumeric() {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'_' {
                        j += 1;
                    } else {
                        i += 1;
                        continue;
                    }
                }
                let start = j;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                if j > start {
                    let len: usize = rest[start..j].parse().unwrap_or(0);
                    let j = if j < b.len() && b[j] == b'_' { j + 1 } else { j };
                    if let Some(ident) = rest.get(j..j + len) {
                        return ident.to_string();
                    }
                }
            }
            i += 1;
        }
        return "<v0?>".into();
    }
    "<unmangled>".into()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let path = args.get(2).expect("usage: pecount <cmd> <file> [...]");
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    match cmd {
        "exports" => {
            let pe = PeFile64::parse(&*data).expect("parse PE64");
            let Some(table) = pe.export_table().expect("export table") else {
                println!("EXPORTS functions=0 names=0");
                return;
            };
            let dir = table.directory();
            let functions = dir.number_of_functions.get(LE);
            let names = dir.number_of_names.get(LE);
            println!(
                "EXPORTS functions={functions} names={names} limit=65535 margin={} used={:.1}%",
                65535i64 - functions as i64,
                functions as f64 * 100.0 / 65535.0
            );
            let mut by_crate: BTreeMap<String, usize> = BTreeMap::new();
            let mut generic_like = 0usize;
            for export in table.exports().expect("exports") {
                if let Some(name) = export.name {
                    let name = String::from_utf8_lossy(name);
                    *by_crate.entry(crate_of(&name)).or_default() += 1;
                    if name.contains("$LT$") {
                        generic_like += 1;
                    }
                }
            }
            println!("EXPORTS names_with_generic_args={generic_like}");
            let mut top: Vec<_> = by_crate.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            for (name, count) in top.iter().take(25) {
                println!("  {count:>7}  {name}");
            }
        }
        "imports" => {
            let file = object::File::parse(&*data).expect("parse");
            let libs: BTreeSet<String> = file
                .imports()
                .expect("imports")
                .iter()
                .map(|i| String::from_utf8_lossy(i.library()).into_owned())
                .collect();
            for lib in libs {
                println!("IMPORT {lib}");
            }
        }
        "sections" => {
            let file = object::File::parse(&*data).expect("parse");
            for s in file.sections() {
                println!("SECTION {:<12} size={}", s.name().unwrap_or("?"), s.size());
            }
        }
        "blob" => {
            let want = args.get(3).expect("section name");
            let file = object::File::parse(&*data).expect("parse");
            let hits: Vec<_> = file.sections().filter(|s| s.name().ok() == Some(want.as_str())).collect();
            println!("BLOB sections_named_{want}={}", hits.len());
            if hits.len() == 1 {
                let bytes = hits[0].data().expect("section data");
                let ok = bytes.len() >= 512 && bytes[..512] == expected_blob();
                println!("BLOB size={} raw_len={} content_ok={ok}", hits[0].size(), bytes.len());
            }
            let blob = expected_blob();
            let found = data.windows(512).filter(|w| *w == blob).count();
            println!("BLOB full_pattern_occurrences_in_file={found}");
        }
        _ => panic!("unknown command {cmd}"),
    }
}
