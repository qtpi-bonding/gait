use std::process::{Command, Stdio};

fn gait() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait"));
    cmd.stdin(Stdio::null()).env_remove("OTEL_EXPORTER_OTLP_ENDPOINT");
    cmd
}

#[test]
fn broken_config_exits_65() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), "{ \"inference\": ").unwrap();
    let status = gait()
        .args(["-e", "list files", "--gait-dir"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(65));
}

#[test]
fn unknown_flag_exits_2() {
    // A usage error: the turn's flags are parsed by the registry's fallback.
    let status = gait().arg("--no-such-flag").status().unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn help_exits_0() {
    let status = gait().arg("--help").status().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn missing_gait_dir_flag_exits_65() {
    let cwd = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let output = gait()
        .arg("sessions")
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("GAIT_DIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--gait-dir"));
}

#[test]
fn missing_session_exits_65() {
    let workspace = tempfile::TempDir::new().unwrap();
    let init = gait().args(["init", "--gait-dir"]).arg(workspace.path()).status().unwrap();
    assert!(init.success());
    let status = gait()
        .args(["allow", "--id", "nosuch", "--gait-dir"])
        .arg(workspace.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(65));
}

/// Every get/act/delete on an object that does not exist is a logical
/// not-found (exit 65, "not found" on stderr), never success.
fn assert_not_found(args: &[&str]) {
    let workspace = tempfile::TempDir::new().unwrap();
    let init = gait().args(["init", "--gait-dir"]).arg(workspace.path()).status().unwrap();
    assert!(init.success());
    let output = gait()
        .args(args)
        .arg("--gait-dir")
        .arg(workspace.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(65), "{args:?}: stderr: {stderr}");
    assert!(stderr.contains("not found"), "{args:?}: stderr: {stderr}");
}

#[test]
fn stats_unknown_session_is_not_found() {
    assert_not_found(&["stats", "--id", "nosuch"]);
}

#[test]
fn log_unknown_session_is_not_found() {
    assert_not_found(&["log", "--id", "nosuch"]);
}

#[test]
fn undo_list_unknown_session_is_not_found() {
    assert_not_found(&["undo", "--id", "nosuch", "--list"]);
}

#[test]
fn discard_unknown_session_is_not_found() {
    assert_not_found(&["discard", "--id", "nosuch"]);
}

#[test]
fn persist_unknown_session_is_not_found() {
    assert_not_found(&["persist", "--id", "nosuch"]);
}

#[test]
fn unregister_unknown_mcp_is_not_found() {
    assert_not_found(&["unregister", "--kind", "mcp", "--id", "nosuch"]);
}

#[test]
fn config_get_unknown_key_is_not_found() {
    assert_not_found(&["config", "get", "--id", "nosuch"]);
}

#[test]
fn inference_show_unknown_is_not_found() {
    assert_not_found(&["inference", "show", "--id", "nosuch"]);
}

#[test]
fn mode_show_unknown_is_not_found() {
    assert_not_found(&["mode", "show", "--id", "nosuch"]);
}

#[test]
fn daemons_stop_unknown_is_not_found() {
    assert_not_found(&["daemons", "stop", "--id", "nosuch"]);
}

#[test]
fn reply_unknown_session_is_not_found() {
    assert_not_found(&["reply", "--id", "nosuch", "--message", "hi"]);
}

#[test]
fn load_skill_into_unknown_session_is_not_found() {
    assert_not_found(&["load", "--kind", "skill", "--id", "x", "--session", "nosuch"]);
}

fn assert_usage(args: &[&str]) {
    let workspace = tempfile::TempDir::new().unwrap();
    let init = gait().args(["init", "--gait-dir"]).arg(workspace.path()).status().unwrap();
    assert!(init.success());
    let output = gait()
        .args(args)
        .arg("--gait-dir")
        .arg(workspace.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{args:?}: stderr: {stderr}");
    let last = stderr.lines().last().unwrap_or("");
    let value: serde_json::Value = serde_json::from_str(last).expect("the error is a JSON line");
    assert!(value["error"]["code"].is_string(), "{last}");
}

#[test]
fn a_session_op_without_its_id_is_a_usage_error() {
    for verb in ["stats", "log", "undo", "allow", "deny", "unlock"] {
        assert_usage(&[verb]);
    }
    assert_usage(&["reply", "--message", "hi"]);
}

#[test]
fn persist_and_discard_need_exactly_one_of_id_and_all() {
    for verb in ["persist", "discard"] {
        assert_usage(&[verb]);
        assert_usage(&[verb, "--id", "x", "--all", "--yes"]);
    }
}

#[test]
fn bulk_persist_and_discard_need_yes() {
    for verb in ["persist", "discard"] {
        assert_usage(&[verb, "--all"]);
    }
}

#[test]
fn register_with_two_sources_is_a_usage_error() {
    assert_usage(&["register", "--id", "x", "--kind", "mcp", "--url", "http://a", "--command", "b"]);
    assert_usage(&["register", "--id", "x", "--kind", "skill", "--description", "d", "--content", "c", "--path", "p"]);
    assert_usage(&["register", "--id", "x", "--kind", "mcp"]);
}

#[test]
fn undo_list_with_turn_is_a_usage_error() {
    assert_usage(&["undo", "--id", "x", "--list", "--turn", "1"]);
}

#[test]
fn bulk_on_an_empty_workspace_is_an_empty_result() {
    let workspace = tempfile::TempDir::new().unwrap();
    assert!(gait().args(["init", "--gait-dir"]).arg(workspace.path()).status().unwrap().success());
    for verb in ["persist", "discard"] {
        let output = gait()
            .arg(verb)
            .arg("--gait-dir")
            .arg(workspace.path())
            .args(["--all", "--yes"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{verb}: {}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
        assert_eq!(value["acted"], serde_json::json!([]), "{verb}: {value}");
        assert_eq!(value["eligible"], serde_json::json!([]), "{verb}: {value}");
    }
}

#[test]
fn a_bad_allow_scope_is_a_usage_error() {
    // `agent` was retired for `allow`; it is not one of its values any more.
    for scope in ["agent", "everywhere"] {
        assert_usage(&["allow", "--id", "x", "--scope", scope]);
    }
    assert_usage(&["unlock", "--id", "x", "--scope", "everywhere"]);
}
