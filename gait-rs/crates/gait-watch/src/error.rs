#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("session error: {0}")]
    Session(#[from] gait_state::session::SessionError),
    #[error("filesystem watch error: {0}")]
    Notify(#[from] notify::Error),
}
