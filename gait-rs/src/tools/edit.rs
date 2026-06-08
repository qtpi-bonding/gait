use std::path::PathBuf;
use std::sync::Arc;
use crate::hashline::{format_hash_lines, apply_hashline_edits, HashlineEditOp};
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct HashlineEditParams {
    /// Path to the file (relative to codebase root, or absolute)
    file_path: String,
    /// List of edit operations. Each references lines by their N#XY anchors from the most
    /// recent `read` of this file. Ops: "replace" (swap lines), "append" (insert after),
    /// "prepend" (insert before). All refs are validated before any write.
    edits: Vec<HashlineEditOp>,
}

pub struct EditTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl EditTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<HashlineEditParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }

    fn description(&self) -> &str {
        "Edit a file using hash-anchored line references from `read` output. \
        Always `read` the file first — the N#XY prefix on each line is the anchor for this tool. \
        Pass one or more edits; all refs are validated before any write, so a stale anchor \
        (line changed since your read) returns a clear error with updated refs instead of silently \
        corrupting the file. \
        Operations: \"replace\" (swap a line or range N#XY..M#XY), \
        \"append\" (insert after a line), \"prepend\" (insert before). \
        Returns the updated file with fresh anchors so you can chain edits without re-reading."
    }

    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: HashlineEditParams = serde_json::from_value(args)?;

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.codebase_path.join(&params.file_path)
        };

        if !resolved.exists() {
            anyhow::bail!("File not found: {}", resolved.display());
        }

        let old_content = std::fs::read_to_string(&resolved)?;
        let new_content = apply_hashline_edits(&old_content, &params.edits)?;

        std::fs::write(&resolved, &new_content)?;

        let lines_changed = (new_content.split('\n').count() as i64
            - old_content.split('\n').count() as i64)
            .unsigned_abs() as usize;

        // Return the updated file with fresh hashline annotations so the model
        // can immediately issue follow-up edits without a separate read call.
        let annotated = format_hash_lines(&new_content);
        Ok(Value::String(format!(
            "Edited {} ({} line(s) net change). Updated refs:\n{}",
            resolved.display(),
            lines_changed,
            annotated
        )))
    }

    fn permission(&self, args: &Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("edit", &rel),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::compute_line_hash;
    use tempfile::TempDir;

    #[tokio::test]
    async fn replaces_single_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(2, lines[1]);

        let tool = EditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": format!("2#{}", hash), "lines": ["    baz();"]}]
        })).await.unwrap();

        assert!(result.as_str().unwrap().contains("baz"), "result: {result}");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("baz()"));
        assert!(!on_disk.contains("bar()"));
    }

    #[tokio::test]
    async fn returns_error_on_stale_ref() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.rs"), "fn foo() {}").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "f.rs",
            "edits": [{"op": "replace", "pos": "1#ZZ", "lines": ["fn bar() {}"]}]
        })).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(">>>"));
    }

    #[tokio::test]
    async fn returns_annotated_output_for_follow_up_edits() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello\nworld").unwrap();

        let content = "hello\nworld";
        let lines: Vec<&str> = content.split('\n').collect();
        let hash = compute_line_hash(1, lines[0]);

        let tool = EditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "f.txt",
            "edits": [{"op": "replace", "pos": format!("1#{}", hash), "lines": ["goodbye"]}]
        })).await.unwrap();

        let text = result.as_str().unwrap();
        // Should contain fresh hashline-annotated content
        assert!(text.contains("#"), "should contain hash refs: {text}");
        assert!(text.contains("|goodbye"), "should show new content: {text}");
    }
}
