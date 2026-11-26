//! Workflow node output contracts.
//!
//! The workflow definition owns the user-supplied JSON Schema. This module
//! only parses and enforces that opaque contract at the node boundary.

use jsonschema::JSONSchema;
use serde_json::Value;

/// Validate a step's stdout against its optional output schema.
///
/// The trimmed stdout must parse as JSON and validate against the compiled
/// schema; the parsed value is returned. A violation is an error (a
/// retryable failed attempt), never a non-zero-exit result.
pub fn validate_output(
    step_id: &str,
    schema: &serde_json::Value,
    stdout: &str,
) -> anyhow::Result<Value> {
    let compiled = JSONSchema::compile(schema)
        .map_err(|e| anyhow::anyhow!("node '{step_id}': invalid output schema: {e}"))?;

    let value = serde_json::from_str(stdout.trim()).map_err(|e| {
        anyhow::anyhow!(
            "node '{step_id}': output_schema requires JSON output, but stdout was invalid JSON: {e}"
        )
    })?;

    if let Err(errors) = compiled.validate(&value) {
        let details = errors
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "node '{step_id}': output schema validation failed:\n{}",
            details
        );
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "check";

    fn schema(stdout: &str, schema: &str) -> anyhow::Result<Value> {
        let schema: Value = serde_json::from_str(schema).unwrap();
        validate_output(ID, &schema, stdout)
    }

    #[test]
    fn schema_validates_and_promotes_json_text() {
        let out = schema(
            r#"{"ok":true}"#,
            r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#,
        )
        .unwrap();
        assert_eq!(out, serde_json::json!({"ok": true}));
    }

    #[test]
    fn schema_rejects_wrong_type() {
        let err = schema(
            r#"{"ok":"yes"}"#,
            r#"{"type":"object","properties":{"ok":{"type":"boolean"}}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("output schema validation failed"), "{err}");
    }

    #[test]
    fn schema_requires_json_when_node_has_no_structured_value() {
        let err = schema("plain text", r#"{"type":"object"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires JSON output"), "{err}");
    }
}
