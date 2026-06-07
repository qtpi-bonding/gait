use std::collections::HashMap;

pub async fn run_hook(cmd: &str, env_vars: &HashMap<String, String>) -> (i32, String) {
    let result = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .envs(env_vars)
        .output()
        .await;
    match result {
        Ok(o) => {
            let exit = o.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let out = if stderr.is_empty() { stdout } else { format!("{}{}", stdout, stderr) };
            (exit, out)
        }
        Err(_) => (-1, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hook_runs_and_returns_exit() {
        let (exit, out) = run_hook("echo hello", &Default::default()).await;
        assert_eq!(exit, 0);
        assert!(out.contains("hello"));
    }

    #[tokio::test]
    async fn hook_nonzero_exit_captured() {
        let (exit, _) = run_hook("exit 2", &Default::default()).await;
        assert_eq!(exit, 2);
    }
}
