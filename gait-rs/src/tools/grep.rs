use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

const MAX_LINE_LENGTH: usize = 2000;

#[derive(Deserialize, JsonSchema)]
struct GrepParams {
    /// The regex pattern to search for in file contents
    pattern: String,
    /// Directory or file to search (relative to codebase root). Defaults to entire codebase.
    path: Option<String>,
    /// File glob pattern to include (e.g. "*.rs", "*.{ts,tsx}")
    include: Option<String>,
}

struct MatchEntry {
    path: String,
    line_num: usize,
    line_text: String,
    mod_time: Option<std::time::SystemTime>,
}

pub struct GrepTool {
    ctx: ToolCtx,
    parameters: Value,
}

impl GrepTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<GrepParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { ctx, parameters }
    }
}

/// Returns None if rg is unavailable or errored (caller should fall back to regex_search).
/// Returns Some(matches) — possibly empty — when rg ran and produced an authoritative result.
/// rg exit codes: 0 = matches found, 1 = no matches, 2 = error.
async fn rg_search(
    pattern: &str,
    search_path: &str,
    include: Option<&str>,
) -> anyhow::Result<Option<Vec<MatchEntry>>> {
    let mut cmd = tokio::process::Command::new("rg");
    cmd.args([
        "-nH",
        "--hidden",
        "--no-heading",
        "--no-messages",
        "--regexp",
        pattern,
    ]);
    if let Some(inc) = include {
        cmd.args(["--glob", inc]);
    }
    cmd.arg(search_path);
    cmd.current_dir(std::path::Path::new(search_path));

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None), // rg not installed
        Err(e) => return Err(e.into()),
    };

    match output.status.code() {
        Some(0) | Some(1) => {} // 0 = matches, 1 = no matches — both authoritative, don't re-scan
        _ => return Ok(None),   // exit 2 or signal: rg error, fall back to regex_search
    }

    if output.stdout.is_empty() {
        return Ok(Some(vec![]));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut matches = vec![];
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        // Format from -nH --no-heading: /path/to/file.rs:42:line text here
        // splitn(3, ':') handles paths that contain ':' (the text portion may also contain ':')
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() < 3 {
            continue;
        }
        let file_path = parts[0].to_string();
        let Ok(line_num) = parts[1].parse::<usize>() else {
            continue;
        };
        let line_text = parts[2].to_string();
        let mod_time = std::fs::metadata(&file_path)
            .ok()
            .and_then(|m| m.modified().ok());
        matches.push(MatchEntry {
            path: file_path,
            line_num,
            line_text,
            mod_time,
        });
    }
    Ok(Some(matches))
}

fn regex_search(pattern: &str, search_path: &std::path::Path) -> anyhow::Result<Vec<MatchEntry>> {
    let re =
        regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;
    let mut matches = vec![];
    let files: Box<dyn Iterator<Item = walkdir::DirEntry>> = if search_path.is_file() {
        Box::new(
            walkdir::WalkDir::new(search_path)
                .into_iter()
                .filter_map(|e| e.ok()),
        )
    } else {
        Box::new(
            walkdir::WalkDir::new(search_path)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file()),
        )
    };
    for entry in files {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let mod_time = path.metadata().ok().and_then(|m| m.modified().ok());
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                matches.push(MatchEntry {
                    path: path.display().to_string(),
                    line_num: i + 1,
                    line_text: line.to_string(),
                    mod_time,
                });
            }
        }
    }
    Ok(matches)
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search for a regex pattern across files in the codebase. Returns matching lines with file and line number."
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
        let params: GrepParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;

        let resolved = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => self.ctx.codebase_path.clone(),
        };

        let mut matches = match rg_search(
            &params.pattern,
            &resolved.display().to_string(),
            params.include.as_deref(),
        )
        .await
        .map_err(crate::tools::ToolError::Internal)?
        {
            Some(m) => m, // rg ran — authoritative (even if empty)
            None => regex_search(&params.pattern, &resolved)
                .map_err(crate::tools::ToolError::Internal)?, // rg unavailable/errored — fall back
        };

        if matches.is_empty() {
            return Ok(Value::String("No files found".into()));
        }

        matches.sort_by(|a, b| b.mod_time.cmp(&a.mod_time));

        let mut lines = vec![format!("Found {} matches", matches.len())];
        let mut current_file = String::new();
        for m in &matches {
            if current_file != m.path {
                if !current_file.is_empty() {
                    lines.push(String::new());
                }
                current_file = m.path.clone();
                lines.push(format!("{}:", m.path));
            }
            let text = if m.line_text.len() > MAX_LINE_LENGTH {
                format!("{}...", &m.line_text[..MAX_LINE_LENGTH])
            } else {
                m.line_text.clone()
            };
            lines.push(format!("  Line {}: {}", m.line_num, text));
        }

        Ok(Value::String(lines.join("\n")))
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            ..Default::default()
        }
    }

    fn scope_targets(&self, args: &serde_json::Value) -> Vec<(PathBuf, crate::types::Dimension)> {
        // `path` constrains the search root. No explicit path = codebase root (in ceiling).
        // `pattern` is a regex pattern, not a concrete path — not checked here.
        let resolved = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => return vec![],
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
    async fn finds_pattern_in_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("foo.rs"),
            "fn main() {\n    println!(\"hello world\");\n}",
        )
        .unwrap();

        let tool = GrepTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "println" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("foo.rs"), "got: {text}");
        assert!(text.contains("println"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_no_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let tool = GrepTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "zzz_not_present" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn respects_path_argument() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "needle").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle").unwrap();

        let tool = GrepTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({
                    "pattern": "needle",
                    "path": "sub"
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("a.txt"), "got: {text}");
        assert!(!text.contains("b.txt"), "got: {text}");
    }

    #[test]
    fn schema_has_parse_array_field() {
        let tool = GrepTool::new(ToolCtx::new(
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
