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

use gait_acp::attestation::{Attestor, load_trusted_key};
use identity_crypto::PublicKey;

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.acp.config")]
enum ConfigError {
    #[error("--trusted-multimatrix-key needs the path of a public key-duo JWK")]
    #[coded(code = "trusted_key_path_missing", class = Logical)]
    TrustedKeyPathMissing,
    #[error("--require-attestation needs an agent identity: set GAIT_AGENT_DIR to the agent directory")]
    #[coded(code = "attestation_needs_identity", class = Logical)]
    AttestationNeedsIdentity,
}

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
                    return Err(coded_error::into_anyhow(ConfigError::TrustedKeyPathMissing));
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

fn main() -> std::process::ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    // The identity and process verifier are installed BEFORE role dispatch: a
    // turn spawns `current_exe() __brain`, and the brain folds signed events.
    let identity = match gait_identity::IdentityContext::from_env() {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("{error:#}");
            return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
        }
    };
    if let Some(ctx) = &identity {
        let verifier = match gait_identity::DirectVerifier::from_context(ctx) {
            Ok(verifier) => verifier,
            Err(error) => {
                eprintln!("{error:#}");
                return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
            }
        };
        gait_state::session::retrace::install_process_verifier(Arc::new(verifier));
    }
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()
            .expect("failed to start the tokio runtime");
        runtime.block_on(async { gait_core::__dispatch_role(&raw[1..]).await });
        return std::process::ExitCode::SUCCESS;
    }
    let (require_attestation, trusted_keys) = match parse_attestation_flags(&raw[1..]) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("{error:#}");
            return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
        }
    };
    if require_attestation && identity.is_none() {
        let error = coded_error::into_anyhow(ConfigError::AttestationNeedsIdentity);
        eprintln!("{error:#}");
        return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
    }
    let attestor = match identity.as_ref() {
        Some(ctx) => match Attestor::open(ctx.agent_dir(), trusted_keys) {
            Ok(attestor) => Some(Arc::new(attestor)),
            Err(error) => {
                eprintln!("{error:#}");
                return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
            }
        },
        None => None,
    };
    let telemetry = {
        let mut config = concord_telemetry::Config::new("gait-acp", env!("CARGO_PKG_VERSION"));
        config.local = concord_telemetry::LocalOutput::Json;
        config.principal = identity.as_ref().map(|ctx| concord_telemetry::Principal::new(
            concord_telemetry::PrincipalKind::Agent, ctx.agent_id().to_string()));
        match concord_telemetry::init(config) {
            Ok(guard) => Some(guard),
            Err(error) => {
                eprintln!("failed to initialise telemetry: {error}");
                None
            }
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()
        .expect("failed to start the tokio runtime");
    let result = runtime.block_on(async move {
        tokio::select! {
            result = gait_acp::run_with_attestation(require_attestation, attestor) => result,
            _ = async {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
                let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("SIGINT handler");
                tokio::select! { _ = term.recv() => (), _ = interrupt.recv() => () }
            } => return Ok(()),
        }
    });
    drop(telemetry);
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error))
        }
    }
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

    #[test]
    fn attestation_flag_errors_are_coded() {
        let missing_path = parse_attestation_flags(&["--trusted-multimatrix-key".into()])
            .expect_err("missing trusted key path must fail");
        let missing_identity = coded_error::into_anyhow(ConfigError::AttestationNeedsIdentity);
        let path_error = coded_error::downcast_ref::<ConfigError>(&missing_path)
            .expect("missing path retains ConfigError");
        let identity_error = coded_error::downcast_ref::<ConfigError>(&missing_identity)
            .expect("missing identity retains ConfigError");
        assert_eq!(path_error.to_string(), "--trusted-multimatrix-key needs the path of a public key-duo JWK");
        assert_eq!(identity_error.to_string(), "--require-attestation needs an agent identity: set GAIT_AGENT_DIR to the agent directory");
        assert_eq!(coded_error::facts_anyhow(&missing_path).code, "gait.acp.config.trusted_key_path_missing");
        assert_eq!(coded_error::facts_anyhow(&missing_identity).code, "gait.acp.config.attestation_needs_identity");
    }

    #[test]
    fn config_error_codes_are_unique() {
        use coded_error::CodedError;
        coded_error::check_codes("gait.acp.config.", &[ConfigError::ALL_CODES]).unwrap();
        assert_ne!(ConfigError::TrustedKeyPathMissing.code(), ConfigError::AttestationNeedsIdentity.code());
    }
}
