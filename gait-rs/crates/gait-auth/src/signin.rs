use crate::store::PendingLogin;
use crate::{ct_eq, AuthError, SubscriptionCredential};
use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SIGNIN_PROVIDER_ID: &str = "openai-signin";
pub const SIGNIN_METHOD_ID: &str = "chatgpt-signin";
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const RESOURCE: &str = "https://api.openai.com/v1";

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PendingLogin;
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
