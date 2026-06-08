use std::path::PathBuf;
use std::sync::Arc;
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
use crate::tools::edit::replace;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct EditOp {
    /// The text to replace
    old_string: String,
    /// The replacement text
    new_string: String,
    /// Replace all occurrences (default false)
    replace_all: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct MultieditParams {
    /// Path to the file to modify (relative to codebase root, or absolute)
    file_path: String,
    /// Array of edit operations to perform sequentially
    edits: Vec<EditOp>,
}

pub struct MultieditTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl MultieditTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<MultieditParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

#[async_trait::async_trait]
impl Tool for MultieditTool {
    fn name(&self) -> &str { "multiedit" }
    fn description(&self) -> &str {
        "Apply multiple sequential edits to a single file. Each edit replaces old_string with new_string. Edits are applied in order."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: MultieditParams = serde_json::from_value(args)?;
        if params.edits.is_empty() {
            anyhow::bail!("edits array must not be empty");
        }

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.codebase_path.join(&params.file_path)
        };

        if !resolved.exists() {
            anyhow::bail!("File not found: {}", resolved.display());
        }
        if resolved.is_dir() {
            anyhow::bail!("Path is a directory, not a file: {}", resolved.display());
        }

        let mut content = std::fs::read_to_string(&resolved)?;

        for (i, edit) in params.edits.iter().enumerate() {
            content = replace(&content, &edit.old_string, &edit.new_string,
                edit.replace_all.unwrap_or(false))
                .map_err(|e| anyhow::anyhow!("Edit {}: {}", i + 1, e))?;
        }

        std::fs::write(&resolved, &content)?;
        Ok(Value::String(format!("Applied {} edits to {}", params.edits.len(), resolved.display())))
    }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let edits = args.get("edits").and_then(|v| v.as_array());
        let paths: Vec<String> = match edits {
            None => vec!["*".to_string()],
            Some(arr) => {
                let mut seen = std::collections::HashSet::new();
                arr.iter()
                    .filter_map(|e| e.get("file_path").and_then(|v| v.as_str()))
                    .filter(|p| seen.insert(p.to_string()))
                .map(|p| {
                    std::path::Path::new(p)
                        .strip_prefix(&*self.codebase_path)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string())
                })
                    .collect()
            }
        };
        let requests = paths.iter().map(|rel| PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("edit", rel),
        }).collect();
        ToolPermission::RequiresApproval(requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn applies_single_edit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "hello world").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [{ "old_string": "hello", "new_string": "goodbye" }]
        })).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
    }

    #[tokio::test]
    async fn applies_multiple_edits_in_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "alpha beta gamma").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [
                { "old_string": "alpha", "new_string": "one" },
                { "old_string": "beta", "new_string": "two" },
                { "old_string": "gamma", "new_string": "three" }
            ]
        })).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two three");
    }

    #[tokio::test]
    async fn second_edit_can_use_result_of_first() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "foo").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [
                { "old_string": "foo", "new_string": "foobar" },
                { "old_string": "foobar", "new_string": "baz" }
            ]
        })).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "baz");
    }

    #[tokio::test]
    async fn error_when_file_not_found() {
        let dir = TempDir::new().unwrap();
        let tool = MultieditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "nonexistent.txt",
            "edits": [{ "old_string": "a", "new_string": "b" }]
        })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn error_reports_which_edit_failed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "hello").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": [
                { "old_string": "hello", "new_string": "world" },
                { "old_string": "missing_string", "new_string": "x" }
            ]
        })).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Edit 2"), "expected edit index in error, got: {}", msg);
    }

    #[tokio::test]
    async fn error_on_empty_edits() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "hello").unwrap();

        let tool = MultieditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "file.txt",
            "edits": []
        })).await;
        assert!(result.is_err());
    }

    #[test]
    fn multiedit_permission_deduplicates_files() {
        let codebase = std::path::PathBuf::from("/project");
        let tool = MultieditTool::new(codebase);
        match tool.permission(&serde_json::json!({
            "edits": [
                {"file_path": "/project/src/a.rs", "old_string": "x", "new_string": "y"},
                {"file_path": "/project/src/b.rs", "old_string": "x", "new_string": "y"},
                {"file_path": "/project/src/a.rs", "old_string": "z", "new_string": "w"},
            ]
        })) {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 2);
            }
            other => panic!("{:?}", other),
        }
    }
}
