use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneOff {
    Cmd(String),
    Output(String),
}

/// Two-variant mutually-exclusive schema: exactly one of `cmd` | `output`.
pub fn one_off_schema() -> Value {
    let str_obj = |key: &str| {
        json!({
            "type": "object",
            "required": [key],
            "additionalProperties": false,
            "properties": { key: { "type": "string" } }
        })
    };
    json!({ "oneOf": [ str_obj("cmd"), str_obj("output") ] })
}

pub fn parse_one_off(result: &Value) -> anyhow::Result<OneOff> {
    let cmd = result.get("cmd").and_then(Value::as_str);
    let out = result.get("output").and_then(Value::as_str);
    match (cmd, out) {
        (Some(c), None) => Ok(OneOff::Cmd(c.to_string())),
        (None, Some(o)) => Ok(OneOff::Output(o.to_string())),
        _ => anyhow::bail!("expected exactly one of `cmd` or `output`, got: {result}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cmd_variant() {
        let v = json!({ "cmd": "tar xf a.tar" });
        assert!(matches!(parse_one_off(&v).unwrap(), OneOff::Cmd(c) if c == "tar xf a.tar"));
    }

    #[test]
    fn parses_output_variant() {
        let v = json!({ "output": "Paris" });
        assert!(matches!(parse_one_off(&v).unwrap(), OneOff::Output(o) if o == "Paris"));
    }

    #[test]
    fn rejects_both_or_neither() {
        assert!(parse_one_off(&json!({ "cmd": "x", "output": "y" })).is_err());
        assert!(parse_one_off(&json!({})).is_err());
    }

    #[test]
    fn schema_has_oneof_two_variants() {
        let s = one_off_schema();
        assert_eq!(s["oneOf"].as_array().unwrap().len(), 2);
    }
}
