//! Volatile config fields: changes a running plugin applies in place
//! instead of restarting (cordis's schemastery `meta.volatile`).
//!
//! A field is volatile when its JSON Schema carries `"x-volatile": true`
//! (with schemars: `#[schemars(extend("x-volatile" = true))]`). Only fields
//! at fixed object paths count. When a running row's new config differs from
//! the running one at volatile paths alone, the loader stores it with
//! `FiberView::set_config` (no restart) and sends the plugin a
//! [`VolatileUpdate`] on its instance key ([`volatile_key`]).

use rutis::{Ctx, Event, EventKey};
use serde_json::Value;

pub const MARKER: &str = "x-volatile";

/// Sent to one plugin instance after a volatile-only config change.
#[derive(Debug, Clone)]
pub struct VolatileUpdate {
    /// The changed volatile paths, as object keys from the config root.
    pub paths: Vec<Vec<String>>,
    /// The whole new config, already stored for later loads.
    pub config: Value,
}

impl Event for VolatileUpdate {
    const NAME: &'static str = "rutis-loader::volatile-update";
    type Value = ();
}

/// The key a plugin listens on for its own volatile updates:
/// `ctx.events().on(ctx, &volatile_key(ctx), listener)`.
///
/// Named after the fiber's instance id, which is unique in the process. (An
/// instance-qualified key would not do: only the fiber's own subtree may use
/// those, and the loader is its ancestor.)
pub fn volatile_key(ctx: &Ctx) -> EventKey<VolatileUpdate> {
    key_for(ctx.instance())
}

pub(crate) fn key_for(instance: rutis::InstanceId) -> EventKey<VolatileUpdate> {
    EventKey::dynamic(format!("volatile:{instance}"))
}

fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
        return schema;
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        return schema;
    };
    root.pointer(pointer).unwrap_or(schema)
}

/// Every volatile path of `schema`.
pub fn volatile_paths(schema: &Value) -> Vec<Vec<String>> {
    fn walk(
        root: &Value,
        schema: &Value,
        path: &mut Vec<String>,
        depth: usize,
        out: &mut Vec<Vec<String>>,
    ) {
        if depth > 32 {
            return;
        }
        let schema = resolve(root, schema);
        if schema.get(MARKER).and_then(Value::as_bool) == Some(true) && !path.is_empty() {
            out.push(path.clone());
            return;
        }
        // `allOf: [{ $ref }]` wraps a referenced struct with extra keywords.
        if let Some(Value::Array(parts)) = schema.get("allOf") {
            for part in parts {
                walk(root, part, path, depth + 1, out);
            }
        }
        if let Some(Value::Object(properties)) = schema.get("properties") {
            for (key, property) in properties {
                path.push(key.clone());
                walk(root, property, path, depth + 1, out);
                path.pop();
            }
        }
    }
    let mut out = Vec::new();
    walk(schema, schema, &mut Vec::new(), 0, &mut out);
    out
}

fn get<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| v.get(key))
}

fn without(value: &Value, paths: &[Vec<String>]) -> Value {
    let mut value = value.clone();
    for path in paths {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let pointer: String = parents
            .iter()
            .map(|key| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
            .collect();
        if let Some(map) = value.pointer_mut(&pointer).and_then(Value::as_object_mut) {
            map.remove(last);
        }
    }
    value
}

/// When `old` and `new` differ only at volatile paths, those paths.
pub fn volatile_change(
    old: &Value,
    new: &Value,
    paths: &[Vec<String>],
) -> Option<Vec<Vec<String>>> {
    if paths.is_empty() || old == new || without(old, paths) != without(new, paths) {
        return None;
    }
    Some(
        paths
            .iter()
            .filter(|path| get(old, path) != get(new, path))
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_paths_through_refs_and_compares() {
        let schema = json!({
            "type": "object",
            "properties": {
                "level": { "type": "integer", "x-volatile": true },
                "name": { "type": "string" },
                "inner": { "$ref": "#/$defs/Inner" }
            },
            "$defs": { "Inner": { "type": "object", "properties": {
                "verbose": { "type": "boolean", "x-volatile": true },
                "port": { "type": "integer" }
            } } }
        });
        let paths = volatile_paths(&schema);
        assert_eq!(
            paths,
            vec![
                vec!["level".to_owned()],
                vec!["inner".into(), "verbose".into()]
            ]
        );

        let old = json!({ "level": 1, "name": "a", "inner": { "verbose": false, "port": 1 } });
        let volatile = json!({ "level": 2, "name": "a", "inner": { "verbose": true, "port": 1 } });
        let ordinary = json!({ "level": 2, "name": "b", "inner": { "verbose": false, "port": 1 } });
        assert_eq!(volatile_change(&old, &volatile, &paths).unwrap().len(), 2);
        assert!(volatile_change(&old, &ordinary, &paths).is_none());
        assert!(volatile_change(&old, &old, &paths).is_none());
        // Adding a volatile field that was absent is still volatile.
        let added = json!({ "name": "a", "inner": { "port": 1 } });
        assert!(volatile_change(&added, &old, &paths).is_some());
    }
}
