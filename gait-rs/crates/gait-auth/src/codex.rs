//! OpenAI Codex subscription authentication.
//!
//! Derived from OpenAI Codex commit 315195492c80fdade38e917c18f9584efd599304,
//! Apache-2.0: `codex-rs/login/src/{device_code_auth,pkce,server,token_data}.rs`
//! and `codex-rs/login/src/auth/{manager,revoke}.rs`. See
//! `gait-rs/THIRD_PARTY_NOTICES.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    AuthError, AuthMethod, LoginCompletion, LoginFlow, ResolvedCredential, SubscriptionCredential,
};

pub const CODEX_PROVIDER_ID: &str = "openai-codex";
const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct CodexSubscriptionAuth {
    issuer: String,
    client_id: String,
    http: reqwest::Client,
    pending_device_login: Arc<Mutex<Option<PendingDeviceLogin>>>,
}

impl Default for CodexSubscriptionAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexSubscriptionAuth {
    pub fn new() -> Self {
        Self {
            issuer: ISSUER.to_string(),
            client_id: CLIENT_ID.to_string(),
            http: reqwest::Client::new(),
            pending_device_login: Arc::new(Mutex::new(None)),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.issuer.trim_end_matches('/'), path)
    }

    async fn request_device_login(&self) -> Result<LoginFlow, AuthError> {
        let response = self
            .http
            .post(self.endpoint("/api/accounts/deviceauth/usercode"))
            .json(&DeviceCodeRequest {
                client_id: self.client_id.clone(),
            })
            .send()
            .await
            .map_err(map_reqwest)?;
        if !response.status().is_success() {
            return Err(AuthError::Other(format!(
                "Codex device login request failed with status {}",
                response.status()
            )));
        }
        let response: DeviceCodeResponse = response.json().await.map_err(map_reqwest)?;
        let interval = response.interval.trim().parse::<u64>().map_err(|_| {
            AuthError::Other("Codex device login returned an invalid polling interval".into())
        })?;
        *self.pending_device_login.lock().await = Some(PendingDeviceLogin {
            device_auth_id: response.device_auth_id,
            user_code: response.user_code.clone(),
            interval,
        });
        Ok(LoginFlow::DeviceCode {
            verification_url: self.endpoint("/codex/device"),
            user_code: response.user_code,
            expires_in_seconds: DEVICE_LOGIN_TIMEOUT.as_secs(),
        })
    }

    async fn complete_device_login(&self) -> Result<ResolvedCredential, AuthError> {
        let pending = self
            .pending_device_login
            .lock()
            .await
            .take()
            .ok_or_else(|| {
                AuthError::LoginRequired("start Codex login before completing it".into())
            })?;
        let started = Instant::now();
        let code = loop {
            let response = self
                .http
                .post(self.endpoint("/api/accounts/deviceauth/token"))
                .json(&DeviceTokenRequest {
                    device_auth_id: pending.device_auth_id.clone(),
                    user_code: pending.user_code.clone(),
                })
                .send()
                .await
                .map_err(map_reqwest)?;
            if response.status().is_success() {
                break response
                    .json::<DeviceTokenResponse>()
                    .await
                    .map_err(map_reqwest)?;
            }
            if !matches!(response.status().as_u16(), 403 | 404) {
                return Err(AuthError::Other(format!(
                    "Codex device login failed with status {}",
                    response.status()
                )));
            }
            if started.elapsed() >= DEVICE_LOGIN_TIMEOUT {
                return Err(AuthError::LoginRequired(
                    "Codex device login timed out; start login again".into(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(pending.interval)).await;
        };
        let tokens = self
            .exchange_code(
                &code.authorization_code,
                &code.code_verifier,
                &self.endpoint("/deviceauth/callback"),
            )
            .await?;
        credential_from_tokens(tokens)
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<CodexTokens, AuthError> {
        let response = self
            .http
            .post(self.endpoint("/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", self.client_id.as_str()),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .map_err(map_reqwest)?;
        if !response.status().is_success() {
            return Err(AuthError::Other(format!(
                "Codex token exchange failed with status {}",
                response.status()
            )));
        }
        response.json().await.map_err(map_reqwest)
    }

    async fn refresh_tokens(&self, tokens: CodexTokens) -> Result<CodexTokens, AuthError> {
        let response = self
            .http
            .post(self.endpoint("/oauth/token"))
            .json(&RefreshRequest {
                client_id: self.client_id.clone(),
                grant_type: "refresh_token",
                refresh_token: tokens.refresh_token.clone(),
            })
            .send()
            .await
            .map_err(map_reqwest)?;
        if !response.status().is_success() {
            return Err(AuthError::LoginRequired(
                "Codex subscription credentials expired; run `gait auth login openai-codex`".into(),
            ));
        }
        let refreshed: RefreshResponse = response.json().await.map_err(map_reqwest)?;
        Ok(CodexTokens {
            id_token: refreshed.id_token.unwrap_or(tokens.id_token),
            access_token: refreshed.access_token.unwrap_or(tokens.access_token),
            refresh_token: refreshed.refresh_token.unwrap_or(tokens.refresh_token),
            account_id: tokens.account_id,
        })
    }
}

#[async_trait]
impl AuthMethod for CodexSubscriptionAuth {
    fn method_id(&self) -> &'static str {
        "codex-subscription"
    }

    fn display_name(&self) -> &'static str {
        "ChatGPT / Codex subscription"
    }

    async fn begin_login(&self, provider_id: &str) -> Result<LoginFlow, AuthError> {
        if provider_id != CODEX_PROVIDER_ID {
            return Err(AuthError::IncompatibleCredential(self.method_id()));
        }
        self.request_device_login().await
    }

    async fn complete_login(
        &self,
        provider_id: &str,
        completion: LoginCompletion,
    ) -> Result<ResolvedCredential, AuthError> {
        if provider_id != CODEX_PROVIDER_ID {
            return Err(AuthError::IncompatibleCredential(self.method_id()));
        }
        match completion {
            LoginCompletion::DeviceCodeApproved => self.complete_device_login().await,
            _ => Err(AuthError::UnsupportedCompletion(self.method_id())),
        }
    }

    async fn refresh(
        &self,
        credential: ResolvedCredential,
    ) -> Result<ResolvedCredential, AuthError> {
        let tokens = tokens_from_credential(&credential)?;
        credential_from_tokens(self.refresh_tokens(tokens).await?)
    }

    async fn logout(&self, credential: &ResolvedCredential) -> Result<(), AuthError> {
        let tokens = tokens_from_credential(credential)?;
        let _ = self
            .http
            .post(self.endpoint("/oauth/revoke"))
            .json(&RevokeRequest {
                token: tokens.refresh_token,
                token_type_hint: "refresh_token",
                client_id: self.client_id.clone(),
            })
            .send()
            .await;
        Ok(())
    }
}

/// Returns the subscription token and account routing metadata for the Codex
/// Responses provider. Never serialize or render the returned value.
pub fn credential_details(
    credential: &ResolvedCredential,
) -> Result<CodexCredentialDetails, AuthError> {
    let tokens = tokens_from_credential(credential)?;
    let claims = parse_id_token_claims(&tokens.id_token)?;
    Ok(CodexCredentialDetails {
        access_token: tokens.access_token,
        account_id: claims.account_id.or(tokens.account_id),
        is_fedramp_account: claims.is_fedramp_account,
    })
}

pub struct CodexCredentialDetails {
    pub access_token: String,
    pub account_id: Option<String>,
    pub is_fedramp_account: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct CodexTokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Clone, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    interval: String,
}

#[derive(Serialize)]
struct DeviceCodeRequest {
    client_id: String,
}

struct PendingDeviceLogin {
    device_auth_id: String,
    user_code: String,
    interval: u64,
}

#[derive(Serialize)]
struct DeviceTokenRequest {
    device_auth_id: String,
    user_code: String,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Serialize)]
struct RefreshRequest {
    client_id: String,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Serialize)]
struct RevokeRequest {
    token: String,
    token_type_hint: &'static str,
    client_id: String,
}

#[derive(Default)]
struct IdTokenClaims {
    account_id: Option<String>,
    is_fedramp_account: bool,
    email: Option<String>,
    expires_at_unix_s: Option<u64>,
}

fn credential_from_tokens(tokens: CodexTokens) -> Result<ResolvedCredential, AuthError> {
    let claims = parse_id_token_claims(&tokens.id_token)?;
    let account_id = claims.account_id.or_else(|| tokens.account_id.clone());
    Ok(ResolvedCredential::Subscription(SubscriptionCredential {
        provider_id: CODEX_PROVIDER_ID.into(),
        method_id: "codex-subscription".into(),
        version: 1,
        expires_at_unix_s: claims.expires_at_unix_s,
        account_label: claims.email.or(account_id),
        secret: serde_json::to_value(tokens)
            .map_err(|error| AuthError::Other(error.to_string()))?,
    }))
}

pub(crate) fn credential_from_external_tokens(
    id_token: String,
    access_token: String,
    refresh_token: String,
    account_id: Option<String>,
) -> Result<ResolvedCredential, AuthError> {
    credential_from_tokens(CodexTokens {
        id_token,
        access_token,
        refresh_token,
        account_id,
    })
}

fn tokens_from_credential(credential: &ResolvedCredential) -> Result<CodexTokens, AuthError> {
    let ResolvedCredential::Subscription(credential) = credential else {
        return Err(AuthError::IncompatibleCredential("codex-subscription"));
    };
    if credential.provider_id != CODEX_PROVIDER_ID || credential.method_id != "codex-subscription" {
        return Err(AuthError::IncompatibleCredential("codex-subscription"));
    }
    serde_json::from_value(credential.secret.clone())
        .map_err(|_| AuthError::LoginRequired("Codex credentials are invalid; log in again".into()))
}

fn parse_id_token_claims(token: &str) -> Result<IdTokenClaims, AuthError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| AuthError::LoginRequired("Codex identity token is invalid".into()))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AuthError::LoginRequired("Codex identity token is invalid".into()))?;
    let value: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| AuthError::LoginRequired("Codex identity token is invalid".into()))?;
    let auth = value
        .get("https://api.openai.com/auth")
        .and_then(serde_json::Value::as_object);
    Ok(IdTokenClaims {
        account_id: auth
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        is_fedramp_account: auth
            .and_then(|auth| auth.get("chatgpt_account_is_fedramp"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        email: value
            .get("email")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        expires_at_unix_s: value.get("exp").and_then(serde_json::Value::as_u64),
    })
}

fn map_reqwest(error: reqwest::Error) -> AuthError {
    AuthError::Other(format!("Codex authentication transport failed: {error}"))
}

pub fn generate_pkce() -> (String, String) {
    let mut bytes = [0_u8; 64];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_are_url_safe() {
        let (verifier, challenge) = generate_pkce();
        assert_eq!(verifier.len(), 86);
        assert_eq!(challenge.len(), 43);
        assert!(!verifier.contains('='));
        assert!(!challenge.contains('='));
    }

    #[test]
    fn parses_account_and_expiry_from_id_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"email":"user@example.test","exp":42,"https://api.openai.com/auth":{"chatgpt_account_id":"acct","chatgpt_account_is_fedramp":true}}"#,
        );
        let claims = parse_id_token_claims(&format!("header.{payload}.signature")).unwrap();

        assert_eq!(claims.account_id.as_deref(), Some("acct"));
        assert!(claims.is_fedramp_account);
        assert_eq!(claims.email.as_deref(), Some("user@example.test"));
        assert_eq!(claims.expires_at_unix_s, Some(42));
    }

    #[tokio::test]
    async fn rejects_other_provider_ids() {
        let auth = CodexSubscriptionAuth::new();
        let err = auth.begin_login("openai").await.unwrap_err();

        assert!(matches!(
            err,
            AuthError::IncompatibleCredential("codex-subscription")
        ));
    }
}
