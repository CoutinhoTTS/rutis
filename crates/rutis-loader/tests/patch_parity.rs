//! `apply_patches` against the JavaScript original.
//!
//! `fixtures/patch-expected.json` is the output of cordis-plugin-include's
//! `applyEntryPatches([], layers.flat())` on `fixtures/patch-cases.json`,
//! generated with the dsh-vendored package (see the README in fixtures).

use rutis_loader::{apply_patches, Layer, Owner, Patch};
use serde_json::{json, Value};

fn layers(value: &Value) -> Vec<Layer> {
    value
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, patches)| {
            let patches: Vec<Patch> = serde_json::from_value(patches.clone()).unwrap();
            Layer::new(format!("layer{i}"), patches)
        })
        .collect()
}

#[test]
fn matches_cordis_apply_entry_patches() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/patch-cases.json")).unwrap();
    let expected: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/patch-expected.json")).unwrap();
    assert_eq!(cases.len(), expected.len());
    for (case, expected) in cases.iter().zip(&expected) {
        let name = case["name"].as_str().unwrap();
        let composed = apply_patches(&layers(&case["layers"]));
        assert_eq!(
            Value::Array(composed.rows.clone()),
            expected["rows"],
            "rows differ: {name}"
        );
        assert_eq!(
            composed.warnings.len() as u64,
            expected["warnings"].as_u64().unwrap(),
            "warning count differs: {name}: {:?}",
            composed.warnings
        );
    }
}

#[test]
fn inputs_are_not_modified_and_dropping_a_layer_reverts() {
    let base = Layer::new(
        "base",
        serde_json::from_value(json!([{ "insert": [{ "id": "a", "name": "pa", "config": { "x": 1 } }] }]))
            .unwrap(),
    );
    let user = Layer::new(
        "user",
        serde_json::from_value(json!([{ "id": "a", "config": { "x": 2 } }])).unwrap(),
    );
    let before = (base.clone(), user.clone());
    let both = apply_patches(&[base.clone(), user.clone()]);
    assert_eq!(both.rows[0]["config"], json!({ "x": 2 }));
    assert_eq!((base.clone(), user), before);
    let only_base = apply_patches(&[base]);
    assert_eq!(only_base.rows[0]["config"], json!({ "x": 1 }));
}

#[test]
fn flat_rows_carry_owner_parent_and_overrides() {
    let composed = apply_patches(&[
        Layer::new(
            "base",
            serde_json::from_value(json!([{ "insert": [
                { "id": "g", "name": "grp", "group": true, "config": [{ "id": "c", "name": "pc" }] }
            ] }]))
            .unwrap(),
        ),
        Layer::new(
            "user",
            serde_json::from_value(json!([
                { "id": "c", "disabled": true },
                { "insert": [{ "id": "u", "name": "pu" }], "id": "g" }
            ]))
            .unwrap(),
        ),
        Layer::new(
            "overlay",
            serde_json::from_value(json!([{ "id": "g", "config": [{ "id": "r", "name": "pr" }] }]))
                .unwrap(),
        ),
    ]);
    let summary: Vec<_> = composed
        .flat
        .iter()
        .map(|row| (row.id.clone().unwrap(), row.parent.clone(), row.owner.clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("g".to_owned(), None, Owner::Layer(0)),
            ("r".to_owned(), Some("g".to_owned()), Owner::Replaced(2)),
        ]
    );
    assert_eq!(composed.flat[0].overridden.get("config"), Some(&2));
    assert!(composed.warnings.is_empty());
}
