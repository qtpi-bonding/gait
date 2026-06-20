use crate::proto::ProtoError;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("proto error: {0}")]
    Proto(#[from] ProtoError),
}
