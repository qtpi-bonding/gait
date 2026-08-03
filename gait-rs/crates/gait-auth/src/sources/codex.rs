use std::path::Path;

use gait_platform::Platform;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::codex::{credential_from_external_tokens, CODEX_PROVIDER_ID};
use crate::{AuthError, AuthLocation, AuthSource, CredentialLease};

pub const CODEX_SOURCE_ID: &str = "codex";

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAuthSource;

#[derive(Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    auth_mode: Option<String>,
    tokens: Option<CodexTokenData>,
}

#[derive(Deserialize)]
struct CodexTokenData {
    id_token: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

impl AuthSource for CodexAuthSource {
    fn source_id(&self) -> &'static str {
        CODEX_SOURCE_ID
    }

    fn supports(&self, provider_id: &str) -> bool {
        provider_id == CODEX_PROVIDER_ID
    }

    fn discover(
        &self,
        platform: &dyn Platform,
        explicit_home: Option<&Path>,
    ) -> Result<Vec<AuthLocation>, AuthError> {
        let home = if let Some(path) = explicit_home {
            path.to_path_buf()
        } else if let Some(path) = platform.env("CODEX_HOME") {
            path.into()
        } else {
            platform
                .home_dir()
                .ok_or_else(|| {
                    AuthError::Other("could not determine the user home directory".into())
                })?
                .join(".codex")
        };
        Ok(vec![AuthLocation::File(home.join("auth.json"))])
    }

    fn resolve(
        &self,
        location: &AuthLocation,
        provider_id: &str,
    ) -> Result<Option<CredentialLease>, AuthError> {
        if !self.supports(provider_id) {
            return Err(AuthError::IncompatibleCredential("codex-cli-source"));
        }
        let AuthLocation::File(path) = location else {
            return Err(AuthError::Other(
                "Codex keyring credentials are not supported yet; configure Codex with cli_auth_credentials_store = \"file\""
                    .into(),
            ));
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(AuthError::Other(format!(
                    "failed to read Codex credentials at {}: {error}",
                    path.display()
                )))
            }
        };
        let auth: CodexAuthFile = serde_json::from_slice(&bytes).map_err(|error| {
            AuthError::Other(format!(
                "failed to parse Codex credentials at {}: {error}",
                path.display()
            ))
        })?;
        if let Some(mode) = auth.auth_mode.as_deref() {
            if !matches!(mode, "chatgpt" | "chatgptAuthTokens") {
                return Err(AuthError::LoginRequired(format!(
                    "Codex is using auth mode '{mode}', not a ChatGPT subscription"
                )));
            }
        }
        let tokens = auth.tokens.ok_or_else(|| {
            AuthError::LoginRequired(
                "Codex has no file-backed ChatGPT tokens; run `codex login` or configure Codex with cli_auth_credentials_store = \"file\""
                    .into(),
            )
        })?;
        if tokens.id_token.is_empty()
            || tokens.access_token.is_empty()
            || tokens.refresh_token.is_empty()
        {
            return Err(AuthError::LoginRequired(
                "Codex's ChatGPT token record is incomplete; run `codex login` again".into(),
            ));
        }
        let credential = credential_from_external_tokens(
            tokens.id_token,
            tokens.access_token,
            tokens.refresh_token,
            tokens.account_id,
        )?;
        Ok(Some(CredentialLease {
            credential,
            source_id: self.source_id().into(),
            location: location.clone(),
            revision: Some(format!("{:x}", Sha256::digest(&bytes))),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use base64::Engine;
    use gait_platform::{Platform, PlatformKind};

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

    fn fake_id_token() -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            br#"{"email":"person@example.test","exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-test"}}"#,
        );
        format!("header.{payload}.signature")
    }

    fn write_auth(home: &Path) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": fake_id_token(),
                    "access_token": "access-secret",
                    "refresh_token": "refresh-secret",
                    "account_id": "acct-test"
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn discovery_precedence_is_explicit_then_env_then_default() {
        let source = CodexAuthSource;
        let platform = FakePlatform {
            home: PathBuf::from("/users/test"),
            env: HashMap::from([("CODEX_HOME".into(), OsString::from("/env/codex"))]),
        };
        assert_eq!(
            source
                .discover(&platform, Some(Path::new("/explicit")))
                .unwrap(),
            vec![AuthLocation::File(PathBuf::from("/explicit/auth.json"))]
        );
        assert_eq!(
            source.discover(&platform, None).unwrap(),
            vec![AuthLocation::File(PathBuf::from("/env/codex/auth.json"))]
        );

        let platform = FakePlatform {
            home: PathBuf::from("/users/test"),
            env: HashMap::new(),
        };
        assert_eq!(
            source.discover(&platform, None).unwrap(),
            vec![AuthLocation::File(PathBuf::from(
                "/users/test/.codex/auth.json"
            ))]
        );
    }

    #[test]
    fn resolves_codex_file_without_exposing_tokens_in_debug() {
        let dir = tempfile::TempDir::new().unwrap();
        write_auth(dir.path());
        let location = AuthLocation::File(dir.path().join("auth.json"));
        let lease = CodexAuthSource
            .resolve(&location, CODEX_PROVIDER_ID)
            .unwrap()
            .unwrap();
        assert_eq!(lease.source_id, CODEX_SOURCE_ID);
        assert!(lease.revision.is_some());
        let debug = format!("{lease:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
    }

    #[test]
    fn missing_file_is_not_a_credential() {
        let dir = tempfile::TempDir::new().unwrap();
        let location = AuthLocation::File(dir.path().join("auth.json"));
        assert!(CodexAuthSource
            .resolve(&location, CODEX_PROVIDER_ID)
            .unwrap()
            .is_none());
    }
}
