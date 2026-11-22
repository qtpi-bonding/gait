//! The turn as the registry's free-text fallback: what is a task and what is a
//! command, where the turn's flags and the globals may stand, and what the
//! process exits with. Runs the built `gait`.
//!
//! A turn is recognised by its inference: every case passes `--inference
//! nosuch-inf`, which the turn rejects ("no such inference", naming the gait
//! directory it looked in) before any provider is contacted. A command or a
//! usage error never says that.
use std::path::Path;
use std::process::{Command, Output, Stdio};

const NO_INFERENCE: &str = "no such inference 'nosuch-inf'";

fn gait() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait"));
    cmd.stdin(Stdio::null())
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("GAIT_DIR")
        .env_remove("GAIT_AGENT_DIR")
        .env_remove("GAIT_IDENTITY_DIR");
    cmd
}

/// A fresh `gait init` directory and an empty working directory.
fn workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = tempfile::TempDir::new().unwrap();
    let init = gait()
        .args(["init", "--gait-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    (dir, cwd)
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    gait().args(args).current_dir(cwd).output().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Asserts that the invocation ran a turn: it got as far as resolving the
/// inference in `dir`.
fn assert_turn(out: &Output, dir: &Path, what: &str) {
    let err = stderr(out);
    assert!(err.contains(NO_INFERENCE), "{what}: not a turn: {err}");
    assert!(
        err.contains(&dir.display().to_string()),
        "{what}: the turn did not use gait dir {}: {err}",
        dir.display()
    );
    assert_eq!(out.status.code(), Some(65), "{what}: {err}");
}

#[test]
fn task_text_that_starts_like_a_command_runs_a_turn() {
    let (dir, cwd) = workspace();
    let out = run(
        cwd.path(),
        &[
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--inference",
            "nosuch-inf",
            "log the failures",
        ],
    );
    assert_turn(&out, dir.path(), "log the failures");
    // Nor does a longer sentence starting with another verb.
    let out = run(
        cwd.path(),
        &[
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--inference",
            "nosuch-inf",
            "stats are wrong, fix them",
        ],
    );
    assert_turn(&out, dir.path(), "stats are wrong");
}

#[test]
fn double_dash_escape() {
    let (dir, cwd) = workspace();
    let gait_dir = dir.path().to_str().unwrap();
    // `--` ends the flags: what follows is the task, even a bare command word.
    let out = run(
        cwd.path(),
        &[
            "--gait-dir",
            gait_dir,
            "--inference",
            "nosuch-inf",
            "--",
            "log",
        ],
    );
    assert_turn(&out, dir.path(), "-- log");
    // Straight after the program name as well: everything after it is the
    // turn's own argument line and the registry reads no globals from it, so
    // the flags after it are the turn's to refuse.
    let out = run(
        cwd.path(),
        &[
            "--",
            "log",
            "--gait-dir",
            gait_dir,
            "--inference",
            "nosuch-inf",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "-- log (flags after): {}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains(NO_INFERENCE), "{}", stderr(&out));
}

#[test]
fn flag_values_are_not_verbs() {
    let (dir, cwd) = workspace();
    let gait_dir = dir.path().to_str().unwrap();
    // `--inference ls "x"`: the value `ls` is the inference, not a command.
    let out = run(
        cwd.path(),
        &["--gait-dir", gait_dir, "--inference", "ls", "x"],
    );
    let err = stderr(&out);
    assert!(
        err.contains("no such inference 'ls'"),
        "--inference ls: {err}"
    );
    assert_eq!(out.status.code(), Some(65), "{err}");

    // `-s sessions "x"`: the value is the session id; no sessions listing.
    let out = run(
        cwd.path(),
        &[
            "--gait-dir",
            gait_dir,
            "-s",
            "sessions",
            "--inference",
            "nosuch-inf",
            "x",
        ],
    );
    assert_turn(&out, dir.path(), "-s sessions");
    assert!(
        !stdout(&out).contains("\"active\""),
        "ran `sessions`: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("\"sessionId\":\"sessions\""),
        "{}",
        stdout(&out)
    );

    // `--codebase auth "x"`: the value is a path, not the `auth` command.
    let out = run(
        cwd.path(),
        &[
            "--gait-dir",
            gait_dir,
            "--codebase",
            "auth",
            "--inference",
            "nosuch-inf",
            "x",
        ],
    );
    assert_turn(&out, dir.path(), "--codebase auth");
}

#[test]
fn command_word_runs_the_command() {
    let (dir, cwd) = workspace();
    let out = run(
        cwd.path(),
        &[
            "stats",
            "--id",
            "nosuch",
            "--gait-dir",
            dir.path().to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));
    assert!(
        !stderr(&out).contains(NO_INFERENCE),
        "ran a turn: {}",
        stderr(&out)
    );
}

#[test]
fn near_miss_single_token_is_usage_exit_2() {
    let (_dir, cwd) = workspace();
    for (word, hint) in [("sesions", "sessions"), ("statz", "stats")] {
        let out = run(cwd.path(), &[word]);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(2), "{word}: {err}");
        assert!(
            err.contains(&format!("did you mean `{hint}`")),
            "{word}: {err}"
        );
        assert!(!err.contains(NO_INFERENCE), "{word}: ran a turn: {err}");
        let last = err.lines().last().unwrap_or("");
        let value: serde_json::Value =
            serde_json::from_str(last).expect("the error is a JSON line");
        assert_eq!(
            value["error"]["code"], "op_registry.cli.unknown_command",
            "{last}"
        );
    }
    // Two words are a task, however close the first is.
    let (dir, cwd) = workspace();
    let out = run(
        cwd.path(),
        &[
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--inference",
            "nosuch-inf",
            "sesions please",
        ],
    );
    assert_turn(&out, dir.path(), "sesions please");
}

#[test]
fn gait_dir_before_and_after() {
    let (dir, cwd) = workspace();
    let gait_dir = dir.path().to_str().unwrap();
    let before = run(
        cwd.path(),
        &["--gait-dir", gait_dir, "--inference", "nosuch-inf", "x"],
    );
    assert_turn(&before, dir.path(), "before");
    let after = run(
        cwd.path(),
        &["x", "--inference", "nosuch-inf", "--gait-dir", gait_dir],
    );
    assert_turn(&after, dir.path(), "after");
    let equals = run(
        cwd.path(),
        &[
            "x",
            "--inference",
            "nosuch-inf",
            &format!("--gait-dir={gait_dir}"),
        ],
    );
    assert_turn(&equals, dir.path(), "after, =");
    // The one-off request takes it too.
    let exec = run(
        cwd.path(),
        &[
            "-e",
            "x",
            "--inference",
            "nosuch-inf",
            "--gait-dir",
            gait_dir,
        ],
    );
    assert!(
        stderr(&exec).contains(&dir.path().display().to_string()),
        "{}",
        stderr(&exec)
    );
    assert!(stderr(&exec).contains(NO_INFERENCE), "{}", stderr(&exec));
}

#[test]
fn gait_dir_falls_back_to_env_and_gaitref_for_the_turn() {
    let (dir, cwd) = workspace();
    let out = gait()
        .args(["--inference", "nosuch-inf", "x"])
        .env("GAIT_DIR", dir.path())
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert_turn(&out, dir.path(), "$GAIT_DIR");

    // The flag wins over the environment.
    let other = tempfile::TempDir::new().unwrap();
    let out = gait()
        .args(["--gait-dir"])
        .arg(dir.path())
        .args(["--inference", "nosuch-inf", "x"])
        .env("GAIT_DIR", other.path())
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert_turn(&out, dir.path(), "flag over env");

    // `.gaitref` found walking up from a subdirectory.
    std::fs::write(
        cwd.path().join(".gaitref"),
        format!("gaitdir: {}\n", dir.path().display()),
    )
    .unwrap();
    let sub = cwd.path().join("a/b");
    std::fs::create_dir_all(&sub).unwrap();
    let out = run(&sub, &["--inference", "nosuch-inf", "x"]);
    assert_turn(&out, dir.path(), ".gaitref");
}

#[test]
fn exec_and_interactive_flags_route() {
    use gait_bin::ops::turn::{TurnArgs, TurnPlan};
    use std::ffi::OsString;
    let plan = |args: &[&str]| {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        TurnArgs::parse(&args).expect("parses").plan()
    };
    assert_eq!(
        plan(&["-e", "list files"]),
        TurnPlan::Exec {
            prompt: "list files".into()
        }
    );
    assert_eq!(
        plan(&["--exec", "list files", "--inference", "m"]),
        TurnPlan::Exec {
            prompt: "list files".into()
        }
    );
    assert_eq!(plan(&["-i"]), TurnPlan::Interactive);
    assert_eq!(plan(&["--interactive", "-s", "abc"]), TurnPlan::Interactive);
    assert_eq!(
        plan(&["fix it"]),
        TurnPlan::Task {
            task: "fix it".into()
        }
    );
    assert_eq!(
        plan(&["-s", "sessions", "x"]),
        TurnPlan::Task { task: "x".into() }
    );
    assert_eq!(plan(&["-s", "abc"]), TurnPlan::NoTask);
    // A task and `-i` or `-e` together are refused, as before.
    for args in [&["-i", "x"][..], &["-e", "a", "x"], &["-i", "-e", "a"]] {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        assert!(TurnArgs::parse(&args).is_err(), "{args:?}");
    }
    // The turn's shorts are its own.
    let parsed = TurnArgs::parse(
        &[
            "-s", "S", "-a", "A", "-n", "3", "-R", "r.json", "-c", "C", "x",
        ]
        .map(OsString::from),
    )
    .unwrap();
    assert_eq!(parsed.session.as_deref(), Some("S"));
    assert_eq!(parsed.agent.as_deref(), Some("A"));
    assert_eq!(parsed.max_iter, Some(3));
    assert_eq!(parsed.codebase.as_deref(), Some(Path::new("C")));
    assert!(parsed.result_schema.is_some());
}

#[test]
fn help_and_version_are_reserved() {
    let (_dir, cwd) = workspace();
    for flag in ["--help", "-h"] {
        let out = run(cwd.path(), &[flag]);
        assert_eq!(out.status.code(), Some(0), "{flag}: {}", stderr(&out));
        assert!(
            stdout(&out).contains("AI coding agent"),
            "{flag}: {}",
            stdout(&out)
        );
        assert!(!stderr(&out).contains(NO_INFERENCE));
    }
    let out = run(cwd.path(), &["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout(&out).contains(env!("CARGO_PKG_VERSION")),
        "{}",
        stdout(&out)
    );
    // After a turn flag they are the turn's unknown flags (the registry shows
    // help and version only first): a usage error, never a task.
    for flag in ["--help", "--version"] {
        let out = run(cwd.path(), &["--inference", "m", flag]);
        assert_eq!(out.status.code(), Some(2), "{flag}: {}", stderr(&out));
        assert!(
            !stderr(&out).contains(NO_INFERENCE),
            "{flag}: ran a turn: {}",
            stderr(&out)
        );
    }
    // Help never prints this machine's paths (the home and cwd are not in it).
    let help = stdout(&run(cwd.path(), &["--help"]));
    for banned in ["/Users/", "/home/", cwd.path().to_str().unwrap()] {
        assert!(!help.contains(banned), "help names `{banned}`: {help}");
    }
}

#[test]
fn registry_help_lists_the_turn_and_never_reaches_it() {
    use gait_bin::ops::{registry, GaitHooks};
    use op_registry::cli::{run_with_io, Io};
    let run_in_memory = |args: &[&str]| {
        let argv = std::iter::once("gait")
            .chain(args.iter().copied())
            .map(Into::into)
            .collect();
        let (io, captured) = Io::memory(b"", false, false);
        let code = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_with_io(&registry(), argv, &GaitHooks, io));
        (code, captured.stdout())
    };
    for args in [&["--help"][..], &["-h"], &["--version"]] {
        let (code, out) = run_in_memory(args);
        assert_eq!(code, 0, "{args:?}: {out}");
        assert!(
            out.contains("AI coding agent") || out.contains(env!("CARGO_PKG_VERSION")),
            "{out}"
        );
    }
    let (_, help) = run_in_memory(&["--help"]);
    assert!(
        help.contains("turn: Run a task"),
        "the root help lists the fallback: {help}"
    );
    let (code, out) = run_in_memory(&["help", "turn"]);
    assert_eq!(code, 0);
    assert!(out.contains("put `--` before a task"), "{out}");
}

#[test]
fn no_args_prints_help_exit_0() {
    let (_dir, cwd) = workspace();
    let out = run(cwd.path(), &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("Usage: gait"), "{}", stdout(&out));
    // Flags with no task (the old behaviour: the help) also exit 0.
    let out = run(cwd.path(), &["-s", "abc"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("Usage: gait"), "{}", stdout(&out));
}

#[test]
fn a_turn_flag_error_is_usage_exit_2_with_a_json_line() {
    let (_dir, cwd) = workspace();
    for args in [
        &["--no-such-flag-xyz"][..],
        &["-i", "a task"],
        &["--max-iter", "many", "x"],
    ] {
        let out = run(cwd.path(), args);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        let last = err.lines().last().unwrap_or("");
        let value: serde_json::Value =
            serde_json::from_str(last).expect("the error is a JSON line");
        assert!(
            value["error"]["code"]
                .as_str()
                .unwrap()
                .starts_with("op_registry.usage."),
            "{last}"
        );
    }
}

/// The turn runs on a blocking thread of the runtime; with tokio's default 2 MiB
/// stack the debug build overflowed it as soon as the brain was launched. The
/// default inference has no provider credentials here (empty HOME, cleared
/// environment), so the brain fails cleanly after launch: exit 69, never a signal.
#[test]
fn a_turn_that_launches_the_brain_does_not_overflow_the_stack() {
    let (dir, cwd) = workspace();
    let home = tempfile::TempDir::new().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait"));
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path());
    let out = cmd
        .stdin(Stdio::null())
        .arg("--gait-dir")
        .arg(dir.path())
        .arg("--codebase")
        .arg(cwd.path())
        .args(["--auto-approve", "say hi"])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(69),
        "no exit code (a signal?): {}",
        stderr(&out)
    );
}

#[test]
fn identity_dir_flag_is_the_identity_dir() {
    let (dir, cwd) = workspace();
    let identity = tempfile::TempDir::new().unwrap();
    let out = gait()
        .args(["--gait-dir"])
        .arg(dir.path())
        .args(["--identity-dir"])
        .arg(identity.path())
        .args(["sessions"])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("GAIT_IDENTITY_DIR is set but cannot be loaded"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn identity_dir_env_is_read_and_the_old_env_is_ignored() {
    let (dir, cwd) = workspace();
    let identity = tempfile::TempDir::new().unwrap();
    let out = gait()
        .args(["--gait-dir"])
        .arg(dir.path())
        .args(["sessions"])
        .env("GAIT_IDENTITY_DIR", identity.path())
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("GAIT_IDENTITY_DIR is set but cannot be loaded"),
        "{}",
        stderr(&out)
    );

    let old_identity = tempfile::TempDir::new().unwrap();
    let out = gait()
        .args(["--gait-dir"])
        .arg(dir.path())
        .args(["sessions"])
        .env_remove("GAIT_IDENTITY_DIR")
        .env("GAIT_AGENT_DIR", old_identity.path())
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !stderr(&out).contains("cannot be loaded"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_old_agent_dir_flag_is_gone() {
    let (dir, cwd) = workspace();
    let old_identity = tempfile::TempDir::new().unwrap();
    let out = gait()
        .args(["--gait-dir"])
        .arg(dir.path())
        .args(["--agent-dir"])
        .arg(old_identity.path())
        .args(["sessions"])
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(
        !stderr(&out).contains("cannot be loaded"),
        "{}",
        stderr(&out)
    );

    let help = run(cwd.path(), &["--help"]);
    assert!(
        stdout(&help).contains("--identity-dir"),
        "{}",
        stdout(&help)
    );
    assert!(!stdout(&help).contains("--agent-dir"), "{}", stdout(&help));
}
