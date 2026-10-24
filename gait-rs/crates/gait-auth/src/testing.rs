use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::store::{CredentialStore, PendingLogin, StoreGuard};
use crate::{AuthError, SubscriptionCredential};

#[derive(Clone, Default)]
pub struct MemoryStore {
    state: Arc<AsyncMutex<MemoryState>>,
    lock: Arc<AsyncMutex<()>>,
    fail_write: Arc<Mutex<bool>>,
}

#[derive(Default)]
struct MemoryState {
    credentials: HashMap<String, SubscriptionCredential>,
    pending: HashMap<String, PendingLogin>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next_write(&self) {
        *self.fail_write.lock().expect("fault lock poisoned") = true;
    }
}

struct MemoryGuard {
    state: Arc<AsyncMutex<MemoryState>>,
    _lock: OwnedMutexGuard<()>,
    fail_write: Arc<Mutex<bool>>,
}

#[async_trait]
impl CredentialStore for MemoryStore {
    async fn lock(&self) -> Result<Box<dyn StoreGuard>, AuthError> {
        Ok(Box::new(MemoryGuard {
            state: self.state.clone(),
            _lock: self.lock.clone().lock_owned().await,
            fail_write: self.fail_write.clone(),
        }))
    }
}

fn subscription(
    credential: &SubscriptionCredential,
    method: &'static str,
) -> Result<(), AuthError> {
    if credential.method_id != method {
        return Err(AuthError::IncompatibleCredential(method));
    }
    Ok(())
}

impl StoreGuard for MemoryGuard {
    fn read(&self, provider: &str) -> Result<Option<SubscriptionCredential>, AuthError> {
        let state = self
            .state
            .try_lock()
            .map_err(|_| AuthError::Retryable("store busy".into()))?;
        match state.credentials.get(provider) {
            Some(credential) => {
                subscription(credential, "chatgpt-signin")?;
                Ok(Some(credential.clone()))
            }
            None => Ok(None),
        }
    }

    fn write(
        &mut self,
        provider: &str,
        credential: &SubscriptionCredential,
    ) -> Result<(), AuthError> {
        subscription(credential, "chatgpt-signin")?;
        let mut failed = self.fail_write.lock().expect("fault lock poisoned");
        if *failed {
            *failed = false;
            return Err(AuthError::Other("injected write failure".into()));
        }
        drop(failed);
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| AuthError::Retryable("store busy".into()))?;
        state
            .credentials
            .insert(provider.into(), credential.clone());
        Ok(())
    }

    fn delete(&mut self, provider: &str) -> Result<(), AuthError> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| AuthError::Retryable("store busy".into()))?;
        state.credentials.remove(provider);
        Ok(())
    }

    fn put_pending(&mut self, provider: &str, pending: &PendingLogin) -> Result<(), AuthError> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| AuthError::Retryable("store busy".into()))?;
        state.pending.insert(provider.into(), pending.clone());
        Ok(())
    }

    fn peek_pending(&self, provider: &str) -> Result<Option<PendingLogin>, AuthError> {
        let state = self
            .state
            .try_lock()
            .map_err(|_| AuthError::Retryable("store busy".into()))?;
        Ok(state.pending.get(provider).cloned())
    }

    fn clear_pending(&mut self, provider: &str) -> Result<(), AuthError> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| AuthError::Retryable("store busy".into()))?;
        state.pending.remove(provider);
        Ok(())
    }
}

pub fn fake_jwt(claims: serde_json::Value) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = engine.encode(serde_json::to_vec(&claims).expect("claims serializable"));
    format!("{header}.{payload}.sig")
}

#[derive(Clone, Debug)]
pub struct StubResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

pub struct StubServer {
    address: std::net::SocketAddr,
    routes: Arc<Mutex<HashMap<String, StubResponse>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl StubServer {
    pub async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub server");
        let address = listener.local_addr().expect("stub address");
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_routes = routes.clone();
        let server_requests = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let routes = server_routes.clone();
                let requests = server_requests.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let count = stream.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let header_end = bytes
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap()
                        + 4;
                    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
                    let mut lines = header_text.split("\r\n");
                    let Some(request_line) = lines.next() else {
                        return;
                    };
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    let mut headers = HashMap::new();
                    let mut content_length = 0;
                    for line in lines.filter(|line| !line.is_empty()) {
                        if let Some((name, value)) = line.split_once(':') {
                            let name = name.to_ascii_lowercase();
                            let value = value.trim().to_string();
                            if name == "content-length" {
                                content_length = value.parse().unwrap_or(0);
                            }
                            headers.insert(name, value);
                        }
                    }
                    while bytes.len() - header_end < content_length {
                        let count = stream.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    let body = String::from_utf8_lossy(&bytes[header_end..]).to_string();
                    requests
                        .lock()
                        .expect("request lock poisoned")
                        .push(RecordedRequest {
                            method: method.clone(),
                            path: path.clone(),
                            headers,
                            body,
                        });
                    let response = routes
                        .lock()
                        .expect("route lock poisoned")
                        .get(&path)
                        .cloned()
                        .unwrap_or(StubResponse {
                            status: 404,
                            body: String::new(),
                        });
                    let message = format!(
                        "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        response.body.len(),
                        response.body
                    );
                    let _ = stream.write_all(message.as_bytes()).await;
                });
            }
        });
        Self {
            address,
            routes,
            requests,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn respond(&self, path: &str, status: u16, body: impl Into<String>) {
        self.routes.lock().expect("route lock poisoned").insert(
            path.into(),
            StubResponse {
                status,
                body: body.into(),
            },
        );
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request lock poisoned").clone()
    }
}

#[cfg(test)]
mod support_tests {
    use super::*;
    use std::time::Duration;

    fn credential(counter: u64) -> SubscriptionCredential {
        SubscriptionCredential {
            provider_id: "provider".into(),
            method_id: "chatgpt-signin".into(),
            version: 1,
            expires_at_unix_s: None,
            account_label: None,
            secret: serde_json::json!({"counter": counter}),
        }
    }

    #[tokio::test]
    async fn memory_store_serializes_lock_holders() {
        let store = MemoryStore::new();
        let first = store.lock().await.unwrap();
        let second_store = store.clone();
        let task = tokio::spawn(async move {
            let mut guard = second_store.lock().await.unwrap();
            let mut value = guard.read("provider").unwrap().unwrap();
            tokio::task::yield_now().await;
            value.secret["counter"] = serde_json::json!(2);
            guard.write("provider", &value).unwrap();
        });
        let mut first = first;
        first.write("provider", &credential(1)).unwrap();
        drop(first);
        task.await.unwrap();
        let guard = store.lock().await.unwrap();
        assert_eq!(
            guard.read("provider").unwrap().unwrap().secret["counter"],
            2
        );
    }

    #[tokio::test]
    async fn memory_store_rejects_non_subscription_entries() {
        let store = MemoryStore::new();
        let mut guard = store.lock().await.unwrap();
        let api = SubscriptionCredential {
            method_id: "api-key".into(),
            ..credential(0)
        };
        assert!(matches!(
            guard.write("provider", &api),
            Err(AuthError::IncompatibleCredential("chatgpt-signin"))
        ));
    }

    #[tokio::test]
    async fn fail_next_write_fails_exactly_once() {
        let store = MemoryStore::new();
        store.fail_next_write();
        let mut guard = store.lock().await.unwrap();
        assert!(guard.write("provider", &credential(1)).is_err());
        assert!(guard.write("provider", &credential(1)).is_ok());
    }

    #[tokio::test]
    async fn stub_server_records_form_bodies() {
        let server = StubServer::new().await;
        server.respond("/token", 200, "ok");
        let response = reqwest::Client::new()
            .post(format!("{}/token", server.base_url()))
            .header("x-test", "yes")
            .form(&[("code", "abc")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/token");
        assert_eq!(request.headers["x-test"], "yes");
        assert!(request.body.contains("code=abc"));
    }
}
