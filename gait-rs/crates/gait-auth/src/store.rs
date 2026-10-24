use async_trait::async_trait;

use crate::{AuthError, SubscriptionCredential};

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn lock(&self) -> Result<Box<dyn StoreGuard>, AuthError>;
}

pub trait StoreGuard: Send {
    fn read(&self, provider: &str) -> Result<Option<SubscriptionCredential>, AuthError>;
    fn write(
        &mut self,
        provider: &str,
        credential: &SubscriptionCredential,
    ) -> Result<(), AuthError>;
    fn delete(&mut self, provider: &str) -> Result<(), AuthError>;
    fn put_pending(&mut self, provider: &str, pending: &PendingLogin) -> Result<(), AuthError>;
    fn peek_pending(&self, provider: &str) -> Result<Option<PendingLogin>, AuthError>;
    fn clear_pending(&mut self, provider: &str) -> Result<(), AuthError>;
}

#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PendingLogin {
    pub state: String,
    pub nonce: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    pub client_id: String,
    pub host_id: String,
    pub created_at_unix_s: u64,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLogin")
            .field("state", &self.state)
            .field("nonce", &self.nonce)
            .field("code_verifier", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("client_id", &self.client_id)
            .field("host_id", &self.host_id)
            .field("created_at_unix_s", &self.created_at_unix_s)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_debug_is_redacted() {
        let pending = PendingLogin {
            state: "state".into(),
            nonce: "nonce".into(),
            code_verifier: "secret-verifier".into(),
            redirect_uri: "http://127.0.0.1:1/callback".into(),
            client_id: "client".into(),
            host_id: "host".into(),
            created_at_unix_s: 1,
        };
        assert!(!format!("{pending:?}").contains("secret-verifier"));
    }
}
