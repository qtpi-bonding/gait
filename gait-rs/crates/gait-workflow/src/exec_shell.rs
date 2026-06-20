use crate::schema::ShellConfig;
use crate::vars::{substitute, NodeOutput, VarContext};
use gait_core::proto::gait::log::v1::StreamItem;
use std::path::Path;

pub struct NodeContext<'a> {
    pub workflow_id: &'a str,
    pub session_id: &'a str,
    pub gait_dir: &'a Path,
    pub codebase: &'a Path,
    pub vars: VarContext,
    /// Live channel sender for the workflow run. Sub-turns (prompt nodes) clone
    /// this so their deltas/events reach the workflow's consumer (Phase 3b). It is
    /// a borrow of the run's owning `Recorder` sender — never a long-lived clone —
    /// so dropping that `Recorder` remains the single point that closes the channel.
    pub workflow_tx: &'a tokio::sync::mpsc::UnboundedSender<StreamItem>,
}

pub async fn exec_shell_node(
    shell: &ShellConfig,
    ctx: &NodeContext<'_>,
) -> anyhow::Result<NodeOutput> {
    let cmd = substitute(shell.run(), &ctx.vars);
    let timeout_ms = shell.timeout_ms();
    let cwd = if shell.cwd() == "workspace" {
        ctx.gait_dir
    } else {
        ctx.codebase
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(cwd)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("shell timed out after {}ms", timeout_ms))?
    .map_err(|e| anyhow::anyhow!("shell exec error: {}", e))?;

    let stdout = String::from_utf8_lossy(&result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let combined = if stderr.is_empty() {
        stdout
    } else {
        format!("{}{}", stdout, stderr)
    };

    Ok(NodeOutput {
        text: combined.trim_end().to_string(),
        exit_code: result.status.code(),
        json: None,
        routed_to: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn shell_captures_output_and_exit() {
        let ws = TempDir::new().unwrap();
        let shell = crate::schema::ShellConfig::Short("echo hello".into());
        let ctx = make_test_ctx(ws.path());
        let out = exec_shell_node(&shell, &ctx).await.unwrap();
        assert_eq!(out.text.trim(), "hello");
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn shell_nonzero_exit_ok() {
        let ws = TempDir::new().unwrap();
        let shell = crate::schema::ShellConfig::Short("exit 1".into());
        let ctx = make_test_ctx(ws.path());
        let out = exec_shell_node(&shell, &ctx).await.unwrap();
        assert_eq!(out.exit_code, Some(1));
    }

    fn make_test_ctx(ws: &std::path::Path) -> NodeContext<'static> {
        let ws = Box::leak(Box::new(ws.to_path_buf()));
        // Leak a sender so the ctx is 'static; the receiver is dropped, so sends are
        // silently ignored (these tests don't assert on the channel).
        let tx: &'static _ = Box::leak(Box::new(
            tokio::sync::mpsc::unbounded_channel::<StreamItem>().0,
        ));
        NodeContext {
            workflow_id: "wf1",
            session_id: "s1",
            gait_dir: ws,
            codebase: ws,
            workflow_tx: tx,
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
