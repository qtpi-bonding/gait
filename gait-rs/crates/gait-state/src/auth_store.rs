use async_trait::async_trait;
use gait_auth::store::{CredentialStore, PendingLogin, StoreGuard};
use gait_auth::{AuthError, SubscriptionCredential};
use serde_json::Value;
use std::fs::{self, OpenOptions};
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
        Ok(Self::new(GaitHome::resolve()?.auth_file()))
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
    AuthError::Other(error.to_string())
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
    fs::create_dir_all(dir).map_err(|e| AuthError::Other(e.to_string()))?;
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
        map.remove(provider);
        store_auth_map(&self.auth_path, &map).map_err(map_error)
    }
    fn put_pending(&mut self, provider: &str, pending: &PendingLogin) -> Result<(), AuthError> {
        write_pending(&self.pending_dir, provider, pending)
    }
    fn peek_pending(&self, provider: &str) -> Result<Option<PendingLogin>, AuthError> {
        match fs::read(pending_path(&self.pending_dir, provider)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| AuthError::Other("invalid pending login".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthError::Other(e.to_string())),
        }
    }
    fn clear_pending(&mut self, provider: &str) -> Result<(), AuthError> {
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
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::{sleep, timeout};

    fn credential(method: &str) -> SubscriptionCredential {
        SubscriptionCredential {
            provider_id: "openai-signin".into(),
            method_id: method.into(),
            version: 1,
            expires_at_unix_s: Some(123),
            account_label: Some("ada@example.test".into()),
            secret: serde_json::json!({"tokens":{"access_token":"a"}}),
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
        drop(g);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(p).unwrap()).unwrap()["other"]["key"],
            "keep"
        );
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
        let wait = s.clone_for_test();
        let f = wait.lock();
        tokio::pin!(f);
        assert!(timeout(Duration::from_millis(100), &mut f).await.is_err());
        drop(first);
        assert!(timeout(Duration::from_secs(1), f).await.unwrap().is_ok());
    }
    #[tokio::test]
    async fn dropping_the_lock_future_releases_the_lock() {
        let d = tempfile::tempdir().unwrap();
        let s = FileCredentialStore::new(d.path().join("auth.json"));
        let first = s.lock().await.unwrap();
        let f = s.lock();
        tokio::pin!(f);
        tokio::select! { _=&mut f=>panic!(), _=sleep(Duration::from_millis(10))=>{} }
        drop(f);
        drop(first);
        assert!(timeout(Duration::from_secs(1), s.lock())
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
        assert!(
            matches!(g.read("openai-signin"),Err(AuthError::Other(x)) if x.contains("corrupt authentication file"))
        );
        assert_eq!(fs::read(p).unwrap(), b"{not json");
    }
    #[tokio::test]
    async fn pending_roundtrip_is_0600_and_cleared() {
        let d = tempfile::tempdir().unwrap();
        let s = FileCredentialStore::new(d.path().join("auth.json"));
        let mut g = s.lock().await.unwrap();
        g.put_pending("openai-signin", &pending()).unwrap();
        let p = d.path().join("auth-pending/openai-signin.json");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(g.peek_pending("openai-signin").unwrap(), Some(pending()));
        g.clear_pending("openai-signin").unwrap();
        assert!(!p.exists());
        assert!(!fs::read_dir(d.path().join("auth-pending"))
            .unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().contains("tmp")));
    }
    #[tokio::test]
    async fn two_stores_on_one_path_make_one_refresh_request() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        let a = FileCredentialStore::new(p.clone());
        let b = FileCredentialStore::new(p);
        let g = a.lock().await.unwrap();
        assert!(timeout(Duration::from_millis(100), b.lock()).await.is_err());
        drop(g);
        assert!(b.lock().await.is_ok());
    }

    impl FileCredentialStore {
        fn clone_for_test(&self) -> Self {
            Self {
                auth_path: self.auth_path.clone(),
                pending_dir: self.pending_dir.clone(),
                lock_wait: self.lock_wait,
            }
        }
    }
}
