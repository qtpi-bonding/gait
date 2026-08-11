use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::io::Write as _;
use std::path::PathBuf;

#[derive(Deserialize, JsonSchema)]
struct GrepParams {
    /// The regex pattern to search for in file contents
    pattern: String,
    /// Directory or file to search (relative to codebase root). Defaults to entire codebase.
    path: Option<String>,
    /// File glob pattern to include (e.g. "*.rs", "*.{ts,tsx}")
    include: Option<String>,
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
    output_path: &std::path::Path,
) -> anyhow::Result<Option<bool>> {
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
    let search_path_obj = std::path::Path::new(search_path);
    cmd.current_dir(if search_path_obj.is_dir() {
        search_path_obj
    } else {
        search_path_obj.parent().unwrap_or(search_path_obj)
    });
    // Without this, a timeout only bounds the *wait* — tokio's default is to
    // NOT kill the child on drop, so dropping the timed-out output() future
    // leaves rg running as an orphan reaped whenever it eventually exits on
    // its own. That's the exact process-escape class this plan exists to fix.
    cmd.kill_on_drop(true);
    let output_file = std::fs::File::create(output_path)?;
    cmd.stdout(std::process::Stdio::from(output_file))
        .stderr(std::process::Stdio::null());

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.status()).await {
        Ok(Ok(status)) => match status.code() {
            Some(0) => Ok(Some(true)),
            Some(1) => Ok(Some(false)),
            _ => Ok(None),
        },
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => anyhow::bail!("ripgrep search timed out after {timeout_secs}s"),
    }
}

fn regex_search(
    pattern: &str,
    search_path: &std::path::Path,
    output_path: &std::path::Path,
) -> anyhow::Result<bool> {
    let re =
        regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;
    let mut output = std::io::BufWriter::new(std::fs::File::create(output_path)?);
    let mut found = false;
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
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                found = true;
                writeln!(output, "{}:{}:{}", path.display(), i + 1, line)?;
            }
        }
    }
    output.flush()?;
    Ok(found)
}

fn grep_error_result(
    output_path: &std::path::Path,
    error: &anyhow::Error,
) -> Result<Value, ToolError> {
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output_path)
        .map_err(|write_error| ToolError::Internal(write_error.into()))?;
    writeln!(output, "\n[grep error: {error}]")
        .map_err(|write_error| ToolError::Internal(write_error.into()))?;
    output
        .flush()
        .map_err(|write_error| ToolError::Internal(write_error.into()))?;
    let artifact = crate::tools::artifact_for_existing(output_path.to_path_buf())
        .map_err(|read_error| ToolError::Internal(read_error.into()))?;
    crate::tools::model_text_for_artifact(&artifact)
        .map(Value::String)
        .map_err(|read_error| ToolError::Internal(read_error.into()))
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
        let output_path = match &self.ctx.output_path {
            Some(path) => path.clone(),
            None => crate::tools::prepare_tool_output_path(
                &self.ctx.gait_dir,
                &self.ctx.session_id,
                self.name(),
            )
            .map_err(|error| crate::tools::ToolError::Internal(error.into()))?,
        };
        let rg_result = rg_search(
            &params.pattern,
            &resolved.display().to_string(),
            params.include.as_deref(),
            timeout_secs,
            &output_path,
        )
        .await;
        let found = match rg_result {
            Err(error) => return grep_error_result(&output_path, &error),
            Ok(result) => match result {
                Some(found) => found, // rg ran — authoritative (even if empty)
                None => match regex_search(&params.pattern, &resolved, &output_path) {
                    Ok(found) => found,
                    Err(error) => return grep_error_result(&output_path, &error),
                }, // rg unavailable/errored — fall back
            },
        };

        if !found {
            let artifact = crate::tools::archive_text(output_path, "No files found")
                .map_err(|error| crate::tools::ToolError::Internal(error.into()))?;
            let model = crate::tools::model_text_for_artifact(&artifact)
                .map_err(|error| crate::tools::ToolError::Internal(error.into()))?;
            return Ok(Value::String(model));
        }

        let artifact = crate::tools::artifact_for_existing(output_path)
            .map_err(|error| crate::tools::ToolError::Internal(error.into()))?;
        let model = crate::tools::model_text_for_artifact(&artifact)
            .map_err(|error| crate::tools::ToolError::Internal(error.into()))?;
        Ok(Value::String(model))
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
        let text = result.as_str().unwrap();
        assert!(text.contains("No files found"));
        assert!(text.contains("[full output:"));
    }

    #[tokio::test]
    async fn empty_pattern_archives_every_match_but_bounds_model_preview() {
        let dir = TempDir::new().unwrap();
        let match_count = 5_000;
        let content = (0..match_count)
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
        assert!(text.contains("Tool output truncated by Gait"));
        assert!(text.len() <= crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        let artifact = crate::tools::artifact_from_text(text, dir.path(), "test")
            .expect("model preview should expose the complete output artifact");
        let full = std::fs::read_to_string(artifact.path).unwrap();
        assert_eq!(full.lines().count(), match_count);
        assert!(full.contains("line 4999"));
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
        let result = rg_search(
            "needle",
            &dir.path().display().to_string(),
            None,
            0,
            &dir.path().join("grep-output.txt"),
        )
        .await;
        assert!(result.is_err(), "expected timeout error, got: {result:?}");
        assert!(
            result.unwrap_err().to_string().contains("timed out"),
            "error should mention timeout"
        );
    }
}
