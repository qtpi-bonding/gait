use gait_ai::builtin_tools;
use gait_ai::types::Tool;
use tempfile::TempDir;

#[tokio::test]
async fn builtin_tools_returns_expected_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    assert_eq!(tools.len(), 16);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for expected in &["read", "write", "edit", "glob", "grep", "ls", "shell",
                      "output", "codesearch", "webfetch", "mcp", "skill",
                      "spawn_agent", "list_agents", "clarify", "inform"] {
        assert!(names.contains(expected), "missing tool '{}', got: {:?}", expected, names);
    }
}

#[tokio::test]
async fn all_tools_have_valid_json_schema_parameters() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    for tool in &tools {
        let schema = tool.parameters();
        assert!(schema.is_object(), "tool '{}' has non-object schema", tool.name());
        assert!(schema.get("$schema").is_none(),
            "tool '{}' schema should not have $schema key (Anthropic rejects it)", tool.name());
        assert!(schema.get("title").is_none(),
            "tool '{}' schema should not have title key", tool.name());
        assert!(schema.get("properties").is_some() || schema.get("type").is_some(),
            "tool '{}' schema missing properties/type", tool.name());
    }
}

#[tokio::test]
async fn read_write_roundtrip_via_builtin_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    let write_tool = tools.iter().find(|t| t.name() == "write").unwrap();
    let read_tool  = tools.iter().find(|t| t.name() == "read").unwrap();

    write_tool.execute(serde_json::json!({
        "file_path": "hello.txt",
        "content": "hello from gait-rs"
    })).await.unwrap();

    let result = read_tool.execute(serde_json::json!({
        "file_path": "hello.txt"
    })).await.unwrap();

    let text = result.as_str().unwrap();
    assert!(text.contains("hello from gait-rs"), "got: {text}");
    // hashline format: N#XY|content
    assert!(text.contains("1#") && text.contains("|"), "should have hashline format: {text}");
}

#[tokio::test]
async fn write_then_edit_then_read() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let edit  = tools.iter().find(|t| t.name() == "edit").unwrap();
    let read  = tools.iter().find(|t| t.name() == "read").unwrap();

    let src = "fn hello() -> &'static str { \"hello\" }";
    write.execute(serde_json::json!({
        "file_path": "code.rs",
        "content": src
    })).await.unwrap();

    // Read to get the hashline anchor for line 1
    let read_result = read.execute(serde_json::json!({ "file_path": "code.rs" })).await.unwrap();
    let read_text = read_result.as_str().unwrap();
    // Extract the anchor: first token before '|'
    let anchor = read_text.lines()
        .find(|l| l.contains("|fn hello"))
        .and_then(|l| l.split('|').next())
        .unwrap()
        .trim()
        .to_string();

    edit.execute(serde_json::json!({
        "file_path": "code.rs",
        "edits": [{"op": "replace", "pos": anchor, "lines": ["fn world() -> &'static str { \"world\" }"]}]
    })).await.unwrap();

    let result = read.execute(serde_json::json!({ "file_path": "code.rs" })).await.unwrap();
    let text = result.as_str().unwrap();
    assert!(text.contains("world"), "got: {text}");
}

#[tokio::test]
async fn glob_finds_written_files() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let glob  = tools.iter().find(|t| t.name() == "glob").unwrap();

    write.execute(serde_json::json!({ "file_path": "a.rs", "content": "" })).await.unwrap();
    write.execute(serde_json::json!({ "file_path": "b.rs", "content": "" })).await.unwrap();
    write.execute(serde_json::json!({ "file_path": "c.ts", "content": "" })).await.unwrap();

    let result = glob.execute(serde_json::json!({ "pattern": "*.rs" })).await.unwrap();
    let text = result.as_str().unwrap();
    assert!(text.contains("a.rs"), "got: {text}");
    assert!(text.contains("b.rs"), "got: {text}");
    assert!(!text.contains("c.ts"), "got: {text}");
}
