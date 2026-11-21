//! `gait auth ...` and `gait models` on the op registry: shapes, exit codes,
//! and that a key never shows up anywhere gait prints.
use std::io::Write;
use std::process::{Command, Output, Stdio};

/// A made-up API key. It must not appear in any output below.
const SENTINEL: &str = "sk-SENTINEL-4f9a1c7e5b2d8036-zzTOPSECRET";

/// A private gait home (the credential store refuses a group-readable one).
fn private_home() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    home
}

fn gait(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait"));
    cmd.env("GAIT_HOME", home)
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("GAIT_DIR")
        .stdin(Stdio::null());
    cmd
}

fn run(mut cmd: Command, stdin: &str) -> Output {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(text(&output.stdout).trim())
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", text(&output.stdout)))
}

fn last_error(output: &Output) -> serde_json::Value {
    let stderr = text(&output.stderr);
    let line = stderr.lines().last().unwrap_or("");
    serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("last stderr line is not JSON ({e}): {stderr}"))
}

fn assert_no_secret(label: &str, output: &Output) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            !text(bytes).contains(SENTINEL),
            "{label}: the key is in {stream}: {}",
            text(bytes)
        );
        assert!(
            !text(bytes).contains("sk-SENT"),
            "{label}: a key prefix is in {stream}"
        );
    }
}

#[test]
fn add_status_list_remove_round_trip_without_echoing_the_key() {
    let home = private_home();

    let add = run(
        {
            let mut c = gait(home.path());
            c.args(["auth", "add", "--id", "acme", "--key-file", "-"]);
            c
        },
        &format!("{SENTINEL}\n"),
    );
    assert_eq!(add.status.code(), Some(0), "{}", text(&add.stderr));
    assert_eq!(
        json(&add),
        serde_json::json!({"provider": "acme", "display": "api key", "action": "saved"})
    );
    assert_no_secret("auth add", &add);

    let mut status = gait(home.path());
    status.args(["auth", "status", "--id", "acme"]);
    let status = run(status, "");
    assert_eq!(status.status.code(), Some(0));
    assert_eq!(
        json(&status),
        serde_json::json!({"provider": "acme", "status": "api"})
    );
    assert_no_secret("auth status", &status);

    let mut list = gait(home.path());
    list.args(["auth", "list"]);
    let list = run(list, "");
    assert_eq!(
        json(&list),
        serde_json::json!({"items": [{"provider": "acme", "kind": "api", "status": null}]})
    );
    assert_no_secret("auth list", &list);

    // A bare `gait auth` lists (the group's default).
    let mut bare = gait(home.path());
    bare.arg("auth");
    let bare = run(bare, "");
    assert_eq!(json(&bare), json(&list));

    let stored = std::fs::read_to_string(home.path().join("auth.json")).unwrap();
    assert!(
        stored.contains(SENTINEL),
        "the key is stored: the add did not write it"
    );

    let mut remove = gait(home.path());
    remove.args(["auth", "remove", "--id", "acme"]);
    let remove = run(remove, "");
    assert_eq!(
        json(&remove),
        serde_json::json!({"provider": "acme", "action": "removed"})
    );
    let mut after = gait(home.path());
    after.args(["auth", "list"]);
    assert_eq!(json(&run(after, "")), serde_json::json!({"items": []}));
}

#[test]
fn add_with_an_empty_key_is_an_error_line_not_a_success() {
    let home = private_home();
    let key_file = tempfile::NamedTempFile::new().unwrap();
    let mut add = gait(home.path());
    add.args(["auth", "add", "--id", "acme", "--key-file"])
        .arg(key_file.path());
    let add = run(add, "\n");
    assert_eq!(add.status.code(), Some(65));
    assert_eq!(last_error(&add)["error"]["code"], "gait.op.conflict");
}

#[test]
fn add_without_a_file_in_a_non_terminal_run_is_usage_error() {
    let home = private_home();
    let mut add = gait(home.path());
    add.args(["auth", "add", "--id", "acme"]);
    let add = run(add, "");
    assert_eq!(add.status.code(), Some(2));
    assert_eq!(
        last_error(&add)["error"]["code"],
        "op_registry.usage.missing"
    );
}

#[test]
fn errors_and_help_never_carry_the_key() {
    let home = private_home();
    // A callback file given where it does not belong: it is for openai-signin.
    let mut wrong = gait(home.path());
    wrong.args(["auth", "login", "--id", "anthropic", "--callback-file", "-"]);
    let wrong = run(wrong, "");
    assert_eq!(wrong.status.code(), Some(2), "{}", text(&wrong.stderr));
    assert_eq!(
        last_error(&wrong)["error"]["code"],
        "op_registry.usage.invalid"
    );
    assert_no_secret("auth login --callback-file on the wrong provider", &wrong);

    // The key on stdin of a failing add (an unwritable home).
    let mut relative = gait(home.path());
    relative.env("GAIT_HOME", "relative/home").args([
        "auth",
        "add",
        "--id",
        "acme",
        "--key-file",
        "-",
    ]);
    let relative = run(relative, &format!("{SENTINEL}\n"));
    assert_ne!(relative.status.code(), Some(0));
    assert_no_secret("auth add into a bad home", &relative);

    for args in [
        vec!["--help"],
        vec!["auth", "--help"],
        vec!["auth", "add", "--help"],
        vec!["auth", "login", "--help"],
    ] {
        let mut help = gait(home.path());
        help.args(&args);
        let help = run(help, "");
        assert_eq!(help.status.code(), Some(0), "{args:?}");
        assert_no_secret(&format!("{args:?}"), &help);
    }
}

#[test]
fn the_registry_passes_the_secrets_check() {
    let dir = tempfile::TempDir::new().unwrap();
    let key_file = dir.path().join("key.pem");
    std::fs::write(&key_file, "KEYLINE-gait-auth-7731\n").unwrap();
    let sentinel = op_registry::testing::SecretSentinel::new(SENTINEL, &key_file);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(op_registry::testing::assert_secrets_not_echoed(
        &gait_bin::ops::registry(),
        None,
        &sentinel,
    ));
    rt.block_on(op_registry::testing::assert_secrets_not_echoed(
        &gait_bin::ops::min_registry(),
        None,
        &sentinel,
    ));
}

#[test]
fn link_and_unlink_report_the_binding() {
    let home = private_home();
    let codex_home = tempfile::TempDir::new().unwrap();
    let mut link = gait(home.path());
    link.args([
        "auth",
        "link",
        "--id",
        "openai-codex",
        "--source",
        "codex",
        "--home",
    ])
    .arg(codex_home.path());
    let link = run(link, "");
    assert_eq!(link.status.code(), Some(0), "{}", text(&link.stderr));
    assert_eq!(
        json(&link),
        serde_json::json!({"provider": "openai-codex", "display": "external (codex)", "action": "linked"})
    );
    let mut unlink = gait(home.path());
    unlink.args(["auth", "unlink", "--id", "openai-codex"]);
    let unlink = run(unlink, "");
    assert_eq!(
        json(&unlink),
        serde_json::json!({"provider": "openai-codex", "action": "unlinked"})
    );
}

#[test]
fn usage_errors_exit_2() {
    let home = private_home();
    for args in [
        vec!["auth", "frobnicate"],
        vec!["auth", "status"],
        vec!["auth", "link", "--id", "openai-codex"],
        vec![
            "auth",
            "logout",
            "--id",
            "openai-signin",
            "--callback-file",
            "x",
        ],
        vec!["auth", "logout", "--id", "openai-codex", "--forget"],
    ] {
        let mut cmd = gait(home.path());
        cmd.args(&args);
        let out = run(cmd, "");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
}

#[test]
fn unsupported_providers_are_logical_errors() {
    let home = private_home();
    for args in [
        vec!["auth", "login", "--id", "anthropic"],
        vec!["auth", "logout", "--id", "anthropic"],
        vec!["auth", "add", "--id", "openai-codex", "--key-file", "-"],
        vec!["auth", "add", "--id", "openai-signin", "--key-file", "-"],
    ] {
        let mut cmd = gait(home.path());
        cmd.args(&args);
        let out = run(cmd, "");
        assert_eq!(
            out.status.code(),
            Some(65),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
}

fn seed_models_cache(home: &std::path::Path) {
    std::fs::create_dir_all(home.join("cache")).unwrap();
    std::fs::write(
        home.join("cache/models.json"),
        serde_json::json!([
            {"id": "alpha-1", "name": "Alpha One", "provider": "acme", "context_length": 128000},
            {"id": "beta-2", "name": "Beta Two", "provider": "other", "context_length": null},
        ])
        .to_string(),
    )
    .unwrap();
}

#[test]
fn models_are_typed_items() {
    let home = private_home();
    seed_models_cache(home.path());
    let mut all = gait(home.path());
    all.arg("models");
    let all = run(all, "");
    assert_eq!(all.status.code(), Some(0), "{}", text(&all.stderr));
    assert_eq!(
        json(&all),
        serde_json::json!({"items": [
            {"id": "alpha-1", "provider": "acme", "name": "Alpha One", "contextLength": 128000},
            {"id": "beta-2", "provider": "other", "name": "Beta Two", "contextLength": null},
        ]})
    );
    let mut some = gait(home.path());
    some.args(["models", "--filter", "ACME"]);
    let some = run(some, "");
    let value = json(&some);
    assert_eq!(value["items"].as_array().unwrap().len(), 1);
    assert_eq!(value["items"][0]["id"], "alpha-1");
}

#[test]
fn models_network_failure_is_infrastructure_69() {
    let home = private_home();
    // No cache and a proxy that refuses: the fetch fails.
    let mut cmd = gait(home.path());
    cmd.arg("models")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("https_proxy", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("all_proxy", "http://127.0.0.1:1")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    let out = run(cmd, "");
    assert_eq!(out.status.code(), Some(69), "{}", text(&out.stderr));
    let error = last_error(&out);
    assert_eq!(error["error"]["class"], "infrastructure");
    assert_eq!(error["error"]["code"], "gait.cli.failed");
}

#[test]
fn old_forms_are_gone() {
    let home = private_home();
    // A positional provider is not a thing any more.
    let mut cmd = gait(home.path());
    cmd.args(["auth", "status", "openai-codex"]);
    assert_eq!(run(cmd, "").status.code(), Some(2));
    let mut cmd = gait(home.path());
    cmd.args(["models", "claude"]);
    assert_eq!(run(cmd, "").status.code(), Some(2));
}
