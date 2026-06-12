use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::tools::ToolCtx;
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
    /// Optional working directory for this command. Relative paths resolve against
    /// the current directory and PERSIST (like `cd`); absolute paths are used as-is.
    /// A nonexistent path errors and leaves the working directory unchanged.
    cwd: Option<String>,
}

pub struct ShellTool {
    ctx: ToolCtx,
    parameters: Value,
}

impl ShellTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<ShellParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { ctx, parameters }
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
         Pass `cwd` to run in a subdirectory; it persists for later commands like `cd` \
         (relative paths resolve against the current dir, absolute used as-is). \
         All shell commands require permission approval unless pre-allowed."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

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

        // Refold the sticky cwd from the log (seeded at the codebase root). A `cwd`
        // param resolves against it (relative joins, absolute as-is), and on success
        // emits a `Cwd` event so the change persists across calls — like `cd`.
        let events = crate::session::ingestor::Ingestor::from_session(
            &self.ctx.gait_dir, &self.ctx.session_id).events().unwrap_or_default();
        let current = crate::session::retrace::active_cwd(&events, &self.ctx.codebase_path);

        let run_dir = match params.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(req) => {
                let p = std::path::Path::new(req);
                let target = if p.is_absolute() { p.to_path_buf() } else { current.join(p) };
                match std::fs::canonicalize(&target) {
                    Ok(dir) if dir.is_dir() => {
                        let log = self.ctx.gait_dir.join("sessions")
                            .join(&self.ctx.session_id).join("log.jsonl");
                        if let Some(parent) = log.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = crate::proto::wire::append(
                            &log, crate::proto::events::cwd(&dir.to_string_lossy()));
                        dir
                    }
                    _ => return Ok(serde_json::json!({
                        "output": format!("cd: no such directory: {}", req),
                        "exit_code": 1,
                        "timed_out": false,
                    })),
                }
            }
            None => current,
        };
        let cwd = run_dir;

        let tmp_dir = self.ctx.gait_dir.join("tmp");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let run_id = {
            let s = uuid::Uuid::new_v4().to_string().replace('-', "");
            s[..8].to_string()
        };
        let out_path = tmp_dir.join(format!("shell-{}.txt", run_id));


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


        let (exit_code, combined) = match result {
            Ok(Ok(output)) => {
                let code = output.status.code().unwrap_or(1);
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                (code, format!("{}{}", stdout, stderr))
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("Process error: {}", e)),
            Err(_) => return Ok(serde_json::json!({
                "output": format!("TIMED OUT after {}ms", timeout_ms),
                "exit_code": 124,
                "timed_out": true,
            })),
        };

        let _ = std::fs::write(&out_path, &combined);
        let file_note = format!("[full output: {} ({} bytes)]", out_path.display(), combined.len());

        let display = format!("{}\n{}", combined.trim_end(), file_note);

        // Structured process-tool shape: turn.rs reads `output` for the model-facing text,
        // and lifts `exit_code`/`timed_out` into the proto `ProcessResult`. The "exit N (Xms)"
        // prefix is gone — timing lives in the generic `ToolResult.duration`, exit in `process`.
        Ok(serde_json::json!({
            "output": display,
            "exit_code": exit_code,
            "timed_out": false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_tool(dir: &TempDir) -> ShellTool {
        ShellTool::new(ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "test".into()))
    }

    #[tokio::test]
    async fn runs_command_and_captures_output() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "echo hello",
            "reason": "test"
        })).await.unwrap();
        // Shell now returns a structured process-tool shape: { output, exit_code, timed_out }.
        // The model-facing `output` carries the clean command text + file note; the model
        // never sees the JSON wrapper or any "exit N (Xms)" prefix.
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        let exit_code = result.get("exit_code").and_then(|v| v.as_i64()).expect("exit_code field");
        let timed_out = result.get("timed_out").and_then(|v| v.as_bool()).expect("timed_out field");
        assert_eq!(exit_code, 0, "got: {result}");
        assert!(!timed_out, "got: {result}");
        assert!(output.contains("hello"), "got: {output}");
    }

    #[tokio::test]
    async fn returns_nonzero_exit_code_on_failure() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "false",
            "reason": "test"
        })).await.unwrap();
        let exit_code = result.get("exit_code").and_then(|v| v.as_i64()).expect("exit_code field");
        let timed_out = result.get("timed_out").and_then(|v| v.as_bool()).expect("timed_out field");
        assert_eq!(exit_code, 1, "got: {result}");
        assert!(!timed_out, "got: {result}");
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
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        assert!(output.contains("marker"), "got: {output}");
    }

    #[tokio::test]
    async fn cwd_param_runs_in_subdir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/sentinel.txt"), "submarker").unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "cat sentinel.txt",
            "reason": "test",
            "cwd": "sub"
        })).await.unwrap();
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        assert!(output.contains("submarker"), "got: {output}");
    }

    #[tokio::test]
    async fn cwd_param_persists_across_calls() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let tool = make_tool(&dir);
        // Call 1: cd into sub (emits a Cwd event).
        let _ = tool.execute(serde_json::json!({
            "cmd": "pwd", "reason": "test", "cwd": "sub"
        })).await.unwrap();
        // Call 2: no cwd → should refold the emitted Cwd and run in sub.
        let result = tool.execute(serde_json::json!({
            "cmd": "pwd", "reason": "test"
        })).await.unwrap();
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        assert!(output.contains("sub"), "expected to run in sub, got: {output}");
    }

    #[tokio::test]
    async fn cwd_nonexistent_errors_without_emitting() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "pwd", "reason": "test", "cwd": "nope"
        })).await.unwrap();
        let exit_code = result.get("exit_code").and_then(|v| v.as_i64()).expect("exit_code field");
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        assert_eq!(exit_code, 1, "got: {result}");
        assert!(output.contains("no such directory"), "got: {output}");
        // No Cwd event written: the session log is absent or empty.
        let log = dir.path().join("sessions").join("test").join("log.jsonl");
        let folded = crate::proto::wire::read_events(&log).unwrap_or_default();
        assert!(folded.is_empty(), "a failed cd must not emit a Cwd event, got {} events", folded.len());
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
        // Timeout branch: exit_code=124, timed_out=true, output carries the TIMED OUT message.
        let exit_code = result.get("exit_code").and_then(|v| v.as_i64()).expect("exit_code field");
        let timed_out = result.get("timed_out").and_then(|v| v.as_bool()).expect("timed_out field");
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        assert_eq!(exit_code, 124, "got: {result}");
        assert!(timed_out, "got: {result}");
        assert!(output.contains("TIMED OUT"), "got: {output}");
    }

    #[tokio::test]
    async fn writes_output_to_tmp_file() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let result = tool.execute(serde_json::json!({
            "cmd": "echo payload",
            "reason": "test"
        })).await.unwrap();
        // Result's `output` field references the tmp file (file note is part of display).
        let output = result.get("output").and_then(|v| v.as_str()).expect("output field");
        assert!(output.contains("full output:"), "got: {output}");
        assert!(output.contains("shell-"), "got: {output}");
        // Tmp file actually exists with the content
        let tmp_dir = dir.path().join("tmp");
        assert!(tmp_dir.exists(), "tmp dir not created");
        let files: Vec<_> = std::fs::read_dir(&tmp_dir).unwrap().collect();
        assert!(!files.is_empty(), "no tmp file written");
    }

    #[test]
    fn shell_permission_single_command() {
        let tool = ShellTool::new(ToolCtx::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
            "test".into(),
        ));
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
        let tool = ShellTool::new(ToolCtx::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
            "test".into(),
        ));
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
        let tool = ShellTool::new(ToolCtx::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
            "test".into(),
        ));
        if let crate::types::ToolPermission::RequiresApproval(reqs) =
            tool.permission(&serde_json::json!({"cmd": "cargo test --verbose", "reason": ""}))
        {
            for pat in &reqs[0].patterns {
                assert!(pat.starts_with("shell:"), "pattern missing shell: prefix: {pat}");
            }
        }
    }

    #[test]
    fn schema_has_parse_array_field() {
        let tool = ShellTool::new(ToolCtx::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
            "test".into(),
        ));
        let params = tool.parameters();
        assert_eq!(params["properties"]["parse"]["type"].as_str().unwrap(), "array",
            "parse should be array type: {}", params);
    }

}
