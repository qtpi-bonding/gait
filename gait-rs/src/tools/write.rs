use crate::permissions::path_patterns;
use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{PermissionRequest, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

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
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file (and parent directories) if they don't exist. Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(&self, args: Value, _rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: WriteParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;
        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.ctx.codebase_path.join(&params.file_path)
        };
        // The previous size, so the result can say what an overwrite replaced.
        let old_bytes = if self.ctx.fs.exists(&resolved) {
            self.ctx
                .fs
                .read_to_string(&resolved)
                .ok()
                .map(|old| old.len())
        } else {
            None
        };
        if let Some(parent) = resolved.parent() {
            self.ctx
                .fs
                .create_dir_all(parent)
                .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        }
        self.ctx
            .fs
            .write(&resolved, params.content.as_bytes())
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        Ok(Value::String(write_summary(
            &resolved,
            old_bytes,
            params.content.len(),
        )))
    }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let raw = args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.ctx.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("write", &rel),
        }])
    }

    fn preview(&self, args: &serde_json::Value) -> Option<String> {
        serde_json::from_value::<WriteParams>(args.clone())
            .ok()
            .and_then(|params| {
                let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
                    PathBuf::from(&params.file_path)
                } else {
                    self.ctx.codebase_path.join(&params.file_path)
                };
                if self.ctx.fs.exists(&resolved) {
                    self.ctx
                        .fs
                        .read_to_string(&resolved)
                        .ok()
                        .and_then(|old| crate::tools::preview::diff_preview(&old, &params.content))
                } else {
                    Some(params.content.clone())
                }
            })
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            writes: true,
            ..Default::default()
        }
    }

    fn scope_targets(
        &self,
        args: &serde_json::Value,
    ) -> Vec<(std::path::PathBuf, crate::types::Dimension)> {
        let Some(raw) = args.get("file_path").and_then(|v| v.as_str()) else {
            return vec![];
        };
        let resolved = if std::path::Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.ctx.codebase_path.join(raw)
        };
        vec![(resolved, crate::types::Dimension::Write)]
    }
}

/// What a write did. An overwrite that loses more than half the file warns:
/// a model that meant to edit part of a file must learn at once that the rest
/// is gone, while it can still recover it.
fn write_summary(path: &std::path::Path, old_bytes: Option<usize>, new_bytes: usize) -> String {
    let path = path.display();
    match old_bytes {
        None => format!("Wrote {new_bytes} bytes to {path}"),
        Some(old) => {
            let mut summary = format!("Replaced {old} bytes with {new_bytes} bytes in {path}");
            if new_bytes * 2 < old {
                summary.push_str(&format!(
                    "\nWarning: this removed {}% of the file's previous content. `write` \
                     replaces the whole file; if you meant to change only part of it, the \
                     rest is gone — restore it, and use apply_patch for partial edits.",
                    (old - new_bytes) * 100 / old
                ));
            }
            summary
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_ctx(dir: &TempDir) -> ToolCtx {
        ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "test".into(),
        )
    }

    #[tokio::test]
    async fn writes_file_content() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        tool.execute(
            serde_json::json!({
                "file_path": "output.txt",
                "content": "hello world"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("output.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        tool.execute(
            serde_json::json!({
                "file_path": "sub/nested/file.txt",
                "content": "deep"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("sub/nested/file.txt")).unwrap();
        assert_eq!(content, "deep");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "old").unwrap();
        let tool = WriteTool::new(make_ctx(&dir));
        tool.execute(
            serde_json::json!({
                "file_path": "existing.txt",
                "content": "new"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(content, "new");
    }

    async fn write_message(dir: &TempDir, file: &str, content: &str) -> String {
        WriteTool::new(make_ctx(dir))
            .execute(
                serde_json::json!({ "file_path": file, "content": content }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn overwrite_reports_the_size_change() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "0123456789").unwrap();
        let msg = write_message(&dir, "f.txt", "01234567").await;
        assert!(msg.contains("Replaced 10 bytes with 8 bytes"), "{msg}");
        assert!(
            !msg.contains("Warning"),
            "a small shrink is not a warning: {msg}"
        );
    }

    #[tokio::test]
    async fn large_shrink_warns_that_content_is_gone() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x".repeat(1000)).unwrap();
        let msg = write_message(&dir, "f.txt", "").await;
        assert!(msg.contains("Replaced 1000 bytes with 0 bytes"), "{msg}");
        assert!(msg.contains("Warning"), "{msg}");
        assert!(
            msg.contains("apply_patch"),
            "should point at partial edits: {msg}"
        );
    }

    #[tokio::test]
    async fn new_file_reports_bytes_written() {
        let dir = TempDir::new().unwrap();
        let msg = write_message(&dir, "new.txt", "abc").await;
        assert!(msg.starts_with("Wrote 3 bytes to"), "{msg}");
    }

    #[test]
    fn write_permission_relative_path() {
        let tool = WriteTool::new(ToolCtx::new(
            PathBuf::from("/project"),
            PathBuf::from("/tmp"),
            "test".into(),
        ));
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
