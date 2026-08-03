use std::path::{Path, PathBuf};

use gait_platform::Platform;

use crate::{AuthError, ResolvedCredential};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthLocation {
    Environment(String),
    File(PathBuf),
    Keyring { service: String, account: String },
    Synthetic,
}

/// A resolved credential plus non-secret metadata naming its owner.
#[derive(Debug, Clone)]
pub struct CredentialLease {
    pub credential: ResolvedCredential,
    pub source_id: String,
    pub location: AuthLocation,
    pub revision: Option<String>,
}

pub trait AuthSource: Send + Sync {
    fn source_id(&self) -> &'static str;
    fn supports(&self, provider_id: &str) -> bool;

    fn discover(
        &self,
        platform: &dyn Platform,
        explicit_home: Option<&Path>,
    ) -> Result<Vec<AuthLocation>, AuthError>;

    fn resolve(
        &self,
        location: &AuthLocation,
        provider_id: &str,
    ) -> Result<Option<CredentialLease>, AuthError>;
}
