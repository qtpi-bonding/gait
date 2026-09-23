//! Standalone classifier client for mode `classify_policy` — Jev/TypeSafe's
//! `{state, questions}` shape, NOT a `Provider` trait implementation. Jev's
//! request/response shape is not OpenAI-chat-completions compatible (no
//! `messages` array, no streaming, no tool calls), so it does not fit the
//! `Provider` trait this crate otherwise exists to implement. This is a
//! single one-shot HTTP POST per call, deliberately minimal.
//!
//! noul scale: 1 = confident risk detected on that axis, 0 = confident safe.
//! See docs/superpowers/specs/2026-09-19-classify-policy-design.md.

use reqwest::header::{HeaderMap, AUTHORIZATION};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("model string '{0}' has no '/' separator")]
    BadModelString(String),
    #[error("unknown provider '{0}'")]
    UnknownProvider(String),
    #[error("missing API key: set {0}")]
    MissingApiKey(String),
    #[error("classifier transport failed: {0}")]
    Transport(#[from] crate::provider::ProviderError),
    #[error("classify response missing question '{0}'")]
    MissingAnswer(String),
}

#[derive(serde::Serialize)]
pub struct ClassifyQuestion {
    #[serde(rename = "type")]
    pub question_type: String,
    pub instructions: String,
    pub criteria: HashMap<String, String>,
}

#[derive(serde::Serialize)]
pub struct ClassifyRequest {
    pub state: String,
    pub model: String,
    pub questions: HashMap<String, ClassifyQuestion>,
}

#[derive(serde::Deserialize)]
pub struct ClassifyAnswer {
    #[serde(default)]
    pub noul: f64,
}

#[derive(serde::Deserialize)]
pub struct ClassifyResponse {
    pub answers: HashMap<String, ClassifyAnswer>,
}

/// The three risk scores this module always asks for, already extracted —
/// consumed by Task 4's decision logic.
pub struct RiskScores {
    pub confidentiality_risk: f64,
    pub integrity_risk: f64,
    pub availability_risk: f64,
}

fn noul_question(instructions: &str) -> ClassifyQuestion {
    ClassifyQuestion {
        question_type: "noul".to_string(),
        instructions: instructions.to_string(),
        criteria: HashMap::from([
            ("true".to_string(), "risk present".to_string()),
            ("false".to_string(), "no risk".to_string()),
        ]),
    }
}

fn build_request(model: &str, state: &str) -> ClassifyRequest {
    let mut questions = HashMap::new();
    questions.insert(
        "confidentiality_risk".to_string(),
        noul_question("Does this action risk exposing data that should stay private (secrets, credentials, private files, network exfiltration)?"),
    );
    questions.insert(
        "integrity_risk".to_string(),
        noul_question("Does this action alter or corrupt data/state without proper authorization, especially irreversibly?"),
    );
    questions.insert(
        "availability_risk".to_string(),
        noul_question("Does this action disrupt access to systems, processes, or resources for others (not just the caller)?"),
    );
    ClassifyRequest {
        state: state.to_string(),
        model: model.to_string(),
        questions,
    }
}

fn split_model_string(s: &str) -> Option<(&str, &str)> {
    s.split_once('/')
}

/// Resolve `state` (the tool_name/cmd/patterns text to classify) against an
/// already-resolved `model` string (`"<provider_id>/<model_id>"` — the
/// caller resolves this from a named inference profile via
/// `gait_state::inferences::load_inference` before calling here; see the
/// dependency-direction constraint above for why that resolution can't
/// happen inside this crate), and return the three risk scores.
pub async fn classify(model: &str, state: &str) -> Result<RiskScores, ClassifyError> {
    let (provider_id, model_id) = split_model_string(model)
        .ok_or_else(|| ClassifyError::BadModelString(model.to_string()))?;
    let spec = crate::providers::lookup(provider_id)
        .ok_or_else(|| ClassifyError::UnknownProvider(provider_id.to_string()))?;
    let api_key = spec
        .env_var
        .and_then(|v| std::env::var(v).ok())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| ClassifyError::MissingApiKey(spec.env_var.unwrap_or("<unknown>").to_string()))?;

    let base_url = spec.base_url.unwrap_or_default();
    let req = build_request(model_id, state);
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        crate::transport::sensitive_header(&format!("Bearer {api_key}"))?,
    );
    let request = crate::transport::JsonRequest::new(
        format!("{base_url}/systemone"), // This classifier endpoint is separate from the provider's chat-completions endpoint.
        headers,
        serde_json::to_value(&req).map_err(crate::provider::ProviderError::from)?,
    );
    let transport = crate::transport::ProviderTransport::new(
        crate::transport::TransportConfig::default(),
    )?;
    let resp: ClassifyResponse = transport.post_json(request).await?;

    let get = |k: &str| -> Result<f64, ClassifyError> {
        resp.answers
            .get(k)
            .map(|a| a.noul)
            .ok_or_else(|| ClassifyError::MissingAnswer(k.to_string()))
    };
    Ok(RiskScores {
        confidentiality_risk: get("confidentiality_risk")?,
        integrity_risk: get("integrity_risk")?,
        availability_risk: get("availability_risk")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_provider_and_model() {
        let (provider, model) = split_model_string("openrouter/typesafe/jev-1.13").unwrap();
        assert_eq!(provider, "openrouter");
        assert_eq!(model, "typesafe/jev-1.13");
    }

    #[test]
    fn splits_rejects_missing_slash() {
        assert!(split_model_string("no-slash-here").is_none());
    }

    #[test]
    fn build_request_shapes_three_noul_questions() {
        let req = build_request(
            "jev-latest",
            "shell: rm -rf /tmp/x\n\npatterns: shell:rm *",
        );
        assert_eq!(req.questions.len(), 3);
        assert!(req.questions.contains_key("confidentiality_risk"));
        assert!(req.questions.contains_key("integrity_risk"));
        assert!(req.questions.contains_key("availability_risk"));
        for q in req.questions.values() {
            assert_eq!(q.question_type, "noul");
        }
    }

    #[test]
    fn parse_response_extracts_all_three_scores() {
        let body = r#"{"model":"jev-latest","answers":{
            "confidentiality_risk":{"type":"noul","noul":0.02},
            "integrity_risk":{"type":"noul","noul":0.87},
            "availability_risk":{"type":"noul","noul":0.01}
        }}"#;
        let resp: ClassifyResponse = serde_json::from_str(body).unwrap();
        assert!((resp.answers["confidentiality_risk"].noul - 0.02).abs() < 1e-9);
        assert!((resp.answers["integrity_risk"].noul - 0.87).abs() < 1e-9);
        assert!((resp.answers["availability_risk"].noul - 0.01).abs() < 1e-9);
    }
}
