//! The entry-list YAML dialect against js-yaml, on every bundle patch file
//! dsh ships, plus round trips through `Document`.

mod support;

use rutis_dsh::profile::yaml::{self, Document};
use serde_json::{json, Value};

#[test]
fn parses_like_js_yaml() {
    let Some(files) = support::bundle_patch_files() else {
        eprintln!("skipped: the dsh npm project is not installed");
        return;
    };
    assert!(files.len() > 5, "{files:?}");
    let expected = support::node(&["yaml"], files.iter().map(|f| f.to_str().unwrap()));
    for case in expected.as_array().unwrap() {
        let file = case["file"].as_str().unwrap();
        let ours = yaml::parse(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(ours, case["value"], "{file}");
    }
}

#[test]
fn rewriting_unchanged_items_keeps_the_text() {
    let Some(files) = support::bundle_patch_files() else {
        return;
    };
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        let doc = Document::parse(&source).unwrap();
        let values = doc.values();
        assert_eq!(
            doc.render(&values, |v| v.clone()),
            source,
            "{}",
            file.display()
        );
        // Fresh output reads back to the same values.
        let fresh = doc.render_fresh(&values);
        assert_eq!(
            yaml::parse(&fresh).unwrap(),
            Value::Array(values),
            "{}",
            file.display()
        );
    }
}

#[test]
fn scalars_and_expressions_like_js_yaml() {
    if support::project().is_none() {
        return;
    }
    let source = "\
- a: !!js process.cwd()
  b: !!js \"x ?? 'y'\"
  c: yes
  d: ~
  e: 0x1F
  f: 1e3
  g: '010'
  h: 010
  i: [1, two, {k: v}]
  'j k': true
  l: 1_000
  m: .5
  n: +12
  o: 0o17
  p: -0.5e-2
  q: .inf
  r: 1.
  s: \"multi\\nline\"
  t: !!str 12
  u: &x {a: 1}
  v: *x
";
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s.yml");
    std::fs::write(&file, source).unwrap();
    let expected = support::node(&["yaml"], [file.to_str().unwrap()]);
    assert_eq!(yaml::parse(source).unwrap(), expected[0]["value"]);
    assert_eq!(
        yaml::parse(source).unwrap()[0]["a"],
        json!({ "__jsExpr": "process.cwd()" })
    );
}

#[test]
fn edits_keep_comments_of_untouched_items() {
    let source = "\
# Your patch layer.
# Two lines of header.

# about a
- id: a
  disabled: true   # trailing note

# about b
- id: b
  config:
    level: 1
";
    let doc = Document::parse(source).unwrap();
    let mut values = doc.values();
    values[1] =
        json!({ "id": "b", "config": { "level": 2, "expr": { "__jsExpr": "process.env.X" } } });
    values.push(json!({ "insert": [{ "id": "c", "name": "@scope/pkg", "config": {} }] }));
    let rendered = doc.render(&values, |v| v.clone());
    assert!(rendered.starts_with("# Your patch layer.\n# Two lines of header.\n\n# about a\n- id: a\n  disabled: true   # trailing note\n"), "{rendered}");
    assert!(
        !rendered.contains("# about b"),
        "the changed item is rewritten: {rendered}"
    );
    assert_eq!(yaml::parse(&rendered).unwrap(), Value::Array(values));

    // An empty template keeps its header.
    let empty = Document::parse("# header\n[]\n").unwrap();
    assert_eq!(empty.render(&[], |v| v.clone()), "# header\n[]\n");
    let one = empty.render(&[json!({ "id": "x", "disabled": true })], |v| v.clone());
    assert_eq!(one, "# header\n- id: x\n  disabled: true\n");
}
