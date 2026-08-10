use crate::vars::{substitute_shell, NodeOutput, VarContext};
use gait_core::embed::SinkFrame;
use gait_core::proto::gait::workflow::v1::ShellNode;
use std::path::Path;

pub struct NodeContext<'a> {
    pub workflow_id: &'a str,
    pub session_id: &'a str,
    /// This node's effective turn session id (item 13b/13c) — fresh per default
    /// prompt node, or the named `continue_session`; "" for shell/branch/gate.
    pub node_session: &'a str,
    pub gait_dir: &'a Path,
    pub codebase: &'a Path,
    pub run_worktree: &'a Path,
    pub vars: VarContext,
    /// Live channel sender for the workflow run. Sub-turns (prompt nodes) clone
    /// this so their deltas/events reach the workflow's consumer (Phase 3b). It is
    /// a borrow of the run's owning `Recorder` sender — never a long-lived clone —
    /// so dropping that `Recorder` remains the single point that closes the channel.
    pub workflow_tx: &'a tokio::sync::mpsc::UnboundedSender<SinkFrame>,
    /// CLI-only degenerate-run overrides (spec-06 §3 Task 3.2). `None`/absent
    /// for every real workflow node — only `single_prompt_run`'s one node ever
    /// sets these, so `exec_prompt_node` prefers them over disk config.
    pub sandbox_on: Option<bool>,
    pub cascade_group_arg: Option<String>,
    pub cascade: Option<&'a gait_core::embed::CascadeKillHandle>,
    pub profile: Option<&'a crate::single_prompt::ProfileOverride>,
    /// Optional editor-buffer delegate injected by a workflow-level consumer
    /// (e.g. a future ACP adapter). `None` for every real workflow node.
    pub fs_delegate: Option<std::sync::Arc<dyn gait_core::embed::FsDelegate>>,
    /// Launcher-set override for the in-tree safety guard (design §7), sourced
    /// from the CLI's `--allow-intree` flag. `false` for every real workflow node.
    pub allow_intree: bool,
}

pub async fn exec_shell_node(
    shell: &ShellNode,
    ctx: &NodeContext<'_>,
    output_schema_json: &str,
) -> anyhow::Result<NodeOutput> {
    let cmd_str = substitute_shell(&shell.command, &ctx.vars);
    let timeout_ms: u64 = shell.timeout_ms.unwrap_or(120_000);
    let cwd = ctx.run_worktree;

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&cmd_str).current_dir(cwd);
    // Without this, a timeout only bounds the *wait* — tokio's default is to
    // NOT kill the child on drop, so dropping the timed-out output() future
    // (or the whole scheduler task pool on a sibling node's failure, see
    // runner.rs's FuturesUnordered teardown on anyhow::bail!) leaves `sh` and
    // whatever it spawned running as an orphan. Same fix as grep.rs/ls.rs.
    cmd.kill_on_drop(true);

    let result = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("shell timed out after {}ms", timeout_ms))?
        .map_err(|e| anyhow::anyhow!("shell exec error: {}", e))?;

    let stdout = String::from_utf8_lossy(&result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let json = if output_schema_json.is_empty() {
        None
    } else {
        Some(serde_json::from_str(stdout.trim()).map_err(|e| {
            anyhow::anyhow!(
                "node output_schema requires JSON stdout, but shell stdout was invalid JSON: {e}"
            )
        })?)
    };
    let combined = if stderr.is_empty() {
        stdout
    } else {
        format!("{}{}", stdout, stderr)
    };

    Ok(NodeOutput {
        text: combined.trim_end().to_string(),
        exit_code: result.status.code(),
        json,
        routed_to: None,
        session_id: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn shell_captures_output_and_exit() {
        let ws = TempDir::new().unwrap();
        let shell = ShellNode {
            command: "echo hello".into(),
            timeout_ms: None,
        };
        let ctx = make_test_ctx(ws.path());
        let out = exec_shell_node(&shell, &ctx, "").await.unwrap();
        assert_eq!(out.text.trim(), "hello");
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn shell_nonzero_exit_ok() {
        let ws = TempDir::new().unwrap();
        let shell = ShellNode {
            command: "exit 1".into(),
            timeout_ms: None,
        };
        let ctx = make_test_ctx(ws.path());
        let out = exec_shell_node(&shell, &ctx, "").await.unwrap();
        assert_eq!(out.exit_code, Some(1));
    }

    #[tokio::test]
    async fn shell_runs_in_the_run_worktree() {
        let ws = TempDir::new().unwrap();
        let run_wt = ws.path().join("wt");
        std::fs::create_dir_all(&run_wt).unwrap();
        std::fs::write(run_wt.join("marker"), "x").unwrap();

        let shell = ShellNode {
            command: "ls".into(),
            timeout_ms: None,
        };
        let mut ctx = make_test_ctx(ws.path());
        let leaked: &'static Path = Box::leak(Box::new(run_wt.clone()));
        ctx.run_worktree = leaked; // shell must cwd here, not ctx.codebase
        let out = exec_shell_node(&shell, &ctx, "").await.unwrap();
        assert!(
            out.text.contains("marker"),
            "shell cwd must be the run worktree; got: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn shell_timeout_still_produces_the_correct_error() {
        let ws = TempDir::new().unwrap();
        let shell = ShellNode {
            command: "sleep 5".into(),
            timeout_ms: Some(50),
        };
        let ctx = make_test_ctx(ws.path());
        let err = exec_shell_node(&shell, &ctx, "").await.unwrap_err();
        assert_eq!(err.to_string(), "shell timed out after 50ms");
    }

    #[tokio::test]
    async fn shell_node_output_cannot_inject_a_second_command() {
        let ws = TempDir::new().unwrap();
        let shell = ShellNode {
            command: "echo $mal.output".into(),
            timeout_ms: None,
        };
        let mut ctx = make_test_ctx(ws.path());
        ctx.vars.node_outputs.insert(
            "mal".into(),
            crate::vars::NodeOutput {
                text: "; touch injected_marker".into(),
                exit_code: Some(0),
                json: None,
                routed_to: None,
                session_id: String::new(),
            },
        );
        let out = exec_shell_node(&shell, &ctx, "").await.unwrap();
        assert_eq!(
            out.text, "; touch injected_marker",
            "the malicious text must come back as echo's literal argument, not run as a second command"
        );
        assert!(
            !ws.path().join("injected_marker").exists(),
            "injected command must not have executed"
        );
    }

    fn make_test_ctx(ws: &std::path::Path) -> NodeContext<'static> {
        let ws = Box::leak(Box::new(ws.to_path_buf()));
        // Leak a sender so the ctx is 'static; the receiver is dropped, so sends are
        // silently ignored (these tests don't assert on the channel).
        let tx: &'static _ = Box::leak(Box::new(
            tokio::sync::mpsc::unbounded_channel::<SinkFrame>().0,
        ));
        NodeContext {
            workflow_id: "wf1",
            session_id: "s1",
            node_session: "",
            gait_dir: ws,
            codebase: ws,
            run_worktree: ws,
            workflow_tx: tx,
            sandbox_on: None,
            cascade_group_arg: None,
            cascade: None,
            profile: None,
            fs_delegate: None,
            allow_intree: false,
            vars: crate::vars::VarContext {
                workflow_id: "wf1".into(),
                session_id: "s1".into(),
                gait_dir: ws.clone(),
                codebase: ws.clone(),
                args: "".into(),
                node_id: "n".into(),
                node_outputs: Default::default(),
                node_exit: None,
                failed_node: None,
            },
        }
    }
}
