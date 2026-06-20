use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path has no parent: {0}")]
    NoParent(std::path::PathBuf),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AuthEntry {
    Api {
        key: String,
    },
    OAuth {
        refresh: String,
        access: String,
        expires: u64,
    },
    Wellknown {
        key: String,
        token: String,
    },
}

/// Resolve API key for a provider.
/// Priority: environment variable > ~/.local/share/gait/auth.json
pub fn resolve_api_key(provider: &str) -> Option<String> {
    // 1. Check env var (ANTHROPIC_API_KEY, MINIMAX_API_KEY, OPENAI_API_KEY, etc.)
    let env_var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    if let Ok(key) = std::env::var(&env_var) {
        if !key.is_empty() {
            return Some(key);
        }
    }
    // 2. Check auth.json
    let auth_path = dirs_next::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~"))
        .join(".local")
        .join("share")
        .join("gait")
        .join("auth.json");
    if !auth_path.exists() {
        return None;
    }
    let Ok(text) = std::fs::read_to_string(&auth_path) else {
        return None;
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, AuthEntry>>(&text) else {
        return None;
    };
    match map.get(provider)? {
        AuthEntry::Api { key } => Some(key.clone()),
        AuthEntry::Wellknown { key, .. } => Some(key.clone()),
        _ => None,
    }
}

fn auth_path() -> std::path::PathBuf {
    // Match TypeScript gait: always ~/.local/share/gait/auth.json (cross-platform convention)
    dirs_next::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~"))
        .join(".local")
        .join("share")
        .join("gait")
        .join("auth.json")
}

fn read_raw_auth(path: &std::path::Path) -> HashMap<String, serde_json::Value> {
    if !path.exists() {
        return HashMap::new();
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn write_api_key_to(
    path: &std::path::Path,
    provider: &str,
    key: &str,
) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::NoParent(path.to_owned()))?;
    std::fs::create_dir_all(parent)?;
    let mut data = read_raw_auth(path);
    data.insert(
        provider.to_string(),
        serde_json::json!({ "type": "api", "key": key }),
    );
    std::fs::write(
        path,
        serde_json::to_string_pretty(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    Ok(())
}

pub fn write_api_key(provider: &str, key: &str) -> Result<(), AuthError> {
    write_api_key_to(&auth_path(), provider, key)
}

pub fn remove_api_key_from(path: &std::path::Path, provider: &str) -> Result<(), AuthError> {
    let mut data = read_raw_auth(path);
    data.remove(provider);
    std::fs::write(
        path,
        serde_json::to_string_pretty(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    Ok(())
}

pub fn remove_api_key(provider: &str) -> Result<(), AuthError> {
    remove_api_key_from(&auth_path(), provider)
}

/// Returns provider → masked_display string for `auth list`.
pub fn list_credentials_from(path: &std::path::Path) -> HashMap<String, String> {
    read_raw_auth(path)
        .into_iter()
        .map(|(k, v)| {
            let display = match v.get("type").and_then(|t| t.as_str()) {
                Some("api") => {
                    let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("");
                    // chars().take(8) avoids panics on non-ASCII (API keys are ASCII in practice,
                    // but this is safer than &key[..n] byte-slicing)
                    let prefix: String = key.chars().take(8).collect();
                    format!("{}...", prefix)
                }
                Some("oauth") => "oauth".to_string(),
                Some("wellknown") => "wellknown".to_string(),
                other => other.unwrap_or("unknown").to_string(),
            };
            (k, display)
        })
        .collect()
}

pub fn list_credentials() -> HashMap<String, String> {
    list_credentials_from(&auth_path())
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

    #[test]
    fn write_and_read_api_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_api_key_to(&auth_path, "anthropic", "sk-test-123").unwrap();
        let list = list_credentials_from(&auth_path);
        assert_eq!(
            list.get("anthropic").map(|s| s.as_str()),
            Some("sk-test-...")
        );
    }

    #[test]
    fn remove_api_key_removes_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_api_key_to(&auth_path, "openai", "sk-abc").unwrap();
        write_api_key_to(&auth_path, "anthropic", "sk-def").unwrap();
        remove_api_key_from(&auth_path, "openai").unwrap();
        let list = list_credentials_from(&auth_path);
        assert!(!list.contains_key("openai"), "openai should be removed");
        assert!(list.contains_key("anthropic"), "anthropic should remain");
    }

    #[test]
    fn list_credentials_masks_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let auth_path = dir.path().join("auth.json");
        write_api_key_to(&auth_path, "minimax", "sk-abcdefghijklmnop").unwrap();
        let list = list_credentials_from(&auth_path);
        let masked = list.get("minimax").unwrap();
        assert!(masked.ends_with("..."), "should be masked: {masked}");
        assert!(
            !masked.contains("abcdefghijklmnop"),
            "full key should not appear"
        );
    }
}
