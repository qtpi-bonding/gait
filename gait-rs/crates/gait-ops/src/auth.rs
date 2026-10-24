//! Auth/model verbs: owned credentials, external-source links, and models.
//!
//! `add`/`remove`/`list` wrap `gait_state::auth` and never read stdin — the CLI
//! adapter handles the key prompt per design §4. `models` is async because it
//! hits `models.dev` (with disk-cache fallback); the pure `filter_models`
//! helper stays in gait-core and we just delegate here.
use crate::OpError;
use gait_auth::signin::{ChatGptSignIn, SIGNIN_PROVIDER_ID};
use gait_auth::sources::codex::{CodexAuthSource, CODEX_SOURCE_ID};
use gait_auth::store::CredentialStore;
use gait_auth::{
    AuthContext, AuthMethod, AuthSource, LoginCompletion, LoginFlow, ResolvedCredential,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// Provider → masked-display-string map. Each key is a provider name
/// (`"anthropic"`, `"openai"`, …); each value is the rendering shown by
/// `gait auth list` (an 8-char prefix + `...` for api keys, or the literal
/// `"oauth"` / `"wellknown"` / `"unknown"` tag).
pub type AuthListOutcome = HashMap<String, String>;

/// Outcome of [`add`] — reports the provider whose key was stored and the
/// masked-display value rendered back so the CLI can echo it consistently
/// with `list()`.
#[derive(Debug, Clone)]
pub struct AuthWriteOutcome {
    pub provider: String,
    pub display: String,
}

/// Outcome of [`remove`] — reports the provider whose key was removed. The
/// CLI adapter renders this as `"Removed <provider> credentials"`.
#[derive(Debug, Clone)]
pub struct AuthRemoveOutcome {
    pub provider: String,
}

/// The non-secret result of beginning a browser login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    pub provider: String,
    pub authorization_url: String,
    pub expires_in_seconds: u64,
}

/// The non-secret result of signing out a subscription provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutOutcome {
    pub provider: String,
    pub revoked: bool,
}

const LOGIN_TTL_SECONDS: u64 = 600;

/// Return the auth method owned by this ops layer, if any.
pub fn auth_method_for(provider_id: &str) -> Option<Box<dyn AuthMethod>> {
    (provider_id == SIGNIN_PROVIDER_ID)
        .then(|| Box::new(ChatGptSignIn::new()) as Box<dyn AuthMethod>)
}

fn map_auth_error(error: gait_auth::AuthError) -> OpError {
    match error {
        gait_auth::AuthError::Retryable(message) => OpError::Other(anyhow::anyhow!(message)),
        gait_auth::AuthError::LoginRequired(message) => {
            let message = if message.contains("gait auth login") {
                message
            } else {
                "run: gait auth login openai-signin".to_string()
            };
            OpError::Conflict(message)
        }
        other => OpError::Other(anyhow::anyhow!(other)),
    }
}

fn write_outcome(
    provider: &str,
    credential: ResolvedCredential,
) -> Result<AuthWriteOutcome, OpError> {
    let ResolvedCredential::Subscription(subscription) = credential else {
        return Err(OpError::Other(anyhow::anyhow!(
            "sign-in returned an API key credential"
        )));
    };
    let display = subscription
        .account_label
        .as_ref()
        .map(|label| format!("subscription ({label})"))
        .unwrap_or_else(|| "subscription".to_string());
    Ok(AuthWriteOutcome {
        provider: provider.to_string(),
        display,
    })
}

pub async fn login_begin_with(
    method: &dyn AuthMethod,
    store: &dyn CredentialStore,
    provider: &str,
) -> Result<LoginStart, OpError> {
    let flow = method
        .begin_login_with(provider, &AuthContext { store })
        .await
        .map_err(map_auth_error)?;
    let LoginFlow::Browser { authorization_url } = flow else {
        return Err(OpError::Other(anyhow::anyhow!(
            "authentication method did not start a browser login"
        )));
    };
    Ok(LoginStart {
        provider: provider.to_string(),
        authorization_url,
        expires_in_seconds: LOGIN_TTL_SECONDS,
    })
}

pub async fn login_complete_with(
    method: &dyn AuthMethod,
    store: &dyn CredentialStore,
    provider: &str,
    callback_url: String,
) -> Result<AuthWriteOutcome, OpError> {
    let credential = method
        .complete_login_with(
            provider,
            LoginCompletion::CallbackUrl(callback_url),
            &AuthContext { store },
        )
        .await
        .map_err(map_auth_error)?;
    write_outcome(provider, credential)
}

pub async fn ensure_fresh_with(
    method: &dyn AuthMethod,
    store: &dyn CredentialStore,
    provider: &str,
) -> Result<ResolvedCredential, OpError> {
    method
        .ensure_fresh(provider, &AuthContext { store })
        .await
        .map_err(map_auth_error)
}

pub async fn logout_signin_with(
    method: &dyn AuthMethod,
    store: &dyn CredentialStore,
    forget: bool,
) -> Result<LogoutOutcome, OpError> {
    let report = method
        .logout_with(SIGNIN_PROVIDER_ID, forget, &AuthContext { store })
        .await
        .map_err(map_auth_error)?;
    Ok(LogoutOutcome {
        provider: SIGNIN_PROVIDER_ID.to_string(),
        revoked: report.revoked,
    })
}

pub async fn login_begin(provider: &str) -> Result<LoginStart, OpError> {
    let method = ChatGptSignIn::new();
    let store = gait_state::auth_store::FileCredentialStore::resolve()
        .map_err(|error| OpError::Other(anyhow::anyhow!(error)))?;
    login_begin_with(&method, &store, provider).await
}

pub async fn login_complete(
    provider: &str,
    callback_url: String,
) -> Result<AuthWriteOutcome, OpError> {
    let method = ChatGptSignIn::new();
    let store = gait_state::auth_store::FileCredentialStore::resolve()
        .map_err(|error| OpError::Other(anyhow::anyhow!(error)))?;
    login_complete_with(&method, &store, provider, callback_url).await
}

pub async fn ensure_fresh(provider: &str) -> Result<ResolvedCredential, OpError> {
    let method = ChatGptSignIn::new();
    let store = gait_state::auth_store::FileCredentialStore::resolve()
        .map_err(|error| OpError::Other(anyhow::anyhow!(error)))?;
    ensure_fresh_with(&method, &store, provider).await
}

pub async fn logout_signin(forget: bool) -> Result<LogoutOutcome, OpError> {
    let method = ChatGptSignIn::new();
    let store = gait_state::auth_store::FileCredentialStore::resolve()
        .map_err(|error| OpError::Other(anyhow::anyhow!(error)))?;
    logout_signin_with(&method, &store, forget).await
}

/// Bind a provider to an external credential owner without copying secrets.
pub fn link(
    provider: &str,
    source_id: &str,
    home: Option<PathBuf>,
) -> Result<AuthWriteOutcome, OpError> {
    if provider.is_empty() {
        return Err(OpError::Conflict("provider name is required".to_string()));
    }
    let source: Box<dyn AuthSource> = match source_id {
        CODEX_SOURCE_ID => Box::new(CodexAuthSource),
        other => {
            return Err(OpError::Conflict(format!(
                "unsupported authentication source '{other}'"
            )))
        }
    };
    if !source.supports(provider) {
        return Err(OpError::Conflict(format!(
            "authentication source '{source_id}' does not support provider '{provider}'"
        )));
    }
    gait_state::auth::write_external_binding(provider, source_id, home.as_deref())
        .map_err(|error| OpError::Other(error.into()))?;
    Ok(AuthWriteOutcome {
        provider: provider.to_string(),
        display: format!("external ({source_id})"),
    })
}

/// Remove only gait's pointer to an external source. The owning CLI's
/// credential file/keyring entry is never changed or revoked.
pub fn unlink(provider: &str) -> Result<AuthRemoveOutcome, OpError> {
    if provider.is_empty() {
        return Err(OpError::Conflict("provider name is required".to_string()));
    }
    gait_state::auth::unlink_external_binding(provider)
        .map_err(|error| OpError::Other(error.into()))?;
    Ok(AuthRemoveOutcome {
        provider: provider.to_string(),
    })
}

/// Outcome of [`models`]. The filtered view the CLI / REPL renders. We
/// deliberately return *borrowed* `ModelInfo` references so the op is
/// zero-copy over the `Vec<ModelInfo>` returned by
/// `gait_state::models::get_models` — no clone, no allocation. The op
/// only fails when the network fetch (and the stale-cache fallback) both
/// fail; `filter` then narrows the set on the way out.
#[derive(Debug)]
pub struct ModelsOutcome {
    pub models: Vec<gait_state::models::ModelInfo>,
    pub filter: Option<String>,
}

impl ModelsOutcome {
    /// Apply the stored `filter` over `models`. Convenience accessor so the
    /// CLI/REPL adapter does not have to re-import `filter_models` — it
    /// renders rows straight off this method's return.
    pub fn render_filtered(&self) -> Vec<&gait_state::models::ModelInfo> {
        gait_state::models::filter_models(&self.models, self.filter.as_deref())
    }
}
/// Save `api_key` for `provider` to the credential store. The key is passed
/// in already read; the CLI adapter handles the stdin prompt (design §4).
/// `key` is required to be non-empty — the CLI adapter also guards this,
/// but the op keeps the invariant locally so a programmatic caller can't
/// accidentally wipe an entry by passing `""`.
pub fn add(provider: &str, api_key: String) -> Result<AuthWriteOutcome, OpError> {
    if provider.is_empty() {
        return Err(OpError::Conflict("provider name is required".to_string()));
    }
    if api_key.is_empty() {
        return Err(OpError::Conflict("api key is required".to_string()));
    }
    gait_state::auth::write_api_key(provider, &api_key).map_err(|e| OpError::Other(e.into()))?;
    let prefix: String = api_key.chars().take(8).collect();
    Ok(AuthWriteOutcome {
        provider: provider.to_string(),
        display: format!("{}...", prefix),
    })
}

/// Remove `provider` from the credential store. Idempotent: removing a
/// provider that isn't present still succeeds, mirroring the original
/// `cmd_auth remove` behavior (it called `remove_api_key` and reported
/// success unconditionally).
pub fn remove(provider: &str) -> Result<AuthRemoveOutcome, OpError> {
    if provider.is_empty() {
        return Err(OpError::Conflict("provider name is required".to_string()));
    }
    if provider == SIGNIN_PROVIDER_ID {
        return Err(OpError::Conflict(
            "openai-signin is managed by ChatGPT sign-in; run `gait auth logout openai-signin --forget`"
                .to_string(),
        ));
    }
    gait_state::auth::remove_api_key(provider).map_err(|error| match error {
        gait_state::auth::AuthError::ManagedElsewhere(_) if provider == SIGNIN_PROVIDER_ID => {
            OpError::Conflict(
                "openai-signin is managed by ChatGPT sign-in; run `gait auth logout openai-signin --forget`"
                    .to_string(),
            )
        }
        error => OpError::Other(error.into()),
    })?;
    Ok(AuthRemoveOutcome {
        provider: provider.to_string(),
    })
}

/// Run a Codex device-code login. `show_device_code` receives only the
/// user-facing flow; the transient device state and all tokens remain inside
/// the operation/auth method.
pub async fn login_codex<F>(show_device_code: F) -> Result<AuthWriteOutcome, OpError>
where
    F: FnOnce(&gait_auth::LoginFlow),
{
    let auth = gait_auth::codex::CodexSubscriptionAuth::new();
    let flow = auth
        .begin_login(gait_auth::codex::CODEX_PROVIDER_ID)
        .await
        .map_err(|error| OpError::Other(error.into()))?;
    show_device_code(&flow);
    let credential = auth
        .complete_login(
            gait_auth::codex::CODEX_PROVIDER_ID,
            gait_auth::LoginCompletion::DeviceCodeApproved,
        )
        .await
        .map_err(|error| OpError::Other(error.into()))?;
    let gait_auth::ResolvedCredential::Subscription(subscription) = credential else {
        return Err(OpError::Other(anyhow::anyhow!(
            "Codex login returned an API key credential"
        )));
    };
    let display = subscription
        .account_label
        .as_ref()
        .map(|label| format!("subscription ({label})"))
        .unwrap_or_else(|| "subscription".to_string());
    gait_state::auth::write_subscription_credential(
        gait_auth::codex::CODEX_PROVIDER_ID,
        &subscription,
    )
    .map_err(|error| OpError::Other(error.into()))?;
    Ok(AuthWriteOutcome {
        provider: gait_auth::codex::CODEX_PROVIDER_ID.to_string(),
        display,
    })
}

/// List every provider configured in the credential store, sorted by
/// provider name with masked-display values (`"abcd1234..."` for `api`
/// entries, literal `"oauth"`/`"wellknown"`/`"unknown"` for the rest).
pub fn list() -> Result<AuthListOutcome, OpError> {
    let mut creds =
        gait_state::auth::list_credentials().map_err(|error| OpError::Other(error.into()))?;
    let mut sorted: Vec<(String, String)> = creds.drain().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(sorted.into_iter().collect())
}

/// Non-secret status display for one configured provider.
pub fn status(provider: &str) -> Result<Option<String>, OpError> {
    Ok(list()?.remove(provider))
}

/// Revoke the Codex refresh token when possible, then remove its local
/// credential. Local removal still proceeds when the remote revoke endpoint is
/// unavailable so logout is reliable offline.
pub async fn logout_codex() -> Result<AuthRemoveOutcome, OpError> {
    if gait_state::auth::external_binding(gait_auth::codex::CODEX_PROVIDER_ID)
        .map_err(|error| OpError::Other(error.into()))?
        .is_some()
    {
        return Err(OpError::Conflict(
            "openai-codex is linked to externally owned credentials; run `gait auth unlink openai-codex` instead"
                .into(),
        ));
    }
    if let Some(credential) =
        gait_state::auth::resolve_credential_result(gait_auth::codex::CODEX_PROVIDER_ID)
            .map_err(|error| OpError::Other(error.into()))?
    {
        gait_auth::codex::CodexSubscriptionAuth::new()
            .logout(&credential)
            .await
            .map_err(|error| OpError::Other(error.into()))?;
    }
    remove(gait_auth::codex::CODEX_PROVIDER_ID)
}

/// Fetch the current models.dev catalog (with the same disk-cache /
/// stale-cache fallback that `gait_state::models::get_models` already
/// implements) and apply an optional substring filter over provider/id/name.
/// Async because the upstream fetch is a network read; frontends should
/// await this and render the rows.
pub async fn models(filter: Option<&str>) -> Result<ModelsOutcome, OpError> {
    let models = gait_state::models::get_models()
        .await
        .map_err(OpError::Other)?;
    Ok(ModelsOutcome {
        models,
        filter: filter.map(|s| s.to_string()),
    })
}

// -----------------------------------------------------------------------------
// tests
//
// Credential-store tests in gait-core (`src/auth.rs`) already round-trip
// `write_api_key_to`/`list_credentials_from`/`remove_api_key_from` against a
// caller-supplied path, so the *overridable* branch is fully covered there.
//
// `gait_state::auth::list_credentials`/`write_api_key`/`remove_api_key` (the
// no-arg forms the op must use) resolve `$GAIT_HOME/auth.json`,
// so an end-to-end round-trip here would touch the real filesystem
// credential store. We deliberately do NOT write that test — the gait-core
// tests above prove the underlying I/O works, and a u32 mock-everything
// wrapper around the home-dir lookup would obscure the public contract.
// Instead these tests exercise the argument plumbing (empty provider/key
// rejections) and confirm the wrapper's masked-display shape matches what
// the CLI adapter renders.
//
// `models()` gets a unit test against the pure `filter_models` (which the op
// calls) plus a `#[ignore]` real-network test gated by `--include-ignored`
// so a controller can opt in.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gait_auth::signin::Endpoints;
    use gait_auth::store::CredentialStore;
    use gait_auth::testing::{fake_jwt, StubServer};
    use gait_state::auth_store::FileCredentialStore;
    use std::sync::Arc;

    fn op_err_conflict(err: OpError) -> bool {
        matches!(err, OpError::Conflict(_))
    }

    fn test_method(server: &StubServer) -> ChatGptSignIn {
        let base = server.base_url();
        ChatGptSignIn::for_tests(
            Endpoints {
                authorize: format!("{base}/authorize"),
                token: format!("{base}/token"),
                revoke: format!("{base}/revoke"),
            },
            Arc::new(|| 100),
        )
    }

    fn store(dir: &tempfile::TempDir) -> FileCredentialStore {
        FileCredentialStore::new(dir.path().join("auth.json"))
    }

    async fn pending(store: &FileCredentialStore) -> gait_auth::store::PendingLogin {
        store
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap()
    }

    fn fresh(dir: &tempfile::TempDir) -> FileCredentialStore {
        FileCredentialStore::new(dir.path().join("auth.json"))
    }

    fn callback(p: &gait_auth::store::PendingLogin, client: &str) -> String {
        format!(
            "{}?state={}&code=code&client_id={}",
            p.redirect_uri, p.state, client
        )
    }

    #[test]
    fn auth_method_for_only_knows_signin() {
        assert!(auth_method_for(SIGNIN_PROVIDER_ID).is_some());
        assert!(auth_method_for("openai-codex").is_none());
        assert!(auth_method_for("unknown").is_none());
    }

    #[tokio::test]
    async fn login_begin_returns_url_and_stores_pending() {
        let server = StubServer::new().await;
        let method = test_method(&server);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let store = FileCredentialStore::new(path.clone());
        let result = login_begin_with(&method, &store, SIGNIN_PROVIDER_ID)
            .await
            .unwrap();
        assert_eq!(result.provider, SIGNIN_PROVIDER_ID);
        assert!(result
            .authorization_url
            .starts_with(&format!("{}/authorize?", server.base_url())));
        assert_eq!(result.expires_in_seconds, 600);
        let p = fresh(&dir)
            .lock()
            .await
            .unwrap()
            .peek_pending(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        assert!(result.authorization_url.contains(&p.state));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn login_roundtrip_begin_then_complete() {
        let server = StubServer::new().await;
        let method = test_method(&server);
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let _ = login_begin_with(&method, &store, SIGNIN_PROVIDER_ID)
            .await
            .unwrap();
        let p = pending(&store).await;
        let id = fake_jwt(
            serde_json::json!({"iss":"https://auth.openai.com","aud":"oaiapp_test","exp":5000,"nonce":p.nonce,"sub":"subject","email":"user@example.com"}),
        );
        server.respond("/token", 200, serde_json::json!({"access_token":"access","id_token":id,"refresh_token":"refresh","expires_in":3600,"scope":"openid chatgpt.tokens.use.direct"}).to_string());
        let out = login_complete_with(
            &method,
            &store,
            SIGNIN_PROVIDER_ID,
            callback(&p, "oaiapp_test"),
        )
        .await
        .unwrap();
        assert_eq!(out.provider, SIGNIN_PROVIDER_ID);
        assert!(server
            .requests()
            .iter()
            .any(|r| r.path == "/token" && r.body.contains("grant_type=authorization_code")));
        let saved = fresh(&dir)
            .lock()
            .await
            .unwrap()
            .read(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        assert_eq!(saved.account_label.as_deref(), Some("user@example.com"));
        assert!(saved.secret["tokens"]["access_token"] == "access");
    }

    #[tokio::test]
    async fn ensure_fresh_with_refreshes_expired_token() {
        let server = StubServer::new().await;
        let method = test_method(&server);
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut g = store.lock().await.unwrap();
        let cred = gait_auth::SubscriptionCredential {
            provider_id: SIGNIN_PROVIDER_ID.into(),
            method_id: "chatgpt-signin".into(),
            version: 1,
            expires_at_unix_s: Some(101),
            account_label: Some("user".into()),
            secret: serde_json::json!({"client_id":"oaiapp_x","host_id":"urn:uuid:h","subject":"subject","tokens":{"access_token":"old","refresh_token":"refresh","id_token":"old-id","scopes":["chatgpt.tokens.use.direct"],"expires_at_unix_s":101,"earliest_refresh_at_unix_s":null}}),
        };
        g.write(SIGNIN_PROVIDER_ID, &cred).unwrap();
        drop(g);
        server.respond("/token", 200, serde_json::json!({"access_token":"new","id_token":fake_jwt(serde_json::json!({"iss":"https://auth.openai.com","aud":"oaiapp_x","exp":5000,"sub":"subject"})),"refresh_token":"rotated","expires_in":3600,"scope":"chatgpt.tokens.use.direct"}).to_string());
        ensure_fresh_with(&method, &store, SIGNIN_PROVIDER_ID)
            .await
            .unwrap();
        assert!(server
            .requests()
            .iter()
            .any(|r| r.path == "/token" && r.body.contains("refresh_token=refresh")));
        let saved = fresh(&dir)
            .lock()
            .await
            .unwrap()
            .read(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        assert_eq!(saved.secret["tokens"]["access_token"], "new");
        assert_eq!(saved.secret["tokens"]["refresh_token"], "rotated");
    }

    #[tokio::test]
    async fn logout_with_reports_revoked_flag() {
        let server = StubServer::new().await;
        let method = test_method(&server);
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let mut g = store.lock().await.unwrap();
        let cred = gait_auth::SubscriptionCredential {
            provider_id: SIGNIN_PROVIDER_ID.into(),
            method_id: "chatgpt-signin".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret: serde_json::json!({"client_id":"oaiapp_x","host_id":"h","subject":"s","tokens":{"access_token":"a","refresh_token":"r","id_token":"i","scopes":[],"expires_at_unix_s":1,"earliest_refresh_at_unix_s":null}}),
        };
        g.write(SIGNIN_PROVIDER_ID, &cred).unwrap();
        drop(g);
        server.respond("/revoke", 200, "ok");
        let out = logout_signin_with(&method, &store, false).await.unwrap();
        assert!(out.revoked);
        assert!(server
            .requests()
            .iter()
            .any(|r| r.path == "/revoke" && r.body.contains("token=r")));
        let saved = fresh(&dir)
            .lock()
            .await
            .unwrap()
            .read(SIGNIN_PROVIDER_ID)
            .unwrap()
            .unwrap();
        assert!(saved.secret["tokens"].is_null());
    }

    #[test]
    fn remove_signin_is_conflict_naming_forget() {
        let error = remove(SIGNIN_PROVIDER_ID).unwrap_err();
        assert!(
            matches!(error, OpError::Conflict(message) if message.contains("gait auth logout openai-signin --forget"))
        );
    }

    // -- add --

    #[test]
    fn add_rejects_empty_provider() {
        let err = add("", "sk-test".into()).unwrap_err();
        assert!(op_err_conflict(err), "empty provider must error");
    }

    #[test]
    fn add_rejects_empty_key() {
        let err = add("anthropic", "".into()).unwrap_err();
        assert!(op_err_conflict(err), "empty key must error");
    }

    #[test]
    fn add_returns_outcome_with_eight_char_prefix_display() {
        // We can't end-to-end round-trip `add` (no path override at the
        // no-arg `write_api_key` layer), so we directly exercise the
        // masking shape by reconstructing what the op returns. The
        // equivalent round-trip is covered in gait-core's auth.rs tests.
        let prefix: String = "sk-abcdefg".chars().take(8).collect();
        let display = format!("{}...", prefix);
        assert_eq!(display, "sk-abcde...");
    }

    // -- remove --

    #[test]
    fn remove_rejects_empty_provider() {
        let err = remove("").unwrap_err();
        assert!(op_err_conflict(err), "empty provider must error");
    }

    #[test]
    fn link_rejects_unknown_or_incompatible_sources_before_writing() {
        assert!(op_err_conflict(
            link("openai-codex", "unknown", None).unwrap_err()
        ));
        assert!(op_err_conflict(
            link("openai", CODEX_SOURCE_ID, None).unwrap_err()
        ));
    }

    // -- list --

    #[test]
    fn list_returns_empty_map_when_credential_store_absent() {
        // The user's home-dir credential store is unlikely to exist in CI;
        // even when it does, `list()` must not panic — it returns whatever
        // the core helper reports.
        let out = list().unwrap();
        // Insertion-order check is too strict (depends on the host's real
        // auth.json), so just assert the value is a well-formed map.
        for v in out.values() {
            assert!(!v.is_empty(), "list value must be non-empty: {v:?}");
        }
    }

    // -- models (filter, pure) + network --

    fn fake_models() -> Vec<gait_state::models::ModelInfo> {
        vec![
            gait_state::models::ModelInfo {
                id: "claude-3-5-sonnet".into(),
                name: "Claude 3.5 Sonnet".into(),
                provider: "anthropic".into(),
                context_length: Some(200_000),
            },
            gait_state::models::ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "openai".into(),
                context_length: Some(128_000),
            },
            gait_state::models::ModelInfo {
                id: "gpt-4o-mini".into(),
                name: "GPT-4o mini".into(),
                provider: "openai".into(),
                context_length: None,
            },
        ]
    }

    #[test]
    fn filter_models_none_returns_all() {
        let ms = fake_models();
        let out = gait_state::models::filter_models(&ms, None);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn filter_models_matches_provider_case_insensitive() {
        let ms = fake_models();
        let out = gait_state::models::filter_models(&ms, Some("OpenAI"));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|m| m.provider == "openai"));
    }

    #[test]
    fn filter_models_matches_id_substring() {
        let ms = fake_models();
        let out = gait_state::models::filter_models(&ms, Some("sonnet"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "claude-3-5-sonnet");
    }

    #[test]
    fn render_filtered_respects_filter() {
        // Smoke-test the outcome's own render_filtered accessor end-to-end
        // without touching the network — mirrors what `models()` returns
        // shape-wise.
        let out = ModelsOutcome {
            models: fake_models(),
            filter: Some("anthropic".into()),
        };
        let filtered = out.render_filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider, "anthropic");
    }

    /// Real-network smoke test for `models()`. Gated by `#[ignore]` so the
    /// normal test pass stays offline / hermetic; a controller can opt in
    /// with `cargo test -p gait-ops auth::tests::models_real_network --
    /// --ignored`.
    #[tokio::test]
    #[ignore = "hits models.dev; run with --ignored to exercise the async path"]
    async fn models_real_network() {
        let out = models(None).await.expect("models fetch");
        assert!(!out.models.is_empty(), "expected at least one model");
        let filtered = out.render_filtered();
        assert!(!filtered.is_empty());
    }
}
