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

/// Appended to a refusal for a too-open mode: the path is named earlier in the message.
pub const MODE_HINT_SUFFIX: &str =
    "; tighten the permissions (chmod 700 for a directory, chmod 600 for a file) and retry";

/// Appended to a refusal for a wrong owner.
pub const OWNER_HINT_SUFFIX: &str = "; the path must be owned by the current user";

/// For replay state in an old format (the bare-number layout).
pub const OLD_FORMAT_HINT: &str = "the file is in an old format and must be converted";

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
    #[error("identity path {path} is not private ({detail}): it must be owned by this user, a directory 0700 or a key file 0600{hint}")]
    #[coded(code = "identity_not_private", class = Logical)]
    IdentityNotPrivate { path: PathBuf, detail: String, hint: &'static str, #[source] source: KeyfileError },
    #[error("{} holds a root key but no agent identity; restore the device folder next to it, or move the root folder aside to mint a new identity", .root.display())]
    #[coded(code = "orphan_root", class = Logical)]
    OrphanRoot { root: PathBuf, #[source] source: KeyfileError },
    #[error("the machine identity directory cannot be resolved: set MACHINE_IDENTITY_DIR, XDG_DATA_HOME or HOME to an absolute path (the hand's deny list must name it)")]
    #[coded(code = "machine_dir_unresolved", class = Logical)]
    MachineDirUnresolved { #[source] source: KeyfileError },
}

impl IdentityError {
    fn from_keyfile(dir: &Path, source: KeyfileError) -> Self {
        let dir = dir.to_path_buf();
        match &source {
            KeyfileError::InsecureDir(path) | KeyfileError::InsecureFile(path) => {
                Self::IdentityNotPrivate { path: path.clone(), detail: mode_detail(path), hint: MODE_HINT_SUFFIX, source }
            }
            KeyfileError::WrongOwner(path) => Self::IdentityNotPrivate {
                path: path.clone(),
                detail: owner_detail(path),
                hint: OWNER_HINT_SUFFIX,
                source,
            },
            KeyfileError::NotAFile(path) => Self::IdentityNotPrivate {
                path: path.clone(),
                detail: "not a regular file".to_owned(),
                hint: "",
                source,
            },
            KeyfileError::TooLarge { path, cap } => Self::IdentityNotPrivate {
                path: path.clone(),
                detail: format!("larger than {cap} bytes"),
                hint: "",
                source,
            },
            KeyfileError::InvalidMode(mode) => Self::IdentityNotPrivate {
                path: dir,
                detail: format!("mode {:04o}", mode & 0o7777),
                hint: "",
                source,
            },
            KeyfileError::OrphanRoot(root) => Self::OrphanRoot { root: root.clone(), source },
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
            | KeyfileError::AlreadyInitialised(_)
            | _ => Self::AgentDirUnloadable { dir, source },
        }
    }
}

/// `mode 0755`, read from the path now (mode bits only, via stat).
pub fn mode_detail(path: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            format!("{} is a symlink; refusing to follow it", path.display())
        }
        Ok(meta) => format!("mode {:04o}", meta.permissions().mode() & 0o7777),
        Err(_) => "mode unknown".to_owned(),
    }
}

/// `owned by uid 0, not the user running gait` (via stat).
pub fn owner_detail(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            format!("{} is a symlink; refusing to follow it", path.display())
        }
        Ok(meta) => format!("owned by uid {}, not the user running gait", meta.uid()),
        Err(_) => "owned by another user".to_owned(),
    }
}

/// Load the device keys in `agent_dir`, typing any failure as an `IdentityError`.
pub fn load_keys(agent_dir: &Path) -> Result<identity_keyfile::DeviceKeys> {
    identity_keyfile::DeviceKeys::load(agent_dir)
        .map_err(|source| coded_error::into_anyhow(IdentityError::from_keyfile(agent_dir, source)))
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

/// The machine identity directory, resolved through the environment. It always
/// names the deny list; a machine directory that cannot be resolved is a
/// failure, not a dropped entry.
pub fn machine_dir_with(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf> {
    match identity_keyfile::MachineDir::locate_with(var) {
        Ok(dir) => Ok(dir.path().to_path_buf()),
        Err(source) => Err(coded_error::into_anyhow(IdentityError::MachineDirUnresolved { source })),
    }
}

/// [`machine_dir_with`] with the process environment.
pub fn machine_dir() -> Result<PathBuf> {
    machine_dir_with(|name| std::env::var_os(name))
}

/// Folders the model must never read or write: the device folder, the root
/// folder next to a minted one, and the machine identity directory.
pub fn identity_deny_paths_with(agent_dir: &Path, machine: &Path) -> Vec<PathBuf> {
    let mut paths = agent_deny_paths(agent_dir);
    paths.push(machine.to_path_buf());
    paths
}

/// The device folder and, next to a minted one, the root folder (no machine directory).
pub fn agent_deny_paths(agent_dir: &Path) -> Vec<PathBuf> {
    let mut paths = vec![agent_dir.to_path_buf()];
    if agent_dir.file_name().and_then(|n| n.to_str()) == Some("identity") {
        if let Some(parent) = agent_dir.parent() {
            paths.push(parent.join("identity-root"));
        }
    }
    paths
}

/// [`identity_deny_paths_with`] joined for [`DENY_PATHS_ENV`].
pub fn deny_paths_env_value_with(agent_dir: &Path, machine: &Path) -> String {
    join_paths(&identity_deny_paths_with(agent_dir, machine))
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":")
}

/// Renews the delegation of a folder gait minted (the layout has a root). A device-only
/// folder is never written. Returns whether a renewal happened.
pub fn renew_minted(layout: &identity_keyfile::IdentityLayout, now: i64) -> Result<bool> {
    if !layout.has_root() {
        return Ok(false);
    }
    let dir = layout.device_dir();
    let wrap = |source| coded_error::into_anyhow(IdentityError::from_keyfile(dir, source));
    let mut keys = identity_keyfile::IdentityKeys::load_at(layout).map_err(wrap)?;
    keys.renew_if_due_at(
        Some(layout),
        now,
        identity_crypto::DEFAULT_DELEGATION_SECONDS,
        identity_crypto::DEFAULT_RENEW_MARGIN_SECONDS,
    )
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
        // The library only reports a loose device dir as DeviceOnly, which
        // hides the mode, so check each existing dir here first (stat only).
        for dir in [layout.device_dir(), layout.root_dir()] {
            if std::fs::symlink_metadata(dir).is_ok() {
                if let Err(source) = identity_keyfile::private_fs::check_private_dir(dir) {
                    return Err(coded_error::into_anyhow(IdentityError::from_keyfile(dir, source)));
                }
            }
        }
        identity_keyfile::IdentityKeys::create_or_load_at(
            &layout,
            identity_crypto::now_unix(),
            identity_crypto::DEFAULT_DELEGATION_SECONDS,
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
                "gait.identity.identity_not_private",
                "gait.identity.orphan_root",
                "gait.identity.machine_dir_unresolved",
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
        let now = identity_crypto::now_unix();
        let ttl = identity_crypto::DEFAULT_DELEGATION_SECONDS;
        let margin = identity_crypto::DEFAULT_RENEW_MARGIN_SECONDS;
        assert_eq!(margin, 7 * 86_400);
        assert!(!renew_minted(&layout, now).unwrap());
        assert!(!renew_minted(&layout, now + ttl - margin - 600).unwrap());
        assert!(renew_minted(&layout, now + ttl - margin + 600).unwrap());
    }

    #[test]
    fn device_only_folder_is_never_renewed() {
        let agent = TempIdentityDir::new(
            identity_crypto::now_unix(),
            identity_crypto::DEFAULT_DELEGATION_SECONDS,
        );
        let layout = identity_keyfile::IdentityLayout::split(
            &std::fs::canonicalize(tempfile::tempdir().unwrap().keep()).unwrap().join("no-root"),
            &std::fs::canonicalize(agent.path()).unwrap(),
        )
        .unwrap();
        assert!(!renew_minted(&layout, identity_crypto::now_unix() + 10 * identity_crypto::DEFAULT_DELEGATION_SECONDS).unwrap());
    }

    #[test]
    fn deny_paths_cover_a_minted_root_next_to_the_device_folder() {
        let machine = Path::new("/m/mi");
        assert_eq!(
            identity_deny_paths_with(Path::new("/g/identity"), machine),
            vec![PathBuf::from("/g/identity"), PathBuf::from("/g/identity-root"), PathBuf::from("/m/mi")]
        );
        assert_eq!(
            identity_deny_paths_with(Path::new("/handed/agent"), machine),
            vec![PathBuf::from("/handed/agent"), PathBuf::from("/m/mi")]
        );
    }

    #[test]
    fn env_value_joins_the_deny_paths_with_colons() {
        assert_eq!(
            deny_paths_env_value_with(Path::new("/g/identity"), Path::new("/m/mi")),
            "/g/identity:/g/identity-root:/m/mi"
        );
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| std::ffi::OsString::from(*v))
    }

    #[test]
    fn machine_dir_follows_the_library_resolution_order() {
        let both = env_of(&[("MACHINE_IDENTITY_DIR", "/x/m"), ("XDG_DATA_HOME", "/xdg"), ("HOME", "/h")]);
        assert_eq!(machine_dir_with(both).unwrap(), PathBuf::from("/x/m"));
        let xdg = env_of(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/h")]);
        assert_eq!(machine_dir_with(xdg).unwrap(), PathBuf::from("/xdg/machine-identity"));
        let home = env_of(&[("HOME", "/h")]);
        assert_eq!(machine_dir_with(home).unwrap(), PathBuf::from("/h/.local/share/machine-identity"));
    }

    #[test]
    fn an_unresolvable_machine_dir_is_a_coded_error_not_a_dropped_entry() {
        use coded_error::CodedError;
        let err = machine_dir_with(env_of(&[])).unwrap_err();
        let coded = coded_error::downcast_ref::<IdentityError>(&err).expect("typed IdentityError");
        assert_eq!(coded.code(), "gait.identity.machine_dir_unresolved");
        let rel = machine_dir_with(env_of(&[("HOME", "relative")])).unwrap_err();
        assert!(coded_error::downcast_ref::<IdentityError>(&rel).is_some());
    }

    #[test]
    fn the_migration_hint_shows_for_mode_and_owner_only() {
        let p = || PathBuf::from("/agents/one/device.jwk.json");
        for (source, want) in [
            (KeyfileError::InsecureDir(p()), "tighten the permissions (chmod 700 for a directory, chmod 600 for a file) and retry"),
            (KeyfileError::InsecureFile(p()), "tighten the permissions (chmod 700 for a directory, chmod 600 for a file) and retry"),
            (KeyfileError::WrongOwner(p()), "the path must be owned by the current user"),
        ] {
            let (_, text) = mapped(source);
            assert!(text.contains(want), "{text}");
        }
        for source in [KeyfileError::NotAFile(p()), KeyfileError::TooLarge { path: p(), cap: 10 }] {
            let (code, text) = mapped(source);
            assert_eq!(code, "gait.identity.identity_not_private");
            assert!(!text.contains("tighten the permissions") && !text.contains("owned by the current user"), "{text}");
            assert!(text.contains("/agents/one/device.jwk.json"), "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_identity_dir_is_refused_as_a_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        symlink(&target, &link).unwrap();
        for detail in [mode_detail(&link), owner_detail(&link)] {
            assert!(detail.contains("symlink"), "{detail}");
            assert!(detail.contains(&link.display().to_string()), "{detail}");
            assert!(!detail.contains("0777") && !detail.contains("mode 07"), "{detail}");
        }
        let (_, text) = mapped(KeyfileError::InsecureDir(link.clone()));
        assert!(text.contains("symlink") && text.contains(&link.display().to_string()), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn loading_through_a_symlinked_agent_dir_names_the_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = load_keys(&link).err().expect("a symlinked dir is refused");
        let text = format!("{err:#}");
        assert!(text.contains(&link.display().to_string()), "{text}");
        let coded = coded_error::downcast_ref::<IdentityError>(&err).expect("typed");
        let IdentityError::IdentityNotPrivate { detail, .. } = coded else {
            panic!("expected identity_not_private, got {coded:?}");
        };
        assert!(detail.contains("symlink"), "{detail}");
    }

    fn mapped(source: KeyfileError) -> (String, String) {
        use coded_error::CodedError;
        let err = IdentityError::from_keyfile(Path::new("/agents/one"), source);
        (err.code().to_string(), err.to_string())
    }

    #[test]
    fn path_carrying_keyfile_errors_map_to_a_coded_variant_naming_the_path() {
        let p = || PathBuf::from("/agents/one/device.jwk.json");
        let cases = [
            KeyfileError::InsecureDir(p()),
            KeyfileError::InsecureFile(p()),
            KeyfileError::WrongOwner(p()),
            KeyfileError::NotAFile(p()),
            KeyfileError::TooLarge { path: p(), cap: 10 },
        ];
        for source in cases {
            let label = format!("{source:?}");
            let (code, text) = mapped(source);
            assert_eq!(code, "gait.identity.identity_not_private", "{label}");
            assert!(text.contains("/agents/one/device.jwk.json"), "{label}: {text}");
        }
        // InvalidMode carries no path: the agent dir is named instead.
        let (code, text) = mapped(KeyfileError::InvalidMode(0o644));
        assert_eq!(code, "gait.identity.identity_not_private");
        assert!(text.contains("/agents/one"), "{text}");
    }

    #[test]
    fn orphan_root_maps_to_a_coded_variant_with_recovery_text() {
        let (code, text) = mapped(KeyfileError::OrphanRoot(PathBuf::from("/g/identity-root")));
        assert_eq!(code, "gait.identity.orphan_root");
        assert!(text.contains("/g/identity-root"), "{text}");
        assert!(text.contains("restore") && text.contains("move"), "{text}");
    }

    #[test]
    fn an_unmapped_keyfile_error_stays_the_generic_agent_dir_error() {
        let (code, text) = mapped(KeyfileError::OwnerRoot);
        assert_eq!(code, "gait.identity.agent_dir_unloadable");
        assert_eq!(text, "load agent identity from /agents/one");
    }

    #[test]
    fn mint_or_load_over_a_root_without_a_device_folder_is_the_orphan_root_error() {
        let gait = tempfile::tempdir().unwrap();
        IdentityContext::mint_or_load(gait.path()).unwrap();
        let base = std::fs::canonicalize(gait.path()).unwrap();
        std::fs::remove_dir_all(base.join("identity")).unwrap();
        let err = match IdentityContext::mint_or_load(gait.path()) {
            Ok(_) => panic!("a root without an identity must not be adopted"),
            Err(err) => err,
        };
        assert_eq!(coded_error::facts_anyhow(&err).code, "gait.identity.orphan_root");
        assert!(format!("{err:#}").contains("identity-root"), "{err:#}");
    }
}
