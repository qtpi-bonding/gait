use std::path::PathBuf;
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
use crate::tools::ToolCtx;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct WriteParams {
    /// Path to the file to write (relative to codebase root, or absolute)
    file_path: String,
    /// The content to write to the file
    content: String,
}

pub struct WriteTool {
    ctx: ToolCtx,
    parameters: Value,
}

impl WriteTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<WriteParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { ctx, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file (and parent directories) if they don't exist. Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: WriteParams = serde_json::from_value(args)?;
        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.ctx.codebase_path.join(&params.file_path)
        };
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&resolved, &params.content)?;
        let bytes = params.content.len();
        Ok(Value::String(format!("Wrote {} bytes to {}", bytes, resolved.display())))
    }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.ctx.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("write", &rel),
            preview: self.preview(args),
        }])
    }

    fn preview(&self, args: &serde_json::Value) -> Option<String> {
        serde_json::from_value::<WriteParams>(args.clone()).ok().and_then(|params| {
            let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
                PathBuf::from(&params.file_path)
            } else {
                self.ctx.codebase_path.join(&params.file_path)
            };
            if resolved.exists() {
                std::fs::read_to_string(&resolved)
                    .ok()
                    .and_then(|old| crate::tools::preview::diff_preview(&old, &params.content))
            } else {
                Some(params.content.clone())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::path::PathBuf;
    use crate::tools::ToolCtx;

    fn make_ctx(dir: &TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "test".into())
    }

    #[tokio::test]
    async fn writes_file_content() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        tool.execute(serde_json::json!({
            "file_path": "output.txt",
            "content": "hello world"
        })).await.unwrap();
        let content = std::fs::read_to_string(dir.path().join("output.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        tool.execute(serde_json::json!({
            "file_path": "sub/nested/file.txt",
            "content": "deep"
        })).await.unwrap();
        let content = std::fs::read_to_string(dir.path().join("sub/nested/file.txt")).unwrap();
        assert_eq!(content, "deep");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "old").unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        tool.execute(serde_json::json!({
            "file_path": "existing.txt",
            "content": "new"
        })).await.unwrap();
        let content = std::fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }

    #[test]
    fn write_permission_relative_path() {
        let tool = WriteTool::new(ToolCtx::new(PathBuf::from("/project"), PathBuf::from("/tmp"), "test".into()));
        match tool.permission(&serde_json::json!({"file_path": "/project/src/main.rs"})) {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs[0].display, "src/main.rs");
                assert!(reqs[0].patterns.contains(&"write".to_string()));
                assert!(reqs[0].patterns.contains(&"write:src/main.rs".to_string()));
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn permission_preview_is_full_content_for_new_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        let args = serde_json::json!({"file_path": "new.txt", "content": "hello\nworld\n"});
        let crate::types::ToolPermission::RequiresApproval(reqs) = tool.permission(&args) else {
            panic!("expected RequiresApproval");
        };
        assert_eq!(reqs[0].preview.as_deref(), Some("hello\nworld\n"));
    }

    #[test]
    fn permission_preview_is_diff_for_existing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        let args = serde_json::json!({"file_path": "f.txt", "content": "one\nTWO\n"});
        let crate::types::ToolPermission::RequiresApproval(reqs) = tool.permission(&args) else {
            panic!("expected RequiresApproval");
        };
        let preview = reqs[0].preview.as_deref().expect("preview Some");
        assert!(preview.contains("-two"), "preview:\n{preview}");
        assert!(preview.contains("+TWO"), "preview:\n{preview}");
    }

    #[test]
    fn preview_is_full_content_for_new_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        let args = serde_json::json!({"file_path": "new.txt", "content": "hello\nworld\n"});
        assert_eq!(tool.preview(&args).as_deref(), Some("hello\nworld\n"));
    }

    #[test]
    fn preview_is_diff_for_existing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        let args = serde_json::json!({"file_path": "f.txt", "content": "one\nTWO\n"});
        let preview = tool.preview(&args).expect("preview Some");
        assert!(preview.contains("-two"), "preview:\n{preview}");
        assert!(preview.contains("+TWO"), "preview:\n{preview}");
    }
}
