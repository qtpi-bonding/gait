//! The one error type every op returns. Frontends that need structured codes
//! (ACP/JSON-RPC) match the variants; the CLI just renders `Display`.
//!
//! There is no catch-all variant: an op that fails for any other reason returns
//! a plain `anyhow::Error`. Read the typed variants back out of an `anyhow`
//! chain with `coded_error::downcast_ref::<OpError>`, never plain `downcast_ref`.
#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.op")]
pub enum OpError {
    #[error("not found: {0}")]
    #[coded(code = "not_found", class = Logical)]
    NotFound(String),
    #[error("conflict: {0}")]
    #[coded(code = "conflict", class = Logical)]
    Conflict(String),
    #[error("session busy: {0}")]
    #[coded(code = "session_busy", class = Transient)]
    SessionBusy(String),
    #[error("confirmation required: {0}")]
    #[coded(code = "confirmation_required", class = Logical)]
    ConfirmationRequired(String),
}

impl OpError {
    /// The typed failure carried by an op's `anyhow::Error`, if it has one.
    pub fn find(err: &anyhow::Error) -> Option<&OpError> {
        coded_error::downcast_ref::<OpError>(err)
    }
}

#[cfg(test)]
mod tests {
    use super::OpError;
    use coded_error::CodedError;

    #[test]
    fn op_error_codes_are_unique() {
        coded_error::check_codes("gait.op.", &[OpError::ALL_CODES]).unwrap();
        assert_eq!(
            OpError::ALL_CODES,
            &["gait.op.not_found", "gait.op.conflict", "gait.op.session_busy", "gait.op.confirmation_required"]
        );
    }

    #[test]
    fn op_error_classes() {
        use coded_error::ErrorClass::{Logical, Transient};
        let not_found = OpError::NotFound("x".into());
        assert_eq!(not_found.code(), "gait.op.not_found");
        assert_eq!(not_found.class(), Logical);
        let conflict = OpError::Conflict("x".into());
        assert_eq!(conflict.code(), "gait.op.conflict");
        assert_eq!(conflict.class(), Logical);
        let busy = OpError::SessionBusy("x".into());
        assert_eq!(busy.code(), "gait.op.session_busy");
        assert_eq!(busy.class(), Transient);
        let confirm = OpError::ConfirmationRequired("x".into());
        assert_eq!(confirm.code(), "gait.op.confirmation_required");
        assert_eq!(confirm.class(), Logical);
    }

    #[test]
    fn op_error_has_no_other_variant() {
        let err = coded_error::into_anyhow(OpError::SessionBusy("turn running".into()));
        let typed = coded_error::downcast_ref::<OpError>(&err).expect("typed OpError survives anyhow");
        assert!(matches!(typed, OpError::SessionBusy(_)));
        assert_eq!(typed.class(), coded_error::ErrorClass::Transient);
        assert_eq!(format!("{err}"), "session busy: turn running");
    }
}
