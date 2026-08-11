use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Deserialize, JsonSchema)]
struct GrepParams {
    /// The regex pattern to search for in file contents
    pattern: String,
    /// Directory or file to search (relative to codebase root). Defaults to entire codebase.
    path: Option<String>,
    /// File glob pattern to include (e.g. "*.rs", "*.{ts,tsx}")
    include: Option<String>,
}

#[derive(Debug)]
struct MatchEntry {
    path: String,
    line_num: usize,
    line_text: String,
    mod_time: Option<std::time::SystemTime>,
}

#[derive(Debug)]
struct SearchMatches {
    entries: Vec<MatchEntry>,
    truncated: bool,
}

fn prefix_within_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (&text[..boundary], true)
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
    timeout_secs: u64,
) -> anyhow::Result<Option<SearchMatches>> {
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
    // Without this, a timeout only bounds the *wait* — tokio's default is to
    // NOT kill the child on drop, so dropping the timed-out output() future
    // leaves rg running as an orphan reaped whenever it eventually exits on
    // its own. That's the exact process-escape class this plan exists to fix.
    cmd.kill_on_drop(true);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let search = async move {
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let stdout = child.stdout.take().expect("ripgrep stdout is piped");
        let mut lines = BufReader::new(stdout).lines();
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut captured_bytes = 0usize;

        while let Some(line) = lines.next_line().await? {
            if line.is_empty() {
                continue;
            }
            // Format from -nH --no-heading: /path/to/file.rs:42:line text here.
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() < 3 {
                continue;
            }
            let file_path = parts[0].to_string();
            let Ok(line_num) = parts[1].parse::<usize>() else {
                continue;
            };
            let mod_time = std::fs::metadata(&file_path)
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            let remaining = crate::tools::MAX_TOOL_OUTPUT_BYTES.saturating_sub(captured_bytes);
            let (line_text, byte_truncated) = prefix_within_bytes(parts[2], remaining);
            captured_bytes += line_text.len();
            matches.push(MatchEntry {
                path: file_path,
                line_num,
                line_text: line_text.to_string(),
                mod_time,
            });
            if byte_truncated
                || captured_bytes >= crate::tools::MAX_TOOL_OUTPUT_BYTES
                || matches.len() >= crate::tools::MAX_TOOL_OUTPUT_LINES
            {
                truncated = true;
                let _ = child.kill().await;
                break;
            }
        }

        let status = child.wait().await?;
        if !truncated && !matches!(status.code(), Some(0) | Some(1)) {
            return Ok(None);
        }
        Ok(Some(SearchMatches {
            entries: matches,
            truncated,
        }))
    };

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), search).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("ripgrep search timed out after {timeout_secs}s"),
    }
}

fn regex_search(pattern: &str, search_path: &std::path::Path) -> anyhow::Result<SearchMatches> {
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
    let mut captured_bytes = 0usize;
    for entry in files {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let mod_time = path.metadata().ok().and_then(|m| m.modified().ok());
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                let remaining = crate::tools::MAX_TOOL_OUTPUT_BYTES.saturating_sub(captured_bytes);
                let (line_text, byte_truncated) = prefix_within_bytes(line, remaining);
                captured_bytes += line_text.len();
                matches.push(MatchEntry {
                    path: path.display().to_string(),
                    line_num: i + 1,
                    line_text: line_text.to_string(),
                    mod_time,
                });
                if byte_truncated
                    || captured_bytes >= crate::tools::MAX_TOOL_OUTPUT_BYTES
                    || matches.len() >= crate::tools::MAX_TOOL_OUTPUT_LINES
                {
                    return Ok(SearchMatches {
                        entries: matches,
                        truncated: true,
                    });
                }
            }
        }
    }
    Ok(SearchMatches {
        entries: matches,
        truncated: false,
    })
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

    async fn execute(&self, args: Value, _rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: GrepParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;

        let resolved = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => self.ctx.codebase_path.clone(),
        };

        let timeout_secs = crate::config::load_config(&self.ctx.gait_dir).tool_timeout_secs;
        let mut search = match rg_search(
            &params.pattern,
            &resolved.display().to_string(),
            params.include.as_deref(),
            timeout_secs,
        )
        .await
        .map_err(crate::tools::ToolError::Internal)?
        {
            Some(matches) => matches, // rg ran — authoritative (even if empty)
            None => regex_search(&params.pattern, &resolved)
                .map_err(crate::tools::ToolError::Internal)?, // rg unavailable/errored — fall back
        };

        if search.entries.is_empty() {
            return Ok(Value::String("No files found".into()));
        }

        search.entries.sort_by(|a, b| b.mod_time.cmp(&a.mod_time));

        let mut lines = vec![if search.truncated {
            format!(
                "Showing first {} matches (results truncated)",
                search.entries.len()
            )
        } else {
            format!("Found {} matches", search.entries.len())
        }];
        let mut current_file = String::new();
        for m in &search.entries {
            if current_file != m.path {
                if !current_file.is_empty() {
                    lines.push(String::new());
                }
                current_file = m.path.clone();
                lines.push(format!("{}:", m.path));
            }
            lines.push(format!("  Line {}: {}", m.line_num, m.line_text));
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
    async fn empty_pattern_is_allowed_but_globally_bounded() {
        let dir = TempDir::new().unwrap();
        let content = (0..(crate::tools::MAX_TOOL_OUTPUT_LINES + 100))
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("many.txt"), content).unwrap();

        let tool = GrepTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "pattern": "", "include": "*.txt" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(
            text.starts_with(&format!(
                "Showing first {} matches (results truncated)",
                crate::tools::MAX_TOOL_OUTPUT_LINES
            )),
            "got: {}",
            &text[..text.len().min(200)]
        );
        assert_eq!(
            text.matches("  Line ").count(),
            crate::tools::MAX_TOOL_OUTPUT_LINES
        );
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
    #[tokio::test]
    async fn rg_search_times_out_on_hung_process() {
        // A pattern that would make `rg` hang forever isn't practical to construct
        // portably, so we exercise the timeout path directly: 0-second timeout
        // guarantees the `tokio::time::timeout` wrapper fires before `rg` (which
        // always takes >0ms to fork+exec) can return. This only exercises the
        // timeout branch if `rg` is actually on PATH — if it's absent, rg_search
        // takes the "rg not installed" branch instead and returns Ok(None), which
        // would fail the assertions below with a confusing message. Skip cleanly
        // in that case rather than false-failing.
        if tokio::process::Command::new("rg")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("skipping: rg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle").unwrap();
        let result = rg_search("needle", &dir.path().display().to_string(), None, 0).await;
        assert!(result.is_err(), "expected timeout error, got: {result:?}");
        assert!(
            result.unwrap_err().to_string().contains("timed out"),
            "error should mention timeout"
        );
    }
}
