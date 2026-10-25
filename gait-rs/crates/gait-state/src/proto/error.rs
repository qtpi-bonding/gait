#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.log")]
pub enum ProtoError {
    #[error("unexpected proto variant: {0}")]
    #[coded(code = "unexpected_variant", class = Infrastructure)]
    UnexpectedVariant(String),
    #[error("missing required field: {0}")]
    #[coded(code = "missing_field", class = Infrastructure)]
    MissingField(String),
}

#[cfg(test)]
mod tests {
    use super::ProtoError;
    use coded_error::{CodedError, ErrorClass};

    #[test]
    fn proto_error_codes_are_unique() {
        coded_error::check_codes("gait.log.", &[ProtoError::ALL_CODES]).unwrap();
        assert_eq!(
            ProtoError::ALL_CODES,
            &["gait.log.unexpected_variant", "gait.log.missing_field"]
        );
    }

    #[test]
    fn proto_error_codes_and_classes() {
        let unexpected = ProtoError::UnexpectedVariant("x".into());
        assert_eq!(
            (unexpected.code(), unexpected.class()),
            ("gait.log.unexpected_variant", ErrorClass::Infrastructure)
        );
        let missing = ProtoError::MissingField("x".into());
        assert_eq!(
            (missing.code(), missing.class()),
            ("gait.log.missing_field", ErrorClass::Infrastructure)
        );
    }
}
