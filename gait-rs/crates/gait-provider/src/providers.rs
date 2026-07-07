//! Static provider registry. Base URLs and API-key env vars live here and ONLY
//! here — the vendored models.dev snapshot is metadata-only and must never set
//! routing or auth. Adding an OpenAI-compatible provider is one row.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    OpenAI,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub wire: Wire,
    /// `None` = use the adapter's native default base URL.
    pub base_url: Option<&'static str>,
    /// API-key env var. Explicit (not derived) because of cases like
    /// `togetherai` → `TOGETHER_API_KEY`.
    pub env_var: &'static str,
    /// Whether this provider needs a real API key. `false` for local
    /// runtimes (Ollama, LM Studio) that don't check the `Authorization`
    /// header — callers may proceed without a key or env var set.
    pub requires_auth: bool,
}

pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        wire: Wire::Anthropic,
        base_url: None,
        env_var: "ANTHROPIC_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "minimax",
        wire: Wire::Anthropic,
        base_url: Some("https://api.minimax.io/anthropic/v1"),
        env_var: "MINIMAX_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "openai",
        wire: Wire::OpenAI,
        base_url: None,
        env_var: "OPENAI_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "groq",
        wire: Wire::OpenAI,
        base_url: Some("https://api.groq.com/openai/v1"),
        env_var: "GROQ_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "mistral",
        wire: Wire::OpenAI,
        base_url: Some("https://api.mistral.ai/v1"),
        env_var: "MISTRAL_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "openrouter",
        wire: Wire::OpenAI,
        base_url: Some("https://openrouter.ai/api/v1"),
        env_var: "OPENROUTER_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "deepinfra",
        wire: Wire::OpenAI,
        base_url: Some("https://api.deepinfra.com/v1/openai"),
        env_var: "DEEPINFRA_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "xai",
        wire: Wire::OpenAI,
        base_url: Some("https://api.x.ai/v1"),
        env_var: "XAI_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "cerebras",
        wire: Wire::OpenAI,
        base_url: Some("https://api.cerebras.ai/v1"),
        env_var: "CEREBRAS_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "togetherai",
        wire: Wire::OpenAI,
        base_url: Some("https://api.together.xyz/v1"),
        env_var: "TOGETHER_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "fireworks",
        wire: Wire::OpenAI,
        base_url: Some("https://api.fireworks.ai/inference/v1"),
        env_var: "FIREWORKS_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "perplexity",
        wire: Wire::OpenAI,
        base_url: Some("https://api.perplexity.ai"),
        env_var: "PERPLEXITY_API_KEY",
        requires_auth: true,
    },
    ProviderSpec {
        id: "ollama",
        wire: Wire::OpenAI,
        base_url: Some("http://localhost:11434/v1"),
        env_var: "OLLAMA_API_KEY",
        requires_auth: false,
    },
    ProviderSpec {
        id: "lmstudio",
        wire: Wire::OpenAI,
        base_url: Some("http://localhost:1234/v1"),
        env_var: "LMSTUDIO_API_KEY",
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
        assert_eq!(get("anthropic").env_var, "ANTHROPIC_API_KEY");
        assert_eq!(get("minimax").wire, Wire::Anthropic);
        assert_eq!(
            get("minimax").base_url,
            Some("https://api.minimax.io/anthropic/v1")
        );
        assert_eq!(get("groq").wire, Wire::OpenAI);
        assert_eq!(get("groq").base_url, Some("https://api.groq.com/openai/v1"));
        assert_eq!(get("openai").base_url, None);

        // the env-var divergence the table exists to capture
        assert_eq!(get("togetherai").env_var, "TOGETHER_API_KEY");

        // new rows present
        for id in ["xai", "cerebras", "togetherai", "fireworks", "perplexity"] {
            assert!(PROVIDERS.iter().any(|p| p.id == id), "missing {id}");
        }
        // every spec has a non-empty env var
        assert!(PROVIDERS.iter().all(|p| !p.env_var.is_empty()));
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
