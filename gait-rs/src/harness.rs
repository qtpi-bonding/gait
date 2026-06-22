use crate::proto::{events, wire};
use crate::scheduler::Action;
use crate::turn::SessionLock;
use std::path::PathBuf;
use tokio::sync::mpsc;

#[async_trait::async_trait]
pub trait HarnessBackend: Send + Sync {
    /// Execute a scheduler-produced action (mechanism only).
    async fn execute(&self, action: &Action) -> anyhow::Result<()>;
}

pub struct NoopHarnessBackend;

#[async_trait::async_trait]
impl HarnessBackend for NoopHarnessBackend {
    async fn execute(&self, _action: &Action) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct NotifyPayload {
    pub target_session: String,
    pub from_session: String,
    pub status: String,
    pub summary: String,
}

pub struct NativeHarnessBackend {
    gait_dir: PathBuf,
    /// When `Some`, route `gait run` spawns via the Nerve's Spawn signal instead
    /// of calling `Command::spawn` directly (required when brain is sealed).
    spawn_tx: Option<mpsc::UnboundedSender<(Vec<String>, bool)>>,
    /// When `Some`, route NotifyAndResume writes via the Nerve's Notify signal
    /// instead of writing directly to disk (required when brain is sealed).
    notify_tx: Option<tokio::sync::mpsc::UnboundedSender<NotifyPayload>>,
}

impl NativeHarnessBackend {
    pub fn new(gait_dir: PathBuf) -> Self {
        Self {
            gait_dir,
            spawn_tx: None,
            notify_tx: None,
        }
    }

    pub fn new_with_spawn(
        gait_dir: PathBuf,
        spawn_tx: mpsc::UnboundedSender<(Vec<String>, bool)>,
    ) -> Self {
        Self {
            gait_dir,
            spawn_tx: Some(spawn_tx),
            notify_tx: None,
        }
    }

    pub fn with_notify_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<NotifyPayload>) -> Self {
        self.notify_tx = Some(tx);
        self
    }
}

#[async_trait::async_trait]
impl HarnessBackend for NativeHarnessBackend {
    async fn execute(&self, action: &Action) -> anyhow::Result<()> {
        match action {
            Action::NotifyAndResume {
                target_session,
                from_session,
                status,
                summary,
            } => {
                if let Some(ref tx) = self.notify_tx {
                    // Sealed brain path: delegate write + commit to spine via Nerve.
                    let payload = NotifyPayload {
                        target_session: target_session.clone(),
                        from_session: from_session.clone(),
                        status: status.clone(),
                        summary: summary.clone(),
                    };
                    let _ = tx.send(payload);
                } else {
                    // Non-sealed path (embed/REPL without relay): write directly.
                    let event =
                        events::agent_update(from_session.clone(), status, summary.clone(), None);

                    let target_dir = self.gait_dir.join("sessions").join(target_session);
                    let tx_path = target_dir.join("log.jsonl");

                    // Only retry on "already running" — fail fast on I/O errors.
                    let mut lock: Option<SessionLock> = None;
                    for _ in 0..20 {
                        match SessionLock::acquire(&target_dir) {
                            Ok(l) => {
                                lock = Some(l);
                                break;
                            }
                            Err(e) => {
                                if !e.to_string().contains("already running") {
                                    return Err(e.context(format!(
                                        "unexpected error acquiring lock on session {}",
                                        target_session
                                    )));
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        }
                    }
                    let _lock = lock.ok_or_else(|| {
                        anyhow::anyhow!(
                            "failed to acquire lock on session {} after 20 attempts",
                            target_session
                        )
                    })?;

                    wire::append(&tx_path, event)?;
                    let repos = crate::git::Repos::production();
                    repos.traces.commit_session(
                        &self.gait_dir,
                        target_session,
                        &format!(
                            "session({}): agent_update from {}",
                            target_session, from_session
                        ),
                    )?;
                    drop(_lock);
                }

                let argv = vec![
                    "run".to_string(),
                    "--session".to_string(),
                    target_session.clone(),
                    "--gait-dir".to_string(),
                    self.gait_dir.to_str().unwrap_or(".").to_string(),
                    "--auto".to_string(),
                ];
                if let Some(tx) = &self.spawn_tx {
                    let _ = tx.send((argv, false));
                } else {
                    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
                    std::process::Command::new(&exe)
                        .args(&argv)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()?;
                }
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
            target_session: "parent".into(),
            from_session: "child".into(),
            status: "done".into(),
            summary: "x".into(),
        };
        assert!(NoopHarnessBackend.execute(&action).await.is_ok());
    }
}
