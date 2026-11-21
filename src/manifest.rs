//! The command table generated from the OpenAPI spec (`bun sdk:generate cli`), and the schema
//! lookups that type flag values, check flag names and drive completion.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;

static MANIFEST_JSON: &str = include_str!("../manifest.json");

#[derive(Deserialize)]
pub struct Manifest {
    pub base_url: String,
    pub operations: Vec<Operation>,
    pub schemas: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
pub struct Operation {
    pub command: Vec<String>,
    pub method: String,
    pub path: String,
    pub description: Option<String>,
    pub path_params: Vec<Param>,
    pub query: Vec<Param>,
    pub body: Option<Body>,
    pub response: ResponseKind,
    pub streamable: bool,
    pub idempotent: bool,
    pub response_schema: Option<String>,
    pub columns: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct Param {
    pub name: String,
    pub required: bool,
    pub description: Option<String>,
    pub schema: Value,
}

#[derive(Deserialize)]
pub struct Body {
    pub content_type: String,
    pub required: bool,
    pub schema: Value,
}

#[derive(Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ResponseKind {
    Json,
    List,
    Binary,
}

pub fn manifest() -> &'static Manifest {
    static MANIFEST: OnceLock<Manifest> = OnceLock::new();
    MANIFEST.get_or_init(|| serde_json::from_str(MANIFEST_JSON).expect("embedded manifest parses"))
}

impl Operation {
    pub fn is_multipart(&self) -> bool {
        self.body
            .as_ref()
            .is_some_and(|b| b.content_type == "multipart/form-data")
    }

    /// One object schema covering everything a flag can set: query params and body fields.
    pub fn params_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required: Vec<Value> = Vec::new();
        let mut open = false;
        if let Some(body) = &self.body {
            let schema = resolve(&body.schema);
            for (name, child, _) in properties_of(schema) {
                properties.insert(name, child.clone());
            }
            if let Some(Value::Array(names)) = schema.get("required") {
                required.extend(names.iter().cloned());
            }
            open = is_open(schema);
        }
        for param in &self.query {
            let mut schema = param.schema.clone();
            if let (Some(description), Value::Object(map)) = (&param.description, &mut schema) {
                map.entry("description")
                    .or_insert_with(|| Value::String(description.clone()));
            }
            properties.insert(param.name.clone(), schema);
            if param.required {
                required.push(Value::String(param.name.clone()));
            }
        }
        let mut schema = Map::new();
        schema.insert("type".into(), "object".into());
        schema.insert("properties".into(), Value::Object(properties));
        schema.insert("required".into(), Value::Array(required));
        schema.insert("additionalProperties".into(), Value::Bool(open));
        Value::Object(schema)
    }

    pub fn is_query_param(&self, name: &str) -> bool {
        self.query.iter().any(|p| p.name == name)
    }
}

/// Follows `$ref`s to the schema they name.
pub fn resolve(schema: &Value) -> &Value {
    let mut current = schema;
    for _ in 0..32 {
        match current.get("$ref").and_then(Value::as_str) {
            Some(name) => match manifest().schemas.get(name) {
                Some(next) => current = next,
                None => return current,
            },
            None => return current,
        }
    }
    current
}

/// The alternatives a schema allows: its `anyOf`/`oneOf` branches, flattened, or itself.
pub fn branches(schema: &Value) -> Vec<&Value> {
    let schema = resolve(schema);
    let mut out = Vec::new();
    let mut nested = false;
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(items)) = schema.get(key) {
            nested = true;
            for item in items {
                out.extend(branches(item));
            }
        }
    }
    if !nested {
        out.push(schema);
    }
    out
}

/// The JSON types a schema accepts, across its branches. `integer` also counts as `number`.
pub fn kinds(schema: &Value) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut push = |kind: &'static str| {
        if !out.contains(&kind) {
            out.push(kind);
        }
    };
    for branch in branches(schema) {
        if let Some(c) = branch.get("const") {
            push(kind_of(c));
            continue;
        }
        if let Some(Value::Array(values)) = branch.get("enum") {
            for v in values {
                push(kind_of(v));
            }
            continue;
        }
        match branch.get("type") {
            Some(Value::String(t)) => push(static_kind(t)),
            Some(Value::Array(types)) => {
                for t in types.iter().filter_map(Value::as_str) {
                    push(static_kind(t));
                }
            }
            _ if branch.get("properties").is_some() => push("object"),
            _ if branch.get("items").is_some() => push("array"),
            _ if branch.get("allOf").is_some() => push("object"),
            _ => push("any"),
        }
    }
    out
}

fn static_kind(t: &str) -> &'static str {
    match t {
        "string" => "string",
        "integer" => "integer",
        "number" => "number",
        "boolean" => "boolean",
        "null" => "null",
        "object" => "object",
        "array" => "array",
        _ => "any",
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Every allowed string value when the schema is a closed set of them.
pub fn enum_values(schema: &Value) -> Option<Vec<String>> {
    let mut values = Vec::new();
    for branch in branches(schema) {
        if let Some(Value::String(s)) = branch.get("const") {
            values.push(s.clone());
        } else if let Some(Value::Array(items)) = branch.get("enum") {
            values.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
        } else if !matches!(branch.get("type"), Some(Value::String(t)) if t == "null" || t == "boolean")
        {
            return None;
        }
    }
    (!values.is_empty()).then_some(values)
}

/// The named properties a schema's object branches allow, with each one's schema and whether
/// it's required.
pub fn properties_of(schema: &Value) -> Vec<(String, &Value, bool)> {
    let mut out: Vec<(String, &Value, bool)> = Vec::new();
    for branch in branches(schema) {
        let mut parts = vec![branch];
        if let Some(Value::Array(all)) = branch.get("allOf") {
            parts.extend(all.iter().map(resolve));
        }
        for part in parts {
            let required: Vec<&str> = part
                .get("required")
                .and_then(Value::as_array)
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(Value::Object(props)) = part.get("properties") {
                for (name, child) in props {
                    if !out.iter().any(|(n, _, _)| n == name) {
                        out.push((name.clone(), child, required.contains(&name.as_str())));
                    }
                }
            }
        }
    }
    out
}

/// Whether an object schema takes keys beyond its named properties, such as `metadata`.
pub fn is_open(schema: &Value) -> bool {
    branches(schema).iter().any(|branch| {
        let object_like = kinds(branch).iter().any(|k| *k == "object" || *k == "any");
        object_like
            && match branch.get("additionalProperties") {
                Some(Value::Bool(open)) => *open,
                Some(Value::Object(_)) => true,
                None => branch.get("properties").is_none(),
                _ => false,
            }
    })
}

/// The schema of `key` inside an object schema, or `None` when the object doesn't allow it.
pub fn child<'a>(schema: &'a Value, key: &str) -> Option<&'a Value> {
    static ANY: OnceLock<Value> = OnceLock::new();
    for branch in branches(schema) {
        for (name, child, _) in properties_of(branch) {
            if name == key {
                return Some(child);
            }
        }
    }
    for branch in branches(schema) {
        match branch.get("additionalProperties") {
            Some(extra @ Value::Object(_)) => return Some(extra),
            Some(Value::Bool(true)) => return Some(ANY.get_or_init(|| Value::Object(Map::new()))),
            None if branch.get("properties").is_none()
                && kinds(branch).iter().any(|k| *k == "object" || *k == "any") =>
            {
                return Some(ANY.get_or_init(|| Value::Object(Map::new())));
            }
            _ => {}
        }
    }
    None
}

/// The schema of an array's items.
pub fn items(schema: &Value) -> Option<&Value> {
    branches(schema).into_iter().find_map(|b| b.get("items"))
}

pub fn description(schema: &Value) -> Option<&str> {
    schema
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| resolve(schema).get("description").and_then(Value::as_str))
        .or_else(|| {
            branches(schema)
                .into_iter()
                .find_map(|b| b.get("description").and_then(Value::as_str))
        })
}

/// A short type label for help, e.g. `string`, `integer`, `customer | null`, `one of a, b`.
pub fn type_label(schema: &Value) -> String {
    if let Some(values) = enum_values(schema) {
        if values.len() <= 6 {
            return values.join(" | ");
        }
        return format!("{} | …", values[..5].join(" | "));
    }
    let kinds: Vec<&str> = kinds(schema).into_iter().filter(|k| *k != "null").collect();
    if kinds.is_empty() {
        return "null".into();
    }
    kinds.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(command: &str) -> &'static Operation {
        manifest()
            .operations
            .iter()
            .find(|o| o.command.join(" ") == command)
            .unwrap_or_else(|| panic!("no operation {command}"))
    }

    #[test]
    fn manifest_loads() {
        assert!(manifest().operations.len() > 50);
    }

    #[test]
    fn finds_nested_fields_through_refs_and_nullables() {
        let schema = op("customers create").params_schema();
        let shipping = child(&schema, "shipping_details").unwrap();
        let address = child(shipping, "address").expect("address field");
        assert!(child(address, "line1").is_some());
        assert!(child(&schema, "nope").is_none());
    }

    #[test]
    fn metadata_is_open() {
        let schema = op("customers create").params_schema();
        let metadata = child(&schema, "metadata").unwrap();
        assert!(child(metadata, "anything").is_some());
    }

    #[test]
    fn reads_enums() {
        let schema = op("customers create").params_schema();
        let values = enum_values(child(&schema, "tax_exempt").unwrap()).unwrap();
        assert_eq!(values, vec!["none", "exempt"]);
    }
}
