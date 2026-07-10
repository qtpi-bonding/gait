#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("unexpected proto variant: {0}")]
    UnexpectedVariant(String),
    #[error("missing required field: {0}")]
    MissingField(String),
}
