use crate::permissions::path_patterns;
use crate::tools::fs_backend::FsBackend;
use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, PermissionRequest, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

const DEFAULT_READ_LIMIT: u64 = 2000;

static BINARY_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war", "7z", "doc", "docx", "xls",
    "xlsx", "ppt", "pptx", "odt", "ods", "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm",
    "pyc", "pyo", "png", "jpg", "jpeg", "gif", "bmp", "ico", "tiff", "webp", "mp3", "mp4", "wav",
    "avi", "mkv", "pdf",
];

fn is_binary_by_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_binary_by_content(path: &std::path::Path, fs: &dyn FsBackend) -> bool {
    let Ok(prefix) = fs.read_bytes_prefix(path, 4096) else {
        return false;
    };
    let n = prefix.len();
    if n == 0 {
        return false;
    }
    let sample = &prefix[..n];
    // Null byte = definitely binary
    if sample.contains(&0u8) {
        return true;
    }
    // >30% non-printable (excluding tabs, newlines, carriage returns) = binary
    let non_printable = sample
        .iter()
        .filter(|&&b| b < 9 || (b > 13 && b < 32))
        .count();
    non_printable * 10 > n * 3
}

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// Path to the file (relative to codebase root, or absolute)
    file_path: String,
    /// Line number to start reading from (0-based)
    offset: Option<u64>,
    /// Number of lines to read (default 2000)
    limit: Option<u64>,
}

pub struct ReadTool {
    ctx: ToolCtx,
    parameters: Value,
}

impl ReadTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<ReadParams>(),
            crate::types::OutputFilter::None,
        );
        Self { ctx, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file from the codebase as plain text, optionally with a line range. Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::None
    }

    async fn execute(&self, args: Value, _rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: ReadParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;
        let offset = params.offset.unwrap_or(0) as usize;
        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT) as usize;

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.ctx.codebase_path.join(&params.file_path)
        };

        if !self.ctx.fs.exists(&resolved) {
            // Suggest nearby files
            let dir = resolved.parent().unwrap_or(&resolved);
            let base = resolved
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            let suggestions: Vec<String> = if dir.exists() {
                std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().to_lowercase();
                        name.contains(&base) || base.contains(name.as_str())
                    })
                    .take(3)
                    .map(|e| e.path().display().to_string())
                    .collect()
            } else {
                vec![]
            };
            let hint = if suggestions.is_empty() {
                String::new()
            } else {
                format!("\nDid you mean:\n{}", suggestions.join("\n"))
            };
            return Err(crate::tools::ToolError::Execute(format!(
                "File not found: {}{}",
                resolved.display(),
                hint
            )));
        }

        if is_binary_by_extension(&resolved) || is_binary_by_content(&resolved, &*self.ctx.fs) {
            return Err(crate::tools::ToolError::Execute(format!(
                "Cannot read binary file: {}",
                resolved.display()
            )));
        }

        let text = self
            .ctx
            .fs
            .read_to_string(&resolved)
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        let lines: Vec<&str> = text.split('\n').collect();

        let end = lines.len().min(offset + limit);
        let content = lines[offset.min(lines.len())..end].join("\n");
        let total_lines = lines.len();
        let last_read = end;

        let footer = if total_lines > last_read {
            format!(
                "\n\n(File has more lines. Use 'offset' to read beyond line {})",
                last_read
            )
        } else {
            format!("\n\n(End of file - total {} lines)", total_lines)
        };

        Ok(Value::String(format!(
            "<file>\n{}{}\n</file>",
            content, footer
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
            patterns: path_patterns("read", &rel),
        }])
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
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
        vec![(resolved, crate::types::Dimension::Read)]
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
    async fn reads_file_as_plain_text() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "line1\nline2\nline3").unwrap();

        let tool = ReadTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "file_path": "test.txt" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("line1"), "got: {text}");
        assert!(text.contains("line2"), "got: {text}");
        assert!(text.contains("line3"), "got: {text}");
        assert!(!text.contains("#"), "read should be plain text: {text}");
        // Old `N#XY` format is gone — no leading digit immediately before `#`
    }

    #[tokio::test]
    async fn dup_lines_get_context_tags() {
        let dir = TempDir::new().unwrap();
        // Two identical lines ("same") separated by a unique line ("other").
        // With the 4-2-2 scheme: distinct neighbors → context tags (tier 2), not ordinals.
        std::fs::write(dir.path().join("dup.txt"), "same\nother\nsame").unwrap();

        let tool = ReadTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "file_path": "dup.txt" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        // Both occurrences of "same" get 8-char ids (tier-2, no dash between P4 and C4).
        let lines: Vec<&str> = text.lines().collect();
        let same_lines: Vec<&&str> = lines.iter().filter(|l| **l == "same").collect();
        assert_eq!(
            same_lines.len(),
            2,
            "expected 2 annotated 'same' lines: {text}"
        );
        assert!(text.contains("other"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_error_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let tool = ReadTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "file_path": "nonexistent.txt" }),
                &crate::Recorder::detached(),
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn respects_offset_and_limit() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "a\nb\nc\nd\ne").unwrap();

        let tool = ReadTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({
                    "file_path": "test.txt", "offset": 1, "limit": 2
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("b"), "got: {text}");
        assert!(text.contains("c"), "got: {text}");
        assert!(!text.contains("\na\n"), "should not contain line 1");
    }

    #[tokio::test]
    async fn accepts_float_limit_and_offset() {
        // Models sometimes emit whole-number floats (e.g. `50.0`) for integer fields.
        // coerce_int_params() normalises them before dispatch; the tool should succeed.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "a\nb\nc\nd\ne").unwrap();
        let tool = ReadTool::new(make_ctx(&dir));
        let args = crate::tools::coerce_int_params(serde_json::json!({
            "file_path": "test.txt",
            "offset": 1.0,
            "limit": 2.0
        }));
        let result = tool
            .execute(args, &crate::Recorder::detached())
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("b"), "got: {text}");
        assert!(text.contains("c"), "got: {text}");
        assert!(!text.contains("\na\n"), "should not contain line 1");
    }

    #[tokio::test]
    async fn rejects_binary_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("img.png"), b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let tool = ReadTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "file_path": "img.png" }),
                &crate::Recorder::detached(),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn read_permission_relative_path() {
        let tool = ReadTool::new(ToolCtx::new(
            PathBuf::from("/project"),
            PathBuf::from("/tmp"),
            "test".into(),
        ));
        match tool.permission(&serde_json::json!({"file_path": "/project/src/main.rs"})) {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs[0].display, "src/main.rs");
                assert!(reqs[0].patterns.contains(&"read".to_string()));
                assert!(reqs[0].patterns.contains(&"read:src/main.rs".to_string()));
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn schema_has_no_filter_fields() {
        let tool = ReadTool::new(ToolCtx::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            "test".into(),
        ));
        let params = tool.parameters();
        assert!(params["properties"].get("parse").is_none());
    }
}
