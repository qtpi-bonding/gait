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
        // Extract __cwd BEFORE deserializing ShellParams so the reserved key (which
        // ShellParams silently ignores) is available. __cwd is injected by cmd_run_tool
        // from the brain's active-cwd state, replacing the old Ingestor refold.
        // Falls back to the codebase root for in-process calls (brain path).
        let current = args
            .get("__cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.ctx.codebase_path.clone());

        let params: ShellParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;

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
                        }))
                    }
                }
            }
            None => current,
        };
        let cwd = run_dir;

        let tmp_dir = std::env::temp_dir().join("gait-hand");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let run_id = {
            let s = uuid::Uuid::new_v4().to_string().replace('-', "");
            s[..8].to_string()
        };
        let out_path = tmp_dir.join(format!("shell-{}.txt", run_id));

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", &params.cmd])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "PATH" | "HOME" | "USER" | "SHELL" | "TERM" | "LANG" | "LC_ALL" | "TMPDIR"
                )
            }))
            .spawn()
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut out_file = tokio::fs::File::create(&out_path)
            .await
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let h1 = tokio::spawn({
            let tx = tx.clone();
            async move {
                let mut r = stdout;
                let mut buf = [0u8; 8192];
                loop {
                    match r.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = tx.send(buf[..n].to_vec()).await;
                        }
                    }
                }
            }
        });
        let h2 = tokio::spawn({
            let tx = tx.clone();
            async move {
                let mut r = stderr;
                let mut buf = [0u8; 8192];
                loop {
                    match r.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = tx.send(buf[..n].to_vec()).await;
                        }
                    }
                }
            }
        });
        drop(tx); // drop outer sender so rx closes when both tasks finish

        let drain_and_wait = async {
            let mut total: u64 = 0;
            let mut tail = std::collections::VecDeque::<u8>::new();
            while let Some(chunk) = rx.recv().await {
                out_file.write_all(&chunk).await.ok();
                total += chunk.len() as u64;
                rec.publish_delta(String::from_utf8_lossy(&chunk).into_owned());
                tail.extend(&chunk);
                while tail.len() > 64 * 1024 {
                    tail.pop_front();
                }
            }
            let _ = (h1.await, h2.await);
            out_file.flush().await.ok();
            let status = child
                .wait()
                .await
                .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
            Ok::<_, ToolError>((status, total, tail))
        };

        let (status, total, mut tail) = drain_and_wait.await?;

        let exit_code = status.code().unwrap_or(1);
        let tail_str = String::from_utf8_lossy(tail.make_contiguous()).into_owned();
        let note = format!("[full output: {} ({} bytes)]", out_path.display(), total);

        // Structured process-tool shape: turn.rs reads `output` for the model-facing text,
        // and lifts `exit_code` into the proto `ProcessResult`. The "exit N (Xms)"
        // prefix is gone — timing lives in the generic `ToolResult.duration`, exit in `process`.
        Ok(serde_json::json!({
            "output": format!("{}\n{}", tail_str.trim_end(), note),
            "exit_code": exit_code,
        }))
    }

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            reads: true,
            writes: true,
            spawns: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
