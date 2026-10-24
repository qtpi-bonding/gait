use crate::store::PendingLogin;
use crate::{
    ct_eq, AuthContext, AuthError, AuthMethod, LoginCompletion, LoginFlow, LogoutReport,
    ResolvedCredential, SubscriptionCredential,
};
use async_trait::async_trait;
use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

pub const SIGNIN_PROVIDER_ID: &str = "openai-signin";
pub const SIGNIN_METHOD_ID: &str = "chatgpt-signin";
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const RESOURCE: &str = "https://api.openai.com/v1";
const PENDING_TTL: u64 = 600;
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub revoke: String,
}
impl Endpoints {
    pub fn openai() -> Self {
        Self {
            authorize: "https://auth.openai.com/api/accounts/authorize".into(),
            token: "https://auth.openai.com/api/accounts/oauth/token".into(),
            revoke: "https://auth.openai.com/api/accounts/oauth/revoke".into(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignInSecret {
    pub client_id: Option<String>,
    pub host_id: String,
    pub subject: Option<String>,
    pub tokens: Option<SignInTokens>,
}
impl std::fmt::Debug for SignInSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignInSecret")
            .field("client_id", &self.client_id.as_ref().map(|_| "<redacted>"))
            .field("host_id", &"<redacted>")
            .field("subject", &self.subject.as_ref().map(|_| "<redacted>"))
            .field("tokens", &self.tokens.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignInTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub scopes: Vec<String>,
    pub expires_at_unix_s: u64,
    pub earliest_refresh_at_unix_s: Option<u64>,
}
impl std::fmt::Debug for SignInTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignInTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("id_token", &"<redacted>")
            .field("scopes", &"<redacted>")
            .field("expires_at_unix_s", &self.expires_at_unix_s)
            .field(
                "earliest_refresh_at_unix_s",
                &self.earliest_refresh_at_unix_s,
            )
            .finish()
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrainSecret {
    pub tokens: Option<BrainTokens>,
}
impl std::fmt::Debug for BrainSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrainSecret")
            .field("tokens", &self.tokens.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrainTokens {
    pub access_token: String,
    pub expires_at_unix_s: u64,
}
impl std::fmt::Debug for BrainTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrainTokens")
            .field("access_token", &"<redacted>")
            .field("expires_at_unix_s", &self.expires_at_unix_s)
            .finish()
    }
}

fn random_string(rng: &mut rand::rngs::ThreadRng) -> String {
    let mut b = [0u8; 32];
    rng.fill(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}
pub fn new_pending(client_id: Option<&str>, host_id: &str, now: u64) -> PendingLogin {
    let mut rng = rand::thread_rng();
    let mut port = rng.gen_range(49152..=65535);
    while port == 1455 {
        port = rng.gen_range(49152..=65535);
    }
    PendingLogin {
        state: random_string(&mut rng),
        nonce: random_string(&mut rng),
        code_verifier: random_string(&mut rng),
        redirect_uri: format!("http://127.0.0.1:{port}/auth/callback"),
        client_id: client_id
            .filter(|id| !id.is_empty())
            .unwrap_or(DYNAMIC_CLIENT_ID)
            .into(),
        host_id: host_id.into(),
        created_at_unix_s: now,
    }
}
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
pub fn authorization_url(endpoints: &Endpoints, pending: &PendingLogin) -> String {
    let mut u = reqwest::Url::parse(&endpoints.authorize).expect("valid endpoint");
    let challenge = pkce_challenge(&pending.code_verifier);
    u.query_pairs_mut()
        .append_pair("client_id", &pending.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &pending.redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("resource", RESOURCE)
        .append_pair("state", &pending.state)
        .append_pair("nonce", &pending.nonce)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("ext_agent_host_id", &pending.host_id);
    if pending.client_id == DYNAMIC_CLIENT_ID {
        u.query_pairs_mut().append_pair("agent_name_hint", "gait");
    }
    u.to_string()
}

#[derive(Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub client_id: String,
}
impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callback")
            .field("code", &"<redacted>")
            .field("client_id", &"<redacted>")
            .finish()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackError {
    Typo(String),
    StateMismatch,
    Denied(String),
    Malformed(String),
}
pub fn parse_callback(
    p: &PendingLogin,
    input: &str,
    saved: Option<&str>,
) -> Result<Callback, CallbackError> {
    let u = reqwest::Url::parse(input)
        .map_err(|_| CallbackError::Typo("invalid callback URL".into()))?;
    let e = reqwest::Url::parse(&p.redirect_uri).expect("valid redirect");
    if u.scheme() != e.scheme()
        || u.host_str() != e.host_str()
        || u.port() != e.port()
        || u.path() != e.path()
    {
        return Err(CallbackError::Typo(
            "callback address does not match login".into(),
        ));
    }
    let mut m = std::collections::HashMap::new();
    for (k, v) in u.query_pairs() {
        if ["state", "code", "client_id", "error"].contains(&k.as_ref())
            && m.insert(k.to_string(), v.to_string()).is_some()
        {
            return Err(CallbackError::Malformed(
                "duplicate callback parameter".into(),
            ));
        }
    }
    if !ct_eq(
        m.get("state").map(String::as_bytes).unwrap_or_default(),
        p.state.as_bytes(),
    ) {
        return Err(CallbackError::StateMismatch);
    }
    if let Some(x) = m.get("error") {
        let safe: String = x
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '-'))
            .take(64)
            .collect();
        return Err(CallbackError::Denied(if safe.is_empty() {
            "provider denied login".into()
        } else {
            safe
        }));
    }
    let code = m
        .get("code")
        .ok_or_else(|| CallbackError::Malformed("missing code".into()))?;
    let id = m
        .get("client_id")
        .ok_or_else(|| CallbackError::Malformed("missing client_id".into()))?;
    if id == DYNAMIC_CLIENT_ID
        || id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(CallbackError::Malformed("invalid client_id".into()));
    }
    if saved.is_some_and(|x| x != id) {
        return Err(CallbackError::Malformed(
            "client_id does not match registration".into(),
        ));
    }
    Ok(Callback {
        code: code.clone(),
        client_id: id.clone(),
    })
}

pub struct IdExpect<'a> {
    pub client_id: &'a str,
    pub nonce: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub now: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdClaims {
    pub sub: String,
    pub email: Option<String>,
}
/// Validates claims only; the signature is NOT verified because this token was
/// received directly from the token endpoint over TLS, which OIDC Core permits.
/// JWKS verification is deferred, and the raw token is retained for adding it later.
pub fn validate_id_token(token: &str, e: &IdExpect<'_>) -> Result<IdClaims, AuthError> {
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() != 3 || segments.iter().any(|segment| segment.is_empty()) {
        return Err(AuthError::Other("invalid id token structure".into()));
    }
    let part = segments[1];
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| AuthError::Other("invalid id token encoding".into()))?;
    let text =
        String::from_utf8(bytes).map_err(|_| AuthError::Other("invalid id token UTF-8".into()))?;
    let c: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| AuthError::Other("invalid id token JSON".into()))?;
    if c["iss"] != "https://auth.openai.com" || c["exp"].as_u64().is_none_or(|x| x <= e.now) {
        return Err(AuthError::Other("invalid id token claims".into()));
    }
    let aud = match &c["aud"] {
        serde_json::Value::String(x) => x == e.client_id,
        serde_json::Value::Array(a) => a.iter().any(|x| x.as_str() == Some(e.client_id)),
        _ => false,
    };
    if !aud || e.nonce.is_some_and(|x| c["nonce"].as_str() != Some(x)) {
        return Err(AuthError::Other("invalid id token claims".into()));
    }
    let sub = c["sub"]
        .as_str()
        .filter(|x| !x.is_empty())
        .ok_or_else(|| AuthError::Other("invalid id token claims".into()))?;
    if e.subject.is_some_and(|x| x != sub) {
        return Err(AuthError::Other("id token subject mismatch".into()));
    }
    Ok(IdClaims {
        sub: sub.into(),
        email: c["email"].as_str().map(str::to_owned),
    })
}
pub fn brain_projection(c: &SubscriptionCredential) -> Result<SubscriptionCredential, AuthError> {
    let s: SignInSecret = serde_json::from_value(c.secret.clone())
        .map_err(|_| AuthError::IncompatibleCredential(SIGNIN_METHOD_ID))?;
    let t = s
        .tokens
        .ok_or_else(|| AuthError::LoginRequired("tokens are missing".into()))?;
    let b = BrainSecret {
        tokens: Some(BrainTokens {
            access_token: t.access_token,
            expires_at_unix_s: t.expires_at_unix_s,
        }),
    };
    Ok(SubscriptionCredential {
        provider_id: c.provider_id.clone(),
        method_id: c.method_id.clone(),
        version: c.version,
        expires_at_unix_s: None,
        account_label: None,
        secret: serde_json::to_value(b).expect("serialize"),
    })
}

/// The daemonless ChatGPT subscription login.  The store is deliberately not
/// held by this object: callers choose the store for each operation.
pub struct ChatGptSignIn {
    pub(crate) endpoints: Endpoints,
    http: reqwest::Client,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl ChatGptSignIn {
    pub fn new() -> Self {
        Self {
            endpoints: Endpoints::openai(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("valid HTTP client configuration"),
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            }),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_tests(endpoints: Endpoints, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            endpoints,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("valid HTTP client configuration"),
            now,
        }
    }
}

impl Default for ChatGptSignIn {
    fn default() -> Self {
        Self::new()
    }
}

fn empty_secret(host_id: String) -> SignInSecret {
    SignInSecret {
        client_id: None,
        host_id,
        subject: None,
        tokens: None,
    }
}

fn credential(
    secret: &SignInSecret,
    label: Option<String>,
    expiry: Option<u64>,
) -> SubscriptionCredential {
    SubscriptionCredential {
        provider_id: SIGNIN_PROVIDER_ID.into(),
        method_id: SIGNIN_METHOD_ID.into(),
        version: 1,
        expires_at_unix_s: expiry,
        account_label: label,
        secret: serde_json::to_value(secret).expect("secret is serializable"),
    }
}

fn oauth_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|x| x.as_str()).map(str::to_owned))
}

fn classify_oauth_error(status: u16, body: &str) -> AuthError {
    match oauth_code(body).as_deref() {
        Some(
            "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused",
        ) => AuthError::LoginRequired("start login again".into()),
        _ if status >= 500 || status == 429 => {
            AuthError::Retryable("temporary token service failure".into())
        }
        Some("invalid_client") => AuthError::Other(
            "registration rejected; run `gait auth logout openai-signin --forget` and log in again"
                .into(),
        ),
        Some(_) | None => AuthError::Other("token exchange failed".into()),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    earliest_refresh_at: Option<u64>,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    id_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    earliest_refresh_at: Option<u64>,
}

async fn response_body(response: reqwest::Response) -> Result<(u16, String), AuthError> {
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|_| AuthError::Retryable("token service unavailable".into()))?;
    Ok((status, body))
}

async fn revoke(http: &reqwest::Client, endpoint: &str, client_id: &str, token: &str) {
    let _ = http
        .post(endpoint)
        .form(&[
            ("token", token),
            ("token_type_hint", "refresh_token"),
            ("client_id", client_id),
        ])
        .send()
        .await;
}

#[async_trait]
impl AuthMethod for ChatGptSignIn {
    fn method_id(&self) -> &'static str {
        SIGNIN_METHOD_ID
    }
    fn display_name(&self) -> &'static str {
        "ChatGPT sign-in"
    }

    async fn begin_login(&self, _: &str) -> Result<LoginFlow, AuthError> {
        Err(AuthError::Other("requires a credential store".into()))
    }
    async fn complete_login(
        &self,
        _: &str,
        _: LoginCompletion,
    ) -> Result<ResolvedCredential, AuthError> {
        Err(AuthError::Other("requires a credential store".into()))
    }
    async fn refresh(&self, _: ResolvedCredential) -> Result<ResolvedCredential, AuthError> {
        Err(AuthError::Other("requires a credential store".into()))
    }
    async fn logout(&self, _: &ResolvedCredential) -> Result<(), AuthError> {
        Err(AuthError::Other("requires a credential store".into()))
    }

    async fn begin_login_with(
        &self,
        provider: &str,
        ctx: &AuthContext<'_>,
    ) -> Result<LoginFlow, AuthError> {
        if provider != SIGNIN_PROVIDER_ID {
            return Err(AuthError::MissingProviderId);
        }
        let mut guard = ctx.store.lock().await?;
        let existing = guard.read(provider)?;
        let mut secret = existing
            .as_ref()
            .map(|c| serde_json::from_value(c.secret.clone()))
            .transpose()
            .map_err(|_| AuthError::IncompatibleCredential(SIGNIN_METHOD_ID))?
            .unwrap_or_else(|| {
                empty_secret(format!(
                    "urn:uuid:{}",
                    random_string(&mut rand::thread_rng())
                ))
            });
        if secret.host_id.is_empty() {
            secret.host_id = format!("urn:uuid:{}", random_string(&mut rand::thread_rng()));
        }
        let pending = new_pending(secret.client_id.as_deref(), &secret.host_id, (self.now)());
        let old = existing.as_ref().and_then(|c| c.account_label.clone());
        guard.write(
            provider,
            &credential(
                &secret,
                old,
                existing.as_ref().and_then(|c| c.expires_at_unix_s),
            ),
        )?;
        guard.put_pending(provider, &pending)?;
        Ok(LoginFlow::Browser {
            authorization_url: authorization_url(&self.endpoints, &pending),
        })
    }

    async fn complete_login_with(
        &self,
        provider: &str,
        completion: LoginCompletion,
        ctx: &AuthContext<'_>,
    ) -> Result<ResolvedCredential, AuthError> {
        if provider != SIGNIN_PROVIDER_ID {
            return Err(AuthError::MissingProviderId);
        }
        let mut guard = ctx.store.lock().await?;
        let pending = guard
            .peek_pending(provider)?
            .ok_or_else(|| AuthError::Other("no pending login, start again".into()))?;
        let now = (self.now)();
        if now.saturating_sub(pending.created_at_unix_s) > PENDING_TTL {
            guard.clear_pending(provider)?;
            return Err(AuthError::Other(
                "pending login expired; start again".into(),
            ));
        }
        let url = match completion {
            LoginCompletion::CallbackUrl(x) => x,
            _ => return Err(AuthError::Other("requires a callback URL".into())),
        };
        let current = guard.read(provider)?;
        let current_secret = current
            .as_ref()
            .map(|c| serde_json::from_value::<SignInSecret>(c.secret.clone()))
            .transpose()
            .map_err(|_| AuthError::IncompatibleCredential(SIGNIN_METHOD_ID))?;
        let callback = match parse_callback(
            &pending,
            &url,
            current_secret.as_ref().and_then(|s| s.client_id.as_deref()),
        ) {
            Ok(x) => x,
            Err(CallbackError::Denied(reason)) => {
                guard.clear_pending(provider)?;
                return Err(AuthError::Other(format!("login denied: {reason}")));
            }
            Err(CallbackError::Typo(reason)) => return Err(AuthError::Other(reason)),
            Err(CallbackError::StateMismatch) => {
                return Err(AuthError::Other("callback state mismatch".into()))
            }
            Err(CallbackError::Malformed(reason)) => return Err(AuthError::Other(reason)),
        };
        let mut secret = current_secret.unwrap_or_else(|| empty_secret(pending.host_id.clone()));
        secret.client_id = Some(callback.client_id.clone());
        guard.write(
            provider,
            &credential(
                &secret,
                current.as_ref().and_then(|c| c.account_label.clone()),
                current.as_ref().and_then(|c| c.expires_at_unix_s),
            ),
        )?;
        guard.clear_pending(provider)?;
        let response = self
            .http
            .post(&self.endpoints.token)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", callback.client_id.as_str()),
                ("code", callback.code.as_str()),
                ("code_verifier", pending.code_verifier.as_str()),
                ("redirect_uri", pending.redirect_uri.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Retryable("token service unavailable".into()))?;
        let (status, body) = response_body(response).await?;
        if !(200..300).contains(&status) {
            return Err(classify_oauth_error(status, &body));
        }
        let tokens: TokenResponse = match serde_json::from_str(&body) {
            Ok(tokens) => tokens,
            Err(_) => {
                // A success body we cannot use may still carry a live refresh token;
                // revoke it best-effort so it is not orphaned. The body is never echoed.
                let orphan = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("refresh_token")
                            .and_then(|x| x.as_str())
                            .map(str::to_owned)
                    });
                if let Some(orphan) = orphan {
                    revoke(
                        &self.http,
                        &self.endpoints.revoke,
                        &callback.client_id,
                        &orphan,
                    )
                    .await;
                }
                return Err(AuthError::Other("invalid token response".into()));
            }
        };
        let refresh = tokens.refresh_token.ok_or_else(|| {
            AuthError::Other("token response did not include a refresh token".into())
        })?;
        let scopes = tokens
            .scope
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !scopes.iter().any(|s| s == PLAN_SCOPE) {
            revoke(
                &self.http,
                &self.endpoints.revoke,
                &callback.client_id,
                &refresh,
            )
            .await;
            return Err(AuthError::Other(
                "signed in but plan usage was not granted; re-run login".into(),
            ));
        }
        let claims = match validate_id_token(
            &tokens.id_token,
            &IdExpect {
                client_id: &callback.client_id,
                nonce: Some(&pending.nonce),
                subject: None,
                now,
            },
        ) {
            Ok(x) => x,
            Err(error) => {
                revoke(
                    &self.http,
                    &self.endpoints.revoke,
                    &callback.client_id,
                    &refresh,
                )
                .await;
                return Err(error);
            }
        };
        if secret
            .subject
            .as_deref()
            .is_some_and(|subject| subject != claims.sub)
        {
            revoke(
                &self.http,
                &self.endpoints.revoke,
                &callback.client_id,
                &refresh,
            )
            .await;
            return Err(AuthError::Other(
                "different ChatGPT account; run `gait auth logout openai-signin --forget` first"
                    .into(),
            ));
        }
        let expiry = now.saturating_add(tokens.expires_in.unwrap_or(3600));
        secret.subject = Some(claims.sub.clone());
        secret.tokens = Some(SignInTokens {
            access_token: tokens.access_token,
            refresh_token: refresh,
            id_token: tokens.id_token,
            scopes,
            expires_at_unix_s: expiry,
            earliest_refresh_at_unix_s: tokens.earliest_refresh_at,
        });
        let label = claims.email.or(Some(claims.sub));
        let result = credential(&secret, label, Some(expiry));
        guard.write(provider, &result)?;
        Ok(ResolvedCredential::Subscription(result))
    }

    async fn ensure_fresh(
        &self,
        provider: &str,
        ctx: &AuthContext<'_>,
    ) -> Result<ResolvedCredential, AuthError> {
        let mut guard = ctx.store.lock().await?;
        let stored = guard
            .read(provider)?
            .ok_or_else(|| AuthError::LoginRequired("run: gait auth login openai-signin".into()))?;
        let mut secret: SignInSecret = serde_json::from_value(stored.secret.clone())
            .map_err(|_| AuthError::IncompatibleCredential(SIGNIN_METHOD_ID))?;
        let mut tokens = secret
            .tokens
            .clone()
            .ok_or_else(|| AuthError::LoginRequired("run: gait auth login openai-signin".into()))?;
        let now = (self.now)();
        if tokens.expires_at_unix_s.saturating_sub(now) >= 2700 {
            return Ok(ResolvedCredential::Subscription(stored));
        }
        if let Some(earliest) = tokens.earliest_refresh_at_unix_s {
            if earliest <= tokens.expires_at_unix_s && now < earliest {
                if now < tokens.expires_at_unix_s {
                    return Ok(ResolvedCredential::Subscription(stored));
                }
                return Err(AuthError::Retryable("refresh not yet permitted".into()));
            }
        }
        let response = self
            .http
            .post(&self.endpoints.token)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", secret.client_id.as_deref().unwrap_or_default()),
                ("refresh_token", tokens.refresh_token.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Retryable("token service unavailable".into()))?;
        let (status, body) = response_body(response).await?;
        if !(200..300).contains(&status) {
            let error = classify_oauth_error(status, &body);
            if matches!(error, AuthError::LoginRequired(_)) {
                secret.tokens = None;
                let _ = guard.write(
                    provider,
                    &credential(&secret, stored.account_label.clone(), None),
                );
            }
            return Err(error);
        }
        let response: RefreshResponse = serde_json::from_str(&body)
            .map_err(|_| AuthError::Other("invalid token response".into()))?;
        let id_token = response
            .id_token
            .clone()
            .unwrap_or_else(|| tokens.id_token.clone());
        if response.id_token.is_some() {
            validate_id_token(
                &id_token,
                &IdExpect {
                    client_id: secret.client_id.as_deref().unwrap_or_default(),
                    nonce: None,
                    subject: secret.subject.as_deref(),
                    now,
                },
            )?;
        }
        let expiry = now.saturating_add(response.expires_in.unwrap_or(3600));
        tokens.access_token = response.access_token;
        tokens.id_token = id_token;
        if let Some(refresh) = response.refresh_token {
            tokens.refresh_token = refresh;
        }
        if let Some(scope) = response.scope {
            tokens.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
        tokens.expires_at_unix_s = expiry;
        tokens.earliest_refresh_at_unix_s = response.earliest_refresh_at;
        secret.tokens = Some(tokens);
        let updated = credential(&secret, stored.account_label.clone(), Some(expiry));
        guard.write(provider, &updated)?;
        Ok(ResolvedCredential::Subscription(updated))
    }

    async fn logout_with(
        &self,
        provider: &str,
        forget: bool,
        ctx: &AuthContext<'_>,
    ) -> Result<LogoutReport, AuthError> {
        let mut guard = ctx.store.lock().await?;
        let stored = guard.read(provider)?;
        let mut revoked = true;
        if let Some(stored) = stored {
            let secret: SignInSecret = serde_json::from_value(stored.secret.clone())
                .map_err(|_| AuthError::IncompatibleCredential(SIGNIN_METHOD_ID))?;
            if let (Some(tokens), Some(client_id)) = (secret.tokens, secret.client_id) {
                let response = self
                    .http
                    .post(&self.endpoints.revoke)
                    .form(&[
                        ("token", tokens.refresh_token.as_str()),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", client_id.as_str()),
                    ])
                    .send()
                    .await;
                revoked = response.map(|r| r.status().is_success()).unwrap_or(false);
            }
            if forget {
                guard.delete(provider)?;
            } else {
                let mut cleared: SignInSecret = serde_json::from_value(stored.secret)
                    .map_err(|_| AuthError::IncompatibleCredential(SIGNIN_METHOD_ID))?;
                cleared.tokens = None;
                guard.write(provider, &credential(&cleared, stored.account_label, None))?;
            }
        }
        Ok(LogoutReport { revoked })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CredentialStore, PendingLogin};
    use crate::testing::{fake_jwt, MemoryStore, StubServer};
    use crate::{AuthContext, AuthError, AuthMethod, LoginCompletion, LoginFlow};
    use std::sync::Arc;
    use tokio::time::{sleep, Duration};

    fn clock(value: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(move || value)
    }

    fn test_endpoints(server: &StubServer) -> Endpoints {
        Endpoints {
            authorize: format!("{}/authorize", server.base_url()),
            token: format!("{}/token", server.base_url()),
            revoke: format!("{}/revoke", server.base_url()),
        }
    }

    async fn begin(auth: &ChatGptSignIn, store: &MemoryStore) -> (String, PendingLogin) {
        let ctx = AuthContext { store };
        let LoginFlow::Browser { authorization_url } = auth
            .begin_login_with(SIGNIN_PROVIDER_ID, &ctx)
            .await
            .unwrap()
        else {
            panic!("browser flow expected")
        };
        let guard = store.lock().await.unwrap();
        (
            authorization_url,
            guard.peek_pending(SIGNIN_PROVIDER_ID).unwrap().unwrap(),
        )
    }

    fn callback(pending: &PendingLogin, client_id: &str, code: &str) -> String {
        format!(
            "{}?state={}&code={}&client_id={}",
            pending.redirect_uri, pending.state, code, client_id
        )
    }

    fn claims(client_id: &str, nonce: &str, sub: &str) -> String {
        fake_jwt(serde_json::json!({
            "iss": "https://auth.openai.com", "aud": client_id, "exp": 5000,
            "nonce": nonce, "sub": sub, "email": "person@example.com"
        }))
    }

    fn token_body(client_id: &str, nonce: &str, refresh: &str) -> String {
        serde_json::json!({
            "access_token":"new-access", "id_token":claims(client_id, nonce, "subject"),
            "refresh_token":refresh, "expires_in":3600,
            "scope":SCOPES, "earliest_refresh_at":1000
        })
        .to_string()
    }

    async fn requests(server: &StubServer) -> Vec<crate::testing::RecordedRequest> {
        sleep(Duration::from_millis(20)).await;
        server.requests()
    }

    async fn persisted_secret(store: &MemoryStore) -> SignInSecret {
        let entry = store
            .lock()
            .await
            .unwrap()
            .read(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        serde_json::from_value(entry.secret).unwrap()
    }

    fn form_pairs(body: &str) -> Vec<(String, String)> {
        reqwest::Url::parse(&format!("http://form.invalid/?{body}"))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[tokio::test]
    async fn begin_creates_entry_with_host_id_and_null_client_id_and_stores_pending() {
        let store = MemoryStore::new();
        let auth = ChatGptSignIn::for_tests(Endpoints::openai(), clock(100));
        let (_, pending) = begin(&auth, &store).await;
        let guard = store.lock().await.unwrap();
        let entry = guard.read(SIGNIN_PROVIDER_ID).unwrap().unwrap();
        let secret: SignInSecret = serde_json::from_value(entry.secret).unwrap();
        assert!(secret.host_id.starts_with("urn:uuid:"));
        assert_eq!(secret.client_id, None);
        assert_eq!(secret.tokens, None);
        assert_eq!(
            guard.peek_pending(SIGNIN_PROVIDER_ID).unwrap().unwrap(),
            pending
        );
    }

    #[tokio::test]
    async fn begin_twice_replaces_pending_keeps_host_id() {
        let store = MemoryStore::new();
        let auth = ChatGptSignIn::for_tests(Endpoints::openai(), clock(100));
        let (_, first) = begin(&auth, &store).await;
        let (_, second) = begin(&auth, &store).await;
        let guard = store.lock().await.unwrap();
        let secret: SignInSecret =
            serde_json::from_value(guard.read(SIGNIN_PROVIDER_ID).unwrap().unwrap().secret)
                .unwrap();
        assert_eq!(first.host_id, second.host_id);
        assert_ne!(first.state, second.state);
        assert_eq!(secret.host_id, first.host_id);
        assert_eq!(
            guard.peek_pending(SIGNIN_PROVIDER_ID).unwrap().unwrap(),
            second
        );
    }

    #[tokio::test]
    async fn complete_happy_path() {
        let server = StubServer::new().await;
        let client_id = "oaiapp_issued";
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        let (authorize, pending) = begin(&auth, &store).await;
        let host_id_at_begin = persisted_secret(&store).await.host_id;
        server.respond(
            "/token",
            200,
            token_body(client_id, &pending.nonce, "new-refresh"),
        );
        let ctx = AuthContext { store: &store };
        let result = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, client_id, "code-value")),
                &ctx,
            )
            .await
            .unwrap();
        let credential = match result {
            ResolvedCredential::Subscription(c) => c,
            _ => panic!("subscription expected"),
        };
        assert_eq!(
            credential.account_label.as_deref(),
            Some("person@example.com")
        );

        let token_requests: Vec<_> = requests(&server)
            .await
            .into_iter()
            .filter(|r| r.path == "/token")
            .collect();
        assert_eq!(token_requests.len(), 1);
        let mut form = form_pairs(&token_requests[0].body);
        let mut expected = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("client_id".to_string(), client_id.to_string()),
            ("code".to_string(), "code-value".to_string()),
            ("code_verifier".to_string(), pending.code_verifier.clone()),
            ("redirect_uri".to_string(), pending.redirect_uri.clone()),
            ("resource".to_string(), RESOURCE.to_string()),
        ];
        form.sort();
        expected.sort();
        assert_eq!(form, expected);
        let authorized_redirect = reqwest::Url::parse(&authorize)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "redirect_uri")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(authorized_redirect, pending.redirect_uri);

        let entry = store
            .lock()
            .await
            .unwrap()
            .read(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        assert_eq!(entry.account_label.as_deref(), Some("person@example.com"));
        assert_eq!(entry.expires_at_unix_s, Some(3700));
        assert_eq!(credential.secret, entry.secret);
        let persisted: SignInSecret = serde_json::from_value(entry.secret).unwrap();
        let tokens = persisted.tokens.as_ref().unwrap();
        assert_eq!(persisted.client_id.as_deref(), Some(client_id));
        assert_eq!(persisted.host_id, host_id_at_begin);
        assert_eq!(persisted.subject.as_deref(), Some("subject"));
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "new-refresh");
        assert_eq!(
            tokens.id_token,
            claims(client_id, &pending.nonce, "subject")
        );
        assert_eq!(
            tokens.scopes,
            SCOPES
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
        assert_eq!(tokens.expires_at_unix_s, 3700);
        assert_eq!(tokens.earliest_refresh_at_unix_s, Some(1000));
        assert!(ctx
            .store
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn complete_typo_url_keeps_pending() {
        let store = MemoryStore::new();
        let auth = ChatGptSignIn::for_tests(Endpoints::openai(), clock(100));
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl("not a callback".into()),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Other(message) if message == "invalid callback URL"));
        assert_eq!(
            ctx.store
                .lock()
                .await
                .unwrap()
                .peek_pending(SIGNIN_PROVIDER_ID)
                .unwrap()
                .unwrap(),
            pending
        );
    }

    #[tokio::test]
    async fn complete_state_mismatch_keeps_pending() {
        let store = MemoryStore::new();
        let auth = ChatGptSignIn::for_tests(Endpoints::openai(), clock(100));
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        let url = format!(
            "{}?state=wrong&code=c&client_id=oaiapp_x",
            pending.redirect_uri
        );
        assert!(
            matches!(auth.complete_login_with(SIGNIN_PROVIDER_ID, LoginCompletion::CallbackUrl(url), &ctx).await, Err(AuthError::Other(message)) if message == "callback state mismatch")
        );
        assert!(ctx
            .store
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn complete_access_denied_clears_pending_and_posts_nothing() {
        let server = StubServer::new().await;
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        let url = format!(
            "{}?state={}&error=access_denied",
            pending.redirect_uri, pending.state
        );
        assert!(auth
            .complete_login_with(SIGNIN_PROVIDER_ID, LoginCompletion::CallbackUrl(url), &ctx)
            .await
            .is_err());
        assert!(ctx
            .store
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .is_none());
        assert!(requests(&server).await.is_empty());
    }

    #[tokio::test]
    async fn complete_without_pending_errors() {
        let store = MemoryStore::new();
        let auth = ChatGptSignIn::for_tests(Endpoints::openai(), clock(100));
        let ctx = AuthContext { store: &store };
        assert!(
            matches!(auth.complete_login_with(SIGNIN_PROVIDER_ID, LoginCompletion::CallbackUrl("http://127.0.0.1/x".into()), &ctx).await, Err(AuthError::Other(message)) if message == "no pending login, start again")
        );
    }

    #[tokio::test]
    async fn complete_expired_pending_is_cleared_and_reported() {
        let store = MemoryStore::new();
        let auth = ChatGptSignIn::for_tests(Endpoints::openai(), clock(1000));
        let (_, pending) = begin(&auth, &store).await;
        let expired = ChatGptSignIn::for_tests(Endpoints::openai(), clock(1701));
        let ctx = AuthContext { store: &store };
        assert!(
            matches!(expired.complete_login_with(SIGNIN_PROVIDER_ID, LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "c")), &ctx).await, Err(AuthError::Other(message)) if message == "pending login expired; start again")
        );
        assert!(ctx
            .store
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .is_none());
    }

    fn live_credential(client_id: &str, subject: &str) -> SubscriptionCredential {
        let secret = SignInSecret {
            client_id: Some(client_id.into()),
            host_id: "urn:uuid:old".into(),
            subject: Some(subject.into()),
            tokens: Some(SignInTokens {
                access_token: "old-access".into(),
                refresh_token: "old-refresh".into(),
                id_token: "old-id".into(),
                scopes: vec![PLAN_SCOPE.into()],
                expires_at_unix_s: 4000,
                earliest_refresh_at_unix_s: None,
            }),
        };
        credential(&secret, Some("old@example.com".into()), Some(4000))
    }

    async fn save(store: &MemoryStore, value: SubscriptionCredential) {
        store
            .lock()
            .await
            .unwrap()
            .write(SIGNIN_PROVIDER_ID, &value)
            .unwrap();
    }

    #[tokio::test]
    async fn pending_cleared_after_failed_exchange() {
        let server = StubServer::new().await;
        server.respond("/token", 500, "temporary");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        assert!(auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx
            )
            .await
            .is_err());
        assert!(store
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn client_id_persisted_before_exchange() {
        let server = StubServer::new().await;
        server.respond("/token", 500, "temporary");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        let _ = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_saved", "code")),
                &ctx,
            )
            .await;
        let saved = store
            .lock()
            .await
            .unwrap()
            .read(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        let secret: SignInSecret = serde_json::from_value(saved.secret).unwrap();
        assert_eq!(secret.client_id.as_deref(), Some("oaiapp_saved"));
        assert_eq!(secret.host_id, pending.host_id);
        assert!(secret.tokens.is_none());
    }

    #[tokio::test]
    async fn relogin_over_live_session_keeps_tokens_on_failure() {
        let server = StubServer::new().await;
        server.respond("/token", 500, "temporary");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        save(&store, live_credential("oaiapp_old", "subject")).await;
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        let _ = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_old", "code")),
                &ctx,
            )
            .await;
        let saved: SignInSecret = serde_json::from_value(
            store
                .lock()
                .await
                .unwrap()
                .read(SIGNIN_PROVIDER_ID)
                .unwrap()
                .unwrap()
                .secret,
        )
        .unwrap();
        assert_eq!(saved.tokens.unwrap().refresh_token, "old-refresh");
    }

    #[tokio::test]
    async fn missing_plan_scope_revokes_new_refresh_token_keeps_old_state() {
        let server = StubServer::new().await;
        server.respond("/revoke", 200, "ok");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        save(&store, live_credential("oaiapp_x", "subject")).await;
        let (_, pending) = begin(&auth, &store).await;
        server.respond("/token", 200, serde_json::json!({"access_token":"new","id_token":claims("oaiapp_x", &pending.nonce, "subject"),"refresh_token":"new-refresh","scope":"openid email"}).to_string());
        let ctx = AuthContext { store: &store };
        assert!(auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx
            )
            .await
            .is_err());
        let requests = requests(&server).await;
        assert!(requests
            .iter()
            .any(|r| r.path == "/revoke" && r.body.contains("new-refresh")));
        let saved: SignInSecret = serde_json::from_value(
            store
                .lock()
                .await
                .unwrap()
                .read(SIGNIN_PROVIDER_ID)
                .unwrap()
                .unwrap()
                .secret,
        )
        .unwrap();
        assert_eq!(saved.tokens.unwrap().refresh_token, "old-refresh");
    }

    #[tokio::test]
    async fn bad_claims_revoke_new_refresh_token() {
        let server = StubServer::new().await;
        server.respond("/revoke", 200, "ok");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        save(&store, live_credential("oaiapp_x", "subject")).await;
        let (_, pending) = begin(&auth, &store).await;
        let bad = fake_jwt(
            serde_json::json!({"iss":"https://auth.openai.com","aud":"oaiapp_x","exp":5000,"nonce":"wrong-nonce","sub":"subject"}),
        );
        server.respond("/token", 200, serde_json::json!({"access_token":"new","id_token":bad,"refresh_token":"new-refresh","scope":SCOPES}).to_string());
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, AuthError::Other(message) if message == "invalid id token claims")
        );
        assert!(requests(&server)
            .await
            .iter()
            .any(|r| r.path == "/revoke" && r.body.contains("new-refresh")));
        let saved = persisted_secret(&store).await;
        assert_eq!(saved.subject.as_deref(), Some("subject"));
        let tokens = saved.tokens.unwrap();
        assert_eq!(tokens.access_token, "old-access");
        assert_eq!(tokens.refresh_token, "old-refresh");
    }

    #[tokio::test]
    async fn subject_mismatch_revokes_and_names_forget_and_keeps_old_tokens() {
        let server = StubServer::new().await;
        server.respond("/revoke", 200, "ok");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        save(&store, live_credential("oaiapp_x", "old-subject")).await;
        let (_, pending) = begin(&auth, &store).await;
        server.respond("/token", 200, serde_json::json!({"access_token":"new","id_token":claims("oaiapp_x", &pending.nonce, "new-subject"),"refresh_token":"new-refresh","scope":SCOPES}).to_string());
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, AuthError::Other(message) if message.contains("different ChatGPT account") && message.contains("logout openai-signin --forget"))
        );
        assert!(requests(&server)
            .await
            .iter()
            .any(|r| r.path == "/revoke" && r.body.contains("new-refresh")));
        let saved: SignInSecret = serde_json::from_value(
            store
                .lock()
                .await
                .unwrap()
                .read(SIGNIN_PROVIDER_ID)
                .unwrap()
                .unwrap()
                .secret,
        )
        .unwrap();
        assert_eq!(saved.tokens.unwrap().refresh_token, "old-refresh");
    }

    #[tokio::test]
    async fn revoke_failure_still_surfaces_original_error() {
        let server = StubServer::new().await;
        server.respond("/revoke", 500, "failed");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        let (_, pending) = begin(&auth, &store).await;
        server.respond("/token", 200, serde_json::json!({"access_token":"new","id_token":claims("oaiapp_x", &pending.nonce, "subject"),"refresh_token":"new-refresh","scope":"openid"}).to_string());
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, AuthError::Other(message) if message == "signed in but plan usage was not granted; re-run login")
        );
        assert!(requests(&server)
            .await
            .iter()
            .any(|r| r.path == "/revoke" && r.body.contains("new-refresh")));
    }

    #[tokio::test]
    async fn missing_refresh_token_in_response_is_error() {
        let server = StubServer::new().await;
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        save(&store, live_credential("oaiapp_x", "subject")).await;
        let (_, pending) = begin(&auth, &store).await;
        server.respond("/token", 200, serde_json::json!({"access_token":"new","id_token":claims("oaiapp_x", &pending.nonce, "subject"),"scope":SCOPES}).to_string());
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(&error, AuthError::Other(message) if message.contains("refresh token")));
        assert!(requests(&server).await.iter().all(|r| r.path != "/revoke"));
        let saved = persisted_secret(&store).await;
        assert_eq!(saved.subject.as_deref(), Some("subject"));
        let tokens = saved.tokens.unwrap();
        assert_eq!(tokens.access_token, "old-access");
        assert_eq!(tokens.refresh_token, "old-refresh");
    }

    #[test]
    fn invalid_grant_is_login_required_5xx_and_429_are_retryable() {
        assert!(matches!(
            classify_oauth_error(400, r#"{"error":"invalid_grant"}"#),
            AuthError::LoginRequired(_)
        ));
        assert!(matches!(
            classify_oauth_error(503, "bad"),
            AuthError::Retryable(_)
        ));
        assert!(matches!(
            classify_oauth_error(429, "busy"),
            AuthError::Retryable(_)
        ));
    }

    #[test]
    fn classify_invalid_client_is_other_with_registration_hint() {
        let error = classify_oauth_error(400, r#"{"error":"invalid_client"}"#);
        assert!(
            matches!(&error, AuthError::Other(message) if message == "registration rejected; run `gait auth logout openai-signin --forget` and log in again")
        );
    }

    #[test]
    fn classify_unknown_code_is_other() {
        assert!(matches!(
            classify_oauth_error(400, r#"{"error":"something_new"}"#),
            AuthError::Other(_)
        ));
        assert!(matches!(
            classify_oauth_error(400, "not json"),
            AuthError::Other(_)
        ));
    }

    #[test]
    fn classify_never_echoes_body() {
        let known = classify_oauth_error(
            400,
            r#"{"error":"invalid_grant","error_description":"secret-needle-123"}"#,
        );
        assert!(matches!(known, AuthError::LoginRequired(_)));
        assert!(!known.to_string().contains("secret-needle-123"));
        let unknown = classify_oauth_error(
            400,
            r#"{"error":"weird","error_description":"secret-needle-123"}"#,
        );
        assert!(matches!(unknown, AuthError::Other(_)));
        assert!(!unknown.to_string().contains("secret-needle-123"));
    }

    #[tokio::test]
    async fn single_token_post_on_503() {
        let server = StubServer::new().await;
        server.respond("/token", 503, "try later");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        let (_, pending) = begin(&auth, &store).await;
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Retryable(_)));
        assert_eq!(
            requests(&server)
                .await
                .iter()
                .filter(|r| r.path == "/token")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn undeserializable_success_body_revokes_issued_refresh_token() {
        let server = StubServer::new().await;
        server.respond("/revoke", 200, "ok");
        let auth = ChatGptSignIn::for_tests(test_endpoints(&server), clock(100));
        let store = MemoryStore::new();
        save(&store, live_credential("oaiapp_x", "subject")).await;
        let (_, pending) = begin(&auth, &store).await;
        server.respond(
            "/token",
            200,
            serde_json::json!({"access_token":"new","refresh_token":"orphan-refresh"}).to_string(),
        );
        let ctx = AuthContext { store: &store };
        let error = auth
            .complete_login_with(
                SIGNIN_PROVIDER_ID,
                LoginCompletion::CallbackUrl(callback(&pending, "oaiapp_x", "code")),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(&error, AuthError::Other(message) if message == "invalid token response"));
        assert!(!error.to_string().contains("orphan-refresh"));
        assert!(requests(&server)
            .await
            .iter()
            .any(|r| r.path == "/revoke" && r.body.contains("orphan-refresh")));
        let saved = persisted_secret(&store).await;
        assert_eq!(saved.subject.as_deref(), Some("subject"));
        let tokens = saved.tokens.unwrap();
        assert_eq!(tokens.access_token, "old-access");
        assert_eq!(tokens.refresh_token, "old-refresh");
    }

    fn pending() -> PendingLogin {
        PendingLogin {
            state: "state-value".into(),
            nonce: "nonce-value".into(),
            code_verifier: "verifier".into(),
            redirect_uri: "http://127.0.0.1:50000/auth/callback".into(),
            client_id: "dynamic_agent_client".into(),
            host_id: "h".into(),
            created_at_unix_s: 1,
        }
    }
    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
    #[test]
    fn authorization_url_first_registration() {
        let p = new_pending(None, "urn:uuid:test", 1);
        let url = reqwest::Url::parse(&authorization_url(&Endpoints::openai(), &p)).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], DYNAMIC_CLIENT_ID);
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["redirect_uri"], p.redirect_uri);
        assert_eq!(query["scope"], SCOPES);
        assert_eq!(query["resource"], RESOURCE);
        assert_eq!(query["state"], p.state);
        assert_eq!(query["nonce"], p.nonce);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"], pkce_challenge(&p.code_verifier));
        assert_eq!(query["ext_agent_host_id"], p.host_id);
        assert_eq!(query["agent_name_hint"], "gait");
    }
    #[test]
    fn authorization_url_returning_has_no_name_hint() {
        let p = new_pending(Some("oaiapp_x"), "h", 1);
        let url = reqwest::Url::parse(&authorization_url(&Endpoints::openai(), &p)).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "oaiapp_x");
        assert_eq!(query["ext_agent_host_id"], "h");
        assert!(!query.contains_key("agent_name_hint"));
    }
    #[test]
    fn empty_client_id_uses_dynamic_client() {
        assert_eq!(new_pending(Some(""), "h", 0).client_id, DYNAMIC_CLIENT_ID);
    }
    #[test]
    fn pending_port_never_1455_and_in_range() {
        for _ in 0..500 {
            let p = new_pending(None, "h", 0);
            let x = reqwest::Url::parse(&p.redirect_uri)
                .unwrap()
                .port()
                .unwrap();
            assert!((49152..=65535).contains(&x));
            assert_ne!(x, 1455);
        }
    }
    #[test]
    fn callback_ok() {
        let x = parse_callback(
            &pending(),
            "http://127.0.0.1:50000/auth/callback?state=state-value&code=abc&client_id=oaiapp_x",
            None,
        )
        .unwrap();
        assert_eq!(x.code, "abc");
        assert_eq!(x.client_id, "oaiapp_x");
    }
    #[test]
    fn callback_wrong_path_host_port_or_scheme_is_typo() {
        for x in [
            "https://127.0.0.1:50000/auth/callback?state=state-value&code=x&client_id=oaiapp_x",
            "http://localhost:50000/auth/callback?state=state-value&code=x&client_id=oaiapp_x",
            "http://127.0.0.1:50001/auth/callback?state=state-value&code=x&client_id=oaiapp_x",
            "http://127.0.0.1:50000/no?state=state-value&code=x&client_id=oaiapp_x",
        ] {
            assert!(matches!(
                parse_callback(&pending(), x, None),
                Err(CallbackError::Typo(_))
            ))
        }
    }
    #[test]
    fn callback_duplicate_params_malformed() {
        assert!(matches!(parse_callback(&pending(),"http://127.0.0.1:50000/auth/callback?state=state-value&state=state-value&code=x&client_id=oaiapp_x",None),Err(CallbackError::Malformed(_))));
    }
    #[test]
    fn callback_state_mismatch() {
        assert!(matches!(
            parse_callback(
                &pending(),
                "http://127.0.0.1:50000/auth/callback?state=no&code=x&client_id=oaiapp_x",
                None
            ),
            Err(CallbackError::StateMismatch)
        ));
    }
    #[test]
    fn callback_access_denied_is_denied() {
        assert!(matches!(
            parse_callback(
                &pending(),
                "http://127.0.0.1:50000/auth/callback?state=state-value&error=access_denied",
                None
            ),
            Err(CallbackError::Denied(_))
        ));
    }
    #[test]
    fn callback_denied_error_is_bounded() {
        let hostile = "!@#$%^&*".repeat(40);
        let url = format!("http://127.0.0.1:50000/auth/callback?state=state-value&error={hostile}");
        let Err(CallbackError::Denied(value)) = parse_callback(&pending(), &url, None) else {
            panic!("expected denied")
        };
        assert!(value.len() <= 64);
        assert!(value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '-')));
    }
    #[test]
    fn callback_missing_code_or_client_id_is_malformed() {
        assert!(matches!(
            parse_callback(
                &pending(),
                "http://127.0.0.1:50000/auth/callback?state=state-value&client_id=oaiapp_x",
                None
            ),
            Err(CallbackError::Malformed(_))
        ));
        assert!(matches!(
            parse_callback(
                &pending(),
                "http://127.0.0.1:50000/auth/callback?state=state-value&code=x",
                None
            ),
            Err(CallbackError::Malformed(_))
        ));
    }
    #[test]
    fn callback_matching_saved_client_id_is_ok() {
        assert!(parse_callback(
            &pending(),
            "http://127.0.0.1:50000/auth/callback?state=state-value&code=x&client_id=oaiapp_x",
            Some("oaiapp_x")
        )
        .is_ok());
    }
    #[test]
    fn callback_error_with_bad_state_is_state_mismatch() {
        assert!(matches!(
            parse_callback(
                &pending(),
                "http://127.0.0.1:50000/auth/callback?state=bad&error=access_denied",
                None
            ),
            Err(CallbackError::StateMismatch)
        ));
    }
    #[test]
    fn callback_rejects_dynamic_agent_client_and_bad_charset() {
        for x in ["dynamic_agent_client", "bad!id"] {
            let u = format!(
                "http://127.0.0.1:50000/auth/callback?state=state-value&code=x&client_id={x}"
            );
            assert!(matches!(
                parse_callback(&pending(), &u, None),
                Err(CallbackError::Malformed(_))
            ))
        }
    }
    #[test]
    fn callback_rejects_client_id_differing_from_saved() {
        assert!(matches!(
            parse_callback(
                &pending(),
                "http://127.0.0.1:50000/auth/callback?state=state-value&code=x&client_id=oaiapp_new",
                Some("oaiapp_old")
            ),
            Err(CallbackError::Malformed(_))
        ));
    }
    #[test]
    fn id_token_table() {
        let c = serde_json::json!({"iss":"https://auth.openai.com","aud":"client","exp":200,"nonce":"nonce","sub":"subject","email":"e"});
        let e = IdExpect {
            client_id: "client",
            nonce: Some("nonce"),
            subject: None,
            now: 100,
        };
        let f = |x| crate::testing::fake_jwt(x);
        assert!(validate_id_token(&f(c.clone()), &e).is_ok());
        for (k, v) in [
            ("iss", serde_json::json!("bad")),
            ("aud", serde_json::json!("other")),
            ("exp", serde_json::json!(99)),
            ("nonce", serde_json::json!("bad")),
            ("sub", serde_json::Value::Null),
        ] {
            let mut x = c.clone();
            x[k] = v;
            assert!(validate_id_token(&f(x), &e).is_err())
        }
        let mut x = c.clone();
        x["aud"] = serde_json::json!(["other", "client"]);
        assert!(validate_id_token(&f(x), &e).is_ok());
        let mut x = c.clone();
        x["aud"] = serde_json::json!(["other"]);
        assert!(
            matches!(validate_id_token(&f(x), &e), Err(AuthError::Other(message)) if message == "invalid id token claims")
        );
        let expected = IdExpect {
            subject: Some("subject"),
            ..e
        };
        assert!(validate_id_token(&f(c.clone()), &expected).is_ok());
        let mut x = c.clone();
        x["sub"] = serde_json::json!("different");
        assert!(
            matches!(validate_id_token(&f(x), &expected), Err(AuthError::Other(message)) if message == "id token subject mismatch")
        );
        for (token, message) in [
            ("one.two", "invalid id token structure"),
            ("one..three", "invalid id token structure"),
            ("one.!not-base64!.three", "invalid id token encoding"),
            ("one.bm90IGpzb24.three", "invalid id token JSON"),
            ("one._w.three", "invalid id token UTF-8"),
        ] {
            assert!(
                matches!(validate_id_token(token, &e), Err(AuthError::Other(actual)) if actual == message)
            );
        }
    }
    #[test]
    fn secret_debug_is_redacted() {
        let tokens = SignInTokens {
            access_token: "access-distinct".into(),
            refresh_token: "refresh-distinct".into(),
            id_token: "id-distinct".into(),
            scopes: vec!["email-looking@example.com".into()],
            expires_at_unix_s: 1,
            earliest_refresh_at_unix_s: Some(2),
        };
        let x = SignInSecret {
            client_id: Some("client-secret".into()),
            host_id: "host-secret".into(),
            subject: Some("sub-secret".into()),
            tokens: Some(tokens.clone()),
        };
        let brain = BrainSecret {
            tokens: Some(BrainTokens {
                access_token: "brain-access-distinct".into(),
                expires_at_unix_s: 1,
            }),
        };
        let callback = Callback {
            code: "callback-code-distinct".into(),
            client_id: "callback-client-distinct".into(),
        };
        for debug in [
            format!("{x:?}"),
            format!("{tokens:?}"),
            format!("{brain:?}"),
            format!("{:?}", brain.tokens.as_ref().unwrap()),
            format!("{callback:?}"),
        ] {
            for value in [
                "access-distinct",
                "refresh-distinct",
                "id-distinct",
                "email-looking@example.com",
                "callback-code-distinct",
                "callback-client-distinct",
            ] {
                assert!(!debug.contains(value));
            }
        }
    }
    #[test]
    fn brain_projection_drops_refresh_id_email_and_registration() {
        let c = SubscriptionCredential {
            provider_id: SIGNIN_PROVIDER_ID.into(),
            method_id: SIGNIN_METHOD_ID.into(),
            version: 1,
            expires_at_unix_s: Some(1),
            account_label: Some("email@example.com".into()),
            secret: serde_json::json!({"client_id":"client_id","host_id":"host_id","subject":"subject","tokens":{"access_token":"access","refresh_token":"refresh_token","id_token":"id_token","scopes":[],"expires_at_unix_s":1}}),
        };
        let projected = brain_projection(&c).unwrap();
        assert_eq!(projected.account_label, None);
        assert_eq!(projected.expires_at_unix_s, None);
        let brain: BrainSecret = serde_json::from_value(projected.secret.clone()).unwrap();
        let brain_tokens = brain.tokens.unwrap();
        assert_eq!(brain_tokens.access_token, "access");
        assert_eq!(brain_tokens.expires_at_unix_s, 1);
        let x = projected.secret.to_string();
        for v in [
            "refresh_token",
            "id_token",
            "client_id",
            "host_id",
            "subject",
            "email@example.com",
        ] {
            assert!(!x.contains(v));
        }
    }
}
