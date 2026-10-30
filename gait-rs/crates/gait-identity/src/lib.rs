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
            KeyfileError::Parse { .. } | KeyfileError::Crypto(_) => {
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
    public: identity_keyfile::DevicePublic,
}

impl DirectVerifier {
    pub fn from_context(ctx: &IdentityContext) -> Result<Self> {
        Ok(Self { public: ctx.keys()?.public() })
    }

    fn verify(&self, ev: &Event, session_id: &str) -> Option<PrincipalId> {
        let devices = self.public.device_keys_at((ev.ts / 1000) as i64);
        verify_direct_provenance(ev, self.public.principal_id.genesis(), &devices, session_id)
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
}
