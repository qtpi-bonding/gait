//! gait-acp binary — delegates to the `gait_acp` library. Also answers to the
//! same sealed role tokens (`__brain`, `__run-tool`, `__daemon-exit`) that the
//! `gait` binary does: `gait_turn_runner`'s brain-launch path spawns
//! `current_exe() __brain ...`, and when a turn is driven from inside
//! `gait-acp` (not `gait`), `current_exe()` resolves to this binary. Without
//! this pre-check, that spawn would produce a `gait-acp` child with no role
//! dispatch at all, which just reran `gait_acp::run()` and misread the
//! brain's binary Nerve init frame as malformed ACP JSON-RPC.
//!
//! `--require-attestation`, (repeatable) `--trusted-deliverer p256:<hex>` and
//! `--state-dir <path>` are the process-level attestation policy --
//! deliberately parsed here, not read from any runtime `_meta` claim, since
//! the trust boundary has to be rooted in how the process itself was launched
//! (e.g. by `structmuxd`). A trusted deliverer is named by its PRINCIPAL id;
//! the delivery proves the deliverer's device through its root-to-device
//! delegation. The delivery replay and dedupe state live under the state dir
//! (default `$GAIT_DIR`). The agent identity comes from `GAIT_AGENT_DIR`, read
//! by `IdentityContext`. Unrecognized argv is left alone (no clap dependency
//! here today), except the removed flags, which are refused.
use std::path::PathBuf;
use std::sync::Arc;

use gait_acp::attestation::Attestor;
use identity_crypto::PrincipalId;

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.acp.config")]
enum ConfigError {
    #[error("--trusted-deliverer needs a principal id (p256:<hex>)")]
    #[coded(code = "trusted_deliverer_missing", class = Logical)]
    TrustedDelivererMissing,
    #[error("--trusted-deliverer takes a principal id (p256:<hex>), not {0:?}")]
    #[coded(code = "trusted_deliverer_invalid", class = Logical)]
    TrustedDelivererInvalid(String),
    #[error("{0} was removed; use --trusted-deliverer p256:<hex>")]
    #[coded(code = "removed_flag", class = Logical)]
    RemovedFlag(String),
    #[error("--state-dir needs a directory path")]
    #[coded(code = "state_dir_missing", class = Logical)]
    StateDirMissing,
    #[error("--require-attestation needs an agent identity: set GAIT_AGENT_DIR to the agent directory")]
    #[coded(code = "attestation_needs_identity", class = Logical)]
    AttestationNeedsIdentity,
    #[error("--require-attestation needs a state directory: pass --state-dir or set GAIT_DIR")]
    #[coded(code = "attestation_needs_state_dir", class = Logical)]
    AttestationNeedsStateDir,
}

/// What the process-level attestation flags say.
#[derive(Debug, Default, PartialEq)]
struct AttestationFlags {
    require_attestation: bool,
    trusted_deliverers: Vec<PrincipalId>,
    state_dir: Option<PathBuf>,
}

fn parse_attestation_flags(argv: &[String]) -> anyhow::Result<AttestationFlags> {
    let mut flags = AttestationFlags::default();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--require-attestation" => flags.require_attestation = true,
            "--trusted-deliverer" => {
                let Some(value) = argv.get(i + 1) else {
                    return Err(coded_error::into_anyhow(ConfigError::TrustedDelivererMissing));
                };
                let principal = PrincipalId::parse(value).map_err(|_| {
                    coded_error::into_anyhow(ConfigError::TrustedDelivererInvalid(value.clone()))
                })?;
                flags.trusted_deliverers.push(principal);
                i += 1;
            }
            "--state-dir" => {
                let Some(value) = argv.get(i + 1) else {
                    return Err(coded_error::into_anyhow(ConfigError::StateDirMissing));
                };
                flags.state_dir = Some(PathBuf::from(value));
                i += 1;
            }
            removed @ ("--trusted-multimatrix-key" | "--trusted-recipient-key") => {
                return Err(coded_error::into_anyhow(ConfigError::RemovedFlag(removed.to_owned())));
            }
            _ => {}
        }
        i += 1;
    }
    Ok(flags)
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
    let flags = match parse_attestation_flags(&raw[1..]) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("{error:#}");
            return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
        }
    };
    let require_attestation = flags.require_attestation;
    let state_dir = flags
        .state_dir
        .clone()
        .or_else(|| std::env::var_os("GAIT_DIR").map(PathBuf::from));
    if require_attestation && state_dir.is_none() {
        let error = coded_error::into_anyhow(ConfigError::AttestationNeedsStateDir);
        eprintln!("{error:#}");
        return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
    }
    if require_attestation && identity.is_none() {
        let error = coded_error::into_anyhow(ConfigError::AttestationNeedsIdentity);
        eprintln!("{error:#}");
        return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
    }
    if let Some(ctx) = identity.as_ref() {
        gait_state::session::retrace::install_process_self_id(ctx.agent_id().to_string());
    }
    let attestor = match (identity.as_ref(), state_dir.as_deref()) {
        (Some(ctx), Some(state_dir)) => match Attestor::open(
            ctx.agent_dir(),
            state_dir,
            flags.trusted_deliverers.clone(),
        ) {
            Ok(attestor) => Some(Arc::new(attestor)),
            Err(error) => {
                eprintln!("{error:#}");
                return std::process::ExitCode::from(coded_error::cli::exit_code_anyhow(&error));
            }
        },
        _ => None,
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
    use identity_crypto::jwk_duo::GenerationService;
    use identity_crypto::principal_id;

    fn principal() -> String {
        principal_id(&GenerationService::generate_key_duo()).to_string()
    }

    fn code(error: &anyhow::Error) -> String {
        coded_error::facts_anyhow(error).code.to_string()
    }

    #[test]
    fn parses_require_attestation_flag() {
        let flags = parse_attestation_flags(&["--require-attestation".to_string()]).unwrap();
        assert!(flags.require_attestation);
        assert!(flags.trusted_deliverers.is_empty());
    }

    #[test]
    fn parses_repeated_trusted_deliverers() {
        let (a, b) = (principal(), principal());
        let argv: Vec<String> = ["--trusted-deliverer", &a, "--trusted-deliverer", &b]
            .into_iter()
            .map(String::from)
            .collect();
        let flags = parse_attestation_flags(&argv).unwrap();
        let got: Vec<String> = flags.trusted_deliverers.iter().map(|p| p.to_string()).collect();
        assert_eq!(got, vec![a, b], "order kept");
    }

    #[test]
    fn jwk_path_value_is_rejected() {
        let error = parse_attestation_flags(&[
            "--trusted-deliverer".into(),
            "/service-key-public/central.jwk".into(),
        ])
        .expect_err("a file path is not a principal");
        assert_eq!(code(&error), "gait.acp.config.trusted_deliverer_invalid");
    }

    #[test]
    fn malformed_principal_is_rejected() {
        for bad in ["p256:", "p256:zz", "p256:00", ""] {
            let error = parse_attestation_flags(&["--trusted-deliverer".into(), bad.into()])
                .expect_err("a malformed principal must fail");
            assert_eq!(code(&error), "gait.acp.config.trusted_deliverer_invalid", "{bad:?}");
        }
    }

    #[test]
    fn trusted_deliverer_needs_a_value() {
        let error = parse_attestation_flags(&["--trusted-deliverer".into()]).unwrap_err();
        assert_eq!(code(&error), "gait.acp.config.trusted_deliverer_missing");
    }

    #[test]
    fn removed_flags_are_refused() {
        for flag in ["--trusted-multimatrix-key", "--trusted-recipient-key"] {
            let error = parse_attestation_flags(&[flag.into(), "x".into()])
                .expect_err("a removed flag must fail");
            assert_eq!(code(&error), "gait.acp.config.removed_flag", "{flag}");
        }
    }

    #[test]
    fn parses_state_dir() {
        let flags = parse_attestation_flags(&["--state-dir".into(), "/var/gait".into()]).unwrap();
        assert_eq!(flags.state_dir, Some(PathBuf::from("/var/gait")));
        let error = parse_attestation_flags(&["--state-dir".into()]).unwrap_err();
        assert_eq!(code(&error), "gait.acp.config.state_dir_missing");
    }

    #[test]
    fn no_attestation_flags_yields_permissive_defaults() {
        assert_eq!(parse_attestation_flags(&[]).unwrap(), AttestationFlags::default());
    }

    #[test]
    fn config_error_codes_are_unique() {
        coded_error::check_codes("gait.acp.config.", &[ConfigError::ALL_CODES]).unwrap();
    }
}
