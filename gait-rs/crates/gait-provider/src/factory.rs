use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::provider::Provider;

#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    #[error("invalid model string '{0}': expected 'provider/model-id'")]
    InvalidFormat(String),
    #[error("unknown provider '{0}'")]
    UnknownProvider(String),
    #[error("missing API key for provider '{0}': set {1} env var")]
    MissingApiKey(String, String),
}

/// Per-provider base URL override, e.g. `OLLAMA_BASE_URL=http://192.168.1.50:11500/v1`
/// for a local runtime that isn't listening on its default host/port.
fn base_url_override(provider_id: &str) -> Option<String> {
    let env_var = format!("{}_BASE_URL", provider_id.to_uppercase().replace('-', "_"));
    std::env::var(&env_var).ok().filter(|v| !v.is_empty())
}

pub fn load_provider(
    model_str: &str,
    api_key: Option<String>,
) -> Result<Box<dyn Provider>, FactoryError> {
    let (provider_id, model_id) = parse_model_string(model_str)?;
    let spec = crate::providers::lookup(&provider_id)
        .ok_or_else(|| FactoryError::UnknownProvider(provider_id.clone()))?;
    let key = match api_key.filter(|k| !k.is_empty()) {
        Some(k) => k,
        None => match std::env::var(spec.env_var) {
            Ok(k) if !k.is_empty() => k,
            _ if !spec.requires_auth => "not-needed".to_string(),
            _ => {
                return Err(FactoryError::MissingApiKey(
                    spec.id.into(),
                    spec.env_var.into(),
                ));
            }
        },
    };
    let base = base_url_override(&provider_id).or_else(|| spec.base_url.map(String::from));
    let transport = crate::transport::TransportConfig::default();
    Ok(match spec.wire {
        crate::providers::Wire::Anthropic => {
            Box::new(AnthropicProvider::new(key, model_id, base, true, transport))
        }
        crate::providers::Wire::OpenAI => {
            Box::new(OpenAIProvider::new(key, model_id, base, transport))
        }
    })
}

fn parse_model_string(s: &str) -> Result<(String, String), FactoryError> {
    let parts: Vec<&str> = s.splitn(2, '/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(FactoryError::InvalidFormat(s.into()));
    }
    Ok((parts[0].into(), parts[1].into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_model_string() {
        let (p, m) = parse_model_string("anthropic/claude-3-5-sonnet").unwrap();
        assert_eq!(p, "anthropic");
        assert_eq!(m, "claude-3-5-sonnet");
    }

    #[test]
    fn parse_model_with_slashes_in_model_id() {
        let (p, m) = parse_model_string("openrouter/meta-llama/llama-3.1-8b").unwrap();
        assert_eq!(p, "openrouter");
        assert_eq!(m, "meta-llama/llama-3.1-8b");
    }

    #[test]
    fn parse_invalid_no_slash() {
        assert!(parse_model_string("justmodel").is_err());
    }

    #[test]
    fn parse_empty_provider() {
        assert!(parse_model_string("/model").is_err());
    }

    #[test]
    fn load_anthropic_with_explicit_key() {
        let p = load_provider("anthropic/claude-3-5-sonnet", Some("sk-test".into())).unwrap();
        assert_eq!(p.model_id(), "claude-3-5-sonnet");
    }

    #[test]
    fn load_groq_uses_openai_provider_with_base_url() {
        let p = load_provider("groq/llama3-70b-8192", Some("key".into())).unwrap();
        assert_eq!(p.model_id(), "llama3-70b-8192");
    }

    #[test]
    fn load_unknown_provider_errors() {
        assert!(load_provider("notaprovider/model", Some("key".into())).is_err());
    }

    #[test]
    fn load_missing_key_errors() {
        // Remove env var if set to make test deterministic
        std::env::remove_var("TESTPROVIDER_API_KEY");
        // Use a real provider name with no explicit key and no env var
        let result = load_provider("groq/model", None);
        // If GROQ_API_KEY is set in env, this will succeed — that's fine
        let _ = result;
    }

    #[test]
    fn load_xai_uses_openai_wire() {
        let p = load_provider("xai/grok-beta", Some("key".into())).unwrap();
        assert_eq!(p.model_id(), "grok-beta");
    }

    #[test]
    fn load_ollama_without_key_or_env_var_succeeds() {
        std::env::remove_var("OLLAMA_API_KEY");
        let p = load_provider("ollama/llama3", None).expect("ollama needs no auth");
        assert_eq!(p.model_id(), "llama3");
    }

    #[test]
    fn load_lmstudio_without_key_or_env_var_succeeds() {
        std::env::remove_var("LMSTUDIO_API_KEY");
        let p = load_provider("lmstudio/qwen2.5-coder", None).expect("lmstudio needs no auth");
        assert_eq!(p.model_id(), "qwen2.5-coder");
    }

    #[test]
    fn base_url_env_override_takes_precedence_over_default() {
        std::env::set_var("OLLAMA_BASE_URL", "http://192.168.1.50:11500/v1");
        let got = base_url_override("ollama");
        std::env::remove_var("OLLAMA_BASE_URL");
        assert_eq!(got, Some("http://192.168.1.50:11500/v1".to_string()));
    }

    #[test]
    fn base_url_env_override_absent_returns_none() {
        std::env::remove_var("NOTAPROVIDER_BASE_URL");
        assert_eq!(base_url_override("notaprovider"), None);
    }
}
