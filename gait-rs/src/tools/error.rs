#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// User-facing error: returned to the model as tool output text.
    #[error("{0}")]
    Execute(String),
    /// Bad tool arguments — returned verbatim so the model can self-correct.
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    /// Internal programming error: should not reach the model as-is.
    #[error("internal tool error: {0}")]
    Internal(#[from] anyhow::Error),
}
