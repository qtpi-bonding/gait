//! Transform a tool's JSON Schema into a provider's "strict" dialect for
//! constrained decoding. Returns `None` when the schema cannot be made
//! strict-valid (caller then sends the plain schema without `strict`).
//!
//! Why per-dialect: OpenAI strict requires every property in `required`
//! (optionals expressed as `["T","null"]`) and rejects `format`; Anthropic
//! strict allows real optionals and supports a subset of `format` values, but
//! rejects several schema constraints. Both reject
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
    if dialect == StrictDialect::Anthropic && contains_anthropic_open_object(base) {
        // Strict Anthropic schemas require additionalProperties:false on every
        // object. An object with no declared properties is an open map; don't
        // close it because that would change its meaning. Send this tool plain.
        return None;
    }
    if base.get("type").and_then(|t| t.as_str()) != Some("object") {
        return None; // strict tool input must be an object at the root
    }
    Some(transform(base, dialect))
}

/// Find schemas Anthropic strict can't represent without changing their
/// meaning: open objects and property schemas with no concrete JSON type.
fn contains_anthropic_open_object(v: &Value) -> bool {
    let Value::Object(map) = v else { return false };
    let schema_type = map.get("type").and_then(Value::as_str);
    if (map.contains_key("properties") && schema_type != Some("object"))
        || (map.contains_key("items") && schema_type != Some("array"))
    {
        // JSON Schema keywords are annotations when their matching type isn't
        // declared. Strict transformation would otherwise narrow the schema.
        return true;
    }
    if schema_type == Some("object") {
        let has_properties = map.contains_key("properties");
        let additional_properties = map.get("additionalProperties");
        if (has_properties && additional_properties != Some(&Value::Bool(false)))
            || (!has_properties && additional_properties.and_then(Value::as_bool) != Some(false))
        {
            return true;
        }
    }
    if !map.keys().any(|key| {
        matches!(
            key.as_str(),
            "type"
                | "properties"
                | "items"
                | "enum"
                | "const"
                | "$ref"
                | "anyOf"
                | "oneOf"
                | "allOf"
        )
    }) {
        // Annotation-only schemas (e.g. schemars' `Option<serde_json::Value>`)
        // are not concrete input types for Anthropic strict mode.
        return true;
    }
    if let Some(Value::Object(props)) = map.get("properties") {
        if props.values().any(contains_anthropic_open_object) {
            return true;
        }
    }
    if let Some(items) = map.get("items") {
        if contains_anthropic_open_object(items) {
            return true;
        }
    }
    false
}

/// Keywords schemars emits that OpenAI strict does NOT accept and that we
/// don't need for our goal (preventing type/format violations). Notably
/// `format:"uint64"` is emitted on EVERY unsigned-integer field, and `minimum`
/// on unsigned ints — both must be stripped for OpenAI.
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

/// Constraints unsupported by Anthropic strict tool schemas. The local tool
/// schema remains unchanged, so runtime deserialization/validation is intact.
const ANTHROPIC_STRIP_KEYWORDS: &[&str] = &[
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "maxItems",
    "uniqueItems",
    "contains",
    "minContains",
    "maxContains",
    "pattern",
];

const ANTHROPIC_SUPPORTED_FORMATS: &[&str] = &[
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

fn anthropic_keyword_is_unsupported(keyword: &str, value: &Value) -> bool {
    ANTHROPIC_STRIP_KEYWORDS.contains(&keyword)
        || (keyword == "minItems" && value.as_u64().is_some_and(|count| count > 1))
        || (keyword == "format"
            && !value
                .as_str()
                .is_some_and(|format| ANTHROPIC_SUPPORTED_FORMATS.contains(&format)))
}

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
                if (dialect == StrictDialect::OpenAi && OPENAI_STRIP_KEYWORDS.contains(&k.as_str()))
                    || (dialect == StrictDialect::Anthropic
                        && anthropic_keyword_is_unsupported(k, val))
                {
                    continue; // strip keyword/format unsupported by this strict dialect
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
            "additionalProperties": false,
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
    fn anthropic_strips_unsupported_numeric_and_string_constraints() {
        let base = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "offset": {
                    "type": "integer",
                    "format": "uint64",
                    "minimum": 0.0
                },
                "label": {"type": "string", "minLength": 1, "maxLength": 20}
            },
            "required": ["offset"]
        });
        let out = to_strict_schema(&base, StrictDialect::Anthropic).unwrap();
        assert_eq!(out["properties"]["offset"]["type"], json!("integer"));
        assert!(out["properties"]["offset"].get("format").is_none());
        assert!(out["properties"]["offset"].get("minimum").is_none());
        assert!(out["properties"]["label"].get("minLength").is_none());
        assert!(out["properties"]["label"].get("maxLength").is_none());
    }

    #[test]
    fn anthropic_falls_back_for_open_object_schemas() {
        let base = json!({
            "type": "object",
            "properties": {
                "parse": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "description": "An operation with provider-defined keys"
                    }
                }
            },
            "required": ["parse"]
        });
        assert!(to_strict_schema(&base, StrictDialect::Anthropic).is_none());

        for additional_properties in [json!(true), json!({ "type": "string" })] {
            let base = json!({
                "type": "object",
                "properties": {"meta": {"type": "object", "properties": {"x": {"type": "string"}},
                    "additionalProperties": additional_properties}},
                "required": ["meta"]
            });
            assert!(to_strict_schema(&base, StrictDialect::Anthropic).is_none());
        }

        let base = json!({
            "type": "object",
            "properties": {"meta": {"type": "object", "properties": {"x": {"type": "string"}}}},
            "required": ["meta"]
        });
        assert!(to_strict_schema(&base, StrictDialect::Anthropic).is_none());
    }

    #[test]
    fn anthropic_falls_back_for_untyped_property_schemas() {
        let base = json!({
            "type": "object",
            "properties": {
                "result": {"description": "Arbitrary structured result"}
            },
            "required": ["result"]
        });
        assert!(to_strict_schema(&base, StrictDialect::Anthropic).is_none());
    }

    #[test]
    fn anthropic_falls_back_when_properties_or_items_lack_matching_type() {
        for child in [
            json!({"properties": {"x": {"type": "string"}}, "additionalProperties": false}),
            json!({"items": {"type": "string"}}),
        ] {
            let base = json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {"value": child},
                "required": ["value"]
            });
            assert!(to_strict_schema(&base, StrictDialect::Anthropic).is_none());
        }
    }

    #[test]
    fn anthropic_strips_unsupported_array_constraints_and_formats() {
        let base = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"type": "string", "pattern": "^x"},
                    "minItems": 2,
                    "maxItems": 8,
                    "uniqueItems": true,
                    "contains": {"type": "string"},
                    "minContains": 1,
                    "maxContains": 4
                },
                "id": {"type": "string", "format": "uint32"},
                "optional": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1
                }
            },
            "required": ["items", "id", "optional"]
        });
        let out = to_strict_schema(&base, StrictDialect::Anthropic).unwrap();
        assert!(out["properties"]["items"].get("minItems").is_none());
        assert!(out["properties"]["items"].get("maxItems").is_none());
        assert!(out["properties"]["items"].get("uniqueItems").is_none());
        assert!(out["properties"]["items"].get("contains").is_none());
        assert!(out["properties"]["items"].get("minContains").is_none());
        assert!(out["properties"]["items"].get("maxContains").is_none());
        assert!(out["properties"]["items"]["items"].get("pattern").is_none());
        assert!(out["properties"]["id"].get("format").is_none());
        assert_eq!(out["properties"]["optional"]["minItems"], json!(1));
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
    fn anthropic_falls_back_for_free_form_map_objects() {
        let base = json!({
            "type": "object",
            "properties": {
                "meta": {"type": "object"}
            },
            "required": ["meta"]
        });
        assert!(to_strict_schema(&base, StrictDialect::Anthropic).is_none());
    }
}
