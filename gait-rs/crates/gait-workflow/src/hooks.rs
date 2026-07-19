use std::collections::HashMap;

pub async fn run_hook(
    cmd: &str,
    env_vars: &HashMap<String, String>,
    timeout_ms: u64,
) -> (i32, String) {
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(cmd).envs(env_vars);
    // Without this, a timed-out hook's `sh` (and whatever it spawned) leaks as
    // an orphan on drop — same fix as exec_shell.rs's finding #4 kill_on_drop.
    command.kill_on_drop(true);

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        command.output(),
    )
    .await;

    match result {
        Ok(Ok(o)) => {
            let exit = o.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let out = if stderr.is_empty() {
                stdout
            } else {
                format!("{}{}", stdout, stderr)
            };
            (exit, out)
        }
        Ok(Err(_)) => (-1, String::new()),
        Err(_) => (-1, format!("hook timed out after {}ms", timeout_ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hook_runs_and_returns_exit() {
        let (exit, out) = run_hook("echo hello", &Default::default(), 5_000).await;
        assert_eq!(exit, 0);
        assert!(out.contains("hello"));
    }

    #[tokio::test]
    async fn hook_nonzero_exit_captured() {
        let (exit, _) = run_hook("exit 2", &Default::default(), 5_000).await;
        assert_eq!(exit, 2);
    }

    #[tokio::test]
    async fn hook_times_out_and_reports_negative_exit() {
        let start = tokio::time::Instant::now();
        let (exit, out) = run_hook("sleep 5", &Default::default(), 50).await;
        let elapsed = start.elapsed();
        assert_eq!(exit, -1, "a timed-out hook must report exit -1");
        assert!(
            out.contains("timed out"),
            "timeout message should be descriptive, got: {out}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "must return near the 50ms timeout bound, not wait for the full 5s sleep; took {elapsed:?}"
        );
    }
}
