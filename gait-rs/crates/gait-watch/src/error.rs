#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.watch")]
pub enum WatchError {
    #[error("io error: {0}")]
    #[coded(code = "io", class = Infrastructure)]
    Io(#[from] std::io::Error),
    #[error("event parse error: {0}")]
    #[coded(code = "parse", class = Logical)]
    Parse(#[from] serde_json::Error),
    #[error("session error: {0}")]
    #[coded(code = "session", class = Logical)]
    Session(#[from] gait_state::session::SessionError),
    #[error("filesystem watch error: {0}")]
    #[coded(code = "notify", class = Logical)]
    Notify(#[from] notify::Error),
}

#[cfg(test)]
mod coded_error_tests {
    use super::WatchError;
    use coded_error::{CodedError, ErrorClass};

    #[test]
    fn watch_error_codes_are_unique() {
        coded_error::check_codes("gait.watch.", &[WatchError::ALL_CODES]).unwrap();
    }

    #[test]
    fn watch_error_codes_and_classes_per_variant() {
        let io = WatchError::Io(std::io::Error::other("boom"));
        assert_eq!(io.code(), "gait.watch.io");
        assert_eq!(io.class(), ErrorClass::Infrastructure);

        let parse = WatchError::Parse(serde_json::from_str::<u8>("x").unwrap_err());
        assert_eq!(parse.code(), "gait.watch.parse");
        assert_eq!(parse.class(), ErrorClass::Logical);

        let session = WatchError::Session(
            gait_state::session::SessionError::SessionRunning,
        );
        assert_eq!(session.code(), "gait.watch.session");
        assert_eq!(session.class(), ErrorClass::Logical);

        let notify = WatchError::Notify(notify::Error::generic("nope"));
        assert_eq!(notify.code(), "gait.watch.notify");
        assert_eq!(notify.class(), ErrorClass::Logical);
    }

    #[test]
    fn display_texts_unchanged() {
        let io = WatchError::Io(std::io::Error::other("boom"));
        assert_eq!(io.to_string(), "io error: boom");
        let session = WatchError::Session(
            gait_state::session::SessionError::SessionRunning,
        );
        assert_eq!(
            session.to_string(),
            "session error: session is live; cannot append a registry event while a turn is running"
        );
    }
}
