//! gait-acp binary — delegates to the `gait_acp` library. Also answers to the
//! same sealed role tokens (`__brain`, `__run-tool`, `__daemon-exit`) that the
//! `gait` binary does: `gait_turn_runner`'s brain-launch path spawns
//! `current_exe() __brain ...`, and when a turn is driven from inside
//! `gait-acp` (not `gait`), `current_exe()` resolves to this binary. Without
//! this pre-check, that spawn would produce a `gait-acp` child with no role
//! dispatch at all, which just reran `gait_acp::run()` and misread the
//! brain's binary Nerve init frame as malformed ACP JSON-RPC.
//!
//! `--require-attestation` and (repeatable) `--trusted-multimatrix-key
//! <path>` are the process-level attestation policy -- deliberately parsed
//! here, not read from any runtime `_meta` claim, since the trust boundary has
//! to be rooted in how the process itself was launched (e.g. by
//! `multimatrix-bridge`). The key path names a PUBLIC key-duo JWK file. The
//! agent identity comes from `GAIT_AGENT_DIR`, read by `IdentityContext`. Unrecognized argv is left alone
//! (no clap dependency here today).
use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use gait_acp::attestation::{Attestor, load_trusted_key};
use identity_crypto::PublicKey;

/// Returns `(require_attestation, trusted delivery keys)`. Each key is read from
/// its JWK file, so a missing or private-key file is an error here, at startup.
fn parse_attestation_flags(
    argv: &[String],
) -> anyhow::Result<(bool, Vec<(String, PublicKey)>)> {
    let mut require_attestation = false;
    let mut trusted_keys = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--require-attestation" => require_attestation = true,
            "--trusted-multimatrix-key" => {
                let Some(path) = argv.get(i + 1) else {
                    bail!("--trusted-multimatrix-key needs the path of a public key-duo JWK");
                };
                trusted_keys.push(load_trusted_key(Path::new(path))?);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    Ok((require_attestation, trusted_keys))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().collect();
    // The identity and process verifier are installed BEFORE role dispatch: a
    // turn spawns `current_exe() __brain`, and the brain folds signed events.
    let identity = gait_identity::IdentityContext::from_env()?;
    if let Some(ctx) = &identity {
        gait_state::session::retrace::install_process_verifier(Arc::new(
            gait_identity::DirectVerifier::from_context(ctx)?,
        ));
    }
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return Ok(());
    }
    let (require_attestation, trusted_keys) = parse_attestation_flags(&raw[1..])?;
    if require_attestation && identity.is_none() {
        bail!("--require-attestation needs an agent identity: set GAIT_AGENT_DIR to the agent directory");
    }
    let attestor = match identity {
        Some(ctx) => Some(Arc::new(Attestor::open(ctx.agent_dir(), trusted_keys)?)),
        None => None,
    };
    gait_acp::run_with_attestation(require_attestation, attestor).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity_crypto::jwk_duo::{GenerationService, KeyDuoSerializer};

    fn public_jwk_file(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        let duo = GenerationService::generate_key_duo();
        std::fs::write(
            &path,
            KeyDuoSerializer::export_public_key_duo_jwk(&duo).unwrap(),
        )
        .unwrap();
        path.display().to_string()
    }

    #[test]
    fn parses_require_attestation_flag() {
        let argv = vec!["--require-attestation".to_string()];
        let (require, keys) = parse_attestation_flags(&argv).unwrap();
        assert!(require);
        assert!(keys.is_empty());
    }

    #[test]
    fn parses_repeated_trusted_multimatrix_keys() {
        let dir = tempfile::tempdir().unwrap();
        let first = public_jwk_file(dir.path(), "mm-key-1.jwk.json");
        let second = public_jwk_file(dir.path(), "mm-key-2.jwk.json");
        let argv = vec![
            "--trusted-multimatrix-key".to_string(),
            first,
            "--trusted-multimatrix-key".to_string(),
            second,
        ];
        let (require, keys) = parse_attestation_flags(&argv).unwrap();
        assert!(!require);
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0].0, keys[1].0, "each key is keyed by its own thumbprint");
        assert_eq!(keys[0].0, keys[0].1.thumbprint());
        assert_eq!(keys[1].0, keys[1].1.thumbprint());
    }

    #[test]
    fn missing_trusted_key_file_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.jwk.json").display().to_string();
        let argv = vec!["--trusted-multimatrix-key".to_string(), missing.clone()];
        let error = parse_attestation_flags(&argv)
            .err()
            .expect("a missing key file must not start the process");
        assert!(
            format!("{error:#}").contains(&missing),
            "the error names the missing file: {error:#}"
        );
    }

    #[test]
    fn no_attestation_flags_yields_permissive_defaults() {
        let argv: Vec<String> = vec![];
        let (require, keys) = parse_attestation_flags(&argv).unwrap();
        assert!(!require);
        assert!(keys.is_empty());
    }
}
