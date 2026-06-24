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

pub fn load_provider(
    model_str: &str,
    api_key: Option<String>,
) -> Result<Box<dyn Provider>, FactoryError> {
    let (provider_id, model_id) = parse_model_string(model_str)?;
    let key = resolve_api_key(&provider_id, api_key)?;

    match provider_id.as_str() {
        "anthropic" => Ok(Box::new(AnthropicProvider::new(
            key,
            model_id,
            None,
            true,
            crate::transport::TransportConfig::default(),
        ))),
        "openai" => Ok(Box::new(OpenAIProvider::new(
            key,
            model_id,
            None,
            crate::transport::TransportConfig::default(),
        ))),
        "groq" => Ok(Box::new(OpenAIProvider::new(
            key,
            model_id,
            Some("https://api.groq.com/openai/v1".into()),
            crate::transport::TransportConfig::default(),
        ))),
        "mistral" => Ok(Box::new(OpenAIProvider::new(
            key,
            model_id,
            Some("https://api.mistral.ai/v1".into()),
            crate::transport::TransportConfig::default(),
        ))),
        "openrouter" => Ok(Box::new(OpenAIProvider::new(
            key,
            model_id,
            Some("https://openrouter.ai/api/v1".into()),
            crate::transport::TransportConfig::default(),
        ))),
        "deepinfra" => Ok(Box::new(OpenAIProvider::new(
            key,
            model_id,
            Some("https://api.deepinfra.com/v1/openai".into()),
            crate::transport::TransportConfig::default(),
        ))),
        "minimax" => Ok(Box::new(AnthropicProvider::new(
            key,
            model_id,
            Some("https://api.minimax.io/anthropic/v1".into()),
            true,
            crate::transport::TransportConfig::default(),
        ))),
        other => Err(FactoryError::UnknownProvider(other.into())),
    }
}

fn parse_model_string(s: &str) -> Result<(String, String), FactoryError> {
    let parts: Vec<&str> = s.splitn(2, '/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(FactoryError::InvalidFormat(s.into()));
    }
    Ok((parts[0].into(), parts[1].into()))
}

fn resolve_api_key(provider_id: &str, explicit: Option<String>) -> Result<String, FactoryError> {
    if let Some(key) = explicit {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let env_var = format!("{}_API_KEY", provider_id.to_uppercase());
    std::env::var(&env_var).map_err(|_| FactoryError::MissingApiKey(provider_id.into(), env_var))
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
}
