//! The composed tree as rows the loader can act on.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

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
    pub(super) disabled: Result<bool, LoaderError>,
    pub(super) config: Value,
    pub(super) invalid: Option<LoaderError>,
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

impl Desired {
    pub(super) fn from_composed(composed: Composed) -> Self {
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
                Some(d) if is_expression(d) => Err(LoaderError::Expression(
                    "no expression evaluator is installed".into(),
                )),
                Some(d) => Ok(truthy(d)),
                None => Ok(false),
            };
            let config = if group {
                Value::Null
            } else {
                value.get("config").cloned().unwrap_or(Value::Null)
            };
            let invalid = if !group && name.is_none() {
                Some(LoaderError::InvalidEntry(format!("{id:?} has no name")))
            } else if ["inject", "isolate"]
                .iter()
                .any(|key| value.get(*key).is_some_and(|v| !v.is_null()))
            {
                Some(LoaderError::Unsupported(
                    "inject / isolate in the config need the service catalog".into(),
                ))
            } else if contains_expression(&config) {
                Some(LoaderError::Expression(
                    "no expression evaluator is installed".into(),
                ))
            } else {
                None
            };
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
