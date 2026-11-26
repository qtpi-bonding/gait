use crate::step::{build_shell_env, ShellOpts, StepResult};
use std::collections::BTreeMap;
use std::path::Path;

pub struct NodeContext<'a> {
    pub workflow_id: &'a str,
    pub session_id: &'a str,
    pub node_id: &'a str,
    pub args: &'a str,
    pub gait_dir: &'a Path,
    pub codebase: &'a Path,
    pub run_worktree: &'a Path,
}

/// Run a typed shell step (`argv` directly, or `script` via `sh -c`) with the
/// env-cleared environment built by `step::build_shell_env`.
pub async fn exec_shell_opts(
    opts: &ShellOpts,
    ctx: &NodeContext<'_>,
    passthrough: &[String],
) -> anyhow::Result<StepResult> {
    exec_shell_opts_with_host_env(opts, ctx, passthrough, &BTreeMap::new()).await
}

/// Like [`exec_shell_opts`], but additionally applies `host_env` entries to
/// the child AFTER the cleared environment, the allowlist/passthrough, the
/// user `env`, and AFTER `build_shell_env`'s reserved-name check on
/// `opts.env`. Only `host_env` may carry `GAIT_`-prefixed names (e.g.
/// `GAIT_ARG_<n>`); a user env key starting with `GAIT_` is still rejected
/// with the existing reserved-key error.
pub async fn exec_shell_opts_with_host_env(
    opts: &ShellOpts,
    ctx: &NodeContext<'_>,
    passthrough: &[String],
    host_env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<StepResult> {
    opts.validate()?;
    let timeout_ms: u64 = opts.timeout.unwrap_or(120_000);

    let well_known: BTreeMap<String, String> = [
        ("WORKFLOW_ID".to_string(), ctx.workflow_id.to_string()),
        ("SESSION_ID".to_string(), ctx.session_id.to_string()),
        ("NODE_ID".to_string(), ctx.node_id.to_string()),
        ("CODEBASE".to_string(), ctx.codebase.display().to_string()),
        ("WORKSPACE".to_string(), ctx.gait_dir.display().to_string()),
        ("ARGS".to_string(), ctx.args.to_string()),
    ]
    .into_iter()
    .collect();
    let env =
        build_shell_env(&well_known, &opts.env, passthrough).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut cmd = match (&opts.argv, &opts.script) {
        (Some(argv), _) => {
            let (program, rest) = argv
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("shell step '{}': argv is empty", opts.id))?;
            let mut c = tokio::process::Command::new(program);
            c.args(rest);
            c
        }
        (None, Some(script)) => {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(script);
            c
        }
        (None, None) => unreachable!("validate() rejects neither argv nor script"),
    };
    cmd.env_clear()
        .envs(env)
        .envs(host_env)
        .current_dir(ctx.run_worktree)
        .kill_on_drop(true);

    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("shell timed out after {}ms", timeout_ms))?
        .map_err(|e| anyhow::anyhow!("shell exec error: {}", e))?;

    let stdout = String::from_utf8_lossy(&result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    // outputSchema: validate the shell's stdout JSON against the schema with
    // the shared step-boundary validator (`output::validate_output`). A
    // violation is an error (a retryable failed attempt), never a
    // non-zero-exit result.
    let json = match &opts.output_schema {
        None => None,
        Some(schema) => Some(crate::output::validate_output(&opts.id, schema, &stdout)?),
    };
    let combined = format!("{stdout}{stderr}");

    Ok(StepResult {
        output: combined.trim_end().to_string(),
        exit: result.status.code(),
        json,
        session_id: None,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_test_ctx(ws: &'static std::path::Path) -> NodeContext<'static> {
        NodeContext {
            workflow_id: "wf1",
            session_id: "s1",
            node_id: "n",
            args: "",
            gait_dir: ws,
            codebase: ws,
            run_worktree: ws,
        }
    }

    #[tokio::test]
    async fn shell_runs_in_the_run_worktree() {
        let ws = TempDir::new().unwrap();
        let run_wt = ws.path().join("wt");
        std::fs::create_dir_all(&run_wt).unwrap();
        std::fs::write(run_wt.join("marker"), "x").unwrap();

        let opts = ShellOpts {
            id: "s1".into(),
            argv: None,
            script: Some("ls".into()),
            env: BTreeMap::new(),
            timeout: None,
            output_schema: None,
            retry: None,
            on_fail: None,
            hooks: None,
        };
        let leaked: &'static Path = Box::leak(Box::new(run_wt));
        let ctx = NodeContext {
            run_worktree: leaked, // shell must cwd here, not ctx.codebase
            ..make_test_ctx(Box::leak(Box::new(ws.path().to_path_buf())))
        };
        let out = exec_shell_opts(&opts, &ctx, &[]).await.unwrap();
        assert!(
            out.output.contains("marker"),
            "shell cwd must be the run worktree; got: {}",
            out.output
        );
    }

    #[tokio::test]
    async fn shell_timeout_still_produces_the_correct_error() {
        let ws = TempDir::new().unwrap();
        let opts = ShellOpts {
            id: "s1".into(),
            argv: None,
            script: Some("sleep 5".into()),
            env: BTreeMap::new(),
            timeout: Some(50),
            output_schema: None,
            retry: None,
            on_fail: None,
            hooks: None,
        };
        let ctx = make_test_ctx(Box::leak(Box::new(ws.path().to_path_buf())));
        let err = exec_shell_opts(&opts, &ctx, &[]).await.unwrap_err();
        assert_eq!(err.to_string(), "shell timed out after 50ms");
    }

    #[tokio::test]
    async fn output_schema_validates_stdout() {
        let ws = TempDir::new().unwrap();
        let opts = ShellOpts {
            id: "s1".into(),
            argv: None,
            script: Some(r#"echo '{"ok":true}'"#.into()),
            env: BTreeMap::new(),
            timeout: None,
            output_schema: Some(serde_json::json!({
                "type": "object",
                "required": ["ok"],
                "properties": {"ok": {"type": "boolean"}}
            })),
            retry: None,
            on_fail: None,
            hooks: None,
        };
        let ctx = make_test_ctx(Box::leak(Box::new(ws.path().to_path_buf())));
        let res = exec_shell_opts(&opts, &ctx, &[]).await.unwrap();
        assert_eq!(res.json, Some(serde_json::json!({"ok": true})));
    }

    #[tokio::test]
    async fn host_env_reaches_the_child_and_user_gait_keys_are_still_rejected() {
        let dir = TempDir::new().unwrap();
        let ws: &'static std::path::Path = Box::leak(Box::new(dir.path().to_path_buf()));
        let ctx = NodeContext {
            workflow_id: "wf1",
            session_id: "run1",
            node_id: "sh1",
            args: "",
            gait_dir: ws,
            codebase: ws,
            run_worktree: ws,
        };

        // A host-provided GAIT_ARG_<n> entry must reach the child.
        let opts = ShellOpts {
            id: "sh1".into(),
            argv: None,
            script: Some(r#"printf %s "$GAIT_ARG_0""#.into()),
            env: BTreeMap::new(),
            timeout: None,
            output_schema: None,
            retry: None,
            on_fail: None,
            hooks: None,
        };
        let host_env: BTreeMap<String, String> = [("GAIT_ARG_0".to_string(), "v".to_string())]
            .into_iter()
            .collect();
        let res = exec_shell_opts_with_host_env(&opts, &ctx, &[], &host_env)
            .await
            .unwrap();
        assert_eq!(res.output, "v", "host_env GAIT_ARG_0 must reach the child");

        // A user-supplied GAIT_-prefixed env key is still rejected even when
        // host_env is supplied.
        let bad = ShellOpts {
            id: "sh1".into(),
            argv: None,
            script: Some("true".into()),
            env: [("GAIT_X".to_string(), "1".to_string())]
                .into_iter()
                .collect(),
            timeout: None,
            output_schema: None,
            retry: None,
            on_fail: None,
            hooks: None,
        };
        let err = exec_shell_opts_with_host_env(&bad, &ctx, &[], &host_env)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("GAIT_X"),
            "user GAIT_X env key must still be rejected, got: {err}"
        );
    }
}
