use anyhow::{Context, Result};
use gait_proto::gait::log::v1::{DirectEventDigest, Event, Provenance};
use identity_crypto::{
    mint_session, message_nonce, verify_signed_message_trusted_devices, AcceptedAt, AuthorDevices,
    PrincipalId, PublicKey, When,
};
use prost::Message;
use std::path::PathBuf;

pub const DIRECT_BODY_TYPE: &str = "type.googleapis.com/gait.log.v1.DirectEventDigest";

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
        let keys = identity_keyfile::AgentKeys::load(&agent_dir)
            .with_context(|| format!("load agent identity from {}", agent_dir.display()))?;
        Ok(Self { agent_dir, agent_id: keys.agent_id })
    }

    pub fn agent_id(&self) -> &PrincipalId {
        &self.agent_id
    }

    pub fn agent_dir(&self) -> &std::path::Path {
        &self.agent_dir
    }

    fn keys(&self) -> Result<identity_keyfile::AgentKeys> {
        identity_keyfile::AgentKeys::load(&self.agent_dir)
            .with_context(|| format!("load agent identity from {}", self.agent_dir.display()))
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
        keys.agent_id.genesis(),
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

pub struct DirectVerifier {
    agent_dir: PathBuf,
}

impl DirectVerifier {
    pub fn from_context(ctx: &IdentityContext) -> Result<Self> {
        ctx.keys()?;
        Ok(Self { agent_dir: ctx.agent_dir.clone() })
    }

    fn verify(&self, ev: &Event, session_id: &str) -> Option<PrincipalId> {
        let keys = identity_keyfile::AgentKeys::load(&self.agent_dir).ok()?;
        let devices = identity_keyfile::device_keys_at(&keys, (ev.ts / 1000) as i64);
        verify_direct_provenance(ev, &devices, session_id)
    }
}

impl ProvenanceVerifier for DirectVerifier {
    fn verify_direct(&self, ev: &Event, session_id: &str) -> Option<PrincipalId> {
        self.verify(ev, session_id)
    }
}

/// Verify a direct signed event against the receiver's accepted device set.
pub fn verify_direct_provenance(
    ev: &Event,
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
    let signer = msg.signer.as_ref()?.chain.as_ref()?;
    let cert = match signer {
        identity_crypto::pb::signer_chain::Chain::SelfRooted(s) => s.session.as_ref()?,
        _ => return None,
    };
    let genesis = identity_crypto::PublicKey::try_from(cert.principal_genesis.as_ref()?).ok()?;
    let verified = verify_signed_message_trusted_devices(
        msg,
        session_id,
        AuthorDevices { author_genesis: &genesis, devices },
        When::Accepted(AcceptedAt::from_receiver_record((ev.ts / 1000) as i64)),
    ).ok()?;
    Some(PrincipalId::from_genesis(verified.author_genesis().clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity_keyfile::testkit::TempAgentDir;

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
            keys.agent_id.genesis(),
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
        let fixture = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let signed = signed(&ctx, "event-own", 1_000_000, "hash", "session", 1_000, 60);
        let verifier = DirectVerifier::from_context(&ctx).unwrap();
        assert_eq!(verifier.verify_direct(&signed, "session"), Some(ctx.agent_id().clone()));
    }

    #[test]
    fn event_ts_before_cert_not_before_minus_skew_is_rejected() {
        let now = 1_000;
        let fixture = TempAgentDir::new(now, 3_600);
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
        assert!(verify_direct_provenance(&too_early, &devices, "s").is_none());
        assert!(verify_direct_provenance(&inside, &devices, "s").is_some());
    }

    #[test]
    fn event_ts_after_cert_not_after_is_rejected() {
        let now = 1_000;
        let ttl = 60;
        let fixture = TempAgentDir::new(now, 3_600);
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
        assert!(verify_direct_provenance(&too_late, &devices, "s").is_none());
        assert!(verify_direct_provenance(&at_end, &devices, "s").is_some());
    }

    #[test]
    fn event_moved_to_another_session_is_rejected() {
        let fixture = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-session", 1_000_000, "hash", "right", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&ev, &devices, "wrong").is_none());
        assert!(verify_direct_provenance(&ev, &devices, "right").is_some());
    }

    #[test]
    fn tampered_content_hash_is_rejected() {
        let fixture = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let mut ev = signed(&ctx, "event-hash", 1_000_000, "original", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        ev.content_hash = "tampered".into();
        assert!(verify_direct_provenance(&ev, &devices, "s").is_none());
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let fixture = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let mut ev = signed(&ctx, "event-nonce", 1_000_000, "hash", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        ev.id = "different-id".into();
        assert!(verify_direct_provenance(&ev, &devices, "s").is_none());
    }

    #[test]
    fn device_not_in_set_is_rejected() {
        let fixture = TempAgentDir::new(1_000, 3_600);
        let other = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-device", 1_000_000, "hash", "s", 1_000, 60);
        let real = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        let unrelated = identity_keyfile::device_keys_at(&other.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&ev, &[], "s").is_none());
        assert!(verify_direct_provenance(&ev, &unrelated, "s").is_none());
        assert!(verify_direct_provenance(&ev, &real, "s").is_some());
    }

    #[test]
    fn other_principals_genesis_is_rejected() {
        let first = TempAgentDir::new(1_000, 3_600);
        let second = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(first.path()).unwrap();
        let ev = signed(&ctx, "event-principal", 1_000_000, "hash", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&second.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&ev, &devices, "s").is_none());
    }

    #[test]
    fn retired_device_still_verifies_when_event_precedes_retirement() {
        let t = 1_000;
        let fixture = TempAgentDir::new(t, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-retired-before", t as u64 * 1_000, "hash", "s", t, 60);
        identity_keyfile::AgentKeys::renew(fixture.path(), t + 100, 3_600, true).unwrap();
        let reloaded = fixture.load().unwrap();
        let devices = identity_keyfile::device_keys_at(&reloaded, t);
        assert_eq!(verify_direct_provenance(&ev, &devices, "s"), Some(ctx.agent_id().clone()));
    }

    #[test]
    fn retired_device_fails_when_event_follows_retirement() {
        let t = 1_000;
        let fixture = TempAgentDir::new(t, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-retired-after", (t + 101) as u64 * 1_000, "hash", "s", t, 3_600);
        identity_keyfile::AgentKeys::renew(fixture.path(), t + 100, 3_600, true).unwrap();
        let reloaded = fixture.load().unwrap();
        let devices = identity_keyfile::device_keys_at(&reloaded, t + 101);
        assert!(verify_direct_provenance(&ev, &devices, "s").is_none());
    }

    #[test]
    fn wrong_body_type_is_rejected() {
        let fixture = TempAgentDir::new(1_000, 3_600);
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
        assert!(verify_direct_provenance(&ev, &devices, "s").is_none());
    }

    #[test]
    fn garbage_signed_message_is_rejected_without_panic() {
        let fixture = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let mut empty = signed(&ctx, "event-empty", 1_000_000, "hash", "s", 1_000, 60);
        empty.provenance[0].signed_message = Some(Default::default());
        let mut garbage = signed(&ctx, "event-garbage", 1_000_000, "hash", "s", 1_000, 60);
        garbage.provenance[0].signed_message.as_mut().unwrap().body = vec![0xff, 0x00, 0x7f];
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        assert!(verify_direct_provenance(&empty, &devices, "s").is_none());
        assert!(verify_direct_provenance(&garbage, &devices, "s").is_none());
    }

    #[test]
    fn verified_author_is_the_agent_principal() {
        let fixture = TempAgentDir::new(1_000, 3_600);
        let ctx = IdentityContext::new(fixture.path()).unwrap();
        let ev = signed(&ctx, "event-author", 1_000_000, "hash", "s", 1_000, 60);
        let devices = identity_keyfile::device_keys_at(&fixture.load().unwrap(), 1_000);
        let author = verify_direct_provenance(&ev, &devices, "s").unwrap();
        assert_eq!(&author, ctx.agent_id());
        assert!(author.to_string().starts_with("p256:"));
    }
}
