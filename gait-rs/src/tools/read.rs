use crate::hashline::annotate_lines;
use crate::permission::path_patterns;
use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, PermissionRequest, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

const DEFAULT_READ_LIMIT: u64 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

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

fn is_binary_by_content(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let Ok(n) = file.read(&mut buf) else {
        return false;
    };
    if n == 0 {
        return false;
    }
    let sample = &buf[..n];
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
            crate::types::OutputFilter::Both,
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
        "Read a file from the codebase, optionally with line range. Each line is prefixed with \
         #XY|content where XY is a 2-char content hash (a unique line), or #XY-N|content where \
         N is the instance number if the same content appears multiple times in the file. \
         Use the part after `#` as the edit ref with the `edit` tool — `XY` for unique lines, \
         `XY-N` for duplicate lines — to modify lines without reproducing old content. \
         Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

    async fn execute(&self, args: Value, _rec: &crate::Recorder) -> Result<Value, ToolError> {
        let params: ReadParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;
        let offset = params.offset.unwrap_or(0) as usize;
        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT) as usize;

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.ctx.codebase_path.join(&params.file_path)
        };

        if !resolved.exists() {
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

        if is_binary_by_extension(&resolved) || is_binary_by_content(&resolved) {
            return Err(crate::tools::ToolError::Execute(format!(
                "Cannot read binary file: {}",
                resolved.display()
            )));
        }

        let text = std::fs::read_to_string(&resolved)
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        let lines: Vec<&str> = text.split('\n').collect();

        // Two-pass annotation: hashes depend on counts across the WHOLE file so duplicate
        // instance numbers are correct regardless of the requested offset/limit window.
        let annotated_all = annotate_lines(&lines);

        let mut output_lines: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated_by_bytes = false;
        let end = lines.len().min(offset + limit);

        for annotated in annotated_all.iter().take(end).skip(offset) {
            // Strip the `#XY|` (or `#XY-N|`) prefix, then truncate content if too long,
            // then re-stamp with the same prefix so the truncated line is still anchorable.
            let (prefix, content) = match annotated.find('|') {
                Some(pipe) => (&annotated[..=pipe], &annotated[pipe + 1..]),
                None => ("#??", annotated.as_str()),
            };
            let line = if content.len() > MAX_LINE_LENGTH {
                format!("{}...", &content[..MAX_LINE_LENGTH])
            } else {
                content.to_string()
            };
            let formatted = format!("{}{}", prefix, line);
            let size = formatted.len() + if output_lines.is_empty() { 0 } else { 1 };
            if bytes + size > MAX_BYTES {
                truncated_by_bytes = true;
                break;
            }
            output_lines.push(formatted);
            bytes += size;
        }

        let content = output_lines.join("\n");
        let total_lines = lines.len();
        let last_read = offset + output_lines.len();

        let footer = if truncated_by_bytes {
            format!(
                "\n\n(Output truncated at {} bytes. Use 'offset' to read beyond line {})",
                MAX_BYTES, last_read
            )
        } else if total_lines > last_read {
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
    async fn reads_file_with_hashline_format() {
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
        // Each line has its content after `|`
        assert!(text.contains("|line1"), "got: {text}");
        assert!(text.contains("|line2"), "got: {text}");
        assert!(text.contains("|line3"), "got: {text}");
        // Hashline format: #XY|content — there must be `#` prefixes
        assert!(text.contains("#"), "should have hash anchor: {text}");
        // Old `N#XY` format is gone — no leading digit immediately before `#`
        assert!(
            !text.contains("1#"),
            "should not contain old `1#` format: {text}"
        );
        assert!(
            !text.contains("2#"),
            "should not contain old `2#` format: {text}"
        );
        assert!(
            !text.contains("3#"),
            "should not contain old `3#` format: {text}"
        );
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
        // Both occurrences of "same" get context tags (contain '-' before the '|same').
        let lines: Vec<&str> = text.lines().collect();
        let same_lines: Vec<&&str> = lines.iter().filter(|l| l.ends_with("|same")).collect();
        assert_eq!(
            same_lines.len(),
            2,
            "expected 2 annotated 'same' lines: {text}"
        );
        for l in &same_lines {
            assert!(l.contains('-'), "dup 'same' should have context tag: {l}");
        }
        // The two context tags differ (distinct neighbors).
        assert_ne!(
            same_lines[0], same_lines[1],
            "context tags for 'same' should differ: {text}"
        );
        // The unique line gets NO instance suffix
        assert!(text.contains("|other"), "got: {text}");
        assert!(
            !text.contains("-1|other"),
            "unique line should have no suffix: {text}"
        );
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
        assert!(text.contains("|b"), "got: {text}");
        assert!(text.contains("|c"), "got: {text}");
        assert!(!text.contains("|a"), "should not contain line 1");
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
        assert!(text.contains("|b"), "got: {text}");
        assert!(text.contains("|c"), "got: {text}");
        assert!(!text.contains("|a"), "should not contain line 1");
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
    fn schema_has_parse_array_field() {
        let tool = ReadTool::new(ToolCtx::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
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
}
