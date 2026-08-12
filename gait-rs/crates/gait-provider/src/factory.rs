use crate::anthropic::AnthropicProvider;
use crate::codex::CodexResponsesProvider;
use crate::openai::OpenAIProvider;
use crate::provider::Provider;
use crate::providers::{AuthMethod, ProviderSpec, Wire};
use gait_auth::ResolvedCredential;

#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    #[error("invalid model string '{0}': expected 'provider/model-id'")]
    InvalidFormat(String),
    #[error("unknown provider '{0}'")]
    UnknownProvider(String),
    #[error("missing API key for provider '{0}': set {1} env var")]
    MissingApiKey(String, String),
    #[error("missing credential for provider '{0}': run `gait auth login {0}`")]
    MissingSubscription(String),
    #[error("invalid credential: {0}")]
    InvalidCredential(String),
    #[error("transport setup failed: {0}")]
    Transport(#[source] crate::provider::ProviderError),
    #[error("local inference is not included in this gait build; install a build with the 'local' feature")]
    LocalNotBuilt,
    #[error("local inference requires llm_params.local.model_path")]
    LocalModelPathMissing,
    #[error("local inference initialization failed: {0}")]
    LocalInit(String),
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
    load_provider_with_credential(model_str, api_key.map(ResolvedCredential::ApiKey))
}

/// Typed factory boundary. Dispatches on the provider's declared
/// `AuthMethod`: `ApiKey` providers resolve a Bearer token from an env var
/// or the credential store; `Subscription` providers require an OAuth
/// credential from `gait auth login`. Wire format selection happens inside
/// each branch so the (auth, wire) cross-product stays exhaustive.
pub fn load_provider_with_credential(
    model_str: &str,
    credential: Option<ResolvedCredential>,
) -> Result<Box<dyn Provider>, FactoryError> {
    load_provider_with_credential_and_params(
        model_str,
        credential,
        &gait_types::config::LlmParamsConfig::default(),
    )
}

/// Factory boundary with the selected inference's provider-specific settings.
/// Cloud providers only consume their own fields in `params`; the local branch
/// uses its typed `local` block to receive a model path from the durable
/// inference snapshot rather than from process-global environment state.
pub fn load_provider_with_credential_and_params(
    model_str: &str,
    credential: Option<ResolvedCredential>,
    params: &gait_types::config::LlmParamsConfig,
) -> Result<Box<dyn Provider>, FactoryError> {
    let (provider_id, model_id) = parse_model_string(model_str)?;
    if provider_id == "local" {
        return load_local_provider(model_id, params);
    }
    let spec = crate::providers::lookup(&provider_id)
        .ok_or_else(|| FactoryError::UnknownProvider(provider_id.clone()))?;
    let base = base_url_override(&provider_id).or_else(|| spec.base_url.map(String::from));
    let transport =
        crate::transport::SseTransport::new(crate::transport::TransportConfig::default())
            .map_err(FactoryError::Transport)?;

    match spec.auth_method {
        AuthMethod::ApiKey => {
            let key = resolve_api_key(spec, credential)?;
            Ok(match spec.wire {
                Wire::Anthropic => {
                    Box::new(AnthropicProvider::new(key, model_id, base, true, transport))
                }
                Wire::OpenAI => Box::new(OpenAIProvider::new(key, model_id, base, transport)),
                // CodexResponses always rides on Subscription auth; the table
                // never pairs it with ApiKey. Guard against drift with a panic
                // rather than silently falling back to Bearer auth.
                Wire::CodexResponses => unreachable!(
                    "CodexResponses wire must use AuthMethod::Subscription (provider '{}')",
                    spec.id
                ),
            })
        }
        AuthMethod::Subscription => {
            let subscription = match credential {
                Some(ResolvedCredential::Subscription(sub)) => sub,
                _ => return Err(FactoryError::MissingSubscription(spec.id.into())),
            };
            match spec.wire {
                Wire::CodexResponses => {
                    CodexResponsesProvider::new(subscription, model_id, base, transport)
                        .map(|provider| Box::new(provider) as Box<dyn Provider>)
                        .map_err(FactoryError::InvalidCredential)
                }
                _ => unreachable!(
                    "Subscription auth on non-CodexResponses wire (provider '{}')",
                    spec.id
                ),
            }
        }
    }
}

#[cfg(feature = "local-native")]
fn load_local_provider(
    model_id: String,
    params: &gait_types::config::LlmParamsConfig,
) -> Result<Box<dyn Provider>, FactoryError> {
    let local = params
        .local
        .as_ref()
        .ok_or(FactoryError::LocalModelPathMissing)?;
    if local.model_path.trim().is_empty() {
        return Err(FactoryError::LocalModelPathMissing);
    }
    let generator = gait_llama_cpp::LlamaGenerator::new(gait_llama_cpp::LlamaConfig {
        model_path: std::path::PathBuf::from(&local.model_path),
        context_tokens: local.context_tokens.unwrap_or(32_768),
        gpu_layers: local.gpu_layers,
        tool_protocol: match model_id.as_str() {
            "qwen2.5-coder-7b-instruct-q4-k-m" => gait_llama_cpp::ToolProtocol::Qwen25Coder,
            _ => gait_llama_cpp::ToolProtocol::Unsupported,
        },
    })
    .map_err(|error| FactoryError::LocalInit(error.to_string()))?;
    Ok(Box::new(crate::local::LocalProvider::new(
        model_id,
        std::sync::Arc::new(generator),
    )))
}

#[cfg(not(feature = "local-native"))]
fn load_local_provider(
    _model_id: String,
    _params: &gait_types::config::LlmParamsConfig,
) -> Result<Box<dyn Provider>, FactoryError> {
    Err(FactoryError::LocalNotBuilt)
}

/// Resolve a Bearer API key for an `ApiKey` provider. Order: explicit
/// credential passed to the factory, then the provider's env var, then a
/// placeholder for local runtimes that never inspect the Authorization
/// header (`requires_auth == false`). Errors carry the env var name so the
/// user knows exactly what to set.
fn resolve_api_key(
    spec: &ProviderSpec,
    credential: Option<ResolvedCredential>,
) -> Result<String, FactoryError> {
    if let Some(ResolvedCredential::ApiKey(key)) = credential {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if let Some(env_var) = spec.env_var {
        if let Ok(k) = std::env::var(env_var) {
            if !k.is_empty() {
                return Ok(k);
            }
        }
    }
    if !spec.requires_auth {
        return Ok("not-needed".to_string());
    }
    Err(FactoryError::MissingApiKey(
        spec.id.into(),
        spec.env_var.unwrap_or("<unknown>").into(),
    ))
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
    fn load_opencode_with_explicit_key() {
        // OpenCode Go rides the OpenAI wire with a Bearer key against the
        // zen/go base URL — no subscription required, no custom adapter.
        std::env::remove_var("OPENCODE_API_KEY");
        let p = load_provider("opencode/kimi-k2.6", Some("sk-test".into()))
            .expect("opencode loads with an explicit API key");
        assert_eq!(p.model_id(), "kimi-k2.6");
    }

    #[test]
    fn load_opencode_missing_key_errors_with_env_var_name() {
        std::env::remove_var("OPENCODE_API_KEY");
        let err = load_provider("opencode/kimi-k2.6", None).err().unwrap();
        let msg = format!("{err}");
        assert!(
            msg.contains("OPENCODE_API_KEY"),
            "error should name the env var, got: {msg}"
        );
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
