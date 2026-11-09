//! The spine's own failures, as coded errors a monitor can count. Each one used
//! to end in a transcript-only record, an `eprintln!` or a dropped `Result`.

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.spine")]
pub enum SpineError {
    #[error("failed to spawn __brain: {0}")]
    #[coded(code = "brain_spawn_failed", class = Infrastructure)]
    BrainSpawnFailed(#[source] std::io::Error),
    #[error("the brain exited without finishing its turn")]
    #[coded(code = "brain_died", class = Infrastructure)]
    BrainDied,
    #[error("the brain's process group could not be killed: {0}")]
    #[coded(code = "brain_kill_failed", class = Infrastructure)]
    BrainKillFailed(#[source] BoxError),
    #[error("LLM timed out: {0}")]
    #[coded(code = "llm_stalled", class = Transient)]
    LlmStalled(String),
    #[error("a brain event could not be written to the session log: {0}")]
    #[coded(code = "log_relay_failed", class = Infrastructure)]
    LogRelayFailed(#[source] BoxError),
}

/// Records `error` as a coded error record tagged with the session, and returns
/// it as an `anyhow::Error` (code intact) for a caller that propagates it.
pub(crate) fn raise(error: SpineError, session_id: &str) -> anyhow::Error {
    let error = coded_error::into_anyhow(error);
    #[cfg(feature = "telemetry")]
    concord_telemetry::error_anyhow!(&error, source = "gait.spine", session_id = %session_id);
    #[cfg(not(feature = "telemetry"))]
    let _ = session_id;
    error
}

/// [`raise`] for a failure the spine reports and carries on from.
pub(crate) fn record(error: SpineError, session_id: &str) {
    drop(raise(error, session_id));
}

#[cfg(test)]
mod tests {
    use super::SpineError;
    use coded_error::CodedError;

    #[test]
    fn spine_error_codes_are_unique() {
        coded_error::check_codes("gait.spine.", &[SpineError::ALL_CODES]).unwrap();
    }

    #[test]
    fn a_stall_is_transient_and_a_dead_brain_is_infrastructure() {
        assert_eq!(SpineError::LlmStalled("x".into()).class(), coded_error::ErrorClass::Transient);
        assert_eq!(SpineError::BrainDied.class(), coded_error::ErrorClass::Infrastructure);
        assert_eq!(SpineError::BrainDied.code(), "gait.spine.brain_died");
    }
}
