//! Nested includes: a row named `cordis:include` (or
//! `@deepseek-ai/cordis-plugin-include`) whose config is
//! `{ path, patches?, initial? }` mounts another file's entry list as a
//! subtree.
//!
//! rutis-loader has no includes; this expands each one into a group row
//! whose children are the file's rows with the include's `patches` applied
//! and their ids prefixed `<include id>:` (cordis's subtree ids). The
//! expansion is one last layer that replaces the include row's `config`, so
//! the rows inside are not addressable from the profile's layers and cannot
//! be edited through the loader — they belong to another file, as in dsh's
//! config editor.

use std::path::{Path, PathBuf};

use rutis_loader::{apply_patches, Layer, Patch};
use serde_json::{json, Value};

use super::layers::parse_patch_list;
use super::{paths, yaml};

pub const INCLUDE_NAMES: [&str; 2] = ["cordis:include", "@deepseek-ai/cordis-plugin-include"];
pub const INCLUDES_LAYER: &str = "includes";

/// The expansion layer, the files it read, and includes that could not be
/// read (they stay unexpanded).
#[derive(Debug, Default)]
pub struct Expansion {
    pub layer: Option<Layer>,
    pub files: Vec<PathBuf>,
    pub issues: Vec<String>,
}

fn is_include(row: &Value) -> bool {
    row.get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| INCLUDE_NAMES.contains(&name))
}

/// Expand every include row of the composed `layers`; relative paths
/// resolve against `base` (the profile directory).
pub fn expand(layers: &[Layer], base: &Path) -> Expansion {
    let mut expansion = Expansion::default();
    let mut patches = Vec::new();
    for row in apply_patches(layers).flat {
        let (Some(id), true) = (row.id.clone(), is_include(&row.value)) else {
            continue;
        };
        match children(&id, &row.value, base, &mut expansion, 0) {
            Ok(children) => patches.push(
                serde_json::from_value::<Patch>(
                    json!({ "id": id, "group": true, "config": children }),
                )
                .expect("an override patch"),
            ),
            Err(issue) => expansion.issues.push(format!("include {id:?}: {issue}")),
        }
    }
    if !patches.is_empty() {
        expansion.layer = Some(Layer::new(INCLUDES_LAYER, patches));
    }
    expansion
}

/// The file an include's `path` names: `fileURLToPath(new URL(path, base))`
/// with `base` a directory, as cordis-plugin-include resolves it. So the
/// path is a URL reference: percent-escapes decode, `?` and `#` end it, and
/// `file:` URLs work; what Node's `fileURLToPath` refuses is an error.
pub fn include_file(base: &Path, path: &str) -> Result<PathBuf, String> {
    let invalid = |why: &str| format!("include path {path:?}: {why}");
    let base = url::Url::from_directory_path(paths::absolute(base))
        .map_err(|()| invalid("the base directory is not a file URL"))?;
    let url = base.join(path).map_err(|e| invalid(&e.to_string()))?;
    if url.scheme() != "file" {
        return Err(invalid("not a file URL"));
    }
    let raw = url.path().as_bytes();
    for (i, byte) in raw.iter().enumerate() {
        if *byte != b'%' {
            continue;
        }
        let hex = raw.get(i + 1..i + 3).unwrap_or_default();
        if hex.len() != 2 || !hex.iter().all(u8::is_ascii_hexdigit) {
            return Err(invalid("malformed percent-escape"));
        }
        if hex.eq_ignore_ascii_case(b"2f") {
            return Err(invalid("must not include an encoded /"));
        }
    }
    url.to_file_path()
        .map_err(|()| invalid("names a remote host"))
}

fn children(
    id: &str,
    row: &Value,
    base: &Path,
    expansion: &mut Expansion,
    depth: usize,
) -> Result<Vec<Value>, String> {
    if depth > 16 {
        return Err("includes nest too deep".into());
    }
    let config = row.get("config").cloned().unwrap_or(Value::Null);
    let path = config
        .get("path")
        .and_then(Value::as_str)
        .ok_or("config.path must be a file path")?;
    let file = include_file(base, path)?;
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !matches!(ext, "yml" | "yaml" | "json") {
        return Err(format!("extension {ext:?} not supported"));
    }
    expansion.files.push(file.clone());
    let rows = match std::fs::read_to_string(&file) {
        Ok(text) => match yaml::parse(&text).map_err(|e| e.to_string())? {
            Value::Array(rows) => rows,
            _ => {
                return Err(format!(
                    "{} must be a top-level array of entries",
                    file.display()
                ))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match config.get("initial") {
            Some(Value::Array(rows)) => rows.clone(),
            _ => return Err(format!("config file not found: {}", file.display())),
        },
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let own: Vec<Patch> = match config.get("patches") {
        None | Some(Value::Null) => Vec::new(),
        Some(list) => {
            let text = serde_json::to_string(list).unwrap_or_default();
            parse_patch_list(&file, &text).map_err(|e| e.to_string())?
        }
    };
    let insert: Patch =
        serde_json::from_value(json!({ "insert": rows })).map_err(|e| e.to_string())?;
    let composed = apply_patches(&[Layer::new("file", vec![insert]), Layer::new("patches", own)]);
    let dir = file.parent().unwrap_or(Path::new("/")).to_path_buf();
    composed
        .rows
        .into_iter()
        .map(|row| prefix(id, row, &dir, expansion, depth))
        .collect()
}

/// Prefix the ids of `row` and its group children with `<scope>:` (one id
/// space per include, as cordis's entry tree), and expand nested includes.
fn prefix(
    scope: &str,
    row: Value,
    dir: &Path,
    expansion: &mut Expansion,
    depth: usize,
) -> Result<Value, String> {
    let Value::Object(mut map) = row else {
        return Ok(row);
    };
    let id = match map.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => format!("{scope}:{id}"),
        _ => return Ok(Value::Object(map)),
    };
    map.insert("id".into(), Value::String(id.clone()));
    if is_include(&Value::Object(map.clone())) {
        let nested = children(&id, &Value::Object(map.clone()), dir, expansion, depth + 1)?;
        map.insert("group".into(), Value::Bool(true));
        map.insert("config".into(), Value::Array(nested));
        return Ok(Value::Object(map));
    }
    if map.get("group").and_then(Value::as_bool) == Some(true) {
        if let Some(Value::Array(rows)) = map.remove("config") {
            let rows = rows
                .into_iter()
                .map(|r| prefix(scope, r, dir, expansion, depth))
                .collect::<Result<Vec<_>, _>>()?;
            map.insert("config".into(), Value::Array(rows));
        }
    }
    Ok(Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expectations from Node: `fileURLToPath(new URL(path, base))`.
    #[test]
    fn include_paths_resolve_like_node() {
        let base = Path::new("/p/my profile");
        let cases: [(&str, Option<&str>); 22] = [
            ("./a.yml", Some("/p/my profile/a.yml")),
            ("a.yml", Some("/p/my profile/a.yml")),
            ("my file.yml", Some("/p/my profile/my file.yml")),
            ("my%20file.yml", Some("/p/my profile/my file.yml")),
            ("sub/../b.yml", Some("/p/my profile/b.yml")),
            ("../up.yml", Some("/p/up.yml")),
            ("/abs/c.yml", Some("/abs/c.yml")),
            ("/abs dir/c.yml", Some("/abs dir/c.yml")),
            ("file:///abs/%E6%8F%92%E4%BB%B6.yml", Some("/abs/插件.yml")),
            ("插件.yml", Some("/p/my profile/插件.yml")),
            ("a%23b.yml", Some("/p/my profile/a#b.yml")),
            ("a#frag.yml", Some("/p/my profile/a")),
            ("q.yml?x=1", Some("/p/my profile/q.yml")),
            ("100%25.yml", Some("/p/my profile/100%.yml")),
            ("100%.yml", None),
            ("a%2Fb.yml", None),
            ("file://localhost/l.yml", Some("/l.yml")),
            ("file://server/s.yml", None),
            ("http://x/h.yml", None),
            ("a\\b.yml", Some("/p/my profile/a/b.yml")),
            ("//host/x.yml", None),
            ("~/t.yml", Some("/p/my profile/~/t.yml")),
        ];
        for (path, expected) in cases {
            assert_eq!(
                include_file(base, path).ok(),
                expected.map(PathBuf::from),
                "{path}"
            );
        }
    }

    #[test]
    fn expands_nested_includes_with_prefixed_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sub.yml"),
            "- id: a\n  name: pa\n- id: g\n  group: true\n  config:\n    - id: b\n      name: pb\n- id: deeper\n  name: cordis:include\n  config:\n    path: ./deep.yml\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("deep.yml"), "- id: z\n  name: pz\n").unwrap();
        let layers = vec![Layer::new(
            "user",
            serde_json::from_value(json!([{ "insert": [
                { "id": "inc", "name": "cordis:include", "config": {
                    "path": "./sub.yml",
                    "patches": [{ "id": "a", "disabled": true }]
                } },
                { "id": "missing", "name": "cordis:include", "config": { "path": "./nope.yml" } }
            ] }]))
            .unwrap(),
        )];
        let expansion = expand(&layers, dir.path());
        assert_eq!(expansion.issues.len(), 1, "{:?}", expansion.issues);
        let mut all = layers.clone();
        all.push(expansion.layer.unwrap());
        let composed = apply_patches(&all);
        let ids: Vec<String> = composed.flat.iter().filter_map(|r| r.id.clone()).collect();
        assert_eq!(
            ids,
            [
                "inc",
                "inc:a",
                "inc:g",
                "inc:b",
                "inc:deeper",
                "inc:deeper:z",
                "missing"
            ]
            .map(String::from)
        );
        let a = composed
            .flat
            .iter()
            .find(|r| r.id.as_deref() == Some("inc:a"))
            .unwrap();
        assert_eq!(a.value["disabled"], json!(true));
        assert_eq!(expansion.files.len(), 3);
    }
}
