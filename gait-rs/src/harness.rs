use std::path::PathBuf;
use crate::turn::SessionLock;
use crate::proto::{events, wire};
use crate::scheduler::Action;

#[async_trait::async_trait]
pub trait HarnessBackend: Send + Sync {
    /// Execute a scheduler-produced action (mechanism only).
    async fn execute(&self, action: &Action) -> anyhow::Result<()>;
}

pub struct NoopHarnessBackend;

#[async_trait::async_trait]
impl HarnessBackend for NoopHarnessBackend {
    async fn execute(&self, _action: &Action) -> anyhow::Result<()> { Ok(()) }
}

pub struct NativeHarnessBackend {
    gait_dir: PathBuf,
}

impl NativeHarnessBackend {
    pub fn new(gait_dir: PathBuf) -> Self {
        Self { gait_dir }
    }
}

#[async_trait::async_trait]
impl HarnessBackend for NativeHarnessBackend {
    async fn execute(&self, action: &Action) -> anyhow::Result<()> {
        match action {
            Action::NotifyAndResume { target_session, from_session, status, summary } => {
                let event = events::agent_update(
                    from_session.clone(),
                    status,
                    summary.clone(),
                    None,
                );

                let target_dir = self.gait_dir.join("sessions").join(target_session);
                let tx_path = target_dir.join("log.jsonl");

                // Only retry on "already running" — fail fast on I/O errors.
                let mut lock: Option<SessionLock> = None;
                for _ in 0..20 {
                    match SessionLock::acquire(&target_dir) {
                        Ok(l) => { lock = Some(l); break; }
                        Err(e) => {
                            if !e.to_string().contains("already running") {
                                return Err(e.context(format!(
                                    "unexpected error acquiring lock on session {}", target_session)));
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
                let _lock = lock.ok_or_else(|| anyhow::anyhow!(
                    "failed to acquire lock on session {} after 20 attempts", target_session))?;

                wire::append(&tx_path, event)?;
                let repos = crate::git::Repos::production();
                repos.traces.commit_session(
                    &self.gait_dir,
                    target_session,
                    &format!("session({}): agent_update from {}", target_session, from_session),
                )?;
                drop(_lock);

                let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
                std::process::Command::new(&exe)
                    .args(["run", "--session", target_session.as_str(),
                           "--gait-dir", self.gait_dir.to_str().unwrap_or("."),
                           "--auto"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_harness_execute_succeeds() {
        let action = Action::NotifyAndResume {
            target_session: "parent".into(), from_session: "child".into(),
            status: "done".into(), summary: "x".into(),
        };
        assert!(NoopHarnessBackend.execute(&action).await.is_ok());
    }
}
