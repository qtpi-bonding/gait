use std::path::PathBuf;
use std::sync::Arc;
use crate::types::{Tool, ExecutionMode};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_LINE_LENGTH: usize = 2000;
const GREP_LIMIT: usize = 100;

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
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl GrepTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<GrepParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

fn rg_search(pattern: &str, search_path: &str, include: Option<&str>) -> anyhow::Result<Vec<MatchEntry>> {
    let mut cmd = std::process::Command::new("rg");
    cmd.args(["-nH", "--hidden", "--no-heading", "--no-messages", "--regexp", pattern]);
    if let Some(inc) = include { cmd.args(["--glob", inc]); }
    cmd.arg(search_path);
    let output = cmd.output()?;
    if output.stdout.is_empty() {
        return Ok(vec![]);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut matches = vec![];
    for line in text.lines() {
        if line.is_empty() { continue; }
        // Format from -nH --no-heading: /path/to/file.rs:42:line text here
        // splitn(3, ':') handles paths that contain ':' (the text portion may also contain ':')
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() < 3 { continue; }
        let file_path = parts[0].to_string();
        let Ok(line_num) = parts[1].parse::<usize>() else { continue };
        let line_text = parts[2].to_string();
        let mod_time = std::fs::metadata(&file_path).ok().and_then(|m| m.modified().ok());
        matches.push(MatchEntry { path: file_path, line_num, line_text, mod_time });
    }
    Ok(matches)
}

fn regex_search(pattern: &str, search_path: &std::path::Path) -> anyhow::Result<Vec<MatchEntry>> {
    let re = regex::Regex::new(pattern)
        .map_err(|e| anyhow::anyhow!("Invalid regex pattern: {e}"))?;
    let mut matches = vec![];
    let files: Box<dyn Iterator<Item = walkdir::DirEntry>> = if search_path.is_file() {
        Box::new(walkdir::WalkDir::new(search_path).into_iter().filter_map(|e| e.ok()))
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
        let Ok(content) = std::fs::read_to_string(path) else { continue };
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
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str {
        "Search for a regex pattern across files in the codebase. Returns matching lines with file and line number."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: GrepParams = serde_json::from_value(args)?;

        let resolved = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.codebase_path.join(p),
            None => (*self.codebase_path).clone(),
        };

        let mut matches = rg_search(&params.pattern, &resolved.display().to_string(), params.include.as_deref())
            .unwrap_or_default();

        if matches.is_empty() {
            matches = regex_search(&params.pattern, &resolved)?;
        }

        if matches.is_empty() { return Ok(Value::String("No files found".into())); }

        matches.sort_by(|a, b| b.mod_time.cmp(&a.mod_time));

        let truncated = matches.len() > GREP_LIMIT;
        let final_matches = &matches[..matches.len().min(GREP_LIMIT)];

        let mut lines = vec![format!("Found {} matches", final_matches.len())];
        let mut current_file = String::new();
        for m in final_matches {
            if current_file != m.path {
                if !current_file.is_empty() { lines.push(String::new()); }
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
        if truncated {
            lines.push(String::new());
            lines.push("(Results are truncated. Consider using a more specific path or pattern.)".into());
        }

        Ok(Value::String(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn finds_pattern_in_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("foo.rs"),
            "fn main() {\n    println!(\"hello world\");\n}").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "println" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("foo.rs"), "got: {text}");
        assert!(text.contains("println"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_no_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "zzz_not_present" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn respects_path_argument() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "needle").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "pattern": "needle",
            "path": "sub"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("a.txt"), "got: {text}");
        assert!(!text.contains("b.txt"), "got: {text}");
    }
}
