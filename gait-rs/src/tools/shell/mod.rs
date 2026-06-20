use crate::permission::{command_patterns, split_commands};
use crate::tools::ToolCtx;
use crate::tools::ToolError;
use crate::types::{PermissionRequest, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct ShellParams {
    /// Shell command to execute
    cmd: String,
    /// Why this command is needed
    reason: String,
    /// Timeout in milliseconds. Omit for no timeout (recommended for build/test commands).
    timeout: Option<u64>,
    /// Optional working directory for this command. Relative paths resolve against
    /// the current directory and PERSIST (like `cd`); absolute paths are used as-is.
    /// A nonexistent path errors and leaves the working directory unchanged.
    cwd: Option<String>,
}
#[allow(dead_code)]
#[derive(schemars::JsonSchema)]
struct ShellOutput {
    output: String,
    exit_code: i32,
    timed_out: bool,
}

pub struct ShellTool {
    ctx: ToolCtx,
    parameters: Value,
    output_schema: Value,
}

impl ShellTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<ShellParams>(),
            crate::types::OutputFilter::Both,
        );
        Self {
            ctx,
            parameters,
            output_schema: crate::tools::make_schema::<ShellOutput>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
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
    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&self.output_schema)
    }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        let segs = split_commands(cmd);
        let segs = if segs.is_empty() {
            vec![cmd.trim().to_string()]
        } else {
            segs
        };
        let requests = segs
            .iter()
            .map(|seg| PermissionRequest {
                display: seg.clone(),
                patterns: command_patterns(seg)
                    .into_iter()
                    .map(|p| format!("shell:{}", p))
                    .collect(),
            })
            .collect();
        ToolPermission::RequiresApproval(requests)
    }

    async fn execute(&self, args: Value, rec: &crate::Recorder) -> Result<Value, ToolError> {
        let params: ShellParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        let timeout_ms = params.timeout;

        // Refold the sticky cwd from the log (seeded at the codebase root). A `cwd`
        // param resolves against it (relative joins, absolute as-is), and on success
        // emits a `Cwd` event so the change persists across calls — like `cd`.
        let events = crate::session::ingestor::Ingestor::from_session(
            &self.ctx.gait_dir,
            &self.ctx.session_id,
        )
        .events()
        .unwrap_or_default();
        let current = crate::session::retrace::active_cwd(&events, &self.ctx.codebase_path);

        let run_dir = match params
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(req) => {
                let p = std::path::Path::new(req);
                let target = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    current.join(p)
                };
                match std::fs::canonicalize(&target) {
                    Ok(dir) if dir.is_dir() => {
                        // Record via the Recorder: this both appends the event to the
                        // session transcript (so the next call's active_cwd refold sees it)
                        // AND publishes it on the channel (live to stderr via the StdioSink,
                        // and to any REPL subscriber). The log-path local is gone.
                        // cwd event is best-effort: shell executed successfully, do not fail on record error.
                        let _ = rec.record(crate::proto::events::cwd(&dir.to_string_lossy()));
                        dir
                    }
                    _ => {
                        return Ok(serde_json::json!({
                            "output": format!("cd: no such directory: {}", req),
                            "exit_code": 1,
                            "timed_out": false,
                        }))
                    }
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
            .spawn()
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;

        let wait = child.wait_with_output();
        let output = if let Some(ms) = timeout_ms {
            match tokio::time::timeout(std::time::Duration::from_millis(ms), wait).await {
                Ok(r) => r,
                Err(_) => {
                    return Ok(serde_json::json!({
                        "output": format!("TIMED OUT after {}ms", ms),
                        "exit_code": 124,
                        "timed_out": true,
                    }))
                }
            }
        } else {
            wait.await
        };

        let (exit_code, combined) = match output {
            Ok(output) => {
                let code = output.status.code().unwrap_or(1);
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                (code, format!("{}{}", stdout, stderr))
            }
            Err(e) => {
                return Err(crate::tools::ToolError::Internal(anyhow::anyhow!(
                    "Process error: {}",
                    e
                )))
            }
        };

        let _ = std::fs::write(&out_path, &combined);
        let file_note = format!(
            "[full output: {} ({} bytes)]",
            out_path.display(),
            combined.len()
        );

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
mod tests;
