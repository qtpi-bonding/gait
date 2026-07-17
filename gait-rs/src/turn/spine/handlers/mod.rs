//! Shared context + feedback builders consumed by both spine signal handlers.

pub mod agent;
pub mod tool;

use crate::proto::gait::sandbox::v1::Feedback;

/// Arguments for `spawn_agent`, deserialized from the Impulse Struct.
pub(super) struct SpawnAgentArgs {
    pub task: String,
    /// Explicit named inference for the child process.
    pub inference: Option<String>,
    /// Per-spawn result schema written to `sessions/<id>/result_schema.json`
    /// so the child CLI can enforce it via `ProfileInputs.result_schema`.
    pub schema: Option<serde_json::Value>,
    /// Future: prepend files as context (reserved).
    #[allow(dead_code)]
    pub context_files: Option<Vec<String>>,
    pub detached: bool,
}
/// Context the spine carries for the entire turn (assembled once near the top
/// of `relay_brain_turn`, after the cascade group is established, and cloned
/// into spawned tasks as needed).
#[derive(Clone)]
pub(crate) struct TurnCtx {
    pub gait_dir: std::path::PathBuf,
    pub codebase: std::path::PathBuf,
    pub session_id: String,
    pub repos: crate::git::Repos,
    pub exe: std::path::PathBuf,
    pub group: std::sync::Arc<dyn crate::process::Containment>,
    pub sandbox_on: bool,
}

/// Build a `Feedback` carrying a JSON value result.
pub(super) fn feedback_result(value: serde_json::Value) -> Feedback {
    // Convert serde_json::Value → pbjson_types::Value via serde_json round-trip.
    // This correctly handles objects/arrays as proto Struct/ListValue rather than
    // stringifying them, so the brain receives a structured proto Value.
    let proto_value: pbjson_types::Value =
        serde_json::from_value(value).unwrap_or(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::NullValue(0)),
        });
    Feedback {
        result: Some(proto_value),
        events: vec![],
        denied: None,
        daemon: None,
        final_content: None,
    }
}

/// Build a Feedback carrying a string error result.
pub fn error_feedback(msg: &str) -> Feedback {
    Feedback {
        result: Some(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(msg.to_string())),
        }),
        events: vec![],
        denied: None,
        daemon: None,
        final_content: None,
    }
}

/// Send a `Feedback` back to the brain over the Nerve.
pub(super) async fn reply_feedback(
    writer: std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    corr_id: u64,
    feedback: Feedback,
) {
    use crate::proto::gait::sandbox::v1::{signal, Signal};
    let resp = Signal {
        corr_id,
        body: Some(signal::Body::Feedback(feedback)),
    };
    let mut w = writer.lock().await;
    crate::turn::nerve::write_signal(&mut *w, &resp).await.ok();
    use tokio::io::AsyncWriteExt;
    w.flush().await.ok();
}

#[cfg(test)]
pub(super) fn test_ctx(ws: &std::path::Path, session_id: &str) -> TurnCtx {
    TurnCtx {
        gait_dir: ws.to_path_buf(),
        codebase: ws.to_path_buf(),
        session_id: session_id.to_string(),
        repos: crate::git::Repos::production(),
        exe: std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("gait")),
        group: crate::process::establish(None),
        sandbox_on: false,
    }
}

#[cfg(test)]
mod tests {
    use super::error_feedback;

    #[test]
    fn hand_response_error_arm_becomes_error_feedback() {
        use gait_proto::gait::sandbox::v1::{hand_response, HandError, HandResponse};
        use prost::Message as _;
        let bytes = HandResponse {
            outcome: Some(hand_response::Outcome::Error(HandError {
                message: "nope".into(),
            })),
        }
        .encode_to_vec();
        // mirror the spine's decode mapping
        let fb = match HandResponse::decode(bytes.as_slice()) {
            Ok(HandResponse {
                outcome: Some(hand_response::Outcome::Error(e)),
            }) => error_feedback(&format!("hand error: {}", e.message)),
            _ => panic!("expected error arm"),
        };
        let s = fb.result.and_then(|v| match v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s),
            _ => None,
        });
        assert_eq!(s.as_deref(), Some("hand error: nope"));
    }
}
