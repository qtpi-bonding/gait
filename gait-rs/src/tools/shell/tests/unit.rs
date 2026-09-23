use super::super::*;
use crate::tools::ToolCtx;

#[test]
fn shell_permission_single_command() {
    let tool = ShellTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "test".into(),
    ));
    match tool.permission(&serde_json::json!({"cmd": "cargo test", "reason": ""})) {
        crate::types::ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].display, "cargo test");
            assert!(reqs[0].patterns.contains(&"shell:cargo *".to_string()));
            assert!(reqs[0].patterns.contains(&"shell:cargo test".to_string()));
        }
        other => panic!("{:?}", other),
    }
}

#[test]
fn shell_permission_compound_command() {
    let tool = ShellTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "test".into(),
    ));
    match tool.permission(&serde_json::json!({"cmd": "cargo build && git push", "reason": ""})) {
        crate::types::ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 2);
            assert_eq!(reqs[0].display, "cargo build");
            assert_eq!(reqs[1].display, "git push");
        }
        other => panic!("{:?}", other),
    }
}

#[test]
fn shell_permission_patterns_prefixed_with_shell() {
    let tool = ShellTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "test".into(),
    ));
    if let crate::types::ToolPermission::RequiresApproval(reqs) =
        tool.permission(&serde_json::json!({"cmd": "cargo test --verbose", "reason": ""}))
    {
        for pat in &reqs[0].patterns {
            assert!(
                pat.starts_with("shell:"),
                "pattern missing shell: prefix: {pat}"
            );
        }
    }
}

#[test]
fn schema_has_parse_array_field() {
    let tool = ShellTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "test".into(),
    ));
    let params = tool.parameters();
    assert_eq!(
        params["properties"]["parse"]["type"].as_str().unwrap(),
        "array",
        "parse should be array type: {}",
        params
    );
}

#[tokio::test]
async fn missing_required_argument_is_invalid_params_not_internal() {
    let tool = ShellTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "test".into(),
    ));
    let err = tool
        .execute(
            serde_json::json!({ "file_path": "src/lib.rs", "offset": 1 }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::tools::ToolError::InvalidParams(_)),
        "{err:?}"
    );
}
