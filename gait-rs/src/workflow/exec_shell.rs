use std::path::Path;
use crate::workflow::schema::ShellConfig;
use crate::workflow::vars::{NodeOutput, VarContext, substitute};

pub struct NodeContext<'a> {
    pub workflow_id: &'a str,
    pub session_id: &'a str,
    pub workspace: &'a Path,
    pub codebase: &'a Path,
    pub vars: VarContext,
}

pub async fn exec_shell_node(shell: &ShellConfig, ctx: &NodeContext<'_>) -> anyhow::Result<NodeOutput> {
    let cmd = substitute(shell.run(), &ctx.vars);
    let timeout_ms = shell.timeout_ms();
    let cwd = if shell.cwd() == "workspace" { ctx.workspace } else { ctx.codebase };

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        tokio::process::Command::new("sh").arg("-c").arg(&cmd).current_dir(cwd).output(),
    ).await
    .map_err(|_| anyhow::anyhow!("shell timed out after {}ms", timeout_ms))?
    .map_err(|e| anyhow::anyhow!("shell exec error: {}", e))?;

    let stdout = String::from_utf8_lossy(&result.stdout).to_string();
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let combined = if stderr.is_empty() { stdout } else { format!("{}{}", stdout, stderr) };

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
        let shell = crate::workflow::schema::ShellConfig::Short("echo hello".into());
        let ctx = make_test_ctx(ws.path());
        let out = exec_shell_node(&shell, &ctx).await.unwrap();
        assert_eq!(out.text.trim(), "hello");
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn shell_nonzero_exit_ok() {
        let ws = TempDir::new().unwrap();
        let shell = crate::workflow::schema::ShellConfig::Short("exit 1".into());
        let ctx = make_test_ctx(ws.path());
        let out = exec_shell_node(&shell, &ctx).await.unwrap();
        assert_eq!(out.exit_code, Some(1));
    }

    fn make_test_ctx(ws: &std::path::Path) -> NodeContext<'static> {
        let ws = Box::leak(Box::new(ws.to_path_buf()));
        NodeContext {
            workflow_id: "wf1",
            session_id: "s1",
            workspace: ws,
            codebase: ws,
            vars: crate::workflow::vars::VarContext {
                workflow_id: "wf1".into(), session_id: "s1".into(),
                workspace: ws.clone(), codebase: ws.clone(),
                args: "".into(), node_id: "n".into(),
                node_outputs: Default::default(), loop_iter: None, loop_max: None,
                loop_output: None, loop_signal: None, node_exit: None, failed_node: None,
            },
        }
    }
}
