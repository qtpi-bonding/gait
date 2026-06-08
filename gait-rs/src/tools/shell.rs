use std::path::PathBuf;
use std::sync::Arc;
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::{split_commands, command_patterns};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

#[derive(Deserialize, JsonSchema)]
struct ShellParams {
    /// Shell command to execute
    cmd: String,
    /// Why this command is needed
    reason: String,
    /// Timeout in milliseconds (default 30000)
    timeout_ms: Option<u64>,
    /// Optional pipeline to filter the full output (e.g. "grep FAILED | head -20").
    /// Use this when you expect large output (test runs, build logs) to avoid flooding
    /// context. The full output is always written to a tmp file whose path is returned,
    /// so you can grep/read it later without re-running the command.
    parse: Option<String>,
}

pub struct ShellTool {
    codebase_path: Arc<PathBuf>,
    workspace: Arc<PathBuf>,
    parameters: Value,
}

impl ShellTool {
    pub fn new(codebase_path: PathBuf, workspace: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<ShellParams>();
        Self {
            codebase_path: Arc::new(codebase_path),
            workspace: Arc::new(workspace),
            parameters,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str { "shell" }
    fn description(&self) -> &str {
        "Run a shell command in the codebase directory. Full output is always written to a \
        workspace tmp file and its path is included in the result. For commands that produce \
        large output (cargo test, build logs, grep over large trees) use the `parse` field \
        (e.g. \"grep FAILED | head -30\") to return only relevant lines and keep context small. \
        You can then grep/read the tmp file for more detail without re-running. \
        All shell commands require permission approval unless pre-allowed."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        let segs = split_commands(cmd);
        let segs = if segs.is_empty() { vec![cmd.trim().to_string()] } else { segs };
        let requests = segs.iter().map(|seg| PermissionRequest {
            display: seg.clone(),
            patterns: command_patterns(seg)
                .into_iter()
                .map(|p| format!("shell:{}", p))
                .collect(),
        }).collect();
        ToolPermission::RequiresApproval(requests)
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: ShellParams = serde_json::from_value(args)?;
        let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let cwd = (*self.codebase_path).clone();

        let tmp_dir = self.workspace.join("tmp");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let run_id = {
            let s = uuid::Uuid::new_v4().to_string().replace('-', "");
            s[..8].to_string()
        };
        let out_path = tmp_dir.join(format!("shell-{}.txt", run_id));

        let start = std::time::Instant::now();

        let child = tokio::process::Command::new("sh")
            .args(["-c", &params.cmd])
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            child.wait_with_output(),
        ).await;

        let duration_ms = start.elapsed().as_millis();

        let (exit_code, combined) = match result {
            Ok(Ok(output)) => {
                let code = output.status.code().unwrap_or(1);
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                (code, format!("{}{}", stdout, stderr))
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("Process error: {}", e)),
            Err(_) => return Ok(Value::String(format!("TIMED OUT after {}ms\n", timeout_ms))),
        };

        let _ = std::fs::write(&out_path, &combined);
        let file_note = format!("[full output: {} ({} bytes)]", out_path.display(), combined.len());

        let display = if let Some(parse_cmd) = params.parse {
            let piped = format!("cat {} | {}", out_path.display(), parse_cmd);
            let parse_out = tokio::process::Command::new("sh")
                .args(["-c", &piped])
                .output()
                .await
                .map(|o| {
                    let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                    format!("{}{}", stdout, stderr)
                })
                .unwrap_or_else(|e| format!("[parse error: {}]\n", e));
            format!("{}\n{}", parse_out.trim_end(), file_note)
        } else {
            format!("{}\n{}", combined.trim_end(), file_note)
        };

        Ok(Value::String(format!("exit {} ({}ms)\n{}", exit_code, duration_ms, display)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_tool(dir: &TempDir) -> ShellTool {
        ShellTool::new(dir.path().to_path_buf(), dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "echo hello",
            "reason": "test"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("exit 0"), "got: {text}");
        assert!(text.contains("hello"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_nonzero_exit_code_on_failure() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "false",
            "reason": "test"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.starts_with("exit 1") || text.contains("exit 1"), "got: {text}");
    }

    #[tokio::test]
    async fn runs_in_codebase_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("sentinel.txt"), "marker").unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "cat sentinel.txt",
            "reason": "test"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("marker"), "got: {text}");
    }

    #[tokio::test]
    async fn times_out_long_running_command() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "sleep 10",
            "reason": "test timeout",
            "timeout_ms": 100
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("TIMED OUT"), "got: {text}");
    }

    #[tokio::test]
    async fn writes_output_to_tmp_file() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "echo payload",
            "reason": "test"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        // Result references the tmp file
        assert!(text.contains("full output:"), "got: {text}");
        assert!(text.contains("shell-"), "got: {text}");
        // Tmp file actually exists with the content
        let tmp_dir = dir.path().join("tmp");
        assert!(tmp_dir.exists(), "tmp dir not created");
        let files: Vec<_> = std::fs::read_dir(&tmp_dir).unwrap().collect();
        assert!(!files.is_empty(), "no tmp file written");
    }

    #[tokio::test]
    async fn parse_filters_output() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "printf 'line1\nFAILED: test_foo\nline3\n'",
            "reason": "test",
            "parse": "grep FAILED"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("FAILED: test_foo"), "got: {text}");
        assert!(!text.contains("line1"), "got: {text}");
        assert!(text.contains("full output:"), "got: {text}");
    }

    #[test]
    fn shell_permission_single_command() {
        let tool = ShellTool::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
        );
        match tool.permission(&serde_json::json!({"cmd": "cargo test", "reason": ""})) {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 1);
                assert_eq!(reqs[0].display, "cargo test");
                assert!(reqs[0].patterns.contains(&"shell:cargo *".to_string()));
                assert!(reqs[0].patterns.contains(&"shell:cargo test".to_string()));
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn shell_permission_compound_command() {
        let tool = ShellTool::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
        );
        match tool.permission(&serde_json::json!({"cmd": "cargo build && git push", "reason": ""})) {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 2);
                assert_eq!(reqs[0].display, "cargo build");
                assert_eq!(reqs[1].display, "git push");
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn shell_permission_patterns_prefixed_with_shell() {
        let tool = ShellTool::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
        );
        if let crate::types::ToolPermission::RequiresApproval(reqs) =
            tool.permission(&serde_json::json!({"cmd": "cargo test --verbose", "reason": ""}))
        {
            for pat in &reqs[0].patterns {
                assert!(pat.starts_with("shell:"), "pattern missing shell: prefix: {pat}");
            }
        }
    }

}
