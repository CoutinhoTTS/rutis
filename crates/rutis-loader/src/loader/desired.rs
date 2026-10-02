//! The composed tree as rows the loader can act on.

use std::collections::{BTreeMap, HashMap};

use rutis::{Ctx, TypeKey};
use serde_json::Value;

use crate::catalog::{ExprScope, Expressions, ServiceCatalog};
use crate::patch::{truthy, Composed, Owner, PatchWarning};
use crate::LoaderError;

#[derive(Default)]
pub(super) struct Desired {
    pub(super) rows: Vec<Row>,
    pub(super) by_id: HashMap<String, usize>,
    pub(super) warnings: Vec<PatchWarning>,
    pub(super) issues: Vec<String>,
}

pub(super) struct Row {
    pub(super) id: String,
    pub(super) parent: Option<String>,
    pub(super) value: Value,
    pub(super) name: Option<String>,
    pub(super) group: bool,
    pub(super) owner: Owner,
    pub(super) overridden: BTreeMap<String, usize>,
    /// Evaluated with the loader's root context.
    pub(super) disabled: Result<bool, LoaderError>,
    /// Raw; evaluated per spawn or update in the row's own context.
    pub(super) config: Value,
    pub(super) raw_scope: RawScope,
    /// The scope resolved through the catalog. Rows whose resolver handles
    /// scope itself (`Resolved::foreign_scope`) do without it.
    pub(super) scope: Result<RowScope, LoaderError>,
    pub(super) invalid: Option<LoaderError>,
}

/// The row's `isolate` and `inject`, resolved through the catalog.
#[derive(Clone, Default)]
pub(super) struct RowScope {
    /// (service name, key, label), sorted by name.
    pub(super) isolate: Vec<(String, TypeKey, String)>,
    /// (service name, key), sorted by name.
    pub(super) inject: Vec<(String, TypeKey)>,
}

impl RowScope {
    /// `parent` with every isolate applied.
    pub(super) fn context(&self, parent: &Ctx) -> Ctx {
        self.isolate
            .iter()
            .fold(parent.clone(), |ctx, (_, key, label)| {
                ctx.isolate(key.clone(), label)
            })
    }

    pub(super) fn inject_keys(&self) -> impl Iterator<Item = &TypeKey> {
        self.inject.iter().map(|(_, key)| key)
    }
}

pub(super) fn is_expression(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.len() == 1 && map.get("__jsExpr").is_some_and(Value::is_string))
}

pub(super) fn contains_expression(value: &Value) -> bool {
    match value {
        _ if is_expression(value) => true,
        Value::Array(items) => items.iter().any(contains_expression),
        Value::Object(map) => map.values().any(contains_expression),
        _ => false,
    }
}

/// Evaluates expression nodes in a raw value.
pub(super) struct Eval<'a> {
    pub(super) expressions: Option<&'a dyn Expressions>,
    pub(super) catalog: &'a ServiceCatalog,
}

impl Eval<'_> {
    /// `raw` with every expression node replaced by its value.
    pub(super) fn value(&self, raw: &Value, ctx: Option<&Ctx>) -> Result<Value, LoaderError> {
        if !contains_expression(raw) {
            return Ok(raw.clone());
        }
        let Some(expressions) = self.expressions else {
            return Err(LoaderError::Expression(
                "no expression evaluator is installed".into(),
            ));
        };
        let scope = ExprScope::new(ctx, self.catalog);
        interpolate(raw, &|source| expressions.evaluate(source, &scope))
    }
}

fn interpolate(
    value: &Value,
    evaluate: &dyn Fn(&str) -> Result<Value, LoaderError>,
) -> Result<Value, LoaderError> {
    if is_expression(value) {
        return evaluate(value["__jsExpr"].as_str().unwrap_or_default());
    }
    Ok(match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| interpolate(item, evaluate))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), interpolate(v, evaluate)?)))
                .collect::<Result<_, LoaderError>>()?,
        ),
        other => other.clone(),
    })
}

/// A row's `isolate` and `inject` as written: service names, before the
/// catalog maps them to keys.
#[derive(Clone, Default, PartialEq)]
pub(super) struct RawScope {
    /// (service name, label), sorted by name.
    pub(super) isolate: Vec<(String, String)>,
    /// Service names, sorted.
    pub(super) inject: Vec<String>,
}

impl RawScope {
    /// What identifies the scope: a change means respawning.
    pub(super) fn signature(&self) -> (Vec<(String, String)>, Vec<String>) {
        (self.isolate.clone(), self.inject.clone())
    }

    /// Map the names to keys through the catalog.
    pub(super) fn resolve(&self, catalog: &ServiceCatalog) -> Result<RowScope, LoaderError> {
        let keys = catalog.keys(
            self.isolate
                .iter()
                .map(|(name, _)| name.as_str())
                .chain(self.inject.iter().map(String::as_str)),
        )?;
        let (isolate_keys, inject_keys) = keys.split_at(self.isolate.len());
        Ok(RowScope {
            isolate: self
                .isolate
                .iter()
                .cloned()
                .zip(isolate_keys)
                .map(|((name, label), (_, key))| (name, key.clone(), label))
                .collect(),
            inject: inject_keys.to_vec(),
        })
    }
}

fn parse_scope(id: &str, value: &Value) -> Result<RawScope, LoaderError> {
    let mut isolate_names = Vec::new();
    match value.get("isolate") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (name, spec) in map {
                let label = match spec {
                    Value::Bool(true) => format!("rutis-loader/entry/{id}"),
                    Value::String(label) => format!("rutis-loader/shared/{label}"),
                    Value::Bool(false) | Value::Null => continue,
                    other => {
                        return Err(LoaderError::InvalidEntry(format!(
                            "isolate.{name} of {id:?} must be true or a label, not {other}"
                        )))
                    }
                };
                isolate_names.push((name.clone(), label));
            }
        }
        Some(other) => {
            return Err(LoaderError::InvalidEntry(format!(
                "isolate of {id:?} must be an object, not {other}"
            )))
        }
    }
    let inject_names: Vec<String> = match value.get("inject") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    LoaderError::InvalidEntry(format!("inject of {id:?} must list service names"))
                })
            })
            .collect::<Result<_, _>>()?,
        Some(Value::Object(map)) => {
            // cordis's object form maps a name to intercept config, which
            // rutis does not have; only a bare declaration is accepted.
            for (name, config) in map {
                let bare = matches!(config, Value::Null | Value::Bool(true))
                    || config.as_object().is_some_and(|c| c.is_empty());
                if !bare {
                    return Err(LoaderError::Unsupported(format!(
                        "intercept config for {name:?} in inject of {id:?}"
                    )));
                }
            }
            map.keys().cloned().collect()
        }
        Some(other) => {
            return Err(LoaderError::InvalidEntry(format!(
                "inject of {id:?} must be a list or an object, not {other}"
            )))
        }
    };
    isolate_names.sort();
    let mut inject_names = inject_names;
    inject_names.sort();
    inject_names.dedup();
    Ok(RawScope {
        isolate: isolate_names,
        inject: inject_names,
    })
}

impl Desired {
    /// Read the composed rows. `disabled` expressions are evaluated with
    /// `root`; config expressions are left for spawn time.
    pub(super) fn from_composed(composed: Composed, eval: &Eval<'_>, root: Option<&Ctx>) -> Self {
        let mut desired = Desired {
            warnings: composed.warnings,
            ..Desired::default()
        };
        for flat in composed.flat {
            let Some(id) = flat.id.clone() else {
                desired
                    .issues
                    .push(format!("row without an id skipped: {}", flat.value));
                continue;
            };
            if desired.by_id.contains_key(&id) {
                desired
                    .issues
                    .push(format!("duplicate id {id:?}: the later row is skipped"));
                continue;
            }
            let value = flat.value;
            let group = value.get("group").is_some_and(truthy);
            let name = value.get("name").and_then(Value::as_str).map(str::to_owned);
            let disabled = match value.get("disabled") {
                Some(d) => eval.value(d, root).map(|d| truthy(&d)),
                None => Ok(false),
            };
            let config = if group {
                Value::Null
            } else {
                value.get("config").cloned().unwrap_or(Value::Null)
            };
            let (raw_scope, invalid) = if !group && name.is_none() {
                (
                    RawScope::default(),
                    Some(LoaderError::InvalidEntry(format!("{id:?} has no name"))),
                )
            } else {
                match parse_scope(&id, &value) {
                    Ok(raw) => (raw, None),
                    Err(error) => (RawScope::default(), Some(error)),
                }
            };
            let scope = raw_scope.resolve(eval.catalog);
            desired.by_id.insert(id.clone(), desired.rows.len());
            desired.rows.push(Row {
                id,
                parent: flat.parent,
                value,
                name,
                group,
                owner: flat.owner,
                overridden: flat.overridden,
                disabled,
                config,
                raw_scope,
                scope,
                invalid,
            });
        }
        desired
    }

    pub(super) fn row(&self, id: &str) -> Option<&Row> {
        self.by_id.get(id).map(|&i| &self.rows[i])
    }

    /// The row and every enclosing group are enabled and valid.
    pub(super) fn wanted(&self, row: &Row) -> bool {
        if row.invalid.is_some() || !matches!(row.disabled, Ok(false)) {
            return false;
        }
        match &row.parent {
            None => true,
            Some(parent) => self.row(parent).is_some_and(|p| self.wanted(p)),
        }
    }
}
