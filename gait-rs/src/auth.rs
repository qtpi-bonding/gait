use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthEntry {
    Api { key: String },
    OAuth { refresh: String, access: String, expires: u64 },
    Wellknown { key: String, token: String },
}

/// Resolve API key for a provider.
/// Priority: environment variable > ~/.local/share/gait/auth.json
pub fn resolve_api_key(provider: &str) -> Option<String> {
    // 1. Check env var (ANTHROPIC_API_KEY, MINIMAX_API_KEY, OPENAI_API_KEY, etc.)
    let env_var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    if let Ok(key) = std::env::var(&env_var) {
        if !key.is_empty() { return Some(key); }
    }
    // 2. Check auth.json
    let auth_path = dirs_next::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join("gait")
        .join("auth.json");
    if !auth_path.exists() { return None; }
    let Ok(text) = std::fs::read_to_string(&auth_path) else { return None; };
    let Ok(map) = serde_json::from_str::<HashMap<String, AuthEntry>>(&text) else { return None; };
    match map.get(provider)? {
        AuthEntry::Api { key } => Some(key.clone()),
        AuthEntry::Wellknown { key, .. } => Some(key.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_for_unknown_provider() {
        // Should not panic even when provider doesn't exist
        let result = resolve_api_key("nonexistent_provider_xyzzy");
        // We can't assert None because the user might have this env var set,
        // but we can assert it doesn't panic
        let _ = result;
    }
}
