use async_trait::async_trait;
use gait_auth::store::{CredentialStore, PendingLogin, StoreGuard};
use gait_auth::{AuthError, SubscriptionCredential};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::auth_file::{load_auth_map, store_auth_map, AuthFileLock};
use crate::gait_home::GaitHome;

const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(30);
const SIGNIN_METHOD: &str = "chatgpt-signin";

pub struct FileCredentialStore {
    auth_path: PathBuf,
    pending_dir: PathBuf,
    lock_wait: Duration,
}

impl FileCredentialStore {
    pub fn new(auth_path: PathBuf) -> Self {
        let pending_dir = auth_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("auth-pending");
        Self {
            auth_path,
            pending_dir,
            lock_wait: DEFAULT_LOCK_WAIT,
        }
    }

    pub fn resolve() -> Result<Self, crate::auth::AuthError> {
        let home = GaitHome::resolve()?;
        Ok(Self {
            auth_path: home.auth_file(),
            pending_dir: home.auth_pending_dir(),
            lock_wait: DEFAULT_LOCK_WAIT,
        })
    }

    #[cfg(test)]
    fn with_lock_wait(mut self, lock_wait: Duration) -> Self {
        self.lock_wait = lock_wait;
        self
    }
}

struct FileGuard {
    auth_path: PathBuf,
    pending_dir: PathBuf,
    _lock: AuthFileLock,
}

fn incompatible() -> AuthError {
    AuthError::IncompatibleCredential(SIGNIN_METHOD)
}
fn map_error(error: crate::auth::AuthError) -> AuthError {
    match error {
        crate::auth::AuthError::Corrupt { path, .. } => AuthError::Other(format!(
            "authentication file is corrupt: {}",
            path.display()
        )),
        other => AuthError::Other(other.to_string()),
    }
}

fn validate_provider(provider: &str) -> Result<(), AuthError> {
    if !provider.is_empty()
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && provider != "."
        && provider != ".."
    {
        Ok(())
    } else {
        Err(AuthError::Other("invalid credential provider id".into()))
    }
}

fn read_credential(
    path: &Path,
    provider: &str,
) -> Result<Option<SubscriptionCredential>, AuthError> {
    let map = load_auth_map(path).map_err(map_error)?;
    let Some(value) = map.get(provider) else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) != Some("subscription") {
        return Err(incompatible());
    }
    let entry: crate::auth::AuthEntry = serde_json::from_value(value.clone())
        .map_err(|_| AuthError::Other("invalid subscription credential".into()))?;
    match entry {
        crate::auth::AuthEntry::Subscription {
            provider_id,
            method_id,
            version,
            expires_at_unix_s,
            account_label,
            secret,
        } if method_id == SIGNIN_METHOD => Ok(Some(SubscriptionCredential {
            provider_id,
            method_id,
            version,
            expires_at_unix_s,
            account_label,
            secret,
        })),
        _ => Err(incompatible()),
    }
}

fn write_credential(
    path: &Path,
    provider: &str,
    credential: &SubscriptionCredential,
) -> Result<(), AuthError> {
    if credential.method_id != SIGNIN_METHOD {
        return Err(incompatible());
    }
    if credential.provider_id != provider {
        return Err(AuthError::Other(
            "credential provider does not match provider".into(),
        ));
    }
    let mut map = load_auth_map(path).map_err(map_error)?;
    map.insert(
        provider.into(),
        serde_json::json!({
            "type":"subscription", "provider_id":credential.provider_id,
            "method_id":credential.method_id, "version":credential.version,
            "expires_at_unix_s":credential.expires_at_unix_s,
            "account_label":credential.account_label, "secret":credential.secret
        }),
    );
    store_auth_map(path, &map).map_err(map_error)
}

fn pending_path(dir: &Path, provider: &str) -> PathBuf {
    dir.join(format!("{provider}.json"))
}

fn write_pending(dir: &Path, provider: &str, pending: &PendingLogin) -> Result<(), AuthError> {
    validate_provider(provider)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| AuthError::Other(e.to_string()))?;
    let temp = dir.join(format!(".{provider}.{}.tmp", std::process::id()));
    let result = (|| {
        let bytes = serde_json::to_vec(pending).map_err(|e| AuthError::Other(e.to_string()))?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|e| AuthError::Other(e.to_string()))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| AuthError::Other(e.to_string()))?;
        drop(file);
        fs::rename(&temp, pending_path(dir, provider))
            .map_err(|e| AuthError::Other(e.to_string()))?;
        #[cfg(unix)]
        if let Ok(directory) = File::open(dir) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn lock(&self) -> Result<Box<dyn StoreGuard>, AuthError> {
        let deadline = Instant::now() + self.lock_wait;
        loop {
            match AuthFileLock::try_acquire(&self.auth_path) {
                Ok(Some(lock)) => {
                    return Ok(Box::new(FileGuard {
                        auth_path: self.auth_path.clone(),
                        pending_dir: self.pending_dir.clone(),
                        _lock: lock,
                    }))
                }
                Ok(None) => {}
                Err(error) => return Err(map_error(error)),
            }
            if Instant::now() >= deadline {
                return Err(AuthError::Retryable(
                    "timed out waiting for authentication file lock".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl StoreGuard for FileGuard {
    fn read(&self, provider: &str) -> Result<Option<SubscriptionCredential>, AuthError> {
        read_credential(&self.auth_path, provider)
    }
    fn write(
        &mut self,
        provider: &str,
        credential: &SubscriptionCredential,
    ) -> Result<(), AuthError> {
        write_credential(&self.auth_path, provider, credential)
    }
    fn delete(&mut self, provider: &str) -> Result<(), AuthError> {
        let mut map = load_auth_map(&self.auth_path).map_err(map_error)?;
        if map.remove(provider).is_some() {
            store_auth_map(&self.auth_path, &map).map_err(map_error)?;
        }
        Ok(())
    }
    fn put_pending(&mut self, provider: &str, pending: &PendingLogin) -> Result<(), AuthError> {
        validate_provider(provider)?;
        write_pending(&self.pending_dir, provider, pending)
    }
    fn peek_pending(&self, provider: &str) -> Result<Option<PendingLogin>, AuthError> {
        validate_provider(provider)?;
        match fs::read(pending_path(&self.pending_dir, provider)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| AuthError::Other("invalid pending login".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthError::Other(e.to_string())),
        }
    }
    fn clear_pending(&mut self, provider: &str) -> Result<(), AuthError> {
        validate_provider(provider)?;
        match fs::remove_file(pending_path(&self.pending_dir, provider)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AuthError::Other(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::timeout;

    fn credential(method: &str) -> SubscriptionCredential {
        SubscriptionCredential {
            provider_id: "openai-signin".into(),
            method_id: method.into(),
            version: 1,
            expires_at_unix_s: Some(123),
            account_label: Some("ada@example.test".into()),
            secret: serde_json::json!({
                "client_id": "oaiapp_test",
                "host_id": "urn:uuid:test",
                "subject": "subject",
                "tokens": {
                    "access_token": "a",
                    "refresh_token": "r",
                    "id_token": "old-id",
                    "scopes": ["chatgpt.tokens.use.direct"],
                    "expires_at_unix_s": 2000,
                    "earliest_refresh_at_unix_s": null
                }
            }),
        }
    }
    fn pending() -> PendingLogin {
        PendingLogin {
            state: "s".into(),
            nonce: "n".into(),
            code_verifier: "v".into(),
            redirect_uri: "http://127.0.0.1:1/callback".into(),
            client_id: "dynamic_agent_client".into(),
            host_id: "h".into(),
            created_at_unix_s: 1,
        }
    }

    #[tokio::test]
    async fn guard_roundtrips_subscription_and_preserves_other_providers() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        fs::write(&p, br#"{"other":{"type":"api","key":"keep"}}"#).unwrap();
        let s = FileCredentialStore::new(p.clone());
        let mut g = s.lock().await.unwrap();
        g.write("openai-signin", &credential(SIGNIN_METHOD))
            .unwrap();
        drop(g);
        let g = FileCredentialStore::new(p.clone()).lock().await.unwrap();
        let actual = g.read("openai-signin").unwrap().unwrap();
        assert_eq!(actual.provider_id, "openai-signin");
        assert_eq!(actual.method_id, SIGNIN_METHOD);
        assert_eq!(actual.expires_at_unix_s, Some(123));
        assert_eq!(actual.account_label.as_deref(), Some("ada@example.test"));
        assert_eq!(actual.secret["tokens"]["refresh_token"], "r");
        drop(g);
        let contents: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            contents["other"],
            serde_json::json!({"type":"api","key":"keep"})
        );
        #[cfg(unix)]
        assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
    }
    #[tokio::test]
    async fn read_write_on_api_entry_is_incompatible() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        fs::write(&p, br#"{"openai-signin":{"type":"api","key":"secret"}}"#).unwrap();
        let g = FileCredentialStore::new(p).lock().await.unwrap();
        assert!(matches!(
            g.read("openai-signin"),
            Err(AuthError::IncompatibleCredential(SIGNIN_METHOD))
        ));
    }
    #[tokio::test]
    async fn second_lock_waits_until_first_guard_dropped() {
        let d = tempfile::tempdir().unwrap();
        let s = FileCredentialStore::new(d.path().join("auth.json"));
        let first = s.lock().await.unwrap();
        let wait = s.with_lock_wait(Duration::from_millis(300));
        let f = wait.lock();
        tokio::pin!(f);
        assert!(timeout(Duration::from_millis(120), &mut f).await.is_err());
        drop(first);
        assert!(timeout(Duration::from_millis(500), wait.lock())
            .await
            .unwrap()
            .is_ok());
    }
    #[tokio::test]
    async fn dropping_the_lock_future_releases_the_lock() {
        let d = tempfile::tempdir().unwrap();
        let s = FileCredentialStore::new(d.path().join("auth.json"));
        let first = s.lock().await.unwrap();
        let b = s.with_lock_wait(Duration::from_millis(300));
        let f = b.lock();
        tokio::pin!(f);
        assert!(timeout(Duration::from_millis(120), &mut f).await.is_err());
        drop(f);
        drop(first);
        assert!(timeout(Duration::from_millis(500), b.lock())
            .await
            .unwrap()
            .is_ok());
    }
    #[tokio::test]
    async fn corrupt_auth_json_makes_read_fail_not_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        fs::write(&p, b"{not json").unwrap();
        let g = FileCredentialStore::new(p.clone()).lock().await.unwrap();
        let result = g.read("openai-signin");
        assert!(matches!(result, Err(AuthError::Other(x)) if x.contains("corrupt")));
        assert_eq!(fs::read(p).unwrap(), b"{not json");
    }
    #[tokio::test]
    async fn pending_roundtrip_is_0600_and_cleared() {
        let d = tempfile::tempdir().unwrap();
        let s = FileCredentialStore::new(d.path().join("auth.json"));
        let mut g = s.lock().await.unwrap();
        g.put_pending("openai-signin", &pending()).unwrap();
        let p = d.path().join("auth-pending/openai-signin.json");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(d.path().join("auth-pending"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let entries: Vec<_> = fs::read_dir(d.path().join("auth-pending"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("openai-signin.json")]
        );
        assert_eq!(g.peek_pending("openai-signin").unwrap(), Some(pending()));
        g.clear_pending("openai-signin").unwrap();
        assert!(!p.exists());
        assert_eq!(
            fs::read_dir(d.path().join("auth-pending")).unwrap().count(),
            0
        );
    }
    #[tokio::test]
    async fn two_stores_on_one_path_make_one_refresh_request() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        let seed = FileCredentialStore::new(p.clone());
        let mut guard = seed.lock().await.unwrap();
        guard
            .write("openai-signin", &credential(SIGNIN_METHOD))
            .unwrap();
        drop(guard);

        let server = gait_auth::testing::StubServer::new().await;
        let endpoints = gait_auth::signin::Endpoints {
            authorize: format!("{}/authorize", server.base_url()),
            token: format!("{}/token", server.base_url()),
            revoke: format!("{}/revoke", server.base_url()),
        };
        server.respond(
            "/token",
            200,
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"scope":"chatgpt.tokens.use.direct"}"#,
        );
        let auth =
            gait_auth::signin::ChatGptSignIn::for_tests(endpoints, std::sync::Arc::new(|| 100));
        let a = FileCredentialStore::new(p.clone());
        let b = FileCredentialStore::new(p.clone());
        let context_a = gait_auth::AuthContext { store: &a };
        let context_b = gait_auth::AuthContext { store: &b };
        let (left, right) = tokio::join!(
            gait_auth::AuthMethod::ensure_fresh(&auth, "openai-signin", &context_a,),
            gait_auth::AuthMethod::ensure_fresh(&auth, "openai-signin", &context_b,)
        );
        let access =
            |result: Result<gait_auth::ResolvedCredential, gait_auth::AuthError>| match result
                .unwrap()
            {
                gait_auth::ResolvedCredential::Subscription(value) => {
                    let secret: Value = value.secret;
                    secret["tokens"]["access_token"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                }
                _ => panic!("expected subscription credential"),
            };
        assert_eq!(access(left), "new-access");
        assert_eq!(access(right), "new-access");
        assert_eq!(
            server
                .requests()
                .into_iter()
                .filter(|request| request.path == "/token" && request.method == "POST")
                .count(),
            1
        );
        let fresh = FileCredentialStore::new(p);
        let guard = fresh.lock().await.unwrap();
        assert_eq!(
            guard.read("openai-signin").unwrap().unwrap().secret["tokens"]["refresh_token"],
            "new-refresh"
        );
    }

    #[tokio::test]
    async fn pending_rejects_traversal_provider_ids() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("sentinel");
        fs::write(&outside, b"keep").unwrap();
        let store = FileCredentialStore::new(d.path().join("auth.json"));
        let mut guard = store.lock().await.unwrap();
        for provider in ["../x", "a/b", "", "..", "/abs"] {
            assert!(guard.put_pending(provider, &pending()).is_err());
            assert!(guard.peek_pending(provider).is_err());
            assert!(guard.clear_pending(provider).is_err());
        }
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        assert!(!d.path().join("x.json").exists());
        assert!(!d.path().join("a").exists());
        assert!(!d.path().join("auth-pending").exists());
    }

    #[tokio::test]
    async fn delete_absent_provider_does_not_create_auth_json() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("auth.json");
        let store = FileCredentialStore::new(path.clone());
        let mut guard = store.lock().await.unwrap();
        guard.delete("openai-signin").unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn write_rejects_mismatched_provider_id() {
        let d = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(d.path().join("auth.json"));
        let mut guard = store.lock().await.unwrap();
        let mut mismatched = credential("chatgpt-signin");
        mismatched.provider_id = "other-provider".into();
        assert!(matches!(
            guard.write("openai-signin", &mismatched),
            Err(AuthError::Other(message)) if message.contains("does not match")
        ));
    }

    #[test]
    fn new_store_uses_gait_home_pending_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = GaitHome::from_path(root.path().to_path_buf()).unwrap();
        let store = FileCredentialStore::new(home.auth_file());
        assert_eq!(store.pending_dir, home.auth_pending_dir());
    }
}
