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
    search_path: &std::path::Path,
    include: Option<&str>,
    timeout_secs: u64,
    output_path: &std::path::Path,
    excluded_root: &std::path::Path,
) -> anyhow::Result<Option<bool>> {
    if search_path.starts_with(excluded_root) {
        std::fs::File::create(output_path)?;
        return Ok(Some(false));
    }

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
    if let Some(exclusion) = rg_exclusion_glob(search_path, excluded_root) {
        cmd.args(["--glob", &exclusion]);
    }
    cmd.arg(search_path);
    cmd.current_dir(if search_path.is_dir() {
        search_path
    } else {
        search_path.parent().unwrap_or(search_path)
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

fn rg_exclusion_glob(
    search_path: &std::path::Path,
    excluded_root: &std::path::Path,
) -> Option<String> {
    let search_root = if search_path.is_dir() {
        search_path
    } else {
        search_path.parent()?
    };
    let relative = excluded_root.strip_prefix(search_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let relative = relative.to_string_lossy().replace('\\', "/");
    let escaped = relative
        .chars()
        .fold(String::new(), |mut escaped, character| {
            if matches!(character, '*' | '?' | '[' | ']' | '{' | '}') {
                escaped.push('\\');
            }
            escaped.push(character);
            escaped
        });
    Some(format!("!{escaped}/**"))
}

fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(relative_close) = pattern[open + 1..].find('}') else {
        return vec![pattern.to_string()];
    };
    let close = open + 1 + relative_close;
    let choices = &pattern[open + 1..close];
    if !choices.contains(',') {
        return vec![pattern.to_string()];
    }

    choices
        .split(',')
        .flat_map(|choice| {
            let expanded = format!("{}{}{}", &pattern[..open], choice, &pattern[close + 1..]);
            expand_braces(&expanded)
        })
        .collect()
}

fn compile_include(include: Option<&str>) -> anyhow::Result<Vec<glob::Pattern>> {
    include
        .map(expand_braces)
        .unwrap_or_default()
        .into_iter()
        .map(|pattern| {
            glob::Pattern::new(&pattern)
                .map_err(|error| anyhow::anyhow!("Invalid include glob: {error}"))
        })
        .collect()
}

fn included(patterns: &[glob::Pattern], path: &std::path::Path, root: &std::path::Path) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    patterns.iter().any(|pattern| {
        pattern.matches_path(relative)
            || relative
                .file_name()
                .is_some_and(|name| pattern.matches_path(std::path::Path::new(name)))
    })
}

fn regex_search(
    pattern: &str,
    search_path: &std::path::Path,
    include: Option<&str>,
    output_path: &std::path::Path,
    excluded_root: &std::path::Path,
) -> anyhow::Result<bool> {
    let re =
        regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;
    let include = compile_include(include)?;
    let mut output = std::io::BufWriter::new(std::fs::File::create(output_path)?);
    let mut found = false;
    let search_root = if search_path.is_dir() {
        search_path
    } else {
        search_path.parent().unwrap_or(search_path)
    };
    let files = walkdir::WalkDir::new(search_path)
        .into_iter()
        .filter_entry(|entry| !entry.path().starts_with(excluded_root));
    for entry in files.filter_map(|entry| entry.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !included(&include, path, search_root) {
            continue;
        }
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
    Ok(Value::String(format!("[grep error: {error}]")))
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
        let output_path = match self.ctx.output_capture.spool_path() {
            Some(path) => path.to_path_buf(),
            None => crate::tools::prepare_tool_output_path(
                &self.ctx.gait_dir,
                &self.ctx.session_id,
                self.name(),
            )
            .map_err(|error| crate::tools::ToolError::Internal(error.into()))?,
        };
        let excluded_root =
            crate::session::live::outputs_dir(&self.ctx.gait_dir, &self.ctx.session_id);
        let rg_result = rg_search(
            &params.pattern,
            &resolved,
            params.include.as_deref(),
            timeout_secs,
            &output_path,
            &excluded_root,
        )
        .await;
        let found = match rg_result {
            Err(error) => return grep_error_result(&output_path, &error),
            Ok(result) => match result {
                Some(found) => found, // rg ran — authoritative (even if empty)
                None => match regex_search(
                    &params.pattern,
                    &resolved,
                    params.include.as_deref(),
                    &output_path,
                    &excluded_root,
                ) {
                    Ok(found) => found,
                    Err(error) => return grep_error_result(&output_path, &error),
                }, // rg unavailable/errored — fall back
            },
        };

        if !found {
            crate::tools::archive_text(output_path, "No files found")
                .map_err(|error| crate::tools::ToolError::Internal(error.into()))?;
            return Ok(Value::String("No files found".into()));
        }

        crate::tools::full_output_for_existing(output_path.clone())
            .map_err(|error| crate::tools::ToolError::Internal(error.into()))?;
        Ok(Value::String(format!(
            "[grep output captured at {}]",
            output_path.display()
        )))
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            ..Default::default()
        }
    }

    fn output_capture_mode(&self) -> crate::types::OutputCaptureMode {
        crate::types::OutputCaptureMode::Spool
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

    async fn execute_finalized(dir: &TempDir, args: serde_json::Value) -> serde_json::Value {
        let output_path =
            crate::session::live::outputs_dir(dir.path(), "test").join("tools/test/grep.txt");
        std::fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        let tool = GrepTool::new(make_ctx(dir).with_output_spool(output_path.clone()));
        let raw = tool
            .execute(args, &crate::Recorder::detached())
            .await
            .unwrap();
        crate::tools::archive_and_prepare_model_value(raw, output_path)
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn finds_pattern_in_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("foo.rs"),
            "fn main() {\n    println!(\"hello world\");\n}",
        )
        .unwrap();

        let result = execute_finalized(&dir, serde_json::json!({ "pattern": "println" })).await;
        let text = result.as_str().unwrap();
        assert!(text.contains("foo.rs"), "got: {text}");
        assert!(text.contains("println"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_no_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let result =
            execute_finalized(&dir, serde_json::json!({ "pattern": "zzz_not_present" })).await;
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

        let result = execute_finalized(
            &dir,
            serde_json::json!({ "pattern": "", "include": "*.txt" }),
        )
        .await;
        let text = result.as_str().unwrap();
        assert!(text.contains("Tool output truncated by Gait"));
        assert!(text.len() <= crate::tools::MODEL_TOOL_OUTPUT_MAX_BYTES);
        let full_output = crate::tools::full_output_from_text(text, dir.path(), "test")
            .expect("model preview should expose the complete output file");
        let full = std::fs::read_to_string(full_output.path).unwrap();
        assert_eq!(full.lines().count(), match_count);
        assert!(full.contains("line 4999"));
    }

    #[tokio::test]
    async fn respects_path_argument() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "needle").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle").unwrap();

        let result = execute_finalized(
            &dir,
            serde_json::json!({
                "pattern": "needle",
                "path": "sub"
            }),
        )
        .await;
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
            dir.path(),
            None,
            0,
            &dir.path().join("grep-output.txt"),
            &dir.path().join("excluded"),
        )
        .await;
        assert!(result.is_err(), "expected timeout error, got: {result:?}");
        assert!(
            result.unwrap_err().to_string().contains("timed out"),
            "error should mention timeout"
        );
    }

    #[test]
    fn regex_fallback_honors_include_and_excludes_session_outputs() {
        let dir = TempDir::new().unwrap();
        let output_root = crate::session::live::outputs_dir(dir.path(), "test");
        std::fs::create_dir_all(&output_root).unwrap();
        std::fs::write(dir.path().join("keep.rs"), "needle").unwrap();
        std::fs::write(dir.path().join("skip.txt"), "needle").unwrap();
        std::fs::write(output_root.join("old.rs"), "needle").unwrap();
        let output_path = output_root.join("grep.txt");

        assert!(regex_search(
            "needle",
            dir.path(),
            Some("*.rs"),
            &output_path,
            &output_root,
        )
        .unwrap());

        let output = std::fs::read_to_string(output_path).unwrap();
        assert!(output.contains("keep.rs"));
        assert!(!output.contains("skip.txt"));
        assert!(!output.contains("old.rs"));
    }

    #[test]
    fn regex_fallback_expands_braced_include() {
        let dir = TempDir::new().unwrap();
        let output_root = dir.path().join("excluded");
        std::fs::write(dir.path().join("one.ts"), "needle").unwrap();
        std::fs::write(dir.path().join("two.tsx"), "needle").unwrap();
        std::fs::write(dir.path().join("skip.js"), "needle").unwrap();
        let output_path = dir.path().join("grep.txt");

        assert!(regex_search(
            "needle",
            dir.path(),
            Some("*.{ts,tsx}"),
            &output_path,
            &output_root,
        )
        .unwrap());

        let output = std::fs::read_to_string(output_path).unwrap();
        assert!(output.contains("one.ts"));
        assert!(output.contains("two.tsx"));
        assert!(!output.contains("skip.js"));
    }
}
