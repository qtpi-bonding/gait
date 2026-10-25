use crate::proto::ProtoError;

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.session")]
pub enum SessionError {
    #[error("io error: {0}")]
    #[coded(code = "io", class = Infrastructure)]
    Io(#[from] std::io::Error),
    #[error("event parse error: {0}")]
    #[coded(code = "parse", class = Infrastructure)]
    Parse(#[from] serde_json::Error),
    #[error("proto error: {0}")]
    #[coded(code = "proto", class = Infrastructure)]
    Proto(#[from] ProtoError),
    #[error("unsupported event log version {found}; expected {expected}")]
    #[coded(code = "unsupported_log_version", class = Logical)]
    UnsupportedLogVersion { found: u32, expected: u32 },
    #[error("session is live; cannot append a registry event while a turn is running")]
    #[coded(code = "session_running", class = Transient)]
    SessionRunning,
}

#[cfg(test)]
mod tests {
    use super::SessionError;
    use coded_error::{CodedError, ErrorClass};

    #[test]
    fn session_error_codes_are_unique() {
        coded_error::check_codes("gait.session.", &[SessionError::ALL_CODES]).unwrap();
        assert_eq!(
            SessionError::ALL_CODES,
            &[
                "gait.session.io",
                "gait.session.parse",
                "gait.session.proto",
                "gait.session.unsupported_log_version",
                "gait.session.session_running",
            ]
        );
    }

    #[test]
    fn session_error_codes_and_classes() {
        let io = SessionError::Io(std::io::Error::other("disk"));
        assert_eq!((io.code(), io.class()), ("gait.session.io", ErrorClass::Infrastructure));
        let parse = SessionError::Parse(serde_json::from_str::<u32>("x").unwrap_err());
        assert_eq!((parse.code(), parse.class()), ("gait.session.parse", ErrorClass::Infrastructure));
        let proto = SessionError::Proto(crate::proto::ProtoError::MissingField("f".into()));
        assert_eq!((proto.code(), proto.class()), ("gait.session.proto", ErrorClass::Infrastructure));
        let unsupported = SessionError::UnsupportedLogVersion { found: 1, expected: 2 };
        assert_eq!(
            (unsupported.code(), unsupported.class()),
            ("gait.session.unsupported_log_version", ErrorClass::Logical)
        );
        let running = SessionError::SessionRunning;
        assert_eq!((running.code(), running.class()), ("gait.session.session_running", ErrorClass::Transient));
    }
}
