use anyhow::Context;
use gait_identity::{DirectVerifier, IdentityContext};
use gait_state::session::retrace::install_process_verifier;
use std::process::ExitCode;
use std::sync::Arc;

pub mod manual;
pub mod ops;

/// Record the error (code, class, fingerprint, never the text) unless the turn that
/// failed already did, print it, and return its exit code.
pub fn report(error: &anyhow::Error) -> ExitCode {
    ExitCode::from(report_code(error))
}

/// [`report`] for callers that hand back a plain `u8` exit code (the registry's
/// fallbacks do).
pub fn report_code(error: &anyhow::Error) -> u8 {
    #[cfg(feature = "telemetry")]
    if !gait_cli::cli::error::is_recorded(error) {
        concord_telemetry::error_anyhow!(error, source = "gait.cli");
    }
    eprintln!("{}", gait_cli::cli::error::text(error));
    coded_error::cli::exit_code_anyhow(error)
}

/// Like [`finish`], for a command that returns its exit code as a `u8`.
pub fn finish_code(result: anyhow::Result<u8>) -> ExitCode {
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => report(&error),
    }
}

pub fn finish(result: anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => report(&error),
    }
}

/// Runs the command line `raw` (program name first) on `registry`: the
/// command it names, or the turn as the registry's fallback. The shared tail
/// of `gait` and `gait-min`; the hidden role arguments are handled before it.
pub async fn run_registry(
    raw: Vec<String>,
    registry: op_registry::Registry<ops::GaitContext>,
) -> ExitCode {
    let mut argv: Vec<std::ffi::OsString> = raw.into_iter().map(std::ffi::OsString::from).collect();
    // A bare `gait` prints the help and exits 0, as it always has.
    if argv.len() == 1 {
        argv.push("--help".into());
    }
    op_registry::cli::run(&registry, argv, &ops::GaitHooks).await
}

/// The value of `--agent-dir` in the raw command line, as `--agent-dir PATH` or
/// `--agent-dir=PATH`; the last one wins, and nothing after a bare `--` counts.
pub fn agent_dir_arg(raw: &[String]) -> Option<std::path::PathBuf> {
    let args = raw.get(1..).unwrap_or_default();
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let args = &args[..end];
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--agent-dir" {
            if let Some(value) = args.get(i + 1) {
                found = Some(std::path::PathBuf::from(value));
                i += 1;
            }
        } else if let Some(value) = args[i].strip_prefix("--agent-dir=") {
            found = Some(std::path::PathBuf::from(value));
        }
        i += 1;
    }
    found
}

/// Makes `--agent-dir` visible to this process and to every child it spawns by
/// setting `GAIT_AGENT_DIR`. The registry also declares the flag, so help and
/// parsing accept it, but the environment is only safe to mutate while the
/// process is single-threaded: `main` calls this on the raw arguments before
/// any thread exists.
pub fn apply_agent_dir_flag(raw: &[String]) {
    if let Some(dir) = agent_dir_arg(raw) {
        std::env::set_var("GAIT_AGENT_DIR", dir);
    }
}

/// Installs the process-wide provenance verifier for the agent identity. No
/// identity means nothing is installed. An identity directory that cannot be
/// loaded is a startup error that names the directory, rather than a fold that
/// silently verifies nothing.
pub fn install_agent_identity(
    identity: anyhow::Result<Option<IdentityContext>>,
) -> anyhow::Result<()> {
    let Some(ctx) = identity.context("GAIT_AGENT_DIR is set but cannot be loaded")? else {
        return Ok(());
    };
    install_process_verifier(Arc::new(DirectVerifier::from_context(&ctx)?));
    Ok(())
}

pub use gait_core::telemetry::FlushGuard as TelemetryGuard;

/// Initialise process telemetry without ever disturbing `gait run`'s stderr
/// JSONL protocol (local output stays off). Telemetry is deliberately optional
/// for the lean binary.
pub fn init_telemetry(service: &'static str, agent_id: Option<String>) -> TelemetryGuard {
    gait_core::telemetry::init(service, agent_id, false);
    gait_core::telemetry::FlushGuard
}

/// Like [`init_telemetry`] but only records the setup: the sealed hand starts
/// its exporter (and its threads) after the seal, via
/// `gait_core::telemetry::init_deferred`.
pub fn defer_telemetry(service: &'static str, agent_id: Option<String>) -> TelemetryGuard {
    gait_core::telemetry::defer_init(service, agent_id);
    gait_core::telemetry::FlushGuard
}

/// Whether this invocation is the hand (`__run-tool`), which seals itself with
/// Landlock and so must start no thread before the seal.
pub fn is_hand_argv(raw: &[String]) -> bool {
    raw.get(1).map(String::as_str) == Some("__run-tool")
}

/// Process telemetry for this invocation: deferred for the hand (its exporter
/// threads would predate the seal and stay unconfined), eager otherwise. Shared
/// by `gait` and `gait-min` so the two entry points cannot drift.
pub fn start_telemetry(
    raw: &[String],
    default_service: &'static str,
    agent_id: Option<String>,
) -> TelemetryGuard {
    let service =
        gait_core::telemetry::service_for_argv(raw.get(1).map(String::as_str), default_service);
    if is_hand_argv(raw) {
        defer_telemetry(service, agent_id)
    } else {
        init_telemetry(service, agent_id)
    }
}

/// The tokio runtime for this invocation. The hand seals its own process with
/// Landlock, which confines only the sealing thread and the children it creates
/// afterwards, so it runs on the main thread alone (a multi-thread runtime
/// would start unconfined workers first).
pub fn build_runtime(raw: &[String]) -> tokio::runtime::Runtime {
    let mut builder = if is_hand_argv(raw) {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    // The turn runs on a blocking thread (see `ops::turn::on_turn_thread`); tokio's
    // default 2 MiB stack overflows a debug build's deep turn future, which used
    // to run on the 8 MiB main thread.
    builder
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .expect("failed to start the tokio runtime")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn agent_dir_flag_sets_gait_agent_dir_env() {
        let value = format!("/nonexistent/gait-agent-dir-flag-{}", std::process::id());
        let previous = std::env::var_os("GAIT_AGENT_DIR");
        let raw: Vec<String> = ["gait", "sessions", "--agent-dir", value.as_str()]
            .map(String::from)
            .to_vec();
        apply_agent_dir_flag(&raw);
        let observed = std::env::var_os("GAIT_AGENT_DIR");
        match previous {
            Some(v) => std::env::set_var("GAIT_AGENT_DIR", v),
            None => std::env::remove_var("GAIT_AGENT_DIR"),
        }
        assert_eq!(observed.as_deref(), Some(std::ffi::OsStr::new(&value)));
    }

    #[test]
    fn unloadable_agent_dir_is_a_clear_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-agent");
        let err = install_agent_identity(IdentityContext::new(&missing).map(Some)).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&missing.display().to_string()),
            "the startup error names the directory: {text}"
        );
    }

    #[test]
    fn no_agent_identity_installs_nothing_and_succeeds() {
        install_agent_identity(Ok(None)).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn init_telemetry_without_endpoint_is_silent_and_a_second_init_is_refused() {
        let previous = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        let _first = init_telemetry("gait-test", None);
        // A global subscriber is installed by now (by the first call, or by
        // whoever got there first), so this init is refused: it reports on
        // stderr, keeps running, and hands nothing to gait-core.
        let _second = init_telemetry("gait-test", Some("agent".to_string()));
        match previous {
            Some(value) => std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", value),
            None => std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT"),
        }
    }

    #[test]
    fn the_hand_runs_on_a_current_thread_runtime_and_other_roles_on_multi_thread() {
        use tokio::runtime::RuntimeFlavor;
        let flavor = |arg: &str| {
            let raw = vec!["gait".to_string(), arg.to_string()];
            build_runtime(&raw)
                .block_on(async { tokio::runtime::Handle::current().runtime_flavor() })
        };
        assert!(is_hand_argv(&["gait".into(), "__run-tool".into()]));
        assert!(!is_hand_argv(&["gait".into(), "__brain".into()]));
        assert_eq!(flavor("__run-tool"), RuntimeFlavor::CurrentThread);
        assert_eq!(flavor("__brain"), RuntimeFlavor::MultiThread);
    }

    #[test]
    fn identity_subcommand_is_gone() {
        let registry = ops::registry();
        assert!(!op_registry::cli::command(&registry)
            .get_subcommands()
            .any(|c| c.get_name() == "identity"));
    }

    #[test]
    fn agent_dir_is_read_from_raw_argv_in_both_spellings() {
        let raw = |args: &[&str]| -> Vec<String> {
            std::iter::once("gait")
                .chain(args.iter().copied())
                .map(String::from)
                .collect()
        };
        let path = |p: &str| Some(std::path::PathBuf::from(p));
        assert_eq!(
            agent_dir_arg(&raw(&["--agent-dir", "/a", "task"])),
            path("/a")
        );
        assert_eq!(
            agent_dir_arg(&raw(&["sessions", "--agent-dir=/b"])),
            path("/b")
        );
        assert_eq!(agent_dir_arg(&raw(&["--", "--agent-dir", "/c"])), None);
        assert_eq!(agent_dir_arg(&raw(&["task"])), None);
    }

    #[test]
    fn finish_maps_ok_code_through() {
        assert_eq!(finish(Ok(ExitCode::from(12))), ExitCode::from(12));
    }

    #[test]
    fn finish_maps_logical_error_to_65() {
        assert_eq!(
            finish(Err(gait_cli::cli::error::CliError::not_found("missing"))),
            ExitCode::from(65)
        );
    }

    #[test]
    fn finish_maps_infrastructure_error_to_69() {
        assert_eq!(
            finish(Err(gait_cli::cli::error::CliError::failed("broken"))),
            ExitCode::from(69)
        );
    }

    #[test]
    fn finish_maps_uncoded_error_to_70() {
        assert_eq!(finish(Err(anyhow::anyhow!("unknown"))), ExitCode::from(70));
    }

    #[test]
    fn no_process_exit_left_in_cli_crates() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }

        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let mut files = Vec::new();
        for relative in ["gait-cli/src", "gait-repl/src"] {
            rust_files(&crates.join(relative), &mut files);
        }
        files.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"));
        files.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/min_main.rs"));
        let offenders: Vec<_> = files
            .iter()
            .filter_map(|path| {
                let source = std::fs::read_to_string(path).unwrap();
                source
                    .lines()
                    .any(|line| {
                        let code = line.split("//").next().unwrap_or("");
                        code.contains("process::exit")
                    })
                    .then_some(path.display().to_string())
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "process::exit remains in: {offenders:?}"
        );
    }
}
