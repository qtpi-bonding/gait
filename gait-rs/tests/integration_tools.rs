use gait_ai::builtin_tools;
use gait_ai::types::Tool;
use tempfile::TempDir;

#[tokio::test]
async fn builtin_tools_returns_expected_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    assert_eq!(tools.len(), 15);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for expected in &["read", "write", "edit", "glob", "grep", "ls", "shell",
                      "output", "multiedit", "codesearch", "webfetch", "mcp", "skill",
                      "spawn_agent", "list_agents"] {
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
    assert!(text.contains("00001|"), "should have line numbers");
}

#[tokio::test]
async fn write_then_edit_then_read() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), "stest".into(), dir.path().to_path_buf(), vec![], vec![]);
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let edit  = tools.iter().find(|t| t.name() == "edit").unwrap();
    let read  = tools.iter().find(|t| t.name() == "read").unwrap();

    write.execute(serde_json::json!({
        "file_path": "code.rs",
        "content": "fn hello() -> &'static str { \"hello\" }"
    })).await.unwrap();

    edit.execute(serde_json::json!({
        "file_path": "code.rs",
        "old_string": "hello",
        "new_string": "world",
        "replace_all": true
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
