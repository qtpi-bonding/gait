use std::path::PathBuf;
use std::sync::Arc;
use crate::hashline::{compute_line_hash};
use crate::types::{Tool, ExecutionMode, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_READ_LIMIT: u64 = 2000;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_BYTES: usize = 50 * 1024;

static BINARY_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "exe", "dll", "so", "class", "jar", "war",
    "7z", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods",
    "odp", "bin", "dat", "obj", "o", "a", "lib", "wasm", "pyc", "pyo",
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tiff", "webp",
    "mp3", "mp4", "wav", "avi", "mkv", "pdf",
];

fn is_binary_by_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_binary_by_content(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut buf = [0u8; 4096];
    let Ok(n) = file.read(&mut buf) else { return false };
    if n == 0 { return false; }
    let sample = &buf[..n];
    // Null byte = definitely binary
    if sample.contains(&0u8) { return true; }
    // >30% non-printable (excluding tabs, newlines, carriage returns) = binary
    let non_printable = sample.iter()
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
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl ReadTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<ReadParams>(),
            crate::types::OutputFilter::Parse,
        );
        Self {
            codebase_path: Arc::new(codebase_path),
            parameters,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str {
        "Read a file from the codebase, optionally with line range. Each line is prefixed with \
        N#XY|content where N is the 1-based line number and XY is a 2-char content hash. \
        Use these N#XY anchors with the `edit` tool to modify lines without reproducing old content. \
        Paths are relative to the codebase root."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: ReadParams = serde_json::from_value(args)?;
        let offset = params.offset.unwrap_or(0) as usize;
        let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT) as usize;

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.codebase_path.join(&params.file_path)
        };

        if !resolved.exists() {
            // Suggest nearby files
            let dir = resolved.parent().unwrap_or(&resolved);
            let base = resolved.file_name()
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
            anyhow::bail!("File not found: {}{}", resolved.display(), hint);
        }

        if is_binary_by_extension(&resolved) || is_binary_by_content(&resolved) {
            anyhow::bail!("Cannot read binary file: {}", resolved.display());
        }

        let text = std::fs::read_to_string(&resolved)?;
        let lines: Vec<&str> = text.split('\n').collect();

        let mut output_lines: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated_by_bytes = false;
        let end = lines.len().min(offset + limit);

        for i in offset..end {
            let raw = lines[i];
            let line = if raw.len() > MAX_LINE_LENGTH {
                format!("{}...", &raw[..MAX_LINE_LENGTH])
            } else {
                raw.to_string()
            };
            let size = line.len() + if output_lines.is_empty() { 0 } else { 1 };
            if bytes + size > MAX_BYTES {
                truncated_by_bytes = true;
                break;
            }
            let hash = compute_line_hash(i + 1, &line);
            output_lines.push(format!("{}#{}|{}", i + 1, hash, line));
            bytes += size;
        }

        let content = output_lines.join("\n");
        let total_lines = lines.len();
        let last_read = offset + output_lines.len();

        let footer = if truncated_by_bytes {
            format!("\n\n(Output truncated at {} bytes. Use 'offset' to read beyond line {})", MAX_BYTES, last_read)
        } else if total_lines > last_read {
            format!("\n\n(File has more lines. Use 'offset' to read beyond line {})", last_read)
        } else {
            format!("\n\n(End of file - total {} lines)", total_lines)
        };

        Ok(Value::String(format!("<file>\n{}{}\n</file>", content, footer)))
    }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("read", &rel),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "line1\nline2\nline3").unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "file_path": "test.txt" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("|line1"), "got: {text}");
        assert!(text.contains("|line2"), "got: {text}");
        assert!(text.contains("|line3"), "got: {text}");
        // hashline format: N#XY|content
        assert!(text.contains("1#"), "should have hash anchor: {text}");
    }

    #[tokio::test]
    async fn returns_error_for_missing_file() {
        let dir = TempDir::new().unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "file_path": "nonexistent.txt" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn respects_offset_and_limit() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "a\nb\nc\nd\ne").unwrap();

        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "test.txt", "offset": 1, "limit": 2
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("|b"), "got: {text}");
        assert!(text.contains("|c"), "got: {text}");
        assert!(!text.contains("|a"), "should not contain line 1");
    }

    #[tokio::test]
    async fn rejects_binary_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("img.png"), b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "file_path": "img.png" })).await;
        assert!(result.is_err());
    }

    #[test]
    fn read_permission_relative_path() {
        let codebase = std::path::PathBuf::from("/project");
        let tool = ReadTool::new(codebase);
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
        let tool = ReadTool::new(std::path::PathBuf::from("/tmp"));
        let params = tool.parameters();
        assert_eq!(params["properties"]["parse"]["type"].as_str().unwrap(), "array",
            "parse should be array type: {}", params);
    }
}
