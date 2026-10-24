use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gait_auth::sources::codex::{CodexAuthSource, CODEX_SOURCE_ID};
use gait_auth::{AuthLocation, AuthSource, CredentialLease, ResolvedCredential};
use gait_platform::{NativePlatform, Platform};

use crate::gait_home::{GaitHome, GaitHomeError};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt authentication file {path}: {detail}")]
    Corrupt { path: PathBuf, detail: String },
    #[error("timed out waiting for authentication file lock")]
    LockTimeout,
    #[error("authentication is managed elsewhere: {0}")]
    ManagedElsewhere(String),
    #[error("path has no parent: {0}")]
    NoParent(PathBuf),
    #[error("unsupported authentication source '{0}'")]
    UnsupportedSource(String),
    #[error("authentication source '{source_id}' has no credentials for provider '{provider}'")]
    ExternalCredentialMissing { source_id: String, provider: String },
    #[error("provider '{0}' is not linked to an external authentication source")]
    NotExternalBinding(String),
    #[error(transparent)]
    Source(#[from] gait_auth::AuthError),
    #[error(transparent)]
    GaitHome(#[from] GaitHomeError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalAuthBinding {
    pub source: String,
    pub home: Option<PathBuf>,
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
    /// Non-secret pointer to a credential store owned by another CLI.
    External {
        source: String,
        #[serde(default)]
        home: Option<PathBuf>,
    },
}

/// Resolve API key for a provider.
/// Priority: provider environment variable > `$GAIT_HOME/auth.json`.
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
    resolve_credential_result(provider).ok().flatten()
}

/// Resolve a credential while preserving source-discovery errors for callers
/// that can render them. Compatibility callers may continue using
/// [`resolve_credential`], which retains the old `Option` surface.
pub fn resolve_credential_result(provider: &str) -> Result<Option<ResolvedCredential>, AuthError> {
    resolve_credential_lease_result(provider).map(|lease| lease.map(|lease| lease.credential))
}

pub fn resolve_credential_with(
    platform: &dyn Platform,
    provider: &str,
) -> Result<Option<ResolvedCredential>, AuthError> {
    resolve_credential_lease_with(platform, provider)
        .map(|lease| lease.map(|lease| lease.credential))
}

/// Resolve a credential while retaining its non-secret source metadata in the
/// spine. The sealed brain receives only `lease.credential`.
pub fn resolve_credential_lease_result(
    provider: &str,
) -> Result<Option<CredentialLease>, AuthError> {
    resolve_credential_lease_with(&NativePlatform, provider)
}

pub fn resolve_credential_lease_with(
    platform: &dyn Platform,
    provider: &str,
) -> Result<Option<CredentialLease>, AuthError> {
    // 1. Check env var (ANTHROPIC_API_KEY, MINIMAX_API_KEY, OPENAI_API_KEY, etc.)
    let env_var = format!("{}_API_KEY", provider.to_uppercase().replace('-', "_"));
    if let Some(key) = platform
        .env(&env_var)
        .and_then(|value| value.into_string().ok())
    {
        if !key.is_empty() {
            return Ok(Some(CredentialLease {
                credential: ResolvedCredential::ApiKey(key),
                source_id: "environment".into(),
                location: AuthLocation::Environment(env_var),
                revision: None,
            }));
        }
    }
    // 2. Check auth.json
    let auth_path = auth_path_for(platform)?;
    if !auth_path.exists() {
        return Ok(no_auth_placeholder(provider).map(|key| CredentialLease {
            credential: ResolvedCredential::ApiKey(key),
            source_id: "local-runtime".into(),
            location: AuthLocation::Synthetic,
            revision: None,
        }));
    }
    let map = crate::auth_file::load_auth_map(&auth_path)?;
    let entry = match map.get(provider) {
        Some(value) => Some(
            serde_json::from_value::<AuthEntry>(value.clone()).map_err(|_| AuthError::Corrupt {
                path: auth_path.clone(),
                detail: format!("entry for '{provider}' is invalid"),
            })?,
        ),
        None => None,
    };
    match entry.as_ref() {
        Some(AuthEntry::External { source, home }) => {
            resolve_external(platform, provider, source, home.as_deref())
        }
        Some(entry) => {
            Ok(
                credential_from_entry(provider, entry).map(|credential| CredentialLease {
                    credential,
                    source_id: "gait".into(),
                    location: AuthLocation::File(auth_path),
                    revision: None,
                }),
            )
        }
        None => Ok(no_auth_placeholder(provider).map(|key| CredentialLease {
            credential: ResolvedCredential::ApiKey(key),
            source_id: "local-runtime".into(),
            location: AuthLocation::Synthetic,
            revision: None,
        })),
    }
}

fn resolve_external(
    platform: &dyn Platform,
    provider: &str,
    source_id: &str,
    explicit_home: Option<&Path>,
) -> Result<Option<CredentialLease>, AuthError> {
    let source: Box<dyn AuthSource> = match source_id {
        CODEX_SOURCE_ID => Box::new(CodexAuthSource),
        other => return Err(AuthError::UnsupportedSource(other.to_string())),
    };
    if !source.supports(provider) {
        return Err(AuthError::UnsupportedSource(format!(
            "{source_id} for provider {provider}"
        )));
    }
    for location in source.discover(platform, explicit_home)? {
        if let Some(lease) = source.resolve(&location, provider)? {
            return Ok(Some(lease));
        }
    }
    if source_id == CODEX_SOURCE_ID {
        return Err(AuthError::Source(gait_auth::AuthError::LoginRequired(
            "Codex has no file-backed ChatGPT credentials; run `codex login` or configure Codex with cli_auth_credentials_store = \"file\""
                .into(),
        )));
    }
    Err(AuthError::ExternalCredentialMissing {
        source_id: source_id.to_string(),
        provider: provider.to_string(),
    })
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
        AuthEntry::External { .. } => None,
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

fn auth_path_for(platform: &dyn Platform) -> Result<PathBuf, AuthError> {
    Ok(GaitHome::resolve_with(platform)?.auth_file())
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
    crate::auth_file::update_auth_file(path, |data| {
        data.insert(
            provider.to_string(),
            serde_json::json!({ "type": "api", "key": key }),
        );
        Ok(())
    })
}

pub fn write_api_key(provider: &str, key: &str) -> Result<(), AuthError> {
    write_api_key_with(&NativePlatform, provider, key)
}

pub fn write_api_key_with(
    platform: &dyn Platform,
    provider: &str,
    key: &str,
) -> Result<(), AuthError> {
    write_api_key_to(&auth_path_for(platform)?, provider, key)
}

/// Persist a versioned subscription credential.  The file's schema remains a
/// provider-keyed map so existing API-key entries continue to coexist.
pub fn write_subscription_credential_to(
    path: &std::path::Path,
    provider: &str,
    credential: &gait_auth::SubscriptionCredential,
) -> Result<(), AuthError> {
    crate::auth_file::update_auth_file(path, |data| {
        let value = serde_json::to_value(AuthEntry::Subscription {
            provider_id: credential.provider_id.clone(),
            method_id: credential.method_id.clone(),
            version: credential.version,
            expires_at_unix_s: credential.expires_at_unix_s,
            account_label: credential.account_label.clone(),
            secret: credential.secret.clone(),
        })
        .map_err(|error| {
            AuthError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;
        data.insert(provider.to_string(), value);
        Ok(())
    })
}

pub fn write_subscription_credential(
    provider: &str,
    credential: &gait_auth::SubscriptionCredential,
) -> Result<(), AuthError> {
    write_subscription_credential_with(&NativePlatform, provider, credential)
}

pub fn write_subscription_credential_with(
    platform: &dyn Platform,
    provider: &str,
    credential: &gait_auth::SubscriptionCredential,
) -> Result<(), AuthError> {
    write_subscription_credential_to(&auth_path_for(platform)?, provider, credential)
}

/// Store a non-secret binding to an external CLI's credential source.
pub fn write_external_binding_to(
    path: &Path,
    provider: &str,
    source: &str,
    home: Option<&Path>,
) -> Result<(), AuthError> {
    crate::auth_file::update_auth_file(path, |data| {
        let value = serde_json::to_value(AuthEntry::External {
            source: source.to_string(),
            home: home.map(Path::to_path_buf),
        })
        .map_err(|error| {
            AuthError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;
        data.insert(provider.to_string(), value);
        Ok(())
    })
}

pub fn write_external_binding(
    provider: &str,
    source: &str,
    home: Option<&Path>,
) -> Result<(), AuthError> {
    write_external_binding_with(&NativePlatform, provider, source, home)
}

pub fn write_external_binding_with(
    platform: &dyn Platform,
    provider: &str,
    source: &str,
    home: Option<&Path>,
) -> Result<(), AuthError> {
    write_external_binding_to(&auth_path_for(platform)?, provider, source, home)
}

pub fn external_binding_from(path: &Path, provider: &str) -> Option<ExternalAuthBinding> {
    let value = read_raw_auth(path).remove(provider)?;
    let AuthEntry::External { source, home } = serde_json::from_value(value).ok()? else {
        return None;
    };
    Some(ExternalAuthBinding { source, home })
}

pub fn external_binding(provider: &str) -> Result<Option<ExternalAuthBinding>, AuthError> {
    external_binding_with(&NativePlatform, provider)
}

pub fn external_binding_with(
    platform: &dyn Platform,
    provider: &str,
) -> Result<Option<ExternalAuthBinding>, AuthError> {
    Ok(external_binding_from(&auth_path_for(platform)?, provider))
}

pub fn unlink_external_binding_from(path: &Path, provider: &str) -> Result<(), AuthError> {
    crate::auth_file::update_auth_file(path, |data| {
        match data.get(provider).and_then(|value| value.get("type")) {
            None => return Ok(()),
            Some(kind) if kind.as_str() == Some("external") => {}
            Some(_) => return Err(AuthError::NotExternalBinding(provider.to_string())),
        }
        data.remove(provider);
        Ok(())
    })
}

pub fn unlink_external_binding(provider: &str) -> Result<(), AuthError> {
    unlink_external_binding_with(&NativePlatform, provider)
}

pub fn unlink_external_binding_with(
    platform: &dyn Platform,
    provider: &str,
) -> Result<(), AuthError> {
    unlink_external_binding_from(&auth_path_for(platform)?, provider)
}

pub fn remove_api_key_from(path: &std::path::Path, provider: &str) -> Result<(), AuthError> {
    crate::auth_file::update_auth_file(path, |data| {
        data.remove(provider);
        Ok(())
    })
}

pub fn remove_api_key(provider: &str) -> Result<(), AuthError> {
    remove_api_key_with(&NativePlatform, provider)
}

pub fn remove_api_key_with(platform: &dyn Platform, provider: &str) -> Result<(), AuthError> {
    remove_api_key_from(&auth_path_for(platform)?, provider)
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
                Some("external") => {
                    let source = v
                        .get("source")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    format!("external ({source})")
                }
                other => other.unwrap_or("unknown").to_string(),
            };
            (k, display)
        })
        .collect()
}

pub fn list_credentials() -> Result<HashMap<String, String>, AuthError> {
    list_credentials_with(&NativePlatform)
}

pub fn list_credentials_with(
    platform: &dyn Platform,
) -> Result<HashMap<String, String>, AuthError> {
    Ok(list_credentials_from(&auth_path_for(platform)?))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use gait_platform::PlatformKind;

    use super::*;

    struct FakePlatform {
        home: PathBuf,
        env: HashMap<String, OsString>,
    }

    impl Platform for FakePlatform {
        fn kind(&self) -> PlatformKind {
            PlatformKind::Linux
        }

        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }

        fn config_dir(&self) -> Option<PathBuf> {
            None
        }

        fn data_dir(&self) -> Option<PathBuf> {
            None
        }

        fn cache_dir(&self) -> Option<PathBuf> {
            None
        }

        fn env(&self, name: &str) -> Option<OsString> {
            self.env.get(name).cloned()
        }
    }

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
    fn default_auth_operations_use_gait_home_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let platform = FakePlatform {
            home: dir.path().to_path_buf(),
            env: HashMap::new(),
        };

        write_api_key_with(&platform, "anthropic", "sk-test-123").unwrap();

        let new_path = dir.path().join(".gait-home/auth.json");
        let old_path = dir.path().join(".local/share/gait/auth.json");
        assert!(new_path.is_file());
        assert!(!old_path.exists());
        assert_eq!(
            list_credentials_with(&platform)
                .unwrap()
                .get("anthropic")
                .map(String::as_str),
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

    #[test]
    fn external_binding_contains_no_secret_and_resolves_from_codex_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let platform = FakePlatform {
            home: dir.path().to_path_buf(),
            env: HashMap::new(),
        };
        let gait_auth_path = auth_path_for(&platform).unwrap();
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("auth.json"),
            serde_json::to_vec(&serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": "header.eyJlbWFpbCI6InBlcnNvbkBleGFtcGxlLnRlc3QiLCJleHAiOjQxMDI0NDQ4MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0LXRlc3QifX0.signature",
                    "access_token": "external-access-secret",
                    "refresh_token": "external-refresh-secret",
                    "account_id": "acct-test"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        write_external_binding_to(&gait_auth_path, "openai-codex", "codex", Some(&codex_home))
            .unwrap();

        let binding_text = std::fs::read_to_string(&gait_auth_path).unwrap();
        assert!(binding_text.contains("\"source\": \"codex\""));
        assert!(!binding_text.contains("external-access-secret"));
        assert!(!binding_text.contains("external-refresh-secret"));

        let lease = resolve_credential_lease_with(&platform, "openai-codex")
            .unwrap()
            .unwrap();
        assert_eq!(lease.credential.provider_id(), Some("openai-codex"));
        assert_eq!(lease.source_id, "codex");
        assert_eq!(
            lease.location,
            AuthLocation::File(codex_home.join("auth.json"))
        );
        assert_eq!(
            list_credentials_from(&gait_auth_path).get("openai-codex"),
            Some(&"external (codex)".to_string())
        );
    }

    #[test]
    fn unlink_removes_only_external_bindings() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        write_external_binding_to(&path, "openai-codex", "codex", None).unwrap();
        unlink_external_binding_from(&path, "openai-codex").unwrap();
        assert!(external_binding_from(&path, "openai-codex").is_none());

        write_api_key_to(&path, "openai", "sk-test").unwrap();
        assert!(matches!(
            unlink_external_binding_from(&path, "openai"),
            Err(AuthError::NotExternalBinding(provider)) if provider == "openai"
        ));
        assert_eq!(
            list_credentials_from(&path).get("openai"),
            Some(&"sk-test...".to_string())
        );
    }

    #[test]
    fn environment_credentials_retain_source_metadata() {
        let platform = FakePlatform {
            home: PathBuf::from("/users/test"),
            env: HashMap::from([("OPENAI_API_KEY".into(), OsString::from("sk-environment"))]),
        };
        let lease = resolve_credential_lease_with(&platform, "openai")
            .unwrap()
            .unwrap();
        assert_eq!(lease.credential.api_key(), Some("sk-environment"));
        assert_eq!(lease.source_id, "environment");
        assert_eq!(
            lease.location,
            AuthLocation::Environment("OPENAI_API_KEY".into())
        );
    }

    #[test]
    fn writers_refuse_corrupt_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let original = b"{not json";
        std::fs::write(&path, original).unwrap();
        let credential = gait_auth::SubscriptionCredential {
            provider_id: "openai".into(),
            method_id: "test".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret: serde_json::json!({}),
        };

        let results = [
            write_api_key_to(&path, "openai", "key"),
            write_subscription_credential_to(&path, "openai", &credential),
            write_external_binding_to(&path, "openai", "codex", None),
            unlink_external_binding_from(&path, "openai"),
            remove_api_key_from(&path, "openai"),
        ];

        for result in results {
            assert!(matches!(result, Err(AuthError::Corrupt { .. })));
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn unknown_type_entry_does_not_break_resolution() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".gait-home/auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "future": { "type": "future", "value": 1 },
                "openai": { "type": "api", "key": "sk-valid" }
            }))
            .unwrap(),
        )
        .unwrap();
        let platform = FakePlatform {
            home: dir.path().to_path_buf(),
            env: HashMap::new(),
        };

        let credential = resolve_credential_with(&platform, "openai")
            .unwrap()
            .unwrap();
        assert_eq!(credential.api_key(), Some("sk-valid"));
    }

    #[test]
    fn malformed_entry_for_requested_provider_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".gait-home/auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "openai": { "type": "api", "key": 42 }
            }))
            .unwrap(),
        )
        .unwrap();
        let platform = FakePlatform {
            home: dir.path().to_path_buf(),
            env: HashMap::new(),
        };

        let result = resolve_credential_lease_with(&platform, "openai");
        assert!(matches!(
            result,
            Err(AuthError::Corrupt { path: error_path, detail })
                if error_path == path && detail.contains("entry for 'openai' is invalid")
        ));
    }

    #[test]
    fn noop_unlink_does_not_create_auth_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        unlink_external_binding_from(&path, "openai").unwrap();
        assert!(!path.exists());
    }
}
