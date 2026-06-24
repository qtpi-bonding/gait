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
    /// Run as a background daemon that outlives this turn; returns immediately.
    /// The daemon inherits the agent's sandbox ceiling (not the per-call scope).
    /// Output is written to <gait_dir>/daemons/<name>.log for its lifetime.
    #[serde(default)]
    detached: bool,
    /// Name for this daemon (used in registry and log filename).
    /// Auto-generated as "daemon-<uuid8>" if omitted.
    name: Option<String>,
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

        // ── Detached daemon branch ─────────────────────────────────────────
        if params.detached {
            let name = params.name.clone().unwrap_or_else(|| {
                let uid = uuid::Uuid::new_v4().to_string().replace('-', "");
                format!("daemon-{}", &uid[..8])
            });

            let daemons_dir = self.ctx.gait_dir.join("daemons");
            std::fs::create_dir_all(&daemons_dir)
                .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
            let log_path = daemons_dir.join(format!("{}.log", name));

            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", &params.cmd])
                .current_dir(&cwd)
                .env_clear()
                .envs(std::env::vars().filter(|(k, _)| {
                    matches!(
                        k.as_str(),
                        "PATH" | "HOME" | "USER" | "SHELL" | "TERM" | "LANG" | "LC_ALL" | "TMPDIR"
                    )
                }));

            let (child, pid, started_at) =
                crate::process::spawn_detached_to_file(cmd, &log_path)
                    .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
            // Drop child immediately — daemon is detached; we do not wait on it.
            drop(child);

            let log_path_str = log_path.to_string_lossy().into_owned();
            let cwd_str = cwd.to_string_lossy().into_owned();
            let full_cmd = params.cmd.clone();

            // Architecture note: the hand runtime (hand.rs) calls execute() and wraps the
            // returned Value into HandResponse { Feedback { result, ... } }. To carry the
            // DaemonRecordProto up to the spine we embed it as a "__daemon" key in the
            // result JSON. hand.rs detects this key, extracts it into Feedback.daemon, and
            // removes it from the result before forwarding.
            return Ok(serde_json::json!({
                "output": format!("started daemon {} (pid {}) — log: {}", name, pid, log_path_str),
                "exit_code": 0,
                "__daemon": {
                    "name": name,
                    "pid": pid,
                    "pgid": pid,
                    "started_at": started_at,
                    "cmd": full_cmd,
                    "cwd": cwd_str,
                    "log_path": log_path_str,
                },
            }));
        }

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
                    "PATH" | "HOME" | "USER" | "SHELL" | "LANG" | "LC_ALL" | "TMPDIR"
                )
            }))
            // Suppress colour output: programs check TERM and NO_COLOR before emitting ANSI.
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
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
                out_file
                    .write_all(&chunk)
                    .await
                    .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
                total += chunk.len() as u64;
                rec.publish_delta(String::from_utf8_lossy(&chunk).into_owned());
                tail.extend(&chunk);
                while tail.len() > 64 * 1024 {
                    tail.pop_front();
                }
            }
            h1.await.expect("stdout drain task panicked");
            h2.await.expect("stderr drain task panicked");
            out_file.flush().await.ok();
            let status = child
                .wait()
                .await
                .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
            Ok::<_, ToolError>((status, total, tail))
        };

        let (status, total, mut tail) = drain_and_wait.await?;

        let exit_code = status.code().unwrap_or(1);
        let tail_str = strip_ansi(&String::from_utf8_lossy(tail.make_contiguous()));
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

/// Remove ANSI escape sequences from `s` so models see plain text.
/// Handles CSI (`ESC[…m`) and OSC (`ESC]…BEL/ST`) sequences; passes everything else through.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                // CSI: consume digits/semicolons until a letter finalizer
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: consume until BEL or ESC\
                while let Some(c2) = chars.next() {
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        chars.next(); // consume the trailing '\'
                        break;
                    }
                }
            }
            _ => {} // unknown escape: drop the ESC, let next char through
        }
    }
    out
}

#[cfg(test)]
mod tests;
