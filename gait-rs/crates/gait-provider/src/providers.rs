//! Static provider registry. Base URLs and API-key env vars live here and ONLY
//! here — the vendored models.dev snapshot is metadata-only and must never set
//! routing or auth. Adding an OpenAI-compatible provider is one row.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    OpenAI,
    CodexResponses,
    ResponsesPlan,
}

impl Wire {
    /// Every production SSE schema. Conformance tests match this list
    /// exhaustively so adding a wire requires an explicit fixture.
    pub const ALL: &'static [Self] = &[
        Self::Anthropic,
        Self::OpenAI,
        Self::CodexResponses,
        Self::ResponsesPlan,
    ];
}

/// How a provider authenticates. Kept separate from `Wire` because the
/// two are orthogonal: a Chat Completions provider can use either an
/// API key (`opencode`, `openai`, ...) or a subscription (`openai-codex`
/// uses `CodexResponses`, but in principle a subscription could ride on
/// any wire). Making this explicit lets the factory dispatch on auth
/// semantics instead of pattern-matching wire formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Bearer token in `Authorization` header, sourced from an env var
    /// (or explicit `-k`/credential store).
    ApiKey,
    /// OAuth/subscription credential sourced from `gait auth login`.
    /// There is no env var — callers must run the login flow.
    Subscription,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub wire: Wire,
    pub auth_method: AuthMethod,
    /// `None` = use the adapter's native default base URL.
    pub base_url: Option<&'static str>,
    /// API-key env var. `None` for Subscription providers (no env var —
    /// they require `gait auth login`). Explicit (not derived) because of
    /// cases like `togetherai` → `TOGETHER_API_KEY`.
    pub env_var: Option<&'static str>,
    /// Whether this provider needs a real API key. `false` for local
    /// runtimes (Ollama, LM Studio) that don't check the `Authorization`
    /// header — callers may proceed without a key or env var set.
    pub requires_auth: bool,
}

pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        wire: Wire::Anthropic,
        auth_method: AuthMethod::ApiKey,
        base_url: None,
        env_var: Some("ANTHROPIC_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        // This namespace is separate from `openai` because it uses plan usage,
        // not API-key billing.
        id: "openai-signin",
        wire: Wire::ResponsesPlan,
        auth_method: AuthMethod::Subscription,
        base_url: Some("https://api.openai.com/v1"),
        env_var: None,
        requires_auth: true,
    },
    ProviderSpec {
        id: "minimax",
        wire: Wire::Anthropic,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.minimax.io/anthropic/v1"),
        env_var: Some("MINIMAX_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "openai",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: None,
        env_var: Some("OPENAI_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        // ChatGPT subscription access uses the Codex Responses backend, not
        // the public API-key endpoint. It is intentionally a distinct model
        // namespace so `openai/...` remains API-key-only.
        id: "openai-codex",
        wire: Wire::CodexResponses,
        auth_method: AuthMethod::Subscription,
        base_url: Some("https://chatgpt.com/backend-api/codex"),
        env_var: None,
        requires_auth: true,
    },
    ProviderSpec {
        id: "groq",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.groq.com/openai/v1"),
        env_var: Some("GROQ_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "mistral",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.mistral.ai/v1"),
        env_var: Some("MISTRAL_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "openrouter",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://openrouter.ai/api/v1"),
        env_var: Some("OPENROUTER_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "deepinfra",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.deepinfra.com/v1/openai"),
        env_var: Some("DEEPINFRA_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "xai",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.x.ai/v1"),
        env_var: Some("XAI_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "cerebras",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.cerebras.ai/v1"),
        env_var: Some("CEREBRAS_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "togetherai",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.together.xyz/v1"),
        env_var: Some("TOGETHER_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "fireworks",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.fireworks.ai/inference/v1"),
        env_var: Some("FIREWORKS_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "perplexity",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://api.perplexity.ai"),
        env_var: Some("PERPLEXITY_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        // OpenCode Go subscription rides on the OpenAI Chat Completions
        // wire with a Bearer key from the OPENCODE_API_KEY env var. Models
        // are dynamic (ZEN_MODELS deployment secrets) — never vendored.
        id: "opencode",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("https://opencode.ai/zen/go/v1"),
        env_var: Some("OPENCODE_API_KEY"),
        requires_auth: true,
    },
    ProviderSpec {
        id: "ollama",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("http://localhost:11434/v1"),
        env_var: Some("OLLAMA_API_KEY"),
        requires_auth: false,
    },
    ProviderSpec {
        id: "lmstudio",
        wire: Wire::OpenAI,
        auth_method: AuthMethod::ApiKey,
        base_url: Some("http://localhost:1234/v1"),
        env_var: Some("LMSTUDIO_API_KEY"),
        requires_auth: false,
    },
];

pub fn lookup(provider_id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id == provider_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_existing_and_new_providers() {
        let get = |id: &str| PROVIDERS.iter().find(|p| p.id == id).expect("present");

        // existing parity
        assert_eq!(get("anthropic").wire, Wire::Anthropic);
        assert_eq!(get("anthropic").base_url, None);
        assert_eq!(get("anthropic").env_var, Some("ANTHROPIC_API_KEY"));
        assert_eq!(get("minimax").wire, Wire::Anthropic);
        assert_eq!(
            get("minimax").base_url,
            Some("https://api.minimax.io/anthropic/v1")
        );
        assert_eq!(get("groq").wire, Wire::OpenAI);
        assert_eq!(get("groq").base_url, Some("https://api.groq.com/openai/v1"));
        assert_eq!(get("openai").base_url, None);

        // the env-var divergence the table exists to capture
        assert_eq!(get("togetherai").env_var, Some("TOGETHER_API_KEY"));

        // new rows present
        for id in [
            "xai",
            "cerebras",
            "togetherai",
            "fireworks",
            "perplexity",
            "opencode",
        ] {
            assert!(PROVIDERS.iter().any(|p| p.id == id), "missing {id}");
        }
        // every ApiKey provider has a non-empty env var; subscriptions have None
        for p in PROVIDERS {
            match p.auth_method {
                AuthMethod::ApiKey => assert!(
                    p.env_var.map(|v| !v.is_empty()).unwrap_or(false),
                    "{} is ApiKey but has no env_var",
                    p.id
                ),
                AuthMethod::Subscription => assert!(
                    p.env_var.is_none(),
                    "{} is Subscription but has env_var {:?}",
                    p.id,
                    p.env_var
                ),
            }
        }
    }

    #[test]
    fn opencode_uses_openai_wire_with_zen_go_base_url() {
        let spec = lookup("opencode").expect("opencode provider present");
        assert_eq!(spec.wire, Wire::OpenAI);
        assert_eq!(spec.auth_method, AuthMethod::ApiKey);
        assert_eq!(spec.base_url, Some("https://opencode.ai/zen/go/v1"));
        assert_eq!(spec.env_var, Some("OPENCODE_API_KEY"));
        assert!(spec.requires_auth);
    }

    #[test]
    fn codex_is_subscription_with_no_env_var() {
        let spec = lookup("openai-codex").expect("codex provider present");
        assert_eq!(spec.wire, Wire::CodexResponses);
        assert_eq!(spec.auth_method, AuthMethod::Subscription);
        assert_eq!(spec.env_var, None);
    }

    #[test]
    fn providers_table_row() {
        let rows: Vec<_> = PROVIDERS
            .iter()
            .filter(|row| row.id == "openai-signin")
            .collect();
        assert_eq!(rows.len(), 1);
        let row = rows[0];
        assert_eq!(row.wire, Wire::ResponsesPlan);
        assert_eq!(row.auth_method, AuthMethod::Subscription);
        assert_eq!(row.base_url, Some("https://api.openai.com/v1"));
        assert_eq!(row.env_var, None);
        assert_ne!(row.id, "openai");
    }

    #[test]
    fn local_providers_present_and_no_auth_required() {
        let get = |id: &str| PROVIDERS.iter().find(|p| p.id == id).expect("present");

        let ollama = get("ollama");
        assert_eq!(ollama.wire, Wire::OpenAI);
        assert_eq!(ollama.base_url, Some("http://localhost:11434/v1"));
        assert!(!ollama.requires_auth);

        let lmstudio = get("lmstudio");
        assert_eq!(lmstudio.wire, Wire::OpenAI);
        assert_eq!(lmstudio.base_url, Some("http://localhost:1234/v1"));
        assert!(!lmstudio.requires_auth);

        // every pre-existing remote provider still requires a real key
        for id in [
            "anthropic",
            "minimax",
            "openai",
            "groq",
            "mistral",
            "openrouter",
            "deepinfra",
            "xai",
            "cerebras",
            "togetherai",
            "fireworks",
            "perplexity",
        ] {
            assert!(get(id).requires_auth, "{id} should require auth");
        }
    }
}
