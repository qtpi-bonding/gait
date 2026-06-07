use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
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
}

pub struct ShellTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl ShellTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<ShellParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str { "shell" }
    fn description(&self) -> &str {
        "Run a shell command in the codebase directory. All shell commands require permission approval unless pre-allowed."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: ShellParams = serde_json::from_value(args)?;
        let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let cwd = (*self.codebase_path).clone();

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

        match result {
            Ok(Ok(output)) => {
                let exit_code = output.status.code().unwrap_or(1);
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let combined = format!("{}{}", stdout, stderr);
                Ok(Value::String(format!("exit {} ({}ms)\n{}", exit_code, duration_ms, combined)))
            }
            Ok(Err(e)) => anyhow::bail!("Process error: {}", e),
            Err(_) => {
                Ok(Value::String(format!("TIMED OUT after {}ms\n", timeout_ms)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = TempDir::new().unwrap();
        let tool = ShellTool::new(dir.path().to_path_buf());
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
        let tool = ShellTool::new(dir.path().to_path_buf());
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
        let tool = ShellTool::new(dir.path().to_path_buf());
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
        let tool = ShellTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "cmd": "sleep 10",
            "reason": "test timeout",
            "timeout_ms": 100
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("TIMED OUT"), "got: {text}");
    }
}
