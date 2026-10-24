//! Provider-neutral authentication contracts.
//!
//! [`AuthMethod`] owns a credential method's interactive lifecycle; model
//! providers own inference transport.  The first implementation, [`ApiKeyAuth`],
//! preserves gait's existing API-key behavior. Subscription methods such as the
//! future Codex adapter can share this boundary without making their backend
//! request protocol part of authentication.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod codex;
pub mod signin;
pub mod source;
pub mod sources;
pub mod store;
#[cfg(any(feature = "testing", test))]
pub mod testing;

pub use source::{AuthLocation, AuthSource, CredentialLease};

/// A credential resolved for a specific model provider.
///
/// Intentionally does not implement `Debug`: callers must not accidentally log
/// API keys or subscription tokens.
#[derive(Clone, PartialEq, Eq)]
pub enum ResolvedCredential {
    ApiKey(String),
    Subscription(SubscriptionCredential),
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
            Self::Subscription(credential) => f
                .debug_struct("Subscription")
                .field("provider_id", &credential.provider_id)
                .field("method_id", &credential.method_id)
                .field("version", &credential.version)
                .field("expires_at_unix_s", &credential.expires_at_unix_s)
                .field("account_label", &credential.account_label)
                .field("secret", &"<redacted>")
                .finish(),
        }
    }
}

impl ResolvedCredential {
    pub fn api_key(&self) -> Option<&str> {
        match self {
            Self::ApiKey(key) => Some(key),
            Self::Subscription(_) => None,
        }
    }

    pub fn provider_id(&self) -> Option<&str> {
        match self {
            Self::ApiKey(_) => None,
            Self::Subscription(credential) => Some(&credential.provider_id),
        }
    }
}

/// Provider-private subscription credential material plus safe account metadata.
///
/// `secret` is stored only by the credential store and is never suitable for
/// logs, session events, or process environments.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionCredential {
    pub provider_id: String,
    pub method_id: String,
    pub version: u32,
    pub expires_at_unix_s: Option<u64>,
    pub account_label: Option<String>,
    pub secret: serde_json::Value,
}

/// User-facing, non-secret state rendered by CLI/REPL frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub provider_id: String,
    pub method_id: String,
    pub display: String,
    pub expires_at_unix_s: Option<u64>,
    pub needs_login: bool,
}

/// A frontend-renderable login step. The frontend owns terminal/browser UX;
/// implementations own protocol details and credential validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginFlow {
    ApiKeyPrompt {
        label: String,
    },
    Browser {
        authorization_url: String,
    },
    DeviceCode {
        verification_url: String,
        user_code: String,
        expires_in_seconds: u64,
    },
    ManualCode {
        instructions: String,
    },
}

/// Data supplied by a frontend to finish an initiated login flow.
///
/// This intentionally does not implement `Debug` because it may carry an API
/// key, authorization code, or provider-private callback payload.
#[derive(Clone, PartialEq, Eq)]
pub enum LoginCompletion {
    ApiKey(String),
    AuthorizationCode(String),
    DeviceCodeApproved,
    ProviderPayload(serde_json::Value),
    CallbackUrl(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("provider id is required")]
    MissingProviderId,
    #[error("API key is required")]
    MissingApiKey,
    #[error("unsupported login completion for auth method '{0}'")]
    UnsupportedCompletion(&'static str),
    #[error("credential is not compatible with auth method '{0}'")]
    IncompatibleCredential(&'static str),
    #[error("authentication requires login again: {0}")]
    LoginRequired(String),
    #[error("authentication failed: {0}")]
    Other(String),
    #[error("authentication temporarily unavailable: {0}")]
    Retryable(String),
}

pub struct AuthContext<'a> {
    pub store: &'a dyn store::CredentialStore,
}

pub struct LogoutReport {
    pub revoked: bool,
}

/// A credential method's lifecycle, independent from the provider's inference
/// transport. Implementations may use API keys, OAuth, device codes, or a
/// future provider-specific subscription protocol.
#[async_trait]
pub trait AuthMethod: Send + Sync {
    fn method_id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;

    async fn begin_login(&self, provider_id: &str) -> Result<LoginFlow, AuthError>;

    async fn complete_login(
        &self,
        provider_id: &str,
        completion: LoginCompletion,
    ) -> Result<ResolvedCredential, AuthError>;

    async fn refresh(
        &self,
        credential: ResolvedCredential,
    ) -> Result<ResolvedCredential, AuthError>;

    async fn logout(&self, credential: &ResolvedCredential) -> Result<(), AuthError>;

    async fn begin_login_with(
        &self,
        provider_id: &str,
        _ctx: &AuthContext<'_>,
    ) -> Result<LoginFlow, AuthError> {
        self.begin_login(provider_id).await
    }

    async fn complete_login_with(
        &self,
        provider_id: &str,
        completion: LoginCompletion,
        _ctx: &AuthContext<'_>,
    ) -> Result<ResolvedCredential, AuthError> {
        self.complete_login(provider_id, completion).await
    }

    async fn ensure_fresh(
        &self,
        _provider_id: &str,
        _ctx: &AuthContext<'_>,
    ) -> Result<ResolvedCredential, AuthError> {
        Err(AuthError::UnsupportedCompletion(self.method_id()))
    }

    async fn logout_with(
        &self,
        _provider_id: &str,
        _forget: bool,
        _ctx: &AuthContext<'_>,
    ) -> Result<LogoutReport, AuthError> {
        Err(AuthError::UnsupportedCompletion(self.method_id()))
    }
}

#[allow(dead_code)]
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        difference |= a.get(index).copied().unwrap_or(0) as usize
            ^ b.get(index).copied().unwrap_or(0) as usize;
    }
    difference == 0
}

/// The existing non-interactive API-key credential mechanism, expressed through
/// [`AuthMethod`] so callers can share one typed authentication boundary.
#[derive(Debug, Default, Clone, Copy)]
pub struct ApiKeyAuth;

#[async_trait]
impl AuthMethod for ApiKeyAuth {
    fn method_id(&self) -> &'static str {
        "api-key"
    }

    fn display_name(&self) -> &'static str {
        "API key"
    }

    async fn begin_login(&self, provider_id: &str) -> Result<LoginFlow, AuthError> {
        if provider_id.is_empty() {
            return Err(AuthError::MissingProviderId);
        }
        Ok(LoginFlow::ApiKeyPrompt {
            label: format!("{provider_id} API key"),
        })
    }

    async fn complete_login(
        &self,
        provider_id: &str,
        completion: LoginCompletion,
    ) -> Result<ResolvedCredential, AuthError> {
        if provider_id.is_empty() {
            return Err(AuthError::MissingProviderId);
        }
        match completion {
            LoginCompletion::ApiKey(key) if key.trim().is_empty() => Err(AuthError::MissingApiKey),
            LoginCompletion::ApiKey(key) => Ok(ResolvedCredential::ApiKey(key)),
            _ => Err(AuthError::UnsupportedCompletion(self.method_id())),
        }
    }

    async fn refresh(
        &self,
        credential: ResolvedCredential,
    ) -> Result<ResolvedCredential, AuthError> {
        match credential {
            ResolvedCredential::ApiKey(_) => Ok(credential),
            ResolvedCredential::Subscription(_) => {
                Err(AuthError::IncompatibleCredential(self.method_id()))
            }
        }
    }

    async fn logout(&self, credential: &ResolvedCredential) -> Result<(), AuthError> {
        match credential {
            ResolvedCredential::ApiKey(_) => Ok(()),
            ResolvedCredential::Subscription(_) => {
                Err(AuthError::IncompatibleCredential(self.method_id()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults_report_unsupported_for_api_key_method() {
        let store = crate::testing::MemoryStore::new();
        let context = AuthContext { store: &store };
        let flow = ApiKeyAuth
            .begin_login_with("openai", &context)
            .await
            .unwrap();
        assert_eq!(
            flow,
            LoginFlow::ApiKeyPrompt {
                label: "openai API key".into()
            }
        );
        let error = ApiKeyAuth
            .ensure_fresh("openai", &context)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::UnsupportedCompletion("api-key")));
    }

    #[test]
    fn ct_eq_cases() {
        assert!(ct_eq(b"same", b"same"));
        assert!(!ct_eq(b"same", b"sane"));
        assert!(!ct_eq(b"same", b"same!"));
    }

    #[tokio::test]
    async fn api_key_auth_round_trips_without_refreshing() {
        let auth = ApiKeyAuth;
        let credential = auth
            .complete_login("openai", LoginCompletion::ApiKey("sk-test".into()))
            .await
            .unwrap();

        assert_eq!(credential.api_key(), Some("sk-test"));
        assert_eq!(auth.refresh(credential.clone()).await.unwrap(), credential);
        assert!(auth.logout(&credential).await.is_ok());
    }

    #[tokio::test]
    async fn api_key_auth_only_accepts_api_key_completion() {
        let err = ApiKeyAuth
            .complete_login("openai", LoginCompletion::DeviceCodeApproved)
            .await
            .unwrap_err();

        assert!(matches!(err, AuthError::UnsupportedCompletion("api-key")));
    }
}
