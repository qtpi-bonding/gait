use std::path::PathBuf;
use std::sync::Arc;
use crate::types::{Tool, ExecutionMode};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_RESULTS: usize = 50;

#[derive(Deserialize, JsonSchema)]
struct CodesearchParams {
    /// Symbol name, function name, or regex pattern to search for
    query: String,
    /// Directory to search within (relative to codebase root). Defaults to entire codebase.
    path: Option<String>,
    /// Case-sensitive search (default false)
    case_sensitive: Option<bool>,
}

pub struct CodesearchTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl CodesearchTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<CodesearchParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

pub fn walk_and_search(dir: &std::path::Path, pattern: &regex::Regex, results: &mut Vec<String>) {
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 { return true; }  // always enter the root (its name may start with '.')
            let name = e.file_name().to_str().unwrap_or("");
            !(name.starts_with('.') || name == "node_modules" || name == "target")
        })
        .filter_map(|e| e.ok())
    {
        if results.len() >= MAX_RESULTS { break; }
        if !entry.path().is_file() { continue; }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in content.lines().enumerate() {
            if results.len() >= MAX_RESULTS { break; }
            if pattern.is_match(line) {
                results.push(format!("{}:{}: {}", entry.path().display(), i + 1, line));
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for CodesearchTool {
    fn name(&self) -> &str { "codesearch" }
    fn description(&self) -> &str {
        "Search for symbols, function names, class definitions, or patterns in the codebase. Use for finding where something is defined or used. Returns file:line matches."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: CodesearchParams = serde_json::from_value(args)?;
        let case_sensitive = params.case_sensitive.unwrap_or(false);

        let search_dir = match &params.path {
            Some(p) => self.codebase_path.join(p),
            None => (*self.codebase_path).clone(),
        };

        // Try ripgrep first (exit 0 = matches, exit 1 = no matches, other = not installed)
        let mut rg_cmd = tokio::process::Command::new("rg");
        rg_cmd.arg("--line-number").arg("--no-heading").arg("--no-ignore");
        if !case_sensitive { rg_cmd.arg("-i"); }
        rg_cmd.arg(&params.query).arg(&search_dir);

        if let Ok(out) = rg_cmd.output().await {
            match out.status.code() {
                Some(0) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
                    if lines.is_empty() {
                        return Ok(Value::String("No matches found.".into()));
                    }
                    let truncated = lines.len() > MAX_RESULTS;
                    let body = lines[..lines.len().min(MAX_RESULTS)].join("\n");
                    let output = if truncated {
                        format!("(first {} results)\n{}", MAX_RESULTS, body)
                    } else {
                        body
                    };
                    return Ok(Value::String(output));
                }
                Some(1) => return Ok(Value::String("No matches found.".into())),
                _ => {} // rg not installed or error — fall through to Rust fallback
            }
        }

        // Fallback: walkdir + regex
        let prefix = if case_sensitive { "" } else { "(?i)" };
        let pattern = match regex::Regex::new(&format!("{}{}", prefix, &params.query)) {
            Ok(p) => p,
            Err(_) => return Ok(Value::String(format!("Invalid search pattern: {}", params.query))),
        };
        let mut results = vec![];
        walk_and_search(&search_dir, &pattern, &mut results);
        if results.is_empty() {
            return Ok(Value::String("No matches found.".into()));
        }
        Ok(Value::String(results.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Direct unit tests for walk_and_search (exercises fallback path independently of rg)

    #[test]
    fn walk_and_search_caps_at_max_results() {
        let dir = TempDir::new().unwrap();
        for i in 0..60 {
            std::fs::write(dir.path().join(format!("f{}.txt", i)), "match_me\n").unwrap();
        }
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(results.len() <= MAX_RESULTS, "got {} > {}", results.len(), MAX_RESULTS);
    }

    #[test]
    fn walk_and_search_prunes_dotdirs() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "match_me").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "no_match").unwrap();
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(results.is_empty(), "should not search .git, got: {:?}", results);
    }

    #[test]
    fn walk_and_search_prunes_node_modules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/dep.js"), "match_me").unwrap();
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(results.is_empty(), "should not search node_modules, got: {:?}", results);
    }

    // Integration tests via CodesearchTool.execute

    #[tokio::test]
    async fn finds_literal_in_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn hello_world() {\n    println!(\"hi\");\n}").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "hello_world" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {}", text);
        assert!(text.contains("hello_world"), "got: {}", text);
    }

    #[tokio::test]
    async fn case_insensitive_by_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "struct MyStruct {}").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "mystruct" })).await.unwrap();
        assert!(result.as_str().unwrap().contains("MyStruct"));
    }

    #[tokio::test]
    async fn case_sensitive_does_not_match_wrong_case() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "struct MyStruct {}").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "mystruct", "case_sensitive": true })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No matches found.");
    }

    #[tokio::test]
    async fn no_matches_returns_message() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "totally_nonexistent_xyz" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No matches found.");
    }

    #[tokio::test]
    async fn caps_at_max_results_via_tool() {
        let dir = TempDir::new().unwrap();
        for i in 0..60 {
            std::fs::write(dir.path().join(format!("file_{}.txt", i)), "target_match").unwrap();
        }

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "target_match" })).await.unwrap();
        let text = result.as_str().unwrap();
        let match_lines = text.lines().filter(|l| l.contains("target_match")).count();
        assert!(match_lines <= MAX_RESULTS, "got {} results, expected ≤ {}", match_lines, MAX_RESULTS);
    }

    #[tokio::test]
    async fn invalid_regex_returns_graceful_message() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "test").unwrap();
        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "[invalid_unclosed" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(
            text == "No matches found." || text.contains("Invalid search pattern"),
            "unexpected: {}", text
        );
    }

    #[tokio::test]
    async fn restricts_to_subdirectory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inside.txt"), "find_me").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "find_me").unwrap();

        let tool = CodesearchTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "query": "find_me", "path": "sub" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("inside.txt"), "got: {}", text);
        assert!(!text.contains("outside.txt"), "got: {}", text);
    }
}
