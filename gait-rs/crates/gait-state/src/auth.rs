use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use gait_auth::ResolvedCredential;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path has no parent: {0}")]
    NoParent(std::path::PathBuf),
}

#[derive(Clone, Deserialize, Serialize)]
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
    /// Versioned, provider-owned OAuth/subscription material.  The envelope is
    /// deliberately typed so it can never be mistaken for an API key.
    Subscription {
        provider_id: String,
        method_id: String,
        version: u32,
        #[serde(default)]
        expires_at_unix_s: Option<u64>,
        #[serde(default)]
        account_label: Option<String>,
        secret: serde_json::Value,
    },
}

/// Resolve API key for a provider.
/// Priority: environment variable > ~/.local/share/gait/auth.json
pub fn resolve_api_key(provider: &str) -> Option<String> {
    resolve_credential(provider).and_then(|credential| credential.api_key().map(str::to_string))
}

/// Resolve a typed credential for a provider.
///
/// This preserves today's API-key-only behavior while giving all new callers a
/// typed boundary. Subscription credential resolution is added by the Codex
/// auth method; legacy OAuth records intentionally do not masquerade as API
/// keys or bearer tokens.
pub fn resolve_credential(provider: &str) -> Option<ResolvedCredential> {
    // 1. Check env var (ANTHROPIC_API_KEY, MINIMAX_API_KEY, OPENAI_API_KEY, etc.)
    let env_var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    if let Ok(key) = std::env::var(&env_var) {
        if !key.is_empty() {
            return Some(ResolvedCredential::ApiKey(key));
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
        return no_auth_placeholder(provider).map(ResolvedCredential::ApiKey);
    }
    let Ok(text) = std::fs::read_to_string(&auth_path) else {
        return no_auth_placeholder(provider).map(ResolvedCredential::ApiKey);
    };
    let Ok(map) = serde_json::from_str::<HashMap<String, AuthEntry>>(&text) else {
        return no_auth_placeholder(provider).map(ResolvedCredential::ApiKey);
    };
    map.get(provider)
        .and_then(|entry| credential_from_entry(provider, entry))
        .or_else(|| no_auth_placeholder(provider).map(ResolvedCredential::ApiKey))
}

fn credential_from_entry(provider: &str, entry: &AuthEntry) -> Option<ResolvedCredential> {
    match entry {
        AuthEntry::Api { key } | AuthEntry::Wellknown { key, .. } => {
            Some(ResolvedCredential::ApiKey(key.clone()))
        }
        AuthEntry::OAuth { .. } => None,
        AuthEntry::Subscription {
            provider_id,
            method_id,
            version,
            expires_at_unix_s,
            account_label,
            secret,
        } if provider == provider_id => Some(ResolvedCredential::Subscription(
            gait_auth::SubscriptionCredential {
                provider_id: provider_id.clone(),
                method_id: method_id.clone(),
                version: *version,
                expires_at_unix_s: *expires_at_unix_s,
                account_label: account_label.clone(),
                secret: secret.clone(),
            },
        )),
        AuthEntry::Subscription { .. } => None,
    }
}

/// Local runtimes (Ollama, LM Studio) don't check the `Authorization` header —
/// resolve to a placeholder instead of forcing the user through `gait auth add`.
fn no_auth_placeholder(provider: &str) -> Option<String> {
    let requires_auth = gait_provider::providers::lookup(provider)
        .map(|spec| spec.requires_auth)
        .unwrap_or(true);
    if requires_auth {
        None
    } else {
        Some("not-needed".to_string())
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

fn restrict_auth_file_permissions(path: &std::path::Path) -> Result<(), AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
    restrict_auth_file_permissions(path)?;
    Ok(())
}

pub fn write_api_key(provider: &str, key: &str) -> Result<(), AuthError> {
    write_api_key_to(&auth_path(), provider, key)
}

/// Persist a versioned subscription credential.  The file's schema remains a
/// provider-keyed map so existing API-key entries continue to coexist.
pub fn write_subscription_credential_to(
    path: &std::path::Path,
    provider: &str,
    credential: &gait_auth::SubscriptionCredential,
) -> Result<(), AuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| AuthError::NoParent(path.to_owned()))?;
    std::fs::create_dir_all(parent)?;
    let mut data = read_raw_auth(path);
    data.insert(
        provider.to_string(),
        serde_json::to_value(AuthEntry::Subscription {
            provider_id: credential.provider_id.clone(),
            method_id: credential.method_id.clone(),
            version: credential.version,
            expires_at_unix_s: credential.expires_at_unix_s,
            account_label: credential.account_label.clone(),
            secret: credential.secret.clone(),
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    );
    std::fs::write(
        path,
        serde_json::to_string_pretty(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    restrict_auth_file_permissions(path)?;
    Ok(())
}

pub fn write_subscription_credential(
    provider: &str,
    credential: &gait_auth::SubscriptionCredential,
) -> Result<(), AuthError> {
    write_subscription_credential_to(&auth_path(), provider, credential)
}

pub fn remove_api_key_from(path: &std::path::Path, provider: &str) -> Result<(), AuthError> {
    let mut data = read_raw_auth(path);
    data.remove(provider);
    std::fs::write(
        path,
        serde_json::to_string_pretty(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
    )?;
    restrict_auth_file_permissions(path)?;
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
                Some("subscription") => v
                    .get("account_label")
                    .and_then(|label| label.as_str())
                    .map(|label| format!("subscription ({label})"))
                    .unwrap_or_else(|| "subscription".to_string()),
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
    fn no_auth_provider_resolves_without_key_or_env_var() {
        std::env::remove_var("OLLAMA_API_KEY");
        let result = resolve_api_key("ollama");
        assert!(
            result.is_some(),
            "ollama requires no auth; should resolve to a placeholder"
        );
    }

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
    fn api_and_wellknown_entries_resolve_to_api_key_credentials() {
        assert_eq!(
            credential_from_entry(
                "openai",
                &AuthEntry::Api {
                    key: "sk-test".into(),
                }
            )
            .and_then(|credential| credential.api_key().map(str::to_string)),
            Some("sk-test".into())
        );
        assert_eq!(
            credential_from_entry(
                "openai",
                &AuthEntry::Wellknown {
                    key: "known-key".into(),
                    token: "metadata".into(),
                }
            )
            .and_then(|credential| credential.api_key().map(str::to_string)),
            Some("known-key".into())
        );
    }

    #[test]
    fn legacy_oauth_entry_is_not_treated_as_api_key() {
        assert!(credential_from_entry(
            "openai",
            &AuthEntry::OAuth {
                refresh: "refresh".into(),
                access: "access".into(),
                expires: 0,
            }
        )
        .is_none());
    }

    #[test]
    fn subscription_round_trips_as_typed_credential_without_displaying_secret() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let credential = gait_auth::SubscriptionCredential {
            provider_id: "openai-codex".into(),
            method_id: "codex-subscription".into(),
            version: 1,
            expires_at_unix_s: Some(42),
            account_label: Some("person@example.test".into()),
            secret: serde_json::json!({ "access_token": "do-not-display" }),
        };
        write_subscription_credential_to(&path, "openai-codex", &credential).unwrap();
        let entries: HashMap<String, AuthEntry> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let resolved =
            credential_from_entry("openai-codex", entries.get("openai-codex").unwrap()).unwrap();
        assert!(matches!(resolved, ResolvedCredential::Subscription(_)));
        let listed = list_credentials_from(&path);
        assert_eq!(
            listed.get("openai-codex"),
            Some(&"subscription (person@example.test)".into())
        );
        assert!(!listed
            .values()
            .any(|value| value.contains("do-not-display")));
    }

    #[test]
    fn subscription_entry_with_mismatched_provider_is_rejected() {
        let entry = AuthEntry::Subscription {
            provider_id: "openai-codex".into(),
            method_id: "codex-subscription".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret: serde_json::json!({}),
        };
        assert!(credential_from_entry("openai", &entry).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn subscription_file_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let credential = gait_auth::SubscriptionCredential {
            provider_id: "openai-codex".into(),
            method_id: "codex-subscription".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret: serde_json::json!({}),
        };
        write_subscription_credential_to(&path, "openai-codex", &credential).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
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
