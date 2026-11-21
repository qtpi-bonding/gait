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

// ---- session ops (Task 6) ----

mod session_ops {
    use super::*;
    use gait_bin::ops::decl::ops;
    use gait_bin::ops::session::{SessionId, SessionsParams};
    use op_registry::CallOptions;

    fn gait_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new(env!("CARGO_BIN_EXE_gait"))
            .arg("init")
            .arg("--gait-dir")
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        dir
    }

    fn ctx(dir: &Path) -> GaitContext {
        let globals = Globals { gait_dir: Some(dir.to_path_buf()), agent_dir: None };
        <GaitContext as op_registry::CliContext>::from_globals(&globals).unwrap()
    }

    #[test]
    fn sessions_is_a_bare_array_of_camel_case_rows() {
        let dir = gait_dir();
        let rows = futures_executor_block(registry().call_typed::<ops::Sessions>(
            &ctx(dir.path()),
            SessionsParams { last: Some(3) },
            CallOptions::default(),
        ))
        .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn stats_of_an_unknown_session_is_not_found() {
        let dir = gait_dir();
        let err = futures_executor_block(registry().call_typed::<ops::Stats>(
            &ctx(dir.path()),
            SessionId { id: Ref::new("nosuch") },
            CallOptions::default(),
        ))
        .unwrap_err();
        assert_eq!(not_found_code(err.error()), "gait.op.not_found", "{err:#}");
    }

    #[test]
    fn deny_of_an_unknown_session_is_not_found() {
        let dir = gait_dir();
        let err = futures_executor_block(registry().call_typed::<ops::Deny>(
            &ctx(dir.path()),
            SessionId { id: Ref::new("nosuch") },
            CallOptions::default(),
        ))
        .unwrap_err();
        assert_eq!(not_found_code(err.error()), "gait.op.not_found", "{err:#}");
    }

    #[test]
    fn the_session_ops_are_in_the_registry_and_stop_is_not() {
        let registry = registry();
        for name in [
            "sessions", "ps", "stats", "log", "undo", "persist", "discard", "allow", "deny",
            "unlock", "reply",
        ] {
            assert!(registry.get(name).is_some(), "missing op {name}");
        }
        assert!(registry.get("stop").is_none());
    }
}

/// `init` and `link` are `local:` ops: they run through the registry with no connection,
/// no daemon and no resolvable gait-dir beforehand.
#[test]
#[serial]
fn assert_local_ops_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("ws");
    let d = dir.to_str().unwrap();
    let cwd = tmp.path().join("cwd");
    std::fs::create_dir(&cwd).unwrap();
    let (code, _out, err) = run(&["gait", "init", "--gait-dir", d]);
    assert_eq!(code, 0, "{err}");
    assert!(dir.join(".git").exists());
    let before = std::env::current_dir().unwrap();
    std::env::set_current_dir(&cwd).unwrap();
    let (code, _out, err) = run(&["gait", "link", "--gait-dir", d]);
    std::env::set_current_dir(before).unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(cwd.join(".gaitref").exists());
}

// ---- the library's test kit and the help gate (Task 10) ----

mod kit {
    use super::*;
    use gait_bin::manual::{manual, ManualFlavor};
    use op_registry::testing::{self, Casing, Fixture, HelpRules, Mount};
    use serde_json::json;

    const CANONICAL: &str = include_str!("canonical_ops.txt");

    fn full_manual() -> op_registry::manual::Manual {
        manual(&registry(), ManualFlavor::Full, gait_repl::repl::commands::COMMANDS)
            .expect("the manual manifest is valid")
    }

    /// A fresh gait dir for the fixtures and the context.
    fn gait_dir() -> (tempfile::TempDir, GaitContext) {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new(env!("CARGO_BIN_EXE_gait"))
            .arg("init")
            .arg("--gait-dir")
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(init.status.success());
        let globals = Globals { gait_dir: Some(dir.path().to_path_buf()), agent_dir: None };
        let ctx = <GaitContext as op_registry::CliContext>::from_globals(&globals).unwrap();
        (dir, ctx)
    }

    #[test]
    fn assert_parity() {
        let no_mounts: [Mount<'_>; 0] = [];
        testing::assert_parity(&registry(), &no_mounts);
    }

    #[test]
    fn assert_help_complete() {
        let rules = HelpRules {
            // `daemon` is not banned: gait's `daemons` command is real.
            banned_words: &["round-", "Task ", "/Users/", "/home/", "gait_ops", "Mirrors"],
        };
        let warnings = testing::assert_help_complete(&registry(), &rules);
        for warning in warnings {
            eprintln!("help warning: {warning}");
        }
    }

    #[test]
    fn assert_manual_snapshot() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/manual_snapshot.md");
        testing::assert_manual_snapshot(&registry(), &full_manual(), &path);
    }

    #[test]
    fn assert_exit_table() {
        testing::assert_exit_table(&registry(), &full_manual());
    }

    #[test]
    fn assert_error_codes() {
        let (_dir, ctx) = gait_dir();
        let fixtures = [
            Fixture { op: "stats", args: json!({"id": "nosuch"}) },
            Fixture { op: "log", args: json!({"id": "nosuch"}) },
            Fixture { op: "inference_show", args: json!({"id": "nosuch"}) },
            Fixture { op: "mode_show", args: json!({"id": "nosuch"}) },
            Fixture { op: "daemons_stop", args: json!({"id": "nosuch"}) },
            Fixture { op: "config_get", args: json!({"id": "nosuch"}) },
            Fixture { op: "unregister", args: json!({"kind": "mcp", "id": "nosuch"}) },
            Fixture { op: "enable", args: json!({"kind": "mcp", "id": "nosuch"}) },
            Fixture { op: "auth_status", args: json!({"id": "nosuch"}) },
        ];
        futures_executor_block(testing::assert_error_codes(&registry(), &ctx, &fixtures));
    }

    /// Runs the ignored test `name` of this file in a child process with stdin closed: `auth add`
    /// reads its key from stdin, and the kit calls it with sample arguments.
    fn in_child(name: &str) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("kit::{name}"), "--ignored", "--nocapture", "--test-threads=1"])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn assert_secrets_not_echoed() {
        in_child("child_secrets_not_echoed");
    }

    #[test]
    #[ignore = "run by assert_secrets_not_echoed in a child process"]
    fn child_secrets_not_echoed() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("key.pem");
        std::fs::write(&key_file, "KEYLINE-gait-registry-4417\n").unwrap();
        let sentinel = testing::SecretSentinel::new("sk-SENTINEL1234567890", &key_file);
        let manual = full_manual();
        futures_executor_block(testing::assert_secrets_not_echoed(&registry(), Some(&manual), &sentinel));
    }

    #[test]
    fn assert_canonical() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/canonical_ops.txt");
        if std::env::var("UPDATE_SNAPSHOTS").is_ok_and(|v| !v.is_empty() && v != "0") {
            let reg = registry();
            let mut names: Vec<&str> = reg.names();
            names.sort_unstable();
            std::fs::write(&path, names.join("\n") + "\n").unwrap();
        }
        let canonical: Vec<&str> = CANONICAL.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        testing::assert_canonical(&registry(), &canonical);
    }

    /// Ops whose `--id` names something to create, or whose missing-object answer is a value by
    /// design (listed in OR-notes-t10.txt). The kit expects `not_found` from every op with an `id`.
    const NOT_A_LOOKUP: &[&str] = &[
        "auth_add",        // stores a key under a provider name
        "auth_link",       // links a provider to an external source
        "auth_login",      // starts a sign-in
        "auth_logout",     // signs out of the supported providers only
        "auth_remove",     // idempotent: removing an absent key is success
        "auth_unlink",     // idempotent
        "auth_status",     // an unknown provider is `{"status": null}`
        "inference_add",   // creates a profile
        "mode_add",        // creates a mode
        "config_set",      // sets a key
        "register",        // creates an extension
    ];

    #[test]
    fn assert_unknown_ref_not_found() {
        in_child("child_unknown_ref_not_found");
    }

    #[test]
    #[ignore = "run by assert_unknown_ref_not_found in a child process"]
    fn child_unknown_ref_not_found() {
        let (_dir, ctx) = gait_dir();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures_executor_block(testing::assert_unknown_ref_not_found(&registry(), &ctx, &[]));
        }));
        let Err(payload) = result else { return };
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        let offenders: Vec<&str> = message
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("- "))
            .filter_map(|l| l.split(':').next())
            .collect();
        assert!(!offenders.is_empty(), "unexpected failure: {message}");
        let unexpected: Vec<&&str> = offenders.iter().filter(|o| !NOT_A_LOOKUP.contains(*o)).collect();
        assert!(unexpected.is_empty(), "ops that should be not_found: {unexpected:?}\n{message}");
        let silent: Vec<&&str> = NOT_A_LOOKUP.iter().filter(|o| !offenders.contains(*o)).collect();
        assert!(silent.is_empty(), "allowlisted ops that now pass; drop them: {silent:?}");
    }

    #[test]
    #[serial]
    fn assert_local_ops_offline_kit() {
        let (_dir, ctx) = gait_dir();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("ws");
        let fixtures = [
            Fixture { op: "init", args: json!({"gaitDir": target}) },
            Fixture { op: "link", args: json!({"gaitDir": target}) },
        ];
        let before = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        futures_executor_block(testing::assert_local_ops_offline(&registry(), &ctx, &fixtures));
        std::env::set_current_dir(before).unwrap();
    }

    #[test]
    fn assert_casing() {
        testing::assert_casing(&registry(), Casing::Camel);
    }
}
