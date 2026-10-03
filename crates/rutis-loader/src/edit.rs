//! Imperative edits expressed as patches in the editable layer.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::patch::{apply_patches, truthy, ComposedRow, Layer, Owner, Patch};
use crate::LoaderError;

/// One imperative edit. Edits are semantic, so they can be replayed on newer
/// content after a version conflict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Edit {
    /// Add a row. `entry` is the raw row object and must carry an `id`.
    Create {
        entry: Value,
        parent: Option<String>,
        position: Option<usize>,
    },
    Update {
        id: String,
        config: Value,
    },
    SetDisabled {
        id: String,
        disabled: bool,
    },
    /// Replace the row's `inject` (catalog names); `None` removes it.
    SetInject {
        id: String,
        inject: Option<Vec<String>>,
    },
    /// Replace the row's `isolate` (name → `true` or a label); `None`
    /// removes it.
    SetIsolate {
        id: String,
        isolate: Option<Map<String, Value>>,
    },
    Rename {
        id: String,
        name: String,
    },
    Move {
        id: String,
        parent: Option<String>,
        position: Option<usize>,
    },
    Remove {
        id: String,
    },
}

impl Edit {
    /// The row the edit is about.
    pub fn id(&self) -> Option<&str> {
        match self {
            Edit::Create { entry, .. } => entry.get("id").and_then(Value::as_str),
            Edit::Update { id, .. }
            | Edit::SetDisabled { id, .. }
            | Edit::SetInject { id, .. }
            | Edit::SetIsolate { id, .. }
            | Edit::Rename { id, .. }
            | Edit::Move { id, .. }
            | Edit::Remove { id } => Some(id),
        }
    }
}

/// Apply `edit` to `layers[editable]` and return that layer's new patches.
/// The other layers only inform ownership and override checks.
pub fn apply_edit(
    layers: &[Layer],
    editable: usize,
    edit: &Edit,
) -> Result<Vec<Patch>, LoaderError> {
    let composed = apply_patches(layers);
    let mut patches = layers[editable].patches.clone();
    let ctx = Context {
        layers,
        editable,
        rows: &composed.flat,
    };
    match edit {
        Edit::Create {
            entry,
            parent,
            position,
        } => {
            let Some(id) = entry
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                return Err(LoaderError::InvalidEntry("a new entry needs an id".into()));
            };
            if !entry.is_object() {
                return Err(LoaderError::InvalidEntry(
                    "an entry must be an object".into(),
                ));
            }
            if ctx.row(id).is_some() {
                return Err(LoaderError::InvalidEntry(format!("duplicate id {id:?}")));
            }
            ctx.place(&mut patches, entry.clone(), parent.as_deref(), *position)?;
        }
        Edit::Update { id, config } => {
            let row = ctx.require(id)?;
            if row.value.get("group").is_some_and(truthy) {
                return Err(LoaderError::Unsupported(format!(
                    "the config of group {id:?} is its child list; edit the children instead"
                )));
            }
            ctx.set_field(&mut patches, row, "config", config.clone())?;
        }
        Edit::SetDisabled { id, disabled } => {
            let row = ctx.require(id)?;
            ctx.set_field(&mut patches, row, "disabled", Value::Bool(*disabled))?;
        }
        Edit::SetInject { id, inject } => {
            let row = ctx.require(id)?;
            let value = inject.as_ref().map_or(Value::Null, |names| {
                Value::Array(names.iter().cloned().map(Value::String).collect())
            });
            ctx.set_field(&mut patches, row, "inject", value)?;
        }
        Edit::SetIsolate { id, isolate } => {
            let row = ctx.require(id)?;
            let value = isolate.clone().map_or(Value::Null, Value::Object);
            ctx.set_field(&mut patches, row, "isolate", value)?;
        }
        Edit::Rename { id, name } => {
            let row = ctx.require(id)?;
            ctx.owned(
                row,
                "a patch name only asserts the target's name and cannot rename it",
            )?;
            ctx.check_above(row, "name")?;
            // Fold under the old name: assertions written for it hold now.
            fold_overrides(&mut patches, id, row_name(row));
            let object = owned_object(&mut patches, id)?;
            object.insert("name".into(), Value::String(name.clone()));
        }
        Edit::Move {
            id,
            parent,
            position,
        } => {
            let row = ctx.require(id)?;
            ctx.owned(row, "only rows inserted by the editable layer can move")?;
            if let Some(parent) = parent {
                if parent == id || ctx.is_descendant(parent, id) {
                    return Err(LoaderError::InvalidEntry(format!(
                        "cannot move {id:?} into its own subtree"
                    )));
                }
            }
            fold_overrides(&mut patches, id, row_name(row));
            // Patches that address the moved subtree (inserts into its
            // groups, overrides of its rows) must stay after it, or the
            // composition no longer finds their targets.
            let subtree = ctx.subtree_ids(id);
            let object = take_owned(&mut patches, id)?;
            // Positions count the siblings as they are after removal.
            let after: Vec<Layer> = replace_layer(layers, editable, &patches);
            let composed_after = apply_patches(&after);
            let ctx_after = Context {
                layers: &after,
                editable,
                rows: &composed_after.flat,
            };
            ctx_after.place(
                &mut patches,
                Value::Object(object),
                parent.as_deref(),
                *position,
            )?;
            keep_dependents_after(&mut patches, id, &subtree);
        }
        Edit::Remove { id } => {
            let row = ctx.require(id)?;
            ctx.owned(
                row,
                "patches cannot delete rows, it would come back on the next composition; disable it instead",
            )?;
            let removed = ctx.subtree_ids(id);
            take_owned(&mut patches, id)?;
            patches.retain(|patch| {
                !matches!(patch.id.as_deref(), Some(target) if removed.iter().any(|r| r == target))
            });
            patches.retain(|patch| !matches!(&patch.insert, Some(rows) if rows.is_empty()));
        }
    }
    Ok(patches)
}

fn replace_layer(layers: &[Layer], editable: usize, patches: &[Patch]) -> Vec<Layer> {
    let mut layers = layers.to_vec();
    layers[editable].patches = patches.to_vec();
    layers
}

struct Context<'a> {
    layers: &'a [Layer],
    editable: usize,
    rows: &'a [ComposedRow],
}

impl Context<'_> {
    fn row(&self, id: &str) -> Option<&ComposedRow> {
        self.rows.iter().find(|row| row.id.as_deref() == Some(id))
    }

    fn require(&self, id: &str) -> Result<&ComposedRow, LoaderError> {
        self.row(id)
            .ok_or_else(|| LoaderError::UnknownEntry(id.to_owned()))
    }

    fn not_owned(&self, row: &ComposedRow, reason: &str) -> LoaderError {
        LoaderError::NotOwned {
            id: row.id.clone().unwrap_or_default(),
            reason: reason.to_owned(),
        }
    }

    /// Rows inserted by a layer above the editable one, or living inside a
    /// replaced group config, cannot be addressed from the editable layer.
    fn addressable(&self, row: &ComposedRow) -> Result<(), LoaderError> {
        match row.owner {
            Owner::Layer(layer) if layer > self.editable => Err(self.not_owned(
                row,
                &format!(
                    "inserted by layer {:?} above the editable one",
                    self.layers[layer].name
                ),
            )),
            Owner::Replaced(_) => {
                Err(self.not_owned(row, "it lives inside a replaced group config"))
            }
            Owner::Layer(_) => Ok(()),
        }
    }

    fn owned(&self, row: &ComposedRow, reason: &str) -> Result<(), LoaderError> {
        self.addressable(row)?;
        match row.owner {
            Owner::Layer(layer) if layer == self.editable => Ok(()),
            _ => Err(self.not_owned(row, reason)),
        }
    }

    fn check_above(&self, row: &ComposedRow, field: &str) -> Result<(), LoaderError> {
        match row.overridden.get(field) {
            Some(&layer) if layer > self.editable => Err(LoaderError::OverriddenByLayer {
                id: row.id.clone().unwrap_or_default(),
                field: field.to_owned(),
                layer: self.layers[layer].name.clone(),
            }),
            _ => Ok(()),
        }
    }

    fn set_field(
        &self,
        patches: &mut Vec<Patch>,
        row: &ComposedRow,
        field: &str,
        value: Value,
    ) -> Result<(), LoaderError> {
        self.addressable(row)?;
        self.check_above(row, field)?;
        let id = row.id.as_deref().unwrap_or_default();
        let name = row_name(row);
        if row.owner == Owner::Layer(self.editable) {
            fold_overrides(patches, id, name);
            let object = owned_object(patches, id)?;
            if value.is_null() {
                object.remove(field);
            } else {
                object.insert(field.to_owned(), value);
            }
        } else {
            upsert_override(patches, id, name, field, value);
        }
        Ok(())
    }

    fn is_descendant(&self, candidate: &str, ancestor: &str) -> bool {
        let mut current = self.row(candidate).and_then(|row| row.parent.clone());
        while let Some(parent) = current {
            if parent == ancestor {
                return true;
            }
            current = self.row(&parent).and_then(|row| row.parent.clone());
        }
        false
    }

    fn subtree_ids(&self, id: &str) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| row.id.clone())
            .filter(|row| row == id || self.is_descendant(row, id))
            .collect()
    }

    /// Insert `entry` under `parent` (the root when `None`) at `position`
    /// among the composed siblings; `None` appends.
    fn place(
        &self,
        patches: &mut Vec<Patch>,
        entry: Value,
        parent: Option<&str>,
        position: Option<usize>,
    ) -> Result<(), LoaderError> {
        if let Some(parent) = parent {
            let row = self.require(parent)?;
            if !row.value.get("group").is_some_and(truthy) {
                return Err(LoaderError::InvalidEntry(format!(
                    "{parent:?} is not a group"
                )));
            }
            self.addressable(row)?;
        }
        let siblings: Vec<&ComposedRow> = self
            .rows
            .iter()
            .filter(|row| row.parent.as_deref() == parent)
            .collect();
        let append = match position {
            None => true,
            Some(p) if p >= siblings.len() => true,
            Some(_) => false,
        };
        if append {
            patches.push(Patch {
                id: parent.map(str::to_owned),
                insert: Some(vec![entry]),
                ..Patch::default()
            });
            return Ok(());
        }
        let sibling = siblings[position.unwrap()];
        if sibling.owner != Owner::Layer(self.editable) {
            return Err(LoaderError::Unsupported(format!(
                "position {} points at a row the editable layer does not own; only appending is possible there",
                position.unwrap()
            )));
        }
        let sibling_id = sibling.id.clone().unwrap_or_default();
        let (array, index) = locate_mut(patches, &sibling_id)
            .ok_or_else(|| LoaderError::UnknownEntry(sibling_id.clone()))?;
        array.insert(index, entry);
        Ok(())
    }
}

/// Find the array holding the owned row `id` and its index, searching insert
/// lists and, inside them, group child lists.
fn locate_mut<'a>(patches: &'a mut [Patch], id: &str) -> Option<(&'a mut Vec<Value>, usize)> {
    fn search<'b>(array: &'b mut Vec<Value>, id: &str) -> Option<(&'b mut Vec<Value>, usize)> {
        if let Some(index) = array
            .iter()
            .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
        {
            return Some((array, index));
        }
        let child = array.iter().position(|item| {
            item.get("group").is_some_and(truthy)
                && item
                    .get("config")
                    .and_then(Value::as_array)
                    .is_some_and(|children| contains(children, id))
        })?;
        let children = array[child].get_mut("config")?.as_array_mut()?;
        search(children, id)
    }
    let index = patch_index_of(patches, id)?;
    search(patches[index].insert.as_mut()?, id)
}

fn contains(array: &[Value], id: &str) -> bool {
    array.iter().any(|item| {
        item.get("id").and_then(Value::as_str) == Some(id)
            || (item.get("group").is_some_and(truthy)
                && item
                    .get("config")
                    .and_then(Value::as_array)
                    .is_some_and(|children| contains(children, id)))
    })
}

/// Move every patch addressing a row of `subtree` that now precedes the
/// patch carrying `id` to just after it, keeping their relative order.
fn keep_dependents_after(patches: &mut Vec<Patch>, id: &str, subtree: &[String]) {
    let Some(carrier) = patch_index_of(patches, id) else {
        return;
    };
    let addresses = |patch: &Patch| {
        patch
            .id
            .as_deref()
            .is_some_and(|target| subtree.iter().any(|s| s == target))
    };
    let early: Vec<usize> = (0..carrier).filter(|&i| addresses(&patches[i])).collect();
    if early.is_empty() {
        return;
    }
    let mut moved = Vec::new();
    for &index in early.iter().rev() {
        moved.push(patches.remove(index));
    }
    moved.reverse();
    let carrier = carrier - early.len();
    for (offset, patch) in moved.into_iter().enumerate() {
        patches.insert(carrier + 1 + offset, patch);
    }
}

fn row_name(row: &ComposedRow) -> Option<&str> {
    row.value.get("name").and_then(Value::as_str)
}

/// Index of the insert patch that carries the owned row `id`.
fn patch_index_of(patches: &[Patch], id: &str) -> Option<usize> {
    patches.iter().position(|patch| {
        patch
            .insert
            .as_deref()
            .is_some_and(|rows| contains(rows, id))
    })
}

/// Whether a patch's `name` assertion holds for a row named `name`. A
/// failing assertion makes cordis skip the whole patch, so such a patch is
/// inert: it must be neither reused nor folded.
fn asserts(patch: &Patch, name: Option<&str>) -> bool {
    match patch.name.as_deref().filter(|n| !n.is_empty()) {
        None => true,
        Some(asserted) => Some(asserted) == name,
    }
}

fn owned_object<'a>(
    patches: &'a mut [Patch],
    id: &str,
) -> Result<&'a mut Map<String, Value>, LoaderError> {
    let (array, index) =
        locate_mut(patches, id).ok_or_else(|| LoaderError::UnknownEntry(id.to_owned()))?;
    array[index]
        .as_object_mut()
        .ok_or_else(|| LoaderError::InvalidEntry(format!("{id:?} is not an object")))
}

fn take_owned(patches: &mut Vec<Patch>, id: &str) -> Result<Map<String, Value>, LoaderError> {
    let (array, index) =
        locate_mut(patches, id).ok_or_else(|| LoaderError::UnknownEntry(id.to_owned()))?;
    let Value::Object(object) = array.remove(index) else {
        return Err(LoaderError::InvalidEntry(format!(
            "{id:?} is not an object"
        )));
    };
    patches.retain(|patch| !matches!(&patch.insert, Some(rows) if rows.is_empty()));
    Ok(object)
}

/// Merge the editable layer's own override patches for an owned row into
/// its insert, so the row reads as one object and can move freely.
fn fold_overrides(patches: &mut Vec<Patch>, id: &str, name: Option<&str>) {
    let mut fields = Map::new();
    patches.retain(|patch| {
        if patch.insert.is_none() && patch.id.as_deref() == Some(id) && asserts(patch, name) {
            for (key, value) in &patch.overrides {
                fields.insert(key.clone(), value.clone());
            }
            false
        } else {
            true
        }
    });
    if fields.is_empty() {
        return;
    }
    if let Some((array, index)) = locate_mut(patches, id) {
        if let Some(object) = array[index].as_object_mut() {
            for (key, value) in fields {
                if key != "id" {
                    object.insert(key, value);
                }
            }
        }
    }
}

/// Merge `field` into the editable layer's override patch for `id`, reusing
/// only a patch whose name assertion holds for the row.
fn upsert_override(
    patches: &mut Vec<Patch>,
    id: &str,
    name: Option<&str>,
    field: &str,
    value: Value,
) {
    if let Some(patch) = patches.iter_mut().rev().find(|patch| {
        patch.insert.is_none() && patch.id.as_deref() == Some(id) && asserts(patch, name)
    }) {
        patch.overrides.insert(field.to_owned(), value);
        return;
    }
    let mut overrides = Map::new();
    overrides.insert(field.to_owned(), value);
    patches.push(Patch {
        id: Some(id.to_owned()),
        overrides,
        ..Patch::default()
    });
}
