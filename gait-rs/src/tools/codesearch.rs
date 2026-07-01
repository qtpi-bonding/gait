use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
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
    ctx: ToolCtx,
    parameters: Value,
}

impl CodesearchTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<CodesearchParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { ctx, parameters }
    }
}

pub fn walk_and_search(dir: &std::path::Path, pattern: &regex::Regex, results: &mut Vec<String>) {
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            } // always enter the root (its name may start with '.')
            let name = e.file_name().to_str().unwrap_or("");
            !(name.starts_with('.') || name == "node_modules" || name == "target")
        })
        .filter_map(|e| e.ok())
    {
        if results.len() >= MAX_RESULTS {
            break;
        }
        if !entry.path().is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (i, line) in content.lines().enumerate() {
            if results.len() >= MAX_RESULTS {
                break;
            }
            if pattern.is_match(line) {
                results.push(format!("{}:{}: {}", entry.path().display(), i + 1, line));
            }
        }
    }
}

#[async_trait::async_trait]
impl Tool for CodesearchTool {
    fn name(&self) -> &str {
        "codesearch"
    }
    fn description(&self) -> &str {
        "Search for symbols, function names, class definitions, or patterns in the codebase. Use for finding where something is defined or used. Returns file:line matches."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Curate
    }

    async fn execute(&self, args: Value, _rec: &crate::Recorder) -> Result<Value, ToolError> {
        let params: CodesearchParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;
        let case_sensitive = params.case_sensitive.unwrap_or(false);

        let search_dir = match &params.path {
            Some(p) => self.ctx.codebase_path.join(p),
            None => self.ctx.codebase_path.clone(),
        };

        // Try ripgrep first (exit 0 = matches, exit 1 = no matches, other = not installed).
        // Bounded subprocess — mirror of grep.rs rg_search. kill_on_drop is
        // required so a timed-out future actually kills the child rather than
        // leaving it running as an orphan (the exact process-escape class the
        // daemon-escape plan exists to fix). On timeout we error the tool
        // directly — falling through to the walkdir fallback would just trade
        // one unbounded operation for another.
        let mut rg_cmd = tokio::process::Command::new("rg");
        rg_cmd
            .arg("--line-number")
            .arg("--no-heading")
            .arg("--no-ignore");
        if !case_sensitive {
            rg_cmd.arg("-i");
        }
        rg_cmd.arg(&params.query).arg(&search_dir);
        rg_cmd.kill_on_drop(true);
        let timeout_secs = crate::config::load_config(&self.ctx.gait_dir).tool_timeout_secs;
        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            rg_cmd.output(),
        )
        .await
        {
            Ok(Ok(out)) => match out.status.code() {
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
            },
            Ok(Err(_)) => {} // rg not installed or spawn error — fall through to Rust fallback
            Err(_) => {
                return Err(crate::tools::ToolError::Execute(format!(
                    "codesearch: ripgrep timed out after {timeout_secs}s"
                )));
            }
        }

        // Fallback: walkdir + regex
        let prefix = if case_sensitive { "" } else { "(?i)" };
        let pattern = match regex::Regex::new(&format!("{}{}", prefix, &params.query)) {
            Ok(p) => p,
            Err(_) => {
                return Ok(Value::String(format!(
                    "Invalid search pattern: {}",
                    params.query
                )))
            }
        };
        let mut results = vec![];
        walk_and_search(&search_dir, &pattern, &mut results);
        if results.is_empty() {
            return Ok(Value::String("No matches found.".into()));
        }
        Ok(Value::String(results.join("\n")))
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            ..Default::default()
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
        assert!(
            results.len() <= MAX_RESULTS,
            "got {} > {}",
            results.len(),
            MAX_RESULTS
        );
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
        assert!(
            results.is_empty(),
            "should not search .git, got: {:?}",
            results
        );
    }

    #[test]
    fn walk_and_search_prunes_node_modules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/dep.js"), "match_me").unwrap();
        let pat = regex::Regex::new("match_me").unwrap();
        let mut results = vec![];
        walk_and_search(dir.path(), &pat, &mut results);
        assert!(
            results.is_empty(),
            "should not search node_modules, got: {:?}",
            results
        );
    }

    // Integration tests via CodesearchTool.execute

    #[tokio::test]
    async fn finds_literal_in_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn hello_world() {\n    println!(\"hi\");\n}",
        )
        .unwrap();

        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "hello_world" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {}", text);
        assert!(text.contains("hello_world"), "got: {}", text);
    }

    #[tokio::test]
    async fn case_insensitive_by_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "struct MyStruct {}").unwrap();

        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "mystruct" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert!(result.as_str().unwrap().contains("MyStruct"));
    }

    #[tokio::test]
    async fn case_sensitive_does_not_match_wrong_case() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "struct MyStruct {}").unwrap();

        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "mystruct", "case_sensitive": true }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "No matches found.");
    }

    #[tokio::test]
    async fn no_matches_returns_message() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();

        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "totally_nonexistent_xyz" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "No matches found.");
    }

    #[tokio::test]
    async fn caps_at_max_results_via_tool() {
        let dir = TempDir::new().unwrap();
        for i in 0..60 {
            std::fs::write(dir.path().join(format!("file_{}.txt", i)), "target_match").unwrap();
        }

        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "target_match" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        let match_lines = text.lines().filter(|l| l.contains("target_match")).count();
        assert!(
            match_lines <= MAX_RESULTS,
            "got {} results, expected ≤ {}",
            match_lines,
            MAX_RESULTS
        );
    }

    #[tokio::test]
    async fn invalid_regex_returns_graceful_message() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "test").unwrap();
        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "[invalid_unclosed" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(
            text == "No matches found." || text.contains("Invalid search pattern"),
            "unexpected: {}",
            text
        );
    }

    #[tokio::test]
    async fn restricts_to_subdirectory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inside.txt"), "find_me").unwrap();
        std::fs::write(dir.path().join("outside.txt"), "find_me").unwrap();

        let tool = CodesearchTool::new(make_ctx(&dir));
        let result = tool
            .execute(
                serde_json::json!({ "query": "find_me", "path": "sub" }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("inside.txt"), "got: {}", text);
        assert!(!text.contains("outside.txt"), "got: {}", text);
    }

    #[tokio::test]
    async fn execute_times_out_on_zero_timeout() {
        // Only meaningful if `rg` is actually on PATH — if it's absent, execute()
        // takes the "rg not installed" branch and falls back to the (unbounded)
        // walkdir path, which would find "needle" and return Ok(...), failing
        // the assertions below with a confusing message rather than a real
        // signal that the timeout wiring is broken.
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
        std::fs::write(dir.path().join("a.rs"), "fn needle() {}").unwrap();
        // No gait.jsonc in the temp dir → tool_timeout_secs would default to 300,
        // which wouldn't exercise the timeout path in a fast unit test. Instead
        // this test targets execute()'s error surface once wired: with a 0s
        // config override written to gait.jsonc, the rg call must fail fast with
        // a message mentioning "timed out" rather than hanging or silently
        // falling back to the (also-unbounded) walkdir path.
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"tool_timeout_secs": 0}"#).unwrap();
        let ctx = ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "test".into(),
        );
        let tool = CodesearchTool::new(ctx);
        let result = tool
            .execute(
                serde_json::json!({"query": "needle"}),
                &crate::Recorder::detached(),
            )
            .await;
        assert!(result.is_err(), "expected a timeout error, got: {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "got: {msg}");
    }

    #[test]
    fn schema_has_curate_prompt_field() {
        let tool = CodesearchTool::new(ToolCtx::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            "test".into(),
        ));
        let params = tool.parameters();
        assert_eq!(
            params["properties"]["curate_prompt"]["type"]
                .as_str()
                .unwrap(),
            "string"
        );
    }
}
