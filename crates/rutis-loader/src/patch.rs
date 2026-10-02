//! Patch layers and their composition into the desired entry tree.
//!
//! [`apply_patches`] is a port of `applyEntryPatches` from
//! `@deepseek-ai/cordis-plugin-include`, including its quirks: the id index is
//! built once and only extended by inserted rows, so rows that arrive through
//! a whole-field `config` replacement are invisible to later patches, and
//! patches aimed at rows detached by such a replacement silently change
//! nothing. Entries are kept in an arena so that this reference behaviour
//! matches the JavaScript object graph.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One patch, in the shape of cordis-plugin-include's `PatchOptions`.
///
/// - `insert` present (even empty): append the rows, to the root or, with
///   `id`, to that group;
/// - otherwise: replace each field in `overrides` on the row `id`; a `name`
///   only asserts the target's name and is never written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub overrides: Map<String, Value>,
}

/// An ordered source of patches; later layers win.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layer {
    pub name: String,
    pub patches: Vec<Patch>,
}

impl Layer {
    pub fn new(name: impl Into<String>, patches: Vec<Patch>) -> Self {
        Self {
            name: name.into(),
            patches,
        }
    }
}

/// A patch that could not be applied. Like cordis, these never fail the
/// composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchWarning {
    pub layer: String,
    /// Index of the patch inside its layer.
    pub index: usize,
    pub message: String,
}

/// Where a reachable row came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// Inserted by the patch layer with this index.
    Layer(usize),
    /// A plain value inside a group `config` that the layer with this index
    /// replaced as a whole. Such rows are not individually addressable.
    Replaced(usize),
}

/// One row of the composed tree, flattened in tree order.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposedRow {
    /// The row as composed, with a group's children left inside its `config`.
    pub value: Value,
    /// The row's id, when it has a non-empty string id.
    pub id: Option<String>,
    /// Id of the enclosing group row; `None` at the root.
    pub parent: Option<String>,
    pub owner: Owner,
    /// Field → index of the last layer whose patch replaced it.
    pub overridden: BTreeMap<String, usize>,
}

/// The composed desired tree.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composed {
    /// Root rows, ready to serialize like cordis's entry list.
    pub rows: Vec<Value>,
    /// Every reachable row in depth-first tree order.
    pub flat: Vec<ComposedRow>,
    pub warnings: Vec<PatchWarning>,
}

enum Config {
    Absent,
    Value(Value),
    Children(Vec<Child>),
}

enum Child {
    Node(usize),
    Raw(Value),
}

struct Node {
    fields: Map<String, Value>,
    config: Config,
    owner: Owner,
    overridden: BTreeMap<String, usize>,
}

struct Arena {
    nodes: Vec<Node>,
    index: HashMap<String, usize>,
}

/// JavaScript truthiness of a JSON value.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

impl Arena {
    /// `buildMap` over freshly inserted values: every object becomes a node
    /// and is indexed by id; a group's array `config` is walked recursively.
    fn build(&mut self, value: Value, layer: usize) -> Child {
        let Value::Object(mut fields) = value else {
            return Child::Raw(value);
        };
        let config = match fields.remove("config") {
            None => Config::Absent,
            Some(Value::Array(items)) if fields.get("group").is_some_and(truthy) => Config::Children(
                items.into_iter().map(|item| self.build(item, layer)).collect(),
            ),
            Some(other) => Config::Value(other),
        };
        let id = match fields.get("id") {
            Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
            _ => None,
        };
        let node = self.nodes.len();
        self.nodes.push(Node {
            fields,
            config,
            owner: Owner::Layer(layer),
            overridden: BTreeMap::new(),
        });
        if let Some(id) = id {
            self.index.insert(id, node);
        }
        Child::Node(node)
    }

    fn to_value(&self, child: &Child) -> Value {
        match child {
            Child::Raw(value) => value.clone(),
            Child::Node(node) => {
                let node = &self.nodes[*node];
                let mut object = node.fields.clone();
                match &node.config {
                    Config::Absent => {}
                    Config::Value(value) => {
                        object.insert("config".into(), value.clone());
                    }
                    Config::Children(children) => {
                        let items = children.iter().map(|c| self.to_value(c)).collect();
                        object.insert("config".into(), Value::Array(items));
                    }
                }
                Value::Object(object)
            }
        }
    }

    /// `raw_layer` owns any plain values among `children` (the layer that
    /// replaced the enclosing group's `config`).
    fn flatten(
        &self,
        children: &[Child],
        parent: Option<&str>,
        raw_layer: usize,
        out: &mut Vec<ComposedRow>,
    ) {
        for child in children {
            match child {
                Child::Node(index) => {
                    let node = &self.nodes[*index];
                    let id = string_id(node.fields.get("id"));
                    out.push(ComposedRow {
                        value: self.to_value(child),
                        id: id.clone(),
                        parent: parent.map(str::to_owned),
                        owner: node.owner.clone(),
                        overridden: node.overridden.clone(),
                    });
                    let config_layer = node.overridden.get("config").copied().unwrap_or(0);
                    match &node.config {
                        Config::Children(children) => {
                            self.flatten(children, id.as_deref(), config_layer, out)
                        }
                        Config::Value(Value::Array(items))
                            if node.fields.get("group").is_some_and(truthy) =>
                        {
                            flatten_raw(items, id.as_deref(), config_layer, out)
                        }
                        _ => {}
                    }
                }
                Child::Raw(value) => {
                    flatten_raw(std::slice::from_ref(value), parent, raw_layer, out)
                }
            }
        }
    }
}

fn flatten_raw(items: &[Value], parent: Option<&str>, layer: usize, out: &mut Vec<ComposedRow>) {
    for item in items {
        let Value::Object(fields) = item else {
            continue;
        };
        let id = string_id(fields.get("id"));
        out.push(ComposedRow {
            value: item.clone(),
            id: id.clone(),
            parent: parent.map(str::to_owned),
            owner: Owner::Replaced(layer),
            overridden: BTreeMap::new(),
        });
        if fields.get("group").is_some_and(truthy) {
            if let Some(Value::Array(children)) = fields.get("config") {
                flatten_raw(children, id.as_deref(), layer, out);
            }
        }
    }
}

fn string_id(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        _ => None,
    }
}

/// Compose patch layers over an empty root, in order.
///
/// Inputs are not modified, so dropping or changing a layer and composing
/// again reverts cleanly.
pub fn apply_patches(layers: &[Layer]) -> Composed {
    let mut arena = Arena {
        nodes: Vec::new(),
        index: HashMap::new(),
    };
    let mut root: Vec<Child> = Vec::new();
    let mut warnings = Vec::new();
    let mut warn = |layer: usize, index: usize, message: String| {
        warnings.push(PatchWarning {
            layer: layers[layer].name.clone(),
            index,
            message,
        })
    };

    for (layer_index, layer) in layers.iter().enumerate() {
        for (patch_index, patch) in layer.patches.iter().enumerate() {
            let id = patch.id.as_deref().filter(|id| !id.is_empty());
            if let Some(insert) = &patch.insert {
                let built: Vec<Child> = insert
                    .iter()
                    .map(|value| arena.build(value.clone(), layer_index))
                    .collect();
                match id {
                    None => root.extend(built),
                    Some(id) => {
                        let Some(&target) = arena.index.get(id) else {
                            warn(layer_index, patch_index, format!("patch insert: entry {id:?} not found"));
                            continue;
                        };
                        let node = &mut arena.nodes[target];
                        if !node.fields.get("group").is_some_and(truthy) {
                            warn(layer_index, patch_index, format!("patch insert: entry {id:?} is not a group"));
                            continue;
                        }
                        let config = std::mem::replace(&mut node.config, Config::Absent);
                        let mut children = match config {
                            Config::Children(children) => children,
                            Config::Value(Value::Array(items)) => {
                                items.into_iter().map(Child::Raw).collect()
                            }
                            Config::Value(_) | Config::Absent => Vec::new(),
                        };
                        children.extend(built);
                        node.config = Config::Children(children);
                    }
                }
                continue;
            }
            let Some(id) = id else {
                warn(layer_index, patch_index, "patch: id is required for non-insert patches".into());
                continue;
            };
            let Some(&target) = arena.index.get(id) else {
                warn(layer_index, patch_index, format!("patch: entry {id:?} not found"));
                continue;
            };
            let node = &mut arena.nodes[target];
            if let Some(name) = patch.name.as_deref().filter(|name| !name.is_empty()) {
                let current = node.fields.get("name");
                if current != Some(&Value::String(name.to_owned())) {
                    let expected = current.cloned().unwrap_or(Value::Null);
                    warn(
                        layer_index,
                        patch_index,
                        format!("patch: name mismatch for {id:?} (expected {expected}, got {name:?}), skipping"),
                    );
                    continue;
                }
            }
            for (key, value) in &patch.overrides {
                if key == "id" {
                    continue;
                }
                if key == "config" {
                    node.config = Config::Value(value.clone());
                } else {
                    node.fields.insert(key.clone(), value.clone());
                }
                node.overridden.insert(key.clone(), layer_index);
            }
        }
    }

    let rows = root.iter().map(|child| arena.to_value(child)).collect();
    let mut flat = Vec::new();
    arena.flatten(&root, None, 0, &mut flat);
    Composed {
        rows,
        flat,
        warnings,
    }
}
