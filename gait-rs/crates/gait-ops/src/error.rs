//! The one error type every op returns. Frontends that need structured codes
//! (ACP/JSON-RPC) match the variants; the CLI just renders `Display`.
#[derive(thiserror::Error, Debug)]
pub enum OpError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("session busy: {0}")]
    SessionBusy(String),
    #[error("confirmation required: {0}")]
    ConfirmationRequired(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
