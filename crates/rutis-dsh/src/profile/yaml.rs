//! The cordis entry-list YAML dialect: js-yaml's JSON/core schema plus the
//! `!!js` scalar, which becomes a `{ "__jsExpr": "<source>" }` node and is
//! written back as `!!js`.
//!
//! Patch lists are top-level sequences. [`Document`] keeps the source text of
//! each top-level item, so rewriting a list keeps the comments and layout of
//! every item that did not change.

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};
use serde_json::{Map, Number, Value};

#[derive(Debug, thiserror::Error)]
pub enum YamlError {
    #[error("{0}")]
    Syntax(String),
    #[error("unsupported tag !{0}")]
    Tag(String),
}

const JS_EXPR: &str = "__jsExpr";

fn is_js_tag(tag: &Tag) -> bool {
    tag.handle == "tag:yaml.org,2002:" && tag.suffix == "js"
}

/// Resolve a plain scalar the way js-yaml's core schema does.
fn resolve_plain(text: &str) -> Value {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    if let Some(number) = resolve_number(text) {
        return number;
    }
    Value::String(text.to_owned())
}

fn resolve_number(text: &str) -> Option<Value> {
    if matches!(
        text,
        ".inf"
            | ".Inf"
            | ".INF"
            | "+.inf"
            | "+.Inf"
            | "+.INF"
            | "-.inf"
            | "-.Inf"
            | "-.INF"
            | ".nan"
            | ".NaN"
            | ".NAN"
    ) {
        // JavaScript numbers that JSON writes as null.
        return Some(Value::Null);
    }
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let signed = |n: i64| if negative { -n } else { n };
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = digits.strip_prefix(prefix) {
            return i64::from_str_radix(rest, radix)
                .ok()
                .map(|n| Value::Number(Number::from(signed(n))));
        }
    }
    if digits.is_empty()
        || !digits
            .bytes()
            .all(|b| b.is_ascii_digit() || b".eE+-".contains(&b))
    {
        return None;
    }
    let bytes = digits.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i;
    let mut frac_digits = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if i != bytes.len() {
        return None;
    }
    let value: f64 = format!("{}{digits}", if negative { "-" } else { "" })
        .parse()
        .ok()?;
    Some(js_number(value))
}

/// A JavaScript number as JSON: integral values are integers.
fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// Parse one YAML document into JSON values.
pub fn parse(source: &str) -> Result<Value, YamlError> {
    Ok(parse_spanned(source)?.0)
}

/// The value plus, for a top-level sequence, the start line (0-based) of
/// each item and whether the sequence is in block style.
fn parse_spanned(source: &str) -> Result<(Value, Vec<usize>, bool), YamlError> {
    enum Frame {
        Seq(Vec<Value>, usize),
        Map(Map<String, Value>, Option<String>, usize),
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut anchors: HashMap<usize, Value> = HashMap::new();
    let mut root = None;
    let mut item_lines = Vec::new();
    let mut block = true;

    fn push(
        stack: &mut [Frame],
        root: &mut Option<Value>,
        anchors: &mut HashMap<usize, Value>,
        anchor: usize,
        value: Value,
    ) -> Result<(), YamlError> {
        if anchor > 0 {
            anchors.insert(anchor, value.clone());
        }
        match stack.last_mut() {
            None => *root = Some(value),
            Some(Frame::Seq(items, _)) => items.push(value),
            Some(Frame::Map(map, key, _)) => match key.take() {
                None => {
                    return Err(YamlError::Syntax(
                        "complex mapping keys are not supported".into(),
                    ))
                }
                Some(k) => {
                    map.insert(k, value);
                }
            },
        }
        Ok(())
    }

    let line_of = |span: &Span| span.start.line().saturating_sub(1);
    for event in Parser::new_from_str(source) {
        let (event, span) = event.map_err(|e| YamlError::Syntax(e.to_string()))?;
        // A node starting directly in the top-level sequence is an item.
        let top_item = matches!(stack.as_slice(), [Frame::Seq(_, _)])
            && matches!(
                event,
                Event::Scalar(..)
                    | Event::SequenceStart(..)
                    | Event::MappingStart(..)
                    | Event::Alias(_)
            );
        if top_item {
            item_lines.push(line_of(&span));
        }
        match event {
            Event::Scalar(text, style, anchor, tag) => {
                // A mapping key is its text.
                if let Some(Frame::Map(_, key @ None, _)) = stack.last_mut() {
                    *key = Some(text.into_owned());
                    continue;
                }
                let value = match tag.as_deref() {
                    Some(tag) if is_js_tag(tag) => {
                        let mut node = Map::new();
                        node.insert(JS_EXPR.into(), Value::String(text.into_owned()));
                        Value::Object(node)
                    }
                    Some(tag) if tag.handle == "tag:yaml.org,2002:" && tag.suffix == "str" => {
                        Value::String(text.into_owned())
                    }
                    Some(tag) if tag.handle == "tag:yaml.org,2002:" => resolve_plain(&text),
                    Some(tag) => {
                        return Err(YamlError::Tag(format!("{}{}", tag.handle, tag.suffix)))
                    }
                    None if style == ScalarStyle::Plain => resolve_plain(&text),
                    None => Value::String(text.into_owned()),
                };
                push(&mut stack, &mut root, &mut anchors, anchor, value)?;
            }
            Event::SequenceStart(anchor, tag) => {
                if let Some(tag) = tag {
                    if !(tag.handle == "tag:yaml.org,2002:" && tag.suffix == "seq") {
                        return Err(YamlError::Tag(format!("{}{}", tag.handle, tag.suffix)));
                    }
                }
                if stack.is_empty() {
                    block = source_char_at(source, span.start.index()) != Some('[');
                }
                stack.push(Frame::Seq(Vec::new(), anchor));
            }
            Event::SequenceEnd => {
                let Some(Frame::Seq(items, anchor)) = stack.pop() else {
                    return Err(YamlError::Syntax("unbalanced sequence".into()));
                };
                push(
                    &mut stack,
                    &mut root,
                    &mut anchors,
                    anchor,
                    Value::Array(items),
                )?;
            }
            Event::MappingStart(anchor, tag) => {
                if let Some(tag) = tag {
                    if !(tag.handle == "tag:yaml.org,2002:" && tag.suffix == "map") {
                        return Err(YamlError::Tag(format!("{}{}", tag.handle, tag.suffix)));
                    }
                }
                if matches!(stack.last(), Some(Frame::Map(_, None, _))) {
                    return Err(YamlError::Syntax(
                        "complex mapping keys are not supported".into(),
                    ));
                }
                stack.push(Frame::Map(Map::new(), None, anchor));
            }
            Event::MappingEnd => {
                let Some(Frame::Map(map, _, anchor)) = stack.pop() else {
                    return Err(YamlError::Syntax("unbalanced mapping".into()));
                };
                push(
                    &mut stack,
                    &mut root,
                    &mut anchors,
                    anchor,
                    Value::Object(map),
                )?;
            }
            Event::Alias(id) => {
                let value = anchors
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| YamlError::Syntax("unknown alias".into()))?;
                if let Some(Frame::Map(_, key @ None, _)) = stack.last_mut() {
                    *key = Some(match value {
                        Value::String(s) => s,
                        other => other.to_string(),
                    });
                    continue;
                }
                push(&mut stack, &mut root, &mut anchors, 0, value)?;
            }
            Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart(_)
            | Event::DocumentEnd
            | Event::Nothing => {}
        }
    }
    Ok((root.unwrap_or(Value::Null), item_lines, block))
}

fn source_char_at(source: &str, index: usize) -> Option<char> {
    source.chars().nth(index)
}

// ── writing ─────────────────────────────────────────────────────

fn needs_quotes(text: &str) -> bool {
    if text.is_empty() || text != text.trim() || text.contains('\n') {
        return true;
    }
    if !matches!(resolve_plain(text), Value::String(_)) {
        return true;
    }
    let first = text.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return true;
    }
    text.contains(": ") || text.contains(" #") || text.ends_with(':') || text.contains('\t')
}

fn scalar(text: &str) -> String {
    if needs_quotes(text) {
        serde_json::to_string(text).unwrap()
    } else {
        text.to_owned()
    }
}

fn expression(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) if map.len() == 1 => map.get(JS_EXPR).and_then(Value::as_str),
        _ => None,
    }
}

fn inline(value: &Value) -> Option<String> {
    if let Some(source) = expression(value) {
        return Some(format!("!!js {}", scalar(source)));
    }
    Some(match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => scalar(s),
        Value::Array(items) if items.is_empty() => "[]".into(),
        Value::Object(map) if map.is_empty() => "{}".into(),
        _ => return None,
    })
}

/// Append `value` as the body of a node at `indent`.
fn block(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Array(items) => {
            for item in items {
                out.push_str(&pad);
                out.push('-');
                match inline(item) {
                    Some(text) => {
                        out.push(' ');
                        out.push_str(&text);
                        out.push('\n');
                    }
                    None => {
                        // Nested block: first line continues after "- ".
                        let mut nested = String::new();
                        block(item, indent + 2, &mut nested);
                        out.push(' ');
                        out.push_str(nested.trim_start());
                    }
                }
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                out.push_str(&pad);
                out.push_str(&scalar(key));
                out.push(':');
                match inline(value) {
                    Some(text) => {
                        out.push(' ');
                        out.push_str(&text);
                        out.push('\n');
                    }
                    None => {
                        out.push('\n');
                        let child = if value.is_array() { indent } else { indent + 2 };
                        block(value, child, out);
                    }
                }
            }
        }
        other => {
            out.push_str(&pad);
            out.push_str(&inline(other).unwrap_or_default());
            out.push('\n');
        }
    }
}

/// One top-level sequence item, as block YAML starting with `- `.
pub fn emit_item(value: &Value) -> String {
    let mut out = String::new();
    block(&Value::Array(vec![value.clone()]), 0, &mut out);
    out
}

/// A top-level YAML sequence with each item's source text.
#[derive(Debug, Clone)]
pub struct Document {
    /// Text before the first item (comments, the `[]` of an empty flow list
    /// excluded).
    header: String,
    items: Vec<(String, Value)>,
}

impl Document {
    /// Read a document whose root is a sequence (or empty).
    pub fn parse(source: &str) -> Result<Self, YamlError> {
        let (value, starts, block) = parse_spanned(source)?;
        let items = match value {
            Value::Array(items) => items,
            Value::Null => Vec::new(),
            _ => {
                return Err(YamlError::Syntax(
                    "the document must be a top-level list".into(),
                ))
            }
        };
        let lines: Vec<&str> = source.split_inclusive('\n').collect();
        if !block || items.is_empty() || starts.len() != items.len() {
            // Keep the leading comment block; the rest is rewritten.
            let header: String = lines
                .iter()
                .take_while(|l| {
                    let t = l.trim();
                    t.is_empty() || t.starts_with('#')
                })
                .copied()
                .collect();
            let texts = items.iter().map(emit_item).collect::<Vec<_>>();
            return Ok(Self {
                header,
                items: texts.into_iter().zip(items).collect(),
            });
        }
        // Comments and blank lines directly above an item belong to it.
        let mut bounds: Vec<usize> = starts
            .iter()
            .map(|&start| {
                let mut line = start;
                while line > 0 {
                    let t = lines[line - 1].trim();
                    if t.is_empty() || t.starts_with('#') {
                        line -= 1;
                    } else {
                        break;
                    }
                }
                line
            })
            .collect();
        // The first item's comment block stays the file header.
        bounds[0] = starts[0];
        let header: String = lines[..bounds[0]].concat();
        let mut texts = Vec::new();
        for (i, &start) in bounds.iter().enumerate() {
            let end = bounds.get(i + 1).copied().unwrap_or(lines.len());
            let mut text: String = lines[start..end].concat();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            texts.push(text);
        }
        Ok(Self {
            header,
            items: texts.into_iter().zip(items).collect(),
        })
    }

    pub fn values(&self) -> Vec<Value> {
        self.items.iter().map(|(_, v)| v.clone()).collect()
    }

    /// Render `values`, reusing the text of items whose value is unchanged
    /// (compared after `normalize`), in order.
    pub fn render(&self, values: &[Value], normalize: impl Fn(&Value) -> Value) -> String {
        let mut used = vec![false; self.items.len()];
        let old: Vec<Value> = self.items.iter().map(|(_, v)| normalize(v)).collect();
        let mut out = self.header.clone();
        if values.is_empty() {
            out.push_str("[]\n");
            return out;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        let mut from = 0;
        for value in values {
            let found = (from..old.len())
                .chain(0..from.min(old.len()))
                .find(|&i| !used[i] && old[i] == *value);
            match found {
                Some(i) => {
                    used[i] = true;
                    from = i + 1;
                    out.push_str(&self.items[i].0);
                }
                None => out.push_str(&emit_item(value)),
            }
        }
        out
    }

    /// Render `values` without reusing any item text.
    pub fn render_fresh(&self, values: &[Value]) -> String {
        let fresh = Document {
            header: self.header.clone(),
            items: Vec::new(),
        };
        fresh.render(values, |v| v.clone())
    }
}
