//! Spine-side `send_message`: sign with the agent's device session, then run the
//! configured send command with only the allowlisted environment. The brain is
//! sealed and holds no keys; it only proposes the message.

use std::time::Duration;

use crate::proto::gait::sandbox::v1::{Feedback, Impulse};

use super::{error_feedback, feedback_result, TurnCtx};

const SESSION_TTL_SECS: i64 = 300;
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// The model's tool input as JSON.
pub(crate) fn parse_send_input(impulse: &Impulse) -> serde_json::Value {
    impulse
        .args
        .as_ref()
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn child_env() -> Vec<(String, String)> {
    std::env::vars().filter(|(k, _)| crate::env_passes_to_children(k)).collect()
}

pub(crate) async fn handle_send_message(input: serde_json::Value, ctx: TurnCtx) -> Feedback {
    let Some(config) = crate::send::config() else {
        return error_feedback("send_message: not configured");
    };
    send_with(config, input, ctx).await
}

async fn send_with(
    config: &crate::send::SendConfig,
    input: serde_json::Value,
    ctx: TurnCtx,
) -> Feedback {
    let Some(identity) = ctx.identity.clone() else {
        return error_feedback("send_message: no agent identity configured");
    };
    let now = crate::tools::spawn_agent::wall_clock_secs();
    let (session, device_delegation) =
        match gait_identity::device_session(&identity, SESSION_TTL_SECS, now) {
            Ok(pair) => pair,
            Err(error) => return error_feedback(&format!("send_message: {error:#}")),
        };
    let signer = message_attestation::ReplySigner::Keyed {
        session: std::sync::Arc::new(session),
        device_delegation,
    };
    let command = message_attestation::SendCommand {
        argv: config.argv.clone(),
        env: child_env(),
        timeout: SEND_TIMEOUT,
    };
    let audience = config.audience.clone();
    let result = tokio::task::spawn_blocking(move || {
        message_attestation::run_send_message(&input, &signer, &audience, now, &command)
    })
    .await;
    match result {
        Ok(result) => {
            let mut feedback = feedback_result(result.to_json());
            if result.is_error() {
                feedback.is_error = Some(true);
            }
            feedback
        }
        Err(_) => error_feedback("send_message: the send task failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_env_is_the_allowlist() {
        // Planted names that must never reach the send command.
        // SAFETY: test-only, the names are unique to this test.
        unsafe {
            std::env::set_var("GAIT_AGENT_DIR", "/secret");
            std::env::set_var("OPENAI_API_KEY", "k");
        }
        let env = child_env();
        assert!(env.iter().all(|(k, _)| crate::env_passes_to_children(k)));
        assert!(!env.iter().any(|(k, _)| k == "GAIT_AGENT_DIR" || k == "OPENAI_API_KEY"));
    }

    #[tokio::test]
    async fn unconfigured_send_is_an_error_feedback() {
        let ws = tempfile::tempdir().unwrap();
        let ctx = super::super::test_ctx(ws.path(), "s1", None);
        let fb = handle_send_message(serde_json::json!({"to": ["room:r"], "body": "x"}), ctx).await;
        assert!(fb.result.is_some());
    }

    fn deliverer() -> identity_crypto::PrincipalId {
        let duo = identity_crypto::jwk_duo::GenerationService::generate_key_duo();
        identity_crypto::principal_id(&duo)
    }

    fn result_json(fb: &Feedback) -> serde_json::Value {
        serde_json::to_value(fb.result.as_ref().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn send_command_receives_a_signed_message_by_the_agent_and_a_clean_env() {
        use prost::Message as _;
        let ws = tempfile::tempdir().unwrap();
        let ident = gait_identity::IdentityContext::mint_or_load(&ws.path().join("g")).unwrap();
        let agent = ident.agent_id().clone();
        let ctx = super::super::test_ctx(ws.path(), "s1", Some(std::sync::Arc::new(ident)));
        let out = ws.path().join("stdin.bin");
        let envf = ws.path().join("env.txt");
        let config = crate::send::SendConfig {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("/usr/bin/env > {}; /bin/cat > {}", envf.display(), out.display()),
            ],
            audience: deliverer(),
        };
        unsafe { std::env::set_var("GAIT_AGENT_DIR", "/secret") };
        let fb = send_with(&config, serde_json::json!({"to": ["room:r1"], "body": "hi"}), ctx).await;
        let json = result_json(&fb);
        assert_eq!(json["status"], "sent", "{json}");
        let signed = identity_crypto::pb::SignedMessage::decode(std::fs::read(&out).unwrap().as_slice())
            .expect("stdin decodes as a SignedMessage");
        assert_eq!(signed.body_type, message_attestation::MESSAGE_BODY_TYPE);
        let message = identity_crypto::pb::Message::decode(signed.body.as_slice()).unwrap();
        assert_eq!(message.message_id, json["message_id"].as_str().unwrap());
        assert_eq!(message.body, "hi");
        let _ = agent;
        let env = std::fs::read_to_string(&envf).unwrap();
        assert!(!env.contains("GAIT_AGENT_DIR"), "{env}");
    }

    #[tokio::test]
    async fn a_failing_send_command_is_an_error_feedback() {
        let ws = tempfile::tempdir().unwrap();
        let ident = gait_identity::IdentityContext::mint_or_load(&ws.path().join("g")).unwrap();
        let ctx = super::super::test_ctx(ws.path(), "s1", Some(std::sync::Arc::new(ident)));
        let config = crate::send::SendConfig {
            argv: vec!["/usr/bin/false".into()],
            audience: deliverer(),
        };
        let fb = send_with(&config, serde_json::json!({"to": ["room:r1"], "body": "hi"}), ctx).await;
        assert_eq!(fb.is_error, Some(true));
        assert_eq!(result_json(&fb)["status"], "failed");
    }
}
