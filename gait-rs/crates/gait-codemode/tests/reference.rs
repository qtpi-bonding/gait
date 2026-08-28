use gait_codemode::{render_reference, ToolReference};
use serde_json::json;

#[test]
fn renders_canonical_schemas_and_sorts_tools() {
    let mut first = ToolReference::new(
        "z",
        json!({"required":["x"],"properties":{"x":{"enum":["b","a"],"type":"string"}}}),
    );
    first.description = Some("text tool".into());
    let mut second = ToolReference::new(
        "list_issues",
        json!({"type":"object","oneOf":[{"type":"string"},{"type":"array","items":{"type":"integer"}}]}),
    );
    second.output_schema = Some(json!({"properties":{"id":{"type":"integer"}},"type":"object"}));

    let rendered = render_reference("github", &[first, second]);
    assert!(
        rendered.find("gait.github.list_issues").unwrap() < rendered.find("gait.github.z").unwrap()
    );
    assert!(rendered.contains("Promise<unknown>"));
    assert!(rendered.contains("Promise<string>"));
    assert!(rendered.contains("{\"oneOf\":[{\"type\":\"string\"},{\"items\":{\"type\":\"integer\"},\"type\":\"array\"}],\"type\":\"object\"}"));
    assert!(rendered.contains("JSON.parse(await gait.github.z(args))"));
}

#[test]
fn uses_bracket_access_for_non_identifiers() {
    let rendered = render_reference(
        "git-hub",
        &[ToolReference::new("create-issue", json!({"type":"object"}))],
    );
    assert!(rendered.contains("gait[\"git-hub\"][\"create-issue\"]"));
}
