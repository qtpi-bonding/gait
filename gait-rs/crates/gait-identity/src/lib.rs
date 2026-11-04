use anyhow::{Context, Result};
use gait_proto::gait::log::v1::{DirectEventDigest, Event, Provenance};
use identity_crypto::{
    mint_session, message_nonce, verify_signed_message_trusted_devices, AcceptedAt, AuthorDevices,
    PrincipalId, PublicKey, When,
};
use identity_keyfile::KeyfileError;
use prost::Message;
use std::path::{Path, PathBuf};

pub const DIRECT_BODY_TYPE: &str = "type.googleapis.com/gait.log.v1.DirectEventDigest";

/// Why an agent directory could not supply its identity. Every variant reads
/// `load agent identity from <dir>`; the keyfile failure is the `source`, so an
/// anyhow chain prints the same text as before the coding.
#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.identity")]
pub enum IdentityError {
    #[error("load agent identity from {}", .dir.display())]
    #[coded(code = "agent_dir_unloadable", class = Logical)]
    AgentDirUnloadable { dir: PathBuf, #[source] source: KeyfileError },
    #[error("load agent identity from {}", .dir.display())]
    #[coded(code = "missing_keyfile", class = Logical)]
    MissingKeyfile { dir: PathBuf, #[source] source: KeyfileError },
    #[error("load agent identity from {}", .dir.display())]
    #[coded(code = "device_key_unreadable", class = Logical)]
    DeviceKeyUnreadable { dir: PathBuf, #[source] source: KeyfileError },
    #[error("load agent identity from {}", .dir.display())]
    #[coded(code = "delegation_expired", class = Logical)]
    DelegationExpired { dir: PathBuf, #[source] source: KeyfileError },
}

impl IdentityError {
    fn from_keyfile(dir: &Path, source: KeyfileError) -> Self {
        let dir = dir.to_path_buf();
        match &source {
            KeyfileError::Missing(path) if path != &dir => Self::MissingKeyfile { dir, source },
            KeyfileError::Io { path, .. } if path != &dir => {
                Self::DeviceKeyUnreadable { dir, source }
            }
            KeyfileError::Parse { .. }
            | KeyfileError::Crypto(_)
            | KeyfileError::VerifyFailed(_)
            | KeyfileError::RootMismatch
            | KeyfileError::InvalidLayout(_) => {
                Self::DeviceKeyUnreadable { dir, source }
            }
            KeyfileError::DelegationMismatch => Self::DelegationExpired { dir, source },
            KeyfileError::Missing(_)
            | KeyfileError::Io { .. }
            | KeyfileError::InsecureDir(_)
            | KeyfileError::AlreadyInitialised(_) => Self::AgentDirUnloadable { dir, source },
        }
    }
}

/// Load the device keys in `agent_dir`, typing any failure as an `IdentityError`.
fn load_keys(agent_dir: &Path) -> Result<identity_keyfile::DeviceKeys> {
    identity_keyfile::DeviceKeys::load(agent_dir)
        .map_err(|source| coded_error::into_anyhow(IdentityError::from_keyfile(agent_dir, source)))
}

/// How long a minted or renewed root->device delegation lasts.
pub const DELEGATION_TTL_SECS: i64 = 30 * 86_400;
/// Renew when the delegation has this much time left or less.
pub const RENEW_MARGIN_SECS: i64 = 15 * 86_400;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The split layout gait mints under its state dir.
pub fn minted_layout(gait_dir: &Path) -> Result<identity_keyfile::IdentityLayout> {
    std::fs::create_dir_all(gait_dir)
        .with_context(|| format!("create {}", gait_dir.display()))?;
    let base = std::fs::canonicalize(gait_dir)
        .with_context(|| format!("resolve {}", gait_dir.display()))?;
    let device = base.join("identity");
    identity_keyfile::IdentityLayout::split(&base.join("identity-root"), &device)
        .map_err(|source| coded_error::into_anyhow(IdentityError::from_keyfile(&device, source)))
}

/// The environment variable the spine sets on a hand so the hand's own deny
/// policy (never the brain's) covers the identity folders. `:`-separated.
pub const DENY_PATHS_ENV: &str = "GAIT_IDENTITY_DENY";

/// Folders the model must never read or write: the device folder, and the root
/// folder next to a minted one.
pub fn identity_deny_paths(agent_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![agent_dir.to_path_buf()];
    if agent_dir.file_name().and_then(|n| n.to_str()) == Some("identity") {
        if let Some(parent) = agent_dir.parent() {
            paths.push(parent.join("identity-root"));
        }
    }
    paths
}

/// [`identity_deny_paths`] joined for [`DENY_PATHS_ENV`].
pub fn deny_paths_env_value(agent_dir: &Path) -> String {
    identity_deny_paths(agent_dir)
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// Renews the delegation of a folder gait minted (the root file exists in the
/// layout's root dir). A device-only folder is never written. Returns whether a
/// renewal happened.
pub fn renew_minted(layout: &identity_keyfile::IdentityLayout, now: i64) -> Result<bool> {
    if !layout.root_dir().join("root.jwk.json").exists() {
        return Ok(false);
    }
    let dir = layout.device_dir();
    let wrap = |source| coded_error::into_anyhow(IdentityError::from_keyfile(dir, source));
    let mut keys = identity_keyfile::IdentityKeys::load_at(layout).map_err(wrap)?;
    keys.renew_if_due_at(Some(layout), now, DELEGATION_TTL_SECS, RENEW_MARGIN_SECS)
        .map_err(wrap)
}

/// Identity material is deliberately represented by its directory, not cached keys.
pub struct IdentityContext {
    agent_dir: PathBuf,
    agent_id: PrincipalId,
}

impl IdentityContext {
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var_os("GAIT_AGENT_DIR") {
            Some(path) => Self::new(PathBuf::from(path)).map(Some),
            None => Ok(None),
        }
    }

    /// Load the folder `GAIT_AGENT_DIR` names (load only, never written); when
    /// it is unset, mint-or-load the split identity under `gait_dir` (device
    /// folder `<gait_dir>/identity`, root `<gait_dir>/identity-root`, outside
    /// every sandbox grant). The caller exports `GAIT_AGENT_DIR` afterwards,
    /// before any thread or child exists, so role children only load.
    pub fn from_env_or_mint(gait_dir: &Path) -> Result<Self> {
        if let Some(path) = std::env::var_os("GAIT_AGENT_DIR") {
            return Self::new(PathBuf::from(path));
        }
        Self::mint_or_load(gait_dir)
    }

    /// The unset-`GAIT_AGENT_DIR` half of [`Self::from_env_or_mint`].
    pub fn mint_or_load(gait_dir: &Path) -> Result<Self> {
        let layout = minted_layout(gait_dir)?;
        identity_keyfile::IdentityKeys::create_or_load_at(
            &layout,
            unix_now(),
            DELEGATION_TTL_SECS,
        )
        .map_err(|source| {
            coded_error::into_anyhow(IdentityError::from_keyfile(layout.device_dir(), source))
        })?;
        Self::new(layout.device_dir())
    }

    pub fn new(agent_dir: impl Into<PathBuf>) -> Result<Self> {
        let agent_dir = agent_dir.into();
        let keys = load_keys(&agent_dir)?;
        Ok(Self { agent_dir, agent_id: keys.principal_id })
    }

    pub fn agent_id(&self) -> &PrincipalId {
        &self.agent_id
    }

    pub fn agent_dir(&self) -> &std::path::Path {
        &self.agent_dir
    }

    fn keys(&self) -> Result<identity_keyfile::DeviceKeys> {
        load_keys(&self.agent_dir)
    }
}

pub fn sign_direct_event(
    ctx: &IdentityContext,
    ev: &Event,
    audience: &str,
    ttl_secs: i64,
    now: &dyn Fn() -> i64,
) -> Result<Provenance> {
    let keys = ctx.keys()?;
    let issued = now();
    let session = mint_session(
        keys.device.signing_key_pair(),
        keys.principal_id.genesis(),
        issued,
        issued.checked_add(ttl_secs).context("session expiry overflow")?,
    )?;
    let digest = DirectEventDigest { content_hash: ev.content_hash.clone() };
    let signed = session.sign_message(
        Some(keys.delegation.clone()),
        audience,
        message_nonce(&ev.id),
        ev.ts as i64 / 1000,
        None,
        DIRECT_BODY_TYPE,
        &digest.encode_to_vec(),
    )?;
    Ok(Provenance { signed_message: Some(signed), ..Default::default() })
}

/// A fresh device-signed session credential and the root->device delegation that
/// vouches for it, for signing a message in the spine. Never returns the root.
pub fn device_session(
    ctx: &IdentityContext,
    ttl_secs: i64,
    now: i64,
) -> Result<(identity_crypto::SessionCredential, identity_crypto::pb::KeyDelegation)> {
    let keys = ctx.keys()?;
    let session = mint_session(
        keys.device.signing_key_pair(),
        keys.principal_id.genesis(),
        now,
        now.checked_add(ttl_secs).context("session expiry overflow")?,
    )?;
    Ok((session, keys.delegation.clone()))
}

pub trait ProvenanceVerifier: Send + Sync {
    fn verify_direct(&self, ev: &Event, session_id: &str) -> Option<PrincipalId>;
}

/// A public-only snapshot of the device set, built once at construction.
///
/// It holds no private key material and touches no file afterwards, so a process
/// can build it before sealing its sandbox and keep verifying inside the seal.
/// The retirement times in it are unsigned local claims; the event time passed
/// as `at` comes from the receiver's record of the event.
pub struct DirectVerifier {
    public: std::sync::RwLock<identity_keyfile::DevicePublic>,
}

impl DirectVerifier {
    pub fn from_context(ctx: &IdentityContext) -> Result<Self> {
        Ok(Self { public: std::sync::RwLock::new(ctx.keys()?.public()) })
    }

    /// Re-reads the folder's device history so a device renewed after start
    /// verifies. Events from earlier devices still verify: the history is kept.
    pub fn refresh(&self, ctx: &IdentityContext) -> Result<()> {
        let fresh = ctx.keys()?.public();
        *self.public.write().unwrap_or_else(|p| p.into_inner()) = fresh;
        Ok(())
    }

    fn verify(&self, ev: &Event, session_id: &str) -> Option<PrincipalId> {
        let public = self.public.read().unwrap_or_else(|p| p.into_inner());
        let devices = public.device_keys_at((ev.ts / 1000) as i64);
        verify_direct_provenance(ev, public.principal_id.genesis(), &devices, session_id)
    }
}

impl ProvenanceVerifier for DirectVerifier {
    fn verify_direct(&self, ev: &Event, session_id: &str) -> Option<PrincipalId> {
        self.verify(ev, session_id)
    }
}

/// Verify a direct signed event against the receiver's own principal genesis and
/// accepted device set. The author genesis is never read from the message.
pub fn verify_direct_provenance(
    ev: &Event,
    author_genesis: &PublicKey,
    devices: &[PublicKey],
    session_id: &str,
) -> Option<PrincipalId> {
    let provenance = ev.provenance.iter().find(|p| p.signed_message.is_some())?;
    let msg = provenance.signed_message.as_ref()?;
    if msg.body_type != DIRECT_BODY_TYPE {
        return None;
    }
    let digest = DirectEventDigest::decode(msg.body.as_slice()).ok()?;
    if digest.content_hash != ev.content_hash || msg.nonce != message_nonce(&ev.id) {
        return None;
    }
    let verified = verify_signed_message_trusted_devices(
        msg,
        session_id,
        AuthorDevices { author_genesis, devices },
        When::Accepted(AcceptedAt::from_receiver_record((ev.ts / 1000) as i64)),
    ).ok()?;
    Some(PrincipalId::from_genesis(verified.author_genesis().clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity_keyfile::testkit::TempIdentityDir;

    #[test]
    fn identity_error_codes_are_unique() {
        coded_error::check_codes("gait.identity.", &[IdentityError::ALL_CODES]).unwrap();
        assert_eq!(
            IdentityError::ALL_CODES,
            &[
                "gait.identity.agent_dir_unloadable",
                "gait.identity.missing_keyfile",
                "gait.identity.device_key_unreadable",
                "gait.identity.delegation_expired",
            ]
        );
    }

    #[test]
    fn identity_error_display_texts_unchanged() {
        use coded_error::CodedError;
        let dir = PathBuf::from("/agents/one");
        let keyfile = || KeyfileError::Parse { path: dir.clone(), msg: "bad".into() };
        let texts = [
            IdentityError::AgentDirUnloadable { dir: dir.clone(), source: keyfile() },
            IdentityError::MissingKeyfile { dir: dir.clone(), source: keyfile() },
            IdentityError::DeviceKeyUnreadable { dir: dir.clone(), source: keyfile() },
            IdentityError::DelegationExpired { dir: dir.clone(), source: keyfile() },
        ];
        for err in &texts {
            assert_eq!(err.to_string(), "load agent identity from /agents/one");
        }
        assert_eq!(texts[0].class(), coded_error::ErrorClass::Logical);
    }

    #[test]
    fn identity_context_missing_keyfile_is_coded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let err = match IdentityContext::new(dir.path()) {
            Ok(_) => panic!("an empty agent dir must not load"),
            Err(err) => err,
        };
        let typed = coded_error::downcast_ref::<IdentityError>(&err).expect("typed IdentityError");
        assert!(matches!(typed, IdentityError::MissingKeyfile { .. }), "got {typed:?}");
        assert_eq!(coded_error::facts_anyhow(&err).code, "gait.identity.missing_keyfile");
        assert!(format!("{err:#}").starts_with("load agent identity from "));
    }

    fn signed(
        ctx: &IdentityContext,
        ev_id: &str,
        ev_ts_ms: u64,
        content_hash: &str,
        audience: &str,
        now_secs: i64,
        ttl: i64,
    ) -> Event {
        let ev = Event {
            id: ev_id.into(),
            ts: ev_ts_ms,
            content_hash: content_hash.into(),
            ..Default::default()
        };
        let provenance = sign_direct_event(ctx, &ev, audience, ttl, &|| now_secs).unwrap();
        Event {
            provenance: vec![provenance],
            ..ev
        }
    }

    fn signed_with_body_type(
        ctx: &IdentityContext,
        ev_id: &str,
        ev_ts_ms: u64,
        content_hash: &str,
        audience: &str,
        now_secs: i64,
        ttl: i64,
        body_type: &str,
    ) -> Event {
        let keys = ctx.keys().unwrap();
        let ev = Event {
            id: ev_id.into(),
            ts: ev_ts_ms,
            content_hash: content_hash.into(),
            ..Default::default()
        };
        let session = mint_session(
            keys.device.signing_key_pair(),
            keys.principal_id.genesis(),
            now_secs,
            now_secs + ttl,
        )
        .unwrap();
        let digest = DirectEventDigest {
            content_hash: content_hash.into(),
        };
        let message = session
            .sign_message(
                Some(keys.delegation),
                audience,
                message_nonce(&ev.id),
                (ev_ts_ms / 1000) as i64,
                None,
                body_type,
                &digest.encode_to_vec(),
            )
            .unwrap();
        Event {
            provenance: vec![Provenance {
                signed_message: Some(message),
                ..Default::default()
            }],
            ..ev
        }
    }

    #[test]
    fn signed_event_verifies_for_its_own_workspace() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let signed = signed(&ctx, "event-own", 1_000_000, "hash", "session", 1_000, 60);
        let verifier = DirectVerifier::from_context(&ctx).unwrap();
        assert_eq!(verifier.verify_direct(&signed, "session"), Some(ctx.agent_id().clone()));
    }

    #[test]
    fn event_ts_before_cert_not_before_minus_skew_is_rejected() {
        let now = 1_000;
        let fixture = TempIdentityDir::new(now, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let too_early = signed(
            &ctx,
            "event-early",
            (now - 31) as u64 * 1_000,
            "hash",
            "s",
            now,
            60,
        );
        let inside = signed(
            &ctx,
            "event-inside",
            (now - 29) as u64 * 1_000,
            "hash",
            "s",
            now,
            60,
        );
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), now - 31);
        assert!(verify_direct_provenance(&too_early, ctx.agent_id().genesis(), &devices, "s").is_none());
        assert!(verify_direct_provenance(&inside, ctx.agent_id().genesis(), &devices, "s").is_some());
    }

    #[test]
    fn event_ts_after_cert_not_after_is_rejected() {
        let now = 1_000;
        let ttl = 60;
        let fixture = TempIdentityDir::new(now, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let too_late = signed(
            &ctx,
            "event-late",
            (now + ttl + 1) as u64 * 1_000,
            "hash",
            "s",
            now,
            ttl,
        );
        let at_end = signed(
            &ctx,
            "event-end",
            (now + ttl) as u64 * 1_000,
            "hash",
            "s",
            now,
            ttl,
        );
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), now + ttl);
        assert!(verify_direct_provenance(&too_late, ctx.agent_id().genesis(), &devices, "s").is_none());
        assert!(verify_direct_provenance(&at_end, ctx.agent_id().genesis(), &devices, "s").is_some());
    }

    #[test]
    fn event_moved_to_another_session_is_rejected() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-session", 1_000_000, "hash", "right", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "wrong").is_none());
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "right").is_some());
    }

    #[test]
    fn tampered_content_hash_is_rejected() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let mut ev = signed(&ctx, "event-hash", 1_000_000, "original", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        ev.content_hash = "tampered".into();
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "s").is_none());
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let mut ev = signed(&ctx, "event-nonce", 1_000_000, "hash", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        ev.id = "different-id".into();
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "s").is_none());
    }

    #[test]
    fn device_not_in_set_is_rejected() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let other = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-device", 1_000_000, "hash", "s", 1_000, 60);
        let real = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        let unrelated = identity_keyfile::device_keys_at(&other.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &[], "s").is_none());
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &unrelated, "s").is_none());
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &real, "s").is_some());
    }

    #[test]
    fn other_principals_genesis_is_rejected() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let keys = ctx.keys().unwrap();
        // The own device signs a session under ANOTHER principal's genesis.
        let other = TempIdentityDir::new(1_000, 3_600);
        let other_genesis = other.load().unwrap().principal_id.genesis().clone();
        assert_ne!(&other_genesis, ctx.agent_id().genesis());
        let ev = Event {
            id: "event-principal".into(),
            ts: 1_000_000,
            content_hash: "hash".into(),
            ..Default::default()
        };
        let session = mint_session(keys.device.signing_key_pair(), &other_genesis, 1_000, 1_060)
            .unwrap();
        let digest = DirectEventDigest { content_hash: ev.content_hash.clone() };
        let message = session
            .sign_message(
                Some(keys.delegation.clone()),
                "s",
                message_nonce(&ev.id),
                1_000,
                None,
                DIRECT_BODY_TYPE,
                &digest.encode_to_vec(),
            )
            .unwrap();
        let forged = Event {
            provenance: vec![Provenance { signed_message: Some(message), ..Default::default() }],
            ..ev
        };
        let devices = keys.public().device_keys_at(1_000);
        assert!(verify_direct_provenance(&forged, ctx.agent_id().genesis(), &devices, "s").is_none());
        let verifier = DirectVerifier::from_context(&ctx).unwrap();
        assert!(verifier.verify_direct(&forged, "s").is_none());
    }

    #[test]
    fn signing_and_verifying_work_without_root_key_file() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        std::fs::remove_file(fixture.path().join("root.jwk.json")).unwrap();
        std::fs::remove_file(fixture.path().join("rotations.json")).ok();
        assert!(identity_keyfile::IdentityKeys::load(fixture.path()).is_err());
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-no-root", 1_000_000, "hash", "s", 1_000, 60);
        let verifier = DirectVerifier::from_context(&ctx).unwrap();
        assert_eq!(verifier.verify_direct(&ev, "s"), Some(ctx.agent_id().clone()));
    }

    #[test]
    fn verifier_works_after_the_agent_dir_becomes_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-sealed", 1_000_000, "hash", "s", 1_000, 60);
        let verifier = DirectVerifier::from_context(&ctx).unwrap();
        let original = std::fs::metadata(fixture.path()).unwrap().permissions();
        std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let sealed = identity_keyfile::IdentityKeys::load(fixture.path()).is_err();
        let verified = verifier.verify_direct(&ev, "s");
        std::fs::set_permissions(fixture.path(), original).unwrap();
        assert!(sealed, "chmod 000 must make the agent dir unloadable (tests do not run as root)");
        assert_eq!(verified, Some(ctx.agent_id().clone()));
    }

    #[test]
    fn retired_device_still_verifies_when_event_precedes_retirement() {
        let t = 1_000;
        let fixture = TempIdentityDir::new(t, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-retired-before", t as u64 * 1_000, "hash", "s", t, 60);
        identity_keyfile::IdentityKeys::renew(fixture.path(), t + 100, 3_600, true).unwrap();
        let reloaded = fixture.load().unwrap();
        let devices = identity_keyfile::device_keys_at(&reloaded, t);
        assert_eq!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "s"), Some(ctx.agent_id().clone()));
    }

    #[test]
    fn retired_device_fails_when_event_follows_retirement() {
        let t = 1_000;
        let fixture = TempIdentityDir::new(t, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-retired-after", (t + 101) as u64 * 1_000, "hash", "s", t, 3_600);
        identity_keyfile::IdentityKeys::renew(fixture.path(), t + 100, 3_600, true).unwrap();
        let reloaded = fixture.load().unwrap();
        let devices = identity_keyfile::device_keys_at(&reloaded, t + 101);
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "s").is_none());
    }

    #[test]
    fn wrong_body_type_is_rejected() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed_with_body_type(
            &ctx,
            "event-body",
            1_000_000,
            "hash",
            "s",
            1_000,
            60,
            "type.googleapis.com/other.Body",
        );
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "s").is_none());
    }

    #[test]
    fn garbage_signed_message_is_rejected_without_panic() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let mut empty = signed(&ctx, "event-empty", 1_000_000, "hash", "s", 1_000, 60);
        empty.provenance[0].signed_message = Some(Default::default());
        let mut garbage = signed(&ctx, "event-garbage", 1_000_000, "hash", "s", 1_000, 60);
        garbage.provenance[0].signed_message.as_mut().unwrap().body = vec![0xff, 0x00, 0x7f];
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&empty, ctx.agent_id().genesis(), &devices, "s").is_none());
        assert!(verify_direct_provenance(&garbage, ctx.agent_id().genesis(), &devices, "s").is_none());
    }

    #[test]
    fn verified_author_is_the_agent_principal() {
        let fixture = TempIdentityDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-author", 1_000_000, "hash", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        let author = verify_direct_provenance(&ev, ctx.agent_id().genesis(), &devices, "s").unwrap();
        assert_eq!(&author, ctx.agent_id());
        assert!(author.to_string().starts_with("p256:"));
    }

    #[test]
    fn mint_or_load_mints_once_and_keeps_the_root_out_of_the_device_folder() {
        let gait = tempfile::tempdir().unwrap();
        let first = IdentityContext::mint_or_load(gait.path()).unwrap();
        let second = IdentityContext::mint_or_load(gait.path()).unwrap();
        assert_eq!(first.agent_id(), second.agent_id());
        let base = std::fs::canonicalize(gait.path()).unwrap();
        assert!(base.join("identity-root/root.jwk.json").exists());
        assert!(!base.join("identity/root.jwk.json").exists());
        assert_eq!(first.agent_dir(), base.join("identity"));
    }

    #[test]
    fn minted_folder_renews_before_expiry_and_not_before() {
        let gait = tempfile::tempdir().unwrap();
        IdentityContext::mint_or_load(gait.path()).unwrap();
        let layout = minted_layout(gait.path()).unwrap();
        let now = unix_now();
        assert!(!renew_minted(&layout, now).unwrap());
        assert!(renew_minted(&layout, now + DELEGATION_TTL_SECS - RENEW_MARGIN_SECS + 60).unwrap());
    }

    #[test]
    fn device_only_folder_is_never_renewed() {
        let agent = TempIdentityDir::new(unix_now(), DELEGATION_TTL_SECS);
        let layout = identity_keyfile::IdentityLayout::split(
            &std::fs::canonicalize(tempfile::tempdir().unwrap().keep()).unwrap().join("no-root"),
            &std::fs::canonicalize(agent.path()).unwrap(),
        )
        .unwrap();
        assert!(!renew_minted(&layout, unix_now() + 10 * DELEGATION_TTL_SECS).unwrap());
    }

    #[test]
    fn deny_paths_cover_a_minted_root_next_to_the_device_folder() {
        let paths = identity_deny_paths(Path::new("/g/identity"));
        assert_eq!(paths, vec![PathBuf::from("/g/identity"), PathBuf::from("/g/identity-root")]);
        assert_eq!(identity_deny_paths(Path::new("/handed/agent")), vec![PathBuf::from("/handed/agent")]);
        assert_eq!(deny_paths_env_value(Path::new("/g/identity")), "/g/identity:/g/identity-root");
    }
}
