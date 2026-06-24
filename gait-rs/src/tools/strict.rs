//! Transform a tool's JSON Schema into a provider's "strict" dialect for
//! constrained decoding. Returns `None` when the schema cannot be made
//! strict-valid (caller then sends the plain schema without `strict`).
//!
//! Why per-dialect: OpenAI strict requires every property in `required`
//! (optionals expressed as `["T","null"]`) and rejects `format`; Anthropic
//! strict allows real optionals and keeps `format`. Both reject
//! `$ref`/`anyOf`/`oneOf`/`allOf` — and our schemas are already `$ref`-free
//! via `make_schema`'s `inline_subschemas`.

use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrictDialect {
    OpenAi,
    Anthropic,
}

/// Returns the strict-dialect schema, or `None` if the schema must fall back
/// to non-strict (contains an unsupported construct).
pub fn to_strict_schema(base: &Value, dialect: StrictDialect) -> Option<Value> {
    if contains_unsupported(base) {
        return None;
    }
    if base.get("type").and_then(|t| t.as_str()) != Some("object") {
        return None; // strict tool input must be an object at the root
    }
    Some(transform(base, dialect))
}

/// Keywords schemars emits that OpenAI strict does NOT accept and that we
/// don't need for our goal (preventing type/format violations). Notably
/// `format:"uint64"` is emitted on EVERY unsigned-integer field, and `minimum`
/// on unsigned ints — both must be stripped for OpenAI. Conservative denylist;
/// re-confirm against the live API during T6. (Anthropic keeps all of these.)
const OPENAI_STRIP_KEYWORDS: &[&str] = &[
    "format",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
];

/// Reject schemas with constructs neither provider can constrain. Position-
/// aware: only a *property value* or `items` that is a bare bool/`{}` is
/// "schemaless" (e.g. a `serde_json::Value` field). We do NOT reject arbitrary
/// bool-valued keywords elsewhere (e.g. a future `uniqueItems: true`).
fn contains_unsupported(v: &Value) -> bool {
    let Value::Object(map) = v else { return false };
    for k in ["anyOf", "oneOf", "allOf", "$ref"] {
        if map.contains_key(k) {
            return true;
        }
    }
    if let Some(Value::Object(props)) = map.get("properties") {
        if props.values().any(is_schemaless) || props.values().any(contains_unsupported) {
            return true;
        }
    }
    if let Some(items) = map.get("items") {
        if is_schemaless(items) || contains_unsupported(items) {
            return true;
        }
    }
    false
}

/// A property/items schema that matches any value and so can't be constrained:
/// a bare `true`/`false` (schemars emits `true` for `serde_json::Value`) or an
/// empty `{}`.
fn is_schemaless(v: &Value) -> bool {
    match v {
        Value::Bool(_) => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

fn transform(v: &Value, dialect: StrictDialect) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = Map::new();
            let has_props = map.contains_key("properties");
            for (k, val) in map {
                if dialect == StrictDialect::OpenAi && OPENAI_STRIP_KEYWORDS.contains(&k.as_str()) {
                    continue; // strip keyword OpenAI strict rejects
                }
                out.insert(k.clone(), transform(val, dialect));
            }
            // Only constrain objects that actually declare a property set.
            if has_props {
                out.insert("additionalProperties".into(), json!(false));
                if dialect == StrictDialect::OpenAi {
                    apply_openai_required(&mut out);
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|x| transform(x, dialect)).collect()),
        other => other.clone(),
    }
}

/// OpenAI strict: every property must be in `required`; previously-optional
/// properties get `null` added to their `type`.
fn apply_openai_required(obj: &mut Map<String, Value>) {
    let prop_names: Vec<String> = obj
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let already: std::collections::HashSet<String> = obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for name in &prop_names {
            if already.contains(name) {
                continue;
            }
            if let Some(schema) = props.get_mut(name).and_then(|s| s.as_object_mut()) {
                make_nullable(schema);
            }
        }
    }
    obj.insert("required".into(), json!(prop_names));
}

/// Add `"null"` to a property's `type` (string -> `["string","null"]`). For an
/// enum property, `null` must ALSO be a permitted member of the `enum` array,
/// otherwise OpenAI strict rejects the nullable enum.
fn make_nullable(schema: &mut Map<String, Value>) {
    match schema.get("type").cloned() {
        Some(Value::String(t)) => {
            schema.insert("type".into(), json!([t, "null"]));
        }
        Some(Value::Array(mut arr)) => {
            if !arr.iter().any(|v| v == "null") {
                arr.push(json!("null"));
            }
            schema.insert("type".into(), Value::Array(arr));
        }
        _ => {} // no concrete type — contains_unsupported already screened these
    }
    if let Some(Value::Array(members)) = schema.get_mut("enum") {
        if !members.iter().any(|v| v.is_null()) {
            members.push(Value::Null);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_sets_additional_properties_false_and_all_required() {
        let base = json!({
            "type": "object",
            "properties": {
                "location": {"type": "string"},
                "limit": {"type": "integer"}
            },
            "required": ["location"]
        });
        let out = to_strict_schema(&base, StrictDialect::OpenAi).unwrap();
        assert_eq!(out["additionalProperties"], json!(false));
        // every property is now required
        let req: Vec<String> = out["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(req.contains(&"location".to_string()));
        assert!(req.contains(&"limit".to_string()));
        // the previously-optional `limit` became nullable
        assert_eq!(
            out["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
        // the already-required `location` stays a plain string
        assert_eq!(out["properties"]["location"]["type"], json!("string"));
    }

    #[test]
    fn anthropic_keeps_optionals_and_format() {
        let base = json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "when": {"type": "string", "format": "date"}
            },
            "required": ["city"]
        });
        let out = to_strict_schema(&base, StrictDialect::Anthropic).unwrap();
        assert_eq!(out["additionalProperties"], json!(false));
        assert_eq!(out["required"], json!(["city"])); // optional stays optional
        assert_eq!(out["properties"]["when"]["format"], json!("date")); // format kept
    }

    #[test]
    fn openai_strips_format() {
        let base = json!({
            "type": "object",
            "properties": {"when": {"type": "string", "format": "date"}},
            "required": ["when"]
        });
        let out = to_strict_schema(&base, StrictDialect::OpenAi).unwrap();
        assert!(out["properties"]["when"].get("format").is_none());
    }

    #[test]
    fn openai_strips_numeric_annotation_keywords_on_integers() {
        // schemars emits `format:"uint64"` + `minimum` on unsigned-int fields
        let base = json!({
            "type": "object",
            "properties": {"limit": {"type": "integer", "format": "uint64", "minimum": 0.0}},
            "required": ["limit"]
        });
        let out = to_strict_schema(&base, StrictDialect::OpenAi).unwrap();
        assert!(out["properties"]["limit"].get("format").is_none());
        assert!(out["properties"]["limit"].get("minimum").is_none());
        assert_eq!(out["properties"]["limit"]["type"], json!("integer"));
    }

    #[test]
    fn openai_nullable_enum_adds_null_to_type_and_enum_members() {
        // an OPTIONAL enum: schemars already emits type:["string","null"]
        let base = json!({
            "type": "object",
            "properties": {"fmt": {"enum": ["text", "html"], "type": ["string", "null"]}},
            "required": []
        });
        let out = to_strict_schema(&base, StrictDialect::OpenAi).unwrap();
        assert_eq!(out["required"], json!(["fmt"])); // forced required
        let members = out["properties"]["fmt"]["enum"].as_array().unwrap();
        assert!(
            members.iter().any(|v| v.is_null()),
            "null must be an enum member: {out}"
        );
    }

    #[test]
    fn recurses_into_nested_object_properties() {
        let base = json!({
            "type": "object",
            "properties": {
                "item": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            "required": ["item"]
        });
        let out = to_strict_schema(&base, StrictDialect::OpenAi).unwrap();
        assert_eq!(
            out["properties"]["item"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn recurses_into_array_items() {
        let base = json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"old": {"type": "string"}},
                        "required": ["old"]
                    }
                }
            },
            "required": ["edits"]
        });
        let out = to_strict_schema(&base, StrictDialect::OpenAi).unwrap();
        assert_eq!(
            out["properties"]["edits"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn aborts_on_anyof() {
        let base = json!({
            "type": "object",
            "properties": {"x": {"anyOf": [{"type": "string"}, {"type": "integer"}]}},
            "required": ["x"]
        });
        assert!(to_strict_schema(&base, StrictDialect::OpenAi).is_none());
    }

    #[test]
    fn aborts_on_schemaless_value_property() {
        // `result: serde_json::Value` -> schemars emits `true` or `{}` (no concrete type)
        let base = json!({
            "type": "object",
            "properties": {"result": true},
            "required": ["result"]
        });
        assert!(to_strict_schema(&base, StrictDialect::OpenAi).is_none());
    }

    #[test]
    fn does_not_set_additional_properties_on_schemaless_object() {
        // an object with no `properties` key (free-form map) must be left untouched for Anthropic
        let base = json!({
            "type": "object",
            "properties": {
                "meta": {"type": "object"}
            },
            "required": ["meta"]
        });
        let out = to_strict_schema(&base, StrictDialect::Anthropic).unwrap();
        // meta has no `properties` -> we must NOT force additionalProperties:false (would forbid all keys)
        assert!(out["properties"]["meta"]
            .get("additionalProperties")
            .is_none());
    }
}
