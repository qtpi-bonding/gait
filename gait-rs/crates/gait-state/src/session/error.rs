use crate::proto::ProtoError;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("proto error: {0}")]
    Proto(#[from] ProtoError),
    #[error("unsupported event log version {found}; expected {expected}")]
    UnsupportedLogVersion { found: u32, expected: u32 },
    #[error("session is live; cannot append a registry event while a turn is running")]
    SessionRunning,
}
