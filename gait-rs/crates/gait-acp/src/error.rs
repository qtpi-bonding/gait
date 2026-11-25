use anyhow::Error as AnyhowError;
use gait_identity::{
    MODE_HINT_SUFFIX, OLD_FORMAT_HINT, OWNER_HINT_SUFFIX, mode_detail, owner_detail,
};

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
    #[error("untrusted deliverer: {0}")]
    #[coded(code = "attestation_unknown_deliverer", class = Logical)]
    AttestationUnknownDeliverer(String),
    #[error("delivery is addressed to another agent")]
    #[coded(code = "attestation_recipient_id_mismatch", class = Logical)]
    AttestationRecipientIdMismatch,
    #[error("message {index} was not addressed to the delivering principal")]
    #[coded(code = "attestation_audience_mismatch", class = Logical)]
    AttestationAudienceMismatch { index: usize },
    #[error("invalid message: {0}")]
    #[coded(code = "attestation_invalid_message", class = Logical)]
    AttestationInvalidMessage(String),
    #[error("delivery is stale: {0}")]
    #[coded(code = "attestation_stale", class = Logical)]
    AttestationStale(String),
    #[error("bad signature")]
    #[coded(code = "attestation_bad_signature", class = Logical)]
    AttestationBadSignature,
    #[error("own device is not a recipient")]
    #[coded(code = "attestation_not_recipient", class = Logical)]
    AttestationNotRecipient,
    #[error("replay: {0}")]
    #[coded(code = "attestation_replay", class = Logical)]
    AttestationReplay(String),
    #[error(
        "an attested batch #{got} was already confirmed (last {last}); its message is already logged"
    )]
    #[coded(code = "attestation_replay_confirmed", class = Logical)]
    AttestationReplayConfirmed { last: u64, got: u64 },
    #[error("attestation has no messages")]
    #[coded(code = "attestation_empty", class = Logical)]
    AttestationEmpty,
    #[error("missing own devices")]
    #[coded(code = "attestation_missing_own_devices", class = Logical)]
    AttestationMissingOwnDevices,
    #[error("too many messages in the delivery: {count} (max {max})")]
    #[coded(code = "too_many_messages", class = Logical)]
    TooManyMessages { count: usize, max: usize },
    #[error("recipient device {index} is not a public key")]
    #[coded(code = "invalid_recipient_device", class = Logical)]
    InvalidRecipientDevice { index: usize },
    #[error("invalid author name: {0}")]
    #[coded(code = "invalid_name", class = Logical)]
    InvalidName(String),
    #[error("name for {0}, who authors no message in the delivery")]
    #[coded(code = "stray_name", class = Logical)]
    StrayName(String),
    #[error("session is not valid at {now} (window ends {not_after})")]
    #[coded(code = "session_expired", class = Logical)]
    SessionExpired { not_after: i64, now: i64 },
    #[error("no OS randomness: {0}")]
    #[coded(code = "randomness", class = Infrastructure)]
    Randomness(String),
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
    #[error("cannot delete {sid}: {why}")]
    #[coded(code = "cannot_delete", class = Logical)]
    CannotDelete { sid: String, why: String },
    #[error("failed to replay session: {0}")]
    #[coded(code = "replay_failed", class = Infrastructure)]
    ReplayFailed(String),
    #[error("{0}")]
    #[coded(code = "invalid_request", class = Logical)]
    InvalidRequest(String),
    #[error("could not record the cancellation: {0}")]
    #[coded(code = "cancel_record_failed", class = Infrastructure)]
    CancelRecordFailed(String),
    #[error("answer transport failed: {0}")]
    #[coded(code = "answer_transport_failed", class = Infrastructure)]
    AnswerTransportFailed(String),
    #[error("the client could not write the file: {0}")]
    #[coded(code = "fs_write_failed", class = Infrastructure)]
    FsWriteFailed(#[source] agent_client_protocol::Error),
    #[error("{0}")]
    #[coded(code = "internal", class = Infrastructure)]
    Internal(String),
}

/// The attestation replay state could not be opened, so the process refuses
/// to start (or to confirm a batch) rather than run against state that may
/// let a replayed delivery through. Nothing is changed on disk: the operator
/// fixes the permissions or converts the replay file. The startup exit
/// code is the class's (Logical: 65), which is not a retryable class, so a
/// supervisor does not crash-loop on it.
#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.acp.attestation_state")]
pub enum AttestationStateError {
    #[error("attestation state {} is not private ({detail}){hint}", .path.display())]
    #[coded(code = "insecure", class = Logical)]
    Insecure {
        path: std::path::PathBuf,
        detail: String,
        hint: &'static str,
    },
    #[error("attestation state {} uses the old layout ({reason}); {OLD_FORMAT_HINT}", .path.display())]
    #[coded(code = "old_layout", class = Logical)]
    OldLayout {
        path: std::path::PathBuf,
        reason: String,
    },
    #[error("cannot open attestation state at {}: {reason}", .path.display())]
    #[coded(code = "unavailable", class = Infrastructure)]
    Unavailable {
        path: std::path::PathBuf,
        reason: String,
    },
}

impl AttestationStateError {
    /// From a keyfile failure. `fallback` names the state when the failure
    /// carries no path of its own.
    pub fn from_keyfile(
        fallback: &std::path::Path,
        error: &identity_keyfile::KeyfileError,
    ) -> Self {
        use identity_keyfile::KeyfileError as K;
        match error {
            K::InsecureDir(path) | K::InsecureFile(path) => {
                Self::insecure(path, mode_detail(path), MODE_HINT_SUFFIX)
            }
            K::WrongOwner(path) => Self::insecure(path, owner_detail(path), OWNER_HINT_SUFFIX),
            K::NotAFile(path) => Self::insecure(path, "not a regular file".to_owned(), ""),
            K::TooLarge { path, cap } => {
                Self::insecure(path, format!("larger than {cap} bytes"), "")
            }
            K::InvalidMode(mode) => {
                Self::insecure(fallback, format!("mode {:04o}", mode & 0o7777), "")
            }
            other => Self::Unavailable {
                path: fallback.to_owned(),
                reason: other.to_string(),
            },
        }
    }

    /// From a replay-store failure. `fallback` is the state path to name when
    /// the cause carries none. Classification is by the source's type, never
    /// its message text: a `ReplayStateError` source is the old layout,
    /// keyfile failures are classified from their own types, and everything
    /// else (io, serde_json, any other untyped source, or no source) is
    /// Unavailable.
    pub fn from_store(
        fallback: &std::path::Path,
        error: &message_attestation::AttestationError,
    ) -> Self {
        use std::error::Error as _;
        let source = error.source();
        match source {
            Some(s)
                if s.downcast_ref::<message_attestation::ReplayStateError>()
                    .is_some() =>
            {
                Self::OldLayout {
                    path: fallback.to_owned(),
                    reason: s.to_string(),
                }
            }
            Some(s) if s.downcast_ref::<identity_keyfile::KeyfileError>().is_some() => {
                let keyfile = s.downcast_ref::<identity_keyfile::KeyfileError>().unwrap();
                Self::from_keyfile(fallback, keyfile)
            }
            Some(s) => Self::Unavailable {
                path: fallback.to_owned(),
                reason: s.to_string(),
            },
            None => Self::Unavailable {
                path: fallback.to_owned(),
                reason: error.to_string(),
            },
        }
    }

    fn insecure(path: &std::path::Path, detail: String, hint: &'static str) -> Self {
        Self::Insecure {
            path: path.to_owned(),
            detail,
            hint,
        }
    }
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
        assert_eq!(PromptError::ALL_CODES.len(), 29);
    }

    #[test]
    fn already_seen_wire_shape_is_unchanged() {
        let err = coded_error::into_anyhow(PromptError::AlreadySeen { key: "k1".into() });
        let wire = acp_error(&err, PROMPT);
        assert_eq!(i32::from(wire.code), -32001);
        assert_eq!(wire.message, "prompt already seen by agent");
        assert_eq!(
            wire.data.as_ref().unwrap()["reason"],
            "already_seen_by_agent"
        );
        assert_eq!(wire.data.as_ref().unwrap()["idempotency_key"], "k1");
        assert_eq!(
            wire.data.as_ref().unwrap()["code"],
            "gait.acp.prompt.already_seen"
        );
        assert_eq!(wire.data.as_ref().unwrap()["class"], "logical");
        assert!(
            !wire.data.as_ref().unwrap()["fingerprint"]
                .as_str()
                .unwrap()
                .is_empty()
        );
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
        let capture = concord_telemetry::testing::capture_exclusive();
        let err = coded_error::into_anyhow(PromptError::ReplayFailed("not-for-record".into()));
        let wire = acp_error(&err, PROMPT);
        let fingerprint = wire.data.as_ref().unwrap()["fingerprint"].clone();
        let records = capture.errors_where(|record| record.fingerprint == fingerprint);
        assert_eq!(records.len(), 1);
        // Error text is recorded by default (CONCORD_ERROR_TEXT=full).
        assert!(
            records[0]
                .fields
                .values()
                .any(|value| format!("{value:?}").contains("not-for-record"))
        );
    }

    #[test]
    fn from_store_classifies_by_source_type_not_message_text() {
        use message_attestation::AttestationError as A;
        let path = std::path::Path::new("/state/replay.json");
        // A typed io failure whose text mentions the old-layout words is not the old layout.
        let io = std::io::Error::other("replay entry has no attested_at");
        let err = AttestationStateError::from_store(path, &A::Store(Box::new(io)));
        assert!(
            matches!(err, AttestationStateError::Unavailable { .. }),
            "{err}"
        );
        // A typed parse failure likewise.
        let parse = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = AttestationStateError::from_store(path, &A::Store(Box::new(parse)));
        assert!(
            matches!(err, AttestationStateError::Unavailable { .. }),
            "{err}"
        );
        // The library's typed layout failures are the old layout, whatever their wording.
        for typed in [
            message_attestation::ReplayStateError::OldLayout,
            message_attestation::ReplayStateError::StateNotObject,
            message_attestation::ReplayStateError::InvalidSequence,
        ] {
            let err = AttestationStateError::from_store(path, &A::Store(Box::new(typed)));
            assert!(
                matches!(err, AttestationStateError::OldLayout { .. }),
                "{typed:?}: {err}"
            );
        }
        // Any other untyped source is never classified as the old layout, even when its
        // text says so.
        for text in [
            "reworded",
            "replay entry is in the old layout (no attested_at)",
        ] {
            let err = AttestationStateError::from_store(path, &A::Store(text.to_owned().into()));
            assert!(
                matches!(err, AttestationStateError::Unavailable { .. }),
                "{text}: {err}"
            );
        }
        // A keyfile failure is never the old layout.
        let key = identity_keyfile::KeyfileError::InsecureDir(path.to_owned());
        let err = AttestationStateError::from_store(path, &A::Store(Box::new(key)));
        assert!(
            matches!(err, AttestationStateError::Insecure { .. }),
            "{err}"
        );
    }

    #[test]
    fn the_migration_hint_shows_for_mode_owner_and_old_layout_only() {
        use identity_keyfile::KeyfileError as K;
        use message_attestation::AttestationError as A;
        let path = std::path::Path::new("/state/replay.json");
        let mode =
            "tighten the permissions (chmod 700 for a directory, chmod 600 for a file) and retry";
        let hinted = [
            (
                AttestationStateError::from_keyfile(path, &K::InsecureDir(path.to_owned())),
                mode,
            ),
            (
                AttestationStateError::from_keyfile(path, &K::InsecureFile(path.to_owned())),
                mode,
            ),
            (
                AttestationStateError::from_keyfile(path, &K::WrongOwner(path.to_owned())),
                "the path must be owned by the current user",
            ),
            (
                AttestationStateError::from_store(
                    path,
                    &A::Store(Box::new(message_attestation::ReplayStateError::OldLayout)),
                ),
                "is in an old format and must be converted",
            ),
        ];
        for (err, want) in hinted {
            assert!(err.to_string().contains(want), "{err}");
        }
        let plain = [
            AttestationStateError::from_keyfile(path, &K::NotAFile(path.to_owned())),
            AttestationStateError::from_keyfile(
                path,
                &K::TooLarge {
                    path: path.to_owned(),
                    cap: 9,
                },
            ),
            AttestationStateError::from_store(path, &A::Store("boom".to_owned().into())),
        ];
        for err in plain {
            let text = err.to_string();
            assert!(
                !text.contains("tighten the permissions")
                    && !text.contains("old format")
                    && !text.contains("owned by the current user"),
                "{text}"
            );
            assert!(text.contains("/state/replay.json"), "{text}");
        }
    }

    #[test]
    fn logical_error_class_is_stable() {
        assert_eq!(
            PromptError::SessionUnknown("x".into()).class(),
            ErrorClass::Logical
        );
    }
}
