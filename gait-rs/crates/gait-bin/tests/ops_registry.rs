//! The op-registry skeleton: globals, context, hooks and an empty registry.
use gait_bin::ops::{registry, GaitContext, GaitHooks, Globals};
use op_registry::cli::{run_with_io, Io};
use op_registry::{OpGlobals, Ref, Resolve, Scope};
use serial_test::serial;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

fn argv(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn run(args: &[&str]) -> (u8, String, String) {
    let (io, captured) = Io::memory(b"", false, false);
    let code = futures_executor_block(run_with_io(&registry(), argv(args), &GaitHooks, io));
    (code, captured.stdout(), captured.stderr())
}

fn futures_executor_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn globals_from(args: &[&str]) -> Globals {
    let cmd = <Globals as OpGlobals>::add_args(clap::Command::new("gait"));
    let matches = cmd.try_get_matches_from(args).unwrap();
    <Globals as OpGlobals>::from_matches(&matches).unwrap()
}

#[test]
fn globals_have_no_machine_paths_in_help() {
    let (code, stdout, _) = run(&["gait", "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("--gait-dir"), "{stdout}");
    assert!(stdout.contains("--agent-dir"), "{stdout}");
    for banned in ["/Users/", "/home/"] {
        assert!(!stdout.contains(banned), "help names a machine path: {stdout}");
    }
}

#[test]
#[serial]
fn help_never_prints_the_env_value_of_gait_dir() {
    std::env::set_var("GAIT_DIR", "/sentinel/env/gait-dir");
    let (_, stdout, _) = run(&["gait", "--help"]);
    std::env::remove_var("GAIT_DIR");
    assert!(!stdout.contains("/sentinel/env/gait-dir"), "{stdout}");
}

#[test]
fn exit_code_is_2_for_a_usage_error() {
    // `--no-such-flag-xyz` is the turn's now (see turn_fallback.rs); two
    // contradicting globals are the runner's own usage error.
    let (code, _, stderr) = run(&["gait", "--pretty", "--compact"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    let last = stderr.lines().last().unwrap_or("");
    let value: serde_json::Value = serde_json::from_str(last).expect("the error is a JSON line");
    assert!(value["error"]["code"].is_string(), "{last}");
}

fn write_gaitref(dir: &Path, target: &Path) {
    std::fs::write(dir.join(".gaitref"), format!("gaitdir: {}\n", target.display())).unwrap();
}

#[test]
#[serial]
fn gait_dir_resolves_from_flag_env_and_gaitref() {
    let work = tempfile::tempdir().unwrap();
    let flag_dir = work.path().join("flag");
    let env_dir = work.path().join("env");
    let ref_dir = work.path().join("ref");
    std::fs::create_dir_all(&ref_dir).unwrap();
    write_gaitref(work.path(), &ref_dir);

    // The flag wins over the environment.
    std::env::set_var("GAIT_DIR", &env_dir);
    let both = globals_from(&["gait", "--gait-dir", flag_dir.to_str().unwrap()]);
    let ctx = <GaitContext as op_registry::CliContext>::from_globals(&both).unwrap();
    assert_eq!(ctx.gait_dir_from(work.path()).unwrap(), flag_dir);

    // The environment is read through the global when the flag is absent.
    let env_only = globals_from(&["gait"]);
    assert_eq!(env_only.gait_dir, Some(env_dir.clone()));
    let ctx = <GaitContext as op_registry::CliContext>::from_globals(&env_only).unwrap();
    assert_eq!(ctx.gait_dir_from(work.path()).unwrap(), env_dir);

    // Neither: the .gaitref walk-up from a subdirectory.
    std::env::remove_var("GAIT_DIR");
    let sub: PathBuf = work.path().join("a/b");
    std::fs::create_dir_all(&sub).unwrap();
    let none = globals_from(&["gait"]);
    assert_eq!(none.gait_dir, None);
    let ctx = <GaitContext as op_registry::CliContext>::from_globals(&none).unwrap();
    assert_eq!(ctx.gait_dir_from(&sub).unwrap(), ref_dir);
}

#[test]
fn from_globals_does_no_io_even_for_a_missing_gait_dir() {
    let globals = Globals { gait_dir: Some(PathBuf::from("/nonexistent/x")), agent_dir: None };
    assert!(<GaitContext as op_registry::CliContext>::from_globals(&globals).is_ok());
}

fn not_found_code(err: &anyhow::Error) -> &'static str {
    coded_error::facts_anyhow(err).code
}

#[test]
fn resolver_gives_the_apps_not_found_for_missing_objects() {
    let dir = tempfile::tempdir().unwrap();
    let globals = Globals { gait_dir: Some(dir.path().to_path_buf()), agent_dir: None };
    let ctx = <GaitContext as op_registry::CliContext>::from_globals(&globals).unwrap();
    for kind in ["inference", "mode", "daemon"] {
        let err = futures_executor_block(ctx.resolve(kind, &Ref::new("nosuch"), &Scope::default()))
            .expect_err(kind);
        assert_eq!(not_found_code(&err), "gait.op.not_found", "{kind}: {err:#}");
    }
    let err = futures_executor_block(ctx.resolve("widget", &Ref::new("x"), &Scope::default()))
        .expect_err("unknown kind");
    assert!(format!("{err:#}").contains("widget"));
}
