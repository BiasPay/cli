//! Builds a request's parameters from flags in path notation (`--a.b[0].c=3`) and loose JSON,
//! typing each value by the schema at its path.

use crate::loose;
use crate::manifest::{self, child, enum_values, items, kinds, properties_of};
use serde_json::{Map, Number, Value};
use std::io::Read;

/// The internal marker for a file to upload; a multipart body sends its bytes.
pub const FILE_MARKER: &str = "$file";

#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Key(String),
    Index(usize),
    /// `[]`: the next item of an array.
    Append,
    /// `.0`: an array index, or a key in an object such as metadata. The schema decides.
    Number(String),
}

pub fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let chars: Vec<char> = path.chars().collect();
    let mut i = 0;
    let mut expect_key = true;
    while i < chars.len() {
        match chars[i] {
            '.' if !expect_key => {
                i += 1;
                expect_key = true;
            }
            '[' => {
                let close = chars[i..]
                    .iter()
                    .position(|c| *c == ']')
                    .map(|p| p + i)
                    .ok_or_else(|| format!("`{path}` has an unclosed `[`"))?;
                let inner: String = chars[i + 1..close].iter().collect();
                let inner = inner.trim();
                if inner.is_empty() {
                    segments.push(Segment::Append);
                } else if let Some(quoted) = inner
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                {
                    segments.push(Segment::Key(quoted.to_owned()));
                } else {
                    let index = inner.parse::<usize>().map_err(|_| {
                        format!(
                            "`[{inner}]` in `{path}` isn't an index; quote a key as [\"{inner}\"]"
                        )
                    })?;
                    segments.push(Segment::Index(index));
                }
                i = close + 1;
                expect_key = false;
            }
            _ if expect_key => {
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                let key: String = chars[start..i].iter().collect();
                if key.is_empty() {
                    return Err(format!("`{path}` has an empty key"));
                }
                segments.push(
                    if key.chars().all(|c| c.is_ascii_digit()) && !segments.is_empty() {
                        Segment::Number(key)
                    } else {
                        Segment::Key(key)
                    },
                );
                expect_key = false;
            }
            c => return Err(format!("unexpected `{c}` in `{path}`")),
        }
    }
    if expect_key {
        return Err(if path.is_empty() {
            "empty flag name".to_owned()
        } else {
            format!("`{path}` ends with `.`")
        });
    }
    Ok(segments)
}

/// A value from the command line, before the schema types it.
pub enum Input {
    /// `--flag` with no value: true, for a boolean.
    Present,
    Text(String),
}

/// Accumulates parameters. `schema` is `None` for raw requests, which type values by their look.
pub struct Params<'a> {
    pub root: Map<String, Value>,
    schema: Option<&'a Value>,
}

impl<'a> Params<'a> {
    pub fn new(schema: Option<&'a Value>) -> Self {
        Params {
            root: Map::new(),
            schema,
        }
    }

    /// Merges a whole object, such as a positional `'{name: "john"}'` body.
    pub fn merge(&mut self, value: Value) -> Result<(), String> {
        let Value::Object(map) = value else {
            return Err("the body must be an object, like '{name: \"john\"}'".into());
        };
        if let Some(schema) = self.schema {
            for key in map.keys() {
                if child(schema, key).is_none() {
                    return Err(unknown_field(&format!("`{key}`"), key, schema));
                }
            }
        }
        let mut root = Value::Object(std::mem::take(&mut self.root));
        merge_into(&mut root, Value::Object(map));
        let Value::Object(map) = root else {
            unreachable!()
        };
        self.root = map;
        Ok(())
    }

    /// Sets the value at a flag's path, e.g. `nested[0].property` from `--nested[0].property`.
    pub fn set(&mut self, flag: &str, input: Input) -> Result<(), String> {
        let path = parse_path(flag).map_err(|e| format!("--{flag}: {e}"))?;
        let mut slot_root = Value::Object(std::mem::take(&mut self.root));
        let result = assign(&mut slot_root, self.schema, flag, &path, input);
        let Value::Object(map) = slot_root else {
            unreachable!()
        };
        self.root = map;
        result
    }

    /// The schema at a flag path, for deciding whether `--flag value` takes the next word.
    pub fn schema_at(&self, flag: &str) -> Option<&'a Value> {
        let path = parse_path(flag).ok()?;
        let mut schema = self.schema?;
        for segment in &path {
            schema = match segment {
                Segment::Key(k) => child(schema, k)?,
                Segment::Index(_) | Segment::Append => items(schema)?,
                Segment::Number(n) => {
                    if is_array_schema(schema) {
                        items(schema)?
                    } else {
                        child(schema, n)?
                    }
                }
            };
        }
        Some(schema)
    }
}

fn assign(
    root: &mut Value,
    root_schema: Option<&Value>,
    flag: &str,
    path: &[Segment],
    input: Input,
) -> Result<(), String> {
    let mut slot = root;
    let mut schema = root_schema;
    let mut walked = String::new();
    for (i, segment) in path.iter().enumerate() {
        let is_index = match segment {
            Segment::Index(_) | Segment::Append => true,
            Segment::Key(_) => false,
            Segment::Number(_) => match schema {
                Some(s) => is_array_schema(s),
                None => !slot.is_object(),
            },
        };
        if slot.is_null() {
            *slot = if is_index {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            };
        }
        if is_index {
            let Value::Array(array) = slot else {
                return Err(format!(
                    "--{flag}: `{walked}` was already set to {}, which isn't a list",
                    describe(slot)
                ));
            };
            let index = match segment {
                Segment::Index(n) => *n,
                Segment::Number(n) => n.parse().expect("digits"),
                _ => array.len(),
            };
            if index > array.len() {
                return Err(format!(
                    "--{flag}: set `{walked}[{}]` before `{walked}[{index}]`",
                    array.len()
                ));
            }
            if index == array.len() {
                array.push(Value::Null);
            }
            schema = match schema {
                Some(s) => {
                    Some(items(s).ok_or_else(|| format!("--{flag}: `{walked}` isn't a list"))?)
                }
                None => None,
            };
            walked.push_str(&format!("[{index}]"));
            slot = &mut array[index];
        } else {
            let mut key = match segment {
                Segment::Key(k) | Segment::Number(k) => k.clone(),
                _ => unreachable!(),
            };
            // `--tax-exempt` names `tax_exempt`.
            if let Some(s) = schema
                && key.contains('-')
                && child(s, &key).is_none()
                && child(s, &key.replace('-', "_")).is_some()
            {
                key = key.replace('-', "_");
            }
            let Value::Object(map) = slot else {
                return Err(format!(
                    "--{flag}: `{walked}` was already set to {}, which has no fields",
                    describe(slot)
                ));
            };
            if let Some(s) = schema {
                schema = Some(child(s, &key).ok_or_else(|| {
                    let at = if i == 0 {
                        format!("--{key}")
                    } else {
                        format!("`{key}` in --{walked}")
                    };
                    unknown_field(&at, &key, s)
                })?);
            }
            if !walked.is_empty() {
                walked.push('.');
            }
            walked.push_str(&key);
            slot = map.entry(key).or_insert(Value::Null);
        }
    }

    let value = coerce(flag, input, schema)?;
    // A single value for a list adds an item, so `--tags a --tags b` builds `["a", "b"]`.
    if let Some(s) = schema
        && is_array_schema(s)
        && !value.is_array()
        && !value.is_null()
    {
        let item = match (&value, items(s)) {
            (Value::String(text), Some(item_schema)) => {
                coerce(flag, Input::Text(text.clone()), Some(item_schema))?
            }
            _ => value,
        };
        match slot {
            Value::Array(array) => array.push(item),
            _ => *slot = Value::Array(vec![item]),
        }
        return Ok(());
    }
    if slot.is_object() && value.is_object() {
        merge_into(slot, value);
    } else {
        *slot = value;
    }
    Ok(())
}

fn is_array_schema(schema: &Value) -> bool {
    let kinds = kinds(schema);
    kinds.contains(&"array") && !kinds.contains(&"object") && !kinds.contains(&"any")
}

fn merge_into(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        merge_into(existing, value)
                    }
                    _ => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

fn unknown_field(at: &str, key: &str, schema: &Value) -> String {
    let names: Vec<String> = properties_of(schema)
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    match suggest(key, &names) {
        Some(close) => format!("unknown field {at}. Did you mean `{close}`?"),
        None if names.is_empty() => format!("unknown field {at}"),
        None => format!("unknown field {at}. Expected one of: {}", names.join(", ")),
    }
}

/// The closest name within a couple of edits, for "did you mean".
pub fn suggest<'n>(input: &str, names: &'n [String]) -> Option<&'n str> {
    names
        .iter()
        .map(|n| (edit_distance(input, n), n))
        .filter(|(d, n)| *d <= 2.max(n.len() / 4))
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n.as_str())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

/// Types a command-line value by the schema it's going to.
pub fn coerce(flag: &str, input: Input, schema: Option<&Value>) -> Result<Value, String> {
    let text = match input {
        Input::Present => {
            return match schema {
                None => Ok(Value::Bool(true)),
                Some(s) if kinds(s).contains(&"boolean") => Ok(Value::Bool(true)),
                Some(s) => Err(format!(
                    "--{flag} needs a value ({})",
                    manifest::type_label(s)
                )),
            };
        }
        Input::Text(text) => text,
    };

    // `@path` reads a file and `@-` reads stdin; `\@` keeps a leading `@`.
    let text = if let Some(literal) = text.strip_prefix("\\@") {
        format!("@{literal}")
    } else if let Some(path) = text.strip_prefix('@').filter(|p| !p.is_empty()) {
        if schema.is_some_and(is_binary) {
            let mut marker = Map::new();
            marker.insert(FILE_MARKER.into(), Value::String(path.to_owned()));
            return Ok(Value::Object(marker));
        }
        let contents = read_source(path).map_err(|e| format!("--{flag}: {e}"))?;
        if schema.is_some_and(|s| kinds(s) == ["string"]) {
            return Ok(Value::String(contents));
        }
        return parse_loose(flag, &contents);
    } else {
        text
    };

    let Some(schema) = schema else {
        return Ok(infer(&text));
    };
    let kinds = kinds(schema);
    let has = |k: &str| kinds.contains(&k) || kinds.contains(&"any");
    let trimmed = text.trim_start();

    if (trimmed.starts_with('{') && has("object")) || (trimmed.starts_with('[') && has("array")) {
        return parse_loose(flag, &text);
    }
    if text == "null" && kinds.contains(&"null") {
        return Ok(Value::Null);
    }
    if has("boolean")
        && let Some(b) = parse_bool(&text)
    {
        return Ok(Value::Bool(b));
    }
    if (has("integer") || has("number"))
        && let Ok(n) = text.parse::<i64>()
    {
        return Ok(Value::Number(n.into()));
    }
    if (has("number") || kinds.contains(&"integer"))
        && let Ok(n) = text.parse::<f64>()
        && let Some(n) = Number::from_f64(n)
    {
        if !has("number") {
            return Err(format!("--{flag} expects a whole number, got `{text}`"));
        }
        return Ok(Value::Number(n));
    }
    if has("string") {
        if let Some(values) = enum_values(schema)
            && !values.contains(&text)
        {
            return Err(match suggest(&text, &values) {
                Some(close) => format!("--{flag} can't be `{text}`. Did you mean `{close}`?"),
                None => format!("--{flag} must be one of: {}", values.join(", ")),
            });
        }
        return Ok(Value::String(text));
    }
    // `--expand customer,invoice` for an object of flags.
    if has("object")
        && let Some(object) = flag_list(&text, schema)
    {
        return Ok(object);
    }
    if has("array")
        && let Some(item_schema) = items(schema)
    {
        return coerce(flag, Input::Text(text), Some(item_schema));
    }
    Err(format!(
        "--{flag} expects {}, got `{text}`",
        manifest::type_label(schema)
    ))
}

fn flag_list(text: &str, schema: &Value) -> Option<Value> {
    let names: Vec<&str> = text.split(',').map(str::trim).collect();
    let mut out = Map::new();
    for name in names {
        let field = child(schema, name)?;
        if !kinds(field).contains(&"boolean") {
            return None;
        }
        out.insert(name.to_owned(), Value::Bool(true));
    }
    Some(Value::Object(out))
}

fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Without a schema: JSON-looking text is JSON, numbers and booleans are themselves, the rest
/// is a string.
fn infer(text: &str) -> Value {
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = loose::parse(text)
    {
        return value;
    }
    if let Some(b) = parse_bool(text) {
        return Value::Bool(b);
    }
    if text == "null" {
        return Value::Null;
    }
    if let Ok(n) = text.parse::<i64>() {
        return Value::Number(n.into());
    }
    Value::String(text.to_owned())
}

fn parse_loose(flag: &str, text: &str) -> Result<Value, String> {
    loose::parse(text).map_err(|e| {
        let rendered = e.render(text);
        if flag.is_empty() {
            rendered
        } else {
            format!("--{flag}: {rendered}")
        }
    })
}

/// Parses a positional body: loose JSON, `@file`, or `-` for stdin.
pub fn parse_body(text: &str) -> Result<Value, String> {
    let source = if text == "-" {
        Some("-")
    } else {
        text.strip_prefix('@')
    };
    match source {
        Some(path) => parse_loose("", &read_source(path)?),
        None => parse_loose("", text),
    }
}

fn read_source(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("couldn't read stdin: {e}"))?;
        return Ok(text);
    }
    std::fs::read_to_string(path).map_err(|e| format!("couldn't read {path}: {e}"))
}

fn is_binary(schema: &Value) -> bool {
    manifest::branches(schema)
        .iter()
        .any(|b| b.get("format").and_then(Value::as_str) == Some("binary"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::manifest;
    use serde_json::json;

    fn schema(command: &str) -> Value {
        manifest()
            .operations
            .iter()
            .find(|o| o.command.join(" ") == command)
            .unwrap()
            .params_schema()
    }

    fn text(s: &str) -> Input {
        Input::Text(s.to_owned())
    }

    #[test]
    fn parses_paths() {
        use Segment::*;
        assert_eq!(
            parse_path(r#"a.b[0].c[]["x.y"].7"#).unwrap(),
            vec![
                Key("a".into()),
                Key("b".into()),
                Index(0),
                Key("c".into()),
                Append,
                Key("x.y".into()),
                Number("7".into())
            ]
        );
        assert!(parse_path("a.").is_err());
        assert!(parse_path("a[x]").is_err());
    }

    #[test]
    fn builds_nested_values_without_a_schema() {
        let mut params = Params::new(None);
        params.set("nested[0].property", text("{x: 2}")).unwrap();
        params.set("nested[1].property.x", text("3")).unwrap();
        params.set("nested.0.property.y", text("true")).unwrap();
        params.set("name", text("john")).unwrap();
        assert_eq!(
            Value::Object(params.root),
            json!({
                "nested": [{"property": {"x": 2, "y": true}}, {"property": {"x": 3}}],
                "name": "john"
            })
        );
    }

    #[test]
    fn refuses_index_gaps_and_scalar_parents() {
        let mut params = Params::new(None);
        assert!(params.set("a[1]", text("x")).unwrap_err().contains("a[0]"));
        params.set("b", text("x")).unwrap();
        assert!(params.set("b.c", text("y")).is_err());
    }

    #[test]
    fn types_values_by_schema() {
        let schema = schema("customers create");
        let mut params = Params::new(Some(&schema));
        params.set("name", text("123")).unwrap();
        params.set("metadata.order", text("42")).unwrap();
        params.set("metadata.0", text("zero")).unwrap();
        params
            .set("shipping_details.address.line1", text("1 Main St"))
            .unwrap();
        assert_eq!(
            Value::Object(params.root),
            json!({
                "name": "123",
                "metadata": {"order": 42, "0": "zero"},
                "shipping_details": {"address": {"line1": "1 Main St"}}
            })
        );
    }

    #[test]
    fn checks_names_and_enums() {
        let schema = schema("customers create");
        let mut params = Params::new(Some(&schema));
        assert_eq!(
            params.set("nmae", text("x")).unwrap_err(),
            "unknown field --nmae. Did you mean `name`?"
        );
        assert_eq!(
            params.set("tax_exempt", text("exmpt")).unwrap_err(),
            "--tax_exempt can't be `exmpt`. Did you mean `exempt`?"
        );
        assert!(
            params
                .set("shipping_details.adress.line1", text("x"))
                .is_err()
        );
    }

    #[test]
    fn appends_single_values_to_lists() {
        let payments = schema("payments list");
        let mut params = Params::new(Some(&payments));
        params
            .set("expand", text("customer,payment_method"))
            .unwrap();
        params.set("limit", text("5")).unwrap();
        assert_eq!(
            Value::Object(params.root),
            json!({"expand": {"customer": true, "payment_method": true}, "limit": 5})
        );
        let invoice = schema("invoices create");
        let mut params = Params::new(Some(&invoice));
        params.set("line_items[].price", text("price_1")).unwrap();
        params.set("line_items[0].quantity", text("2")).unwrap();
        assert_eq!(
            Value::Object(params.root),
            json!({"line_items": [{"price": "price_1", "quantity": 2}]})
        );
    }

    #[test]
    fn rejects_mistyped_numbers() {
        let schema = schema("customers list");
        let mut params = Params::new(Some(&schema));
        assert!(params.set("limit", text("lots")).is_err());
        assert!(params.set("limit", text("2.5")).is_err());
    }

    #[test]
    fn merges_body_then_flags() {
        let schema = schema("customers create");
        let mut params = Params::new(Some(&schema));
        params
            .merge(loose::parse("{name: 'john', metadata: {a: 1}}").unwrap())
            .unwrap();
        params.set("metadata.b", text("2")).unwrap();
        assert!(params.merge(json!({"nmae": 1})).is_err());
        assert_eq!(
            Value::Object(params.root),
            json!({"name": "john", "metadata": {"a": 1, "b": 2}})
        );
    }
}
