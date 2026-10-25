use anyhow::Error as AnyhowError;

pub const NEW_SESSION: &str = "session/new";
pub const LOAD_SESSION: &str = "session/load";
pub const RESUME_SESSION: &str = "session/resume";
pub const PROMPT: &str = "session/prompt";
pub const CLOSE_SESSION: &str = "session/close";
pub const DELETE_SESSION: &str = "session/delete";
pub const LIST_SESSIONS: &str = "session/list";
pub const SET_SESSION_MODE: &str = "session/set_mode";

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.acp.prompt")]
pub enum PromptError {
    #[error("attestation required")]
    #[coded(code = "attestation_required", class = Logical)]
    AttestationRequired,
    #[error("unknown key id: {0}")]
    #[coded(code = "attestation_unknown_key", class = Logical)]
    AttestationUnknownKey(String),
    #[error("bad signature")]
    #[coded(code = "attestation_bad_signature", class = Logical)]
    AttestationBadSignature,
    #[error("own device is not a recipient")]
    #[coded(code = "attestation_not_recipient", class = Logical)]
    AttestationNotRecipient,
    #[error("replay: {0}")]
    #[coded(code = "attestation_replay", class = Logical)]
    AttestationReplay(String),
    #[error("attestation has no messages")]
    #[coded(code = "attestation_empty", class = Logical)]
    AttestationEmpty,
    #[error("missing own devices")]
    #[coded(code = "attestation_missing_own_devices", class = Logical)]
    AttestationMissingOwnDevices,
    #[error("prompt already seen by agent")]
    #[coded(code = "already_seen", class = Logical, acp = -32001)]
    AlreadySeen { key: String },
    #[error("unknown session: {0}")]
    #[coded(code = "session_unknown", class = Logical)]
    SessionUnknown(String),
    #[error("session already has an in-flight turn: {0}")]
    #[coded(code = "session_busy", class = Transient)]
    SessionBusy(String),
    #[error("cannot close {sid}: {why}")]
    #[coded(code = "cannot_close", class = Logical)]
    CannotClose { sid: String, why: String },
    #[error("failed to replay session: {0}")]
    #[coded(code = "replay_failed", class = Infrastructure)]
    ReplayFailed(String),
    #[error("{0}")]
    #[coded(code = "internal", class = Infrastructure)]
    Internal(String),
}

pub fn acp_error(err: &AnyhowError, method: &'static str) -> agent_client_protocol::Error {
    let wire = coded_error::acp::to_acp_error_anyhow_redacted(err, method, "request failed");
    concord_telemetry::error_anyhow!(err, source = method, rpc.method = method);
    let mut data = wire.data;
    if let Some(already) = coded_error::downcast_ref::<PromptError>(err)
        && let PromptError::AlreadySeen { key } = already
    {
        data["reason"] = serde_json::Value::String("already_seen_by_agent".to_owned());
        data["idempotency_key"] = serde_json::Value::String(key.clone());
    }
    agent_client_protocol::Error::new(wire.code, wire.message).data(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coded_error::{CodedError, ErrorClass};

    #[test]
    fn prompt_error_codes_are_unique() {
        coded_error::check_codes("gait.acp.prompt.", &[PromptError::ALL_CODES]).unwrap();
        assert_eq!(PromptError::ALL_CODES.len(), 13);
    }

    #[test]
    fn already_seen_wire_shape_is_unchanged() {
        let err = coded_error::into_anyhow(PromptError::AlreadySeen { key: "k1".into() });
        let wire = acp_error(&err, PROMPT);
        assert_eq!(i32::from(wire.code), -32001);
        assert_eq!(wire.message, "prompt already seen by agent");
        assert_eq!(wire.data.as_ref().unwrap()["reason"], "already_seen_by_agent");
        assert_eq!(wire.data.as_ref().unwrap()["idempotency_key"], "k1");
        assert_eq!(wire.data.as_ref().unwrap()["code"], "gait.acp.prompt.already_seen");
        assert_eq!(wire.data.as_ref().unwrap()["class"], "logical");
        assert!(!wire.data.as_ref().unwrap()["fingerprint"].as_str().unwrap().is_empty());
    }

    #[test]
    fn logical_error_keeps_its_message() {
        let err = coded_error::into_anyhow(PromptError::SessionUnknown("s".into()));
        let wire = acp_error(&err, LOAD_SESSION);
        assert_eq!(wire.message, "unknown session: s");
        assert_eq!(wire.data.as_ref().unwrap()["class"], "logical");
    }

    #[test]
    fn infrastructure_error_message_is_redacted_to_request_failed() {
        let err = coded_error::into_anyhow(PromptError::ReplayFailed("secret".into()));
        let wire = acp_error(&err, LOAD_SESSION);
        assert_eq!(wire.message, "request failed");
        assert_eq!(wire.data.as_ref().unwrap()["class"], "infrastructure");
    }

    #[test]
    fn transient_session_busy_maps_to_minus_32603_with_class_transient() {
        let err = coded_error::into_anyhow(PromptError::SessionBusy("s".into()));
        let wire = acp_error(&err, PROMPT);
        assert_eq!(i32::from(wire.code), -32603);
        assert_eq!(wire.data.as_ref().unwrap()["class"], "transient");
    }

    #[test]
    fn acp_error_emits_one_error_record_with_matching_fingerprint() {
        let capture = concord_telemetry::testing::capture();
        let err = coded_error::into_anyhow(PromptError::ReplayFailed("not-for-record".into()));
        let wire = acp_error(&err, PROMPT);
        let records = capture.errors_where(|_| true);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].fingerprint, wire.data.as_ref().unwrap()["fingerprint"]);
        assert!(!records[0]
            .fields
            .values()
            .any(|value| format!("{value:?}").contains("not-for-record")));
    }

    #[test]
    fn logical_error_class_is_stable() {
        assert_eq!(PromptError::SessionUnknown("x".into()).class(), ErrorClass::Logical);
    }
}
