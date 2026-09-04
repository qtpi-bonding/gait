use gait_core::builtin_tools;
use tempfile::TempDir;

#[tokio::test]
async fn builtin_tools_returns_expected_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(
        gait_core::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "stest".into(),
        ),
        vec![],
        vec![],
        vec![],
        gait_core::git::Repos::production(),
    );
    // 17 with the `code-mode` feature's `code` tool registered, 16 without
    // it (Task 6c gates `CodeTool`'s registration on that feature).
    let expected_len = if cfg!(feature = "code-mode") { 17 } else { 16 };
    assert_eq!(tools.len(), expected_len);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    let mut expected_names = vec![
        "read",
        "write",
        "apply_patch",
        "glob",
        "grep",
        "ls",
        "shell",
        "structured_done",
        "codesearch",
        "webfetch",
        "enable",
        "spawn_agent",
        "list_agents",
        "clarify",
        "inform",
        "prune",
    ];
    if cfg!(feature = "code-mode") {
        expected_names.push("code");
    }
    for expected in &expected_names {
        assert!(
            names.contains(expected),
            "missing tool '{}', got: {:?}",
            expected,
            names
        );
    }
}

#[tokio::test]
async fn all_tools_have_valid_json_schema_parameters() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(
        gait_core::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "stest".into(),
        ),
        vec![],
        vec![],
        vec![],
        gait_core::git::Repos::production(),
    );
    for tool in &tools {
        let schema = tool.parameters();
        assert!(
            schema.is_object(),
            "tool '{}' has non-object schema",
            tool.name()
        );
        assert!(
            schema.get("$schema").is_none(),
            "tool '{}' schema should not have $schema key (Anthropic rejects it)",
            tool.name()
        );
        assert!(
            schema.get("title").is_none(),
            "tool '{}' schema should not have title key",
            tool.name()
        );
        assert!(
            schema.get("properties").is_some() || schema.get("type").is_some(),
            "tool '{}' schema missing properties/type",
            tool.name()
        );
    }
}

#[tokio::test]
async fn read_write_roundtrip_via_builtin_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(
        gait_core::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "stest".into(),
        ),
        vec![],
        vec![],
        vec![],
        gait_core::git::Repos::production(),
    );
    let write_tool = tools.iter().find(|t| t.name() == "write").unwrap();
    let read_tool = tools.iter().find(|t| t.name() == "read").unwrap();

    write_tool
        .execute(
            serde_json::json!({
                "file_path": "hello.txt",
                "content": "hello from gait-rs"
            }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();

    let result = read_tool
        .execute(
            serde_json::json!({
                "file_path": "hello.txt"
            }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();

    let text = result.as_str().unwrap();
    assert!(text.contains("hello from gait-rs"), "got: {text}");
    assert!(!text.contains('#'), "read should be plain text: {text}");
}

#[tokio::test]
async fn write_then_edit_then_read() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(
        gait_core::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "stest".into(),
        ),
        vec![],
        vec![],
        vec![],
        gait_core::git::Repos::production(),
    );
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let edit = tools.iter().find(|t| t.name() == "apply_patch").unwrap();
    let read = tools.iter().find(|t| t.name() == "read").unwrap();

    let src = "fn hello() -> &'static str { \"hello\" }";
    write
        .execute(
            serde_json::json!({
                "file_path": "code.rs",
                "content": src
            }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();

    // Single-file Codex-compatible update.
    edit.execute(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: code.rs\n@@\n-fn hello() -> &'static str { \"hello\" }\n+fn world() -> &'static str { \"world\" }\n*** End Patch"
        }),
        &gait_core::Recorder::detached(),
    )
    .await
    .unwrap();

    let result = read
        .execute(
            serde_json::json!({ "file_path": "code.rs" }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();
    let text = result.as_str().unwrap();
    assert!(text.contains("world"), "got: {text}");
}

#[tokio::test]
async fn glob_finds_written_files() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(
        gait_core::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "stest".into(),
        ),
        vec![],
        vec![],
        vec![],
        gait_core::git::Repos::production(),
    );
    let write = tools.iter().find(|t| t.name() == "write").unwrap();
    let glob = tools.iter().find(|t| t.name() == "glob").unwrap();

    write
        .execute(
            serde_json::json!({ "file_path": "a.rs", "content": "" }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();
    write
        .execute(
            serde_json::json!({ "file_path": "b.rs", "content": "" }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();
    write
        .execute(
            serde_json::json!({ "file_path": "c.ts", "content": "" }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();

    let result = glob
        .execute(
            serde_json::json!({ "pattern": "*.rs" }),
            &gait_core::Recorder::detached(),
        )
        .await
        .unwrap();
    let text = result.as_str().unwrap();
    assert!(text.contains("a.rs"), "got: {text}");
    assert!(text.contains("b.rs"), "got: {text}");
    assert!(!text.contains("c.ts"), "got: {text}");
}
