//! Workflow node output contracts.
//!
//! The workflow definition owns the user-supplied JSON Schema. This module
//! only parses and enforces that opaque contract at the node boundary.

use gait_core::proto::gait::workflow::v1::Node;
use jsonschema::JSONSchema;
use serde_json::Value;

use crate::vars::NodeOutput;

/// Validate a completed node against its optional output schema.
///
/// Prompt nodes normally already have `json` populated by the core turn
/// engine. Schema-enabled shell nodes populate `json` from stdout before this
/// check. Nodes without a schema retain their existing output behavior.
pub fn validate_node_output(node: &Node, output: &NodeOutput) -> anyhow::Result<NodeOutput> {
    if node.output_schema_json.is_empty() {
        return Ok(output.clone());
    }

    let schema: Value = serde_json::from_str(&node.output_schema_json)
        .map_err(|e| anyhow::anyhow!("node '{}': invalid output schema: {e}", node.id))?;

    let value = match &output.json {
        Some(value) => value.clone(),
        None => serde_json::from_str(output.text.trim()).map_err(|e| {
            anyhow::anyhow!(
                "node '{}': output_schema requires JSON output, but stdout was invalid JSON: {e}",
                node.id
            )
        })?,
    };

    let compiled = JSONSchema::compile(&schema)
        .map_err(|e| anyhow::anyhow!("node '{}': invalid output schema: {e}", node.id))?;
    if let Err(errors) = compiled.validate(&value) {
        let details = errors
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "node '{}': output schema validation failed:\n{}",
            node.id,
            details
        );
    }

    let mut enriched = output.clone();
    enriched.json = Some(value);
    Ok(enriched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::Node;

    fn node(schema: &str) -> Node {
        Node {
            id: "check".into(),
            output_schema_json: schema.into(),
            ..Default::default()
        }
    }

    fn output(text: &str) -> NodeOutput {
        NodeOutput {
            text: text.into(),
            exit_code: Some(0),
            json: None,
            routed_to: None,
            session_id: String::new(),
        }
    }

    #[test]
    fn schema_validates_and_promotes_json_text() {
        let schema =
            r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#;
        let out = validate_node_output(&node(schema), &output(r#"{"ok":true}"#)).unwrap();
        assert_eq!(out.json, Some(serde_json::json!({"ok": true})));
    }

    #[test]
    fn schema_rejects_wrong_type() {
        let schema = r#"{"type":"object","properties":{"ok":{"type":"boolean"}}}"#;
        let err = validate_node_output(&node(schema), &output(r#"{"ok":"yes"}"#))
            .unwrap_err()
            .to_string();
        assert!(err.contains("output schema validation failed"), "{err}");
    }

    #[test]
    fn schema_requires_json_when_node_has_no_structured_value() {
        let schema = r#"{"type":"object"}"#;
        let err = validate_node_output(&node(schema), &output("plain text"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires JSON output"), "{err}");
    }

    #[test]
    fn no_schema_preserves_unstructured_output() {
        let out = validate_node_output(&node(""), &output("plain text")).unwrap();
        assert_eq!(out.text, "plain text");
        assert!(out.json.is_none());
    }
}
