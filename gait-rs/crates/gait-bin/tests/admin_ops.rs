//! Inference, mode, config, daemons and extension ops on the registry (Task 8),
//! and `workflow` as the one clap extra.
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};

fn gait() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait"));
    cmd.stdin(Stdio::null()).env_remove("OTEL_EXPORTER_OTLP_ENDPOINT").env_remove("GAIT_DIR");
    cmd
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?} / stderr {:?}", self.stdout, self.stderr))
    }
    fn error_code(&self) -> String {
        let last = self.stderr.lines().last().unwrap_or("");
        let value: Value = serde_json::from_str(last)
            .unwrap_or_else(|e| panic!("last stderr line is not JSON ({e}): {:?}", self.stderr));
        value["error"]["code"].as_str().unwrap_or_default().to_owned()
    }
}

fn run(dir: &Path, args: &[&str]) -> Ran {
    let out = gait().args(args).arg("--gait-dir").arg(dir).output().unwrap();
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let init = gait().args(["init", "--gait-dir"]).arg(dir.path()).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    dir
}

#[test]
fn inference_add_list_show_round_trip() {
    let ws = workspace();
    let add = run(
        ws.path(),
        &["inference", "add", "--id", "fast", "--model", "opencode/kimi-k2.6", "--compaction-threshold-tokens", "1000"],
    );
    assert_eq!(add.code, Some(0), "{}", add.stderr);
    assert_eq!(add.json(), json!({"name": "fast", "model": "opencode/kimi-k2.6", "action": "added"}));

    let list = run(ws.path(), &["inference", "list"]);
    assert_eq!(list.code, Some(0), "{}", list.stderr);
    // The workspace's own profile is listed too; the new one is among them.
    let rows = list.json();
    assert!(rows["items"].as_array().unwrap().contains(&json!({"name": "fast", "model": "opencode/kimi-k2.6"})), "{rows}");
    assert!(rows["items"].as_array().unwrap().iter().all(|r| r["name"].is_string() && r["model"].is_string()));

    // Bare `inference` is `list`.
    let bare = run(ws.path(), &["inference"]);
    assert_eq!(bare.json(), list.json());

    let show = run(ws.path(), &["inference", "show", "--id", "fast"]);
    assert_eq!(show.json(), json!({"name": "fast", "model": "opencode/kimi-k2.6"}));

    // Adding it again is refused, not overwritten.
    let again = run(
        ws.path(),
        &["inference", "add", "--id", "fast", "--model", "opencode/kimi-k2.6", "--compaction-threshold-tokens", "1000"],
    );
    assert_ne!(again.code, Some(0));
}

#[test]
fn inference_errors() {
    let ws = workspace();
    let missing = run(ws.path(), &["inference", "show", "--id", "nosuch"]);
    assert_eq!(missing.code, Some(65));
    assert_eq!(missing.error_code(), "gait.op.not_found");
    // `add` needs its model and threshold: a usage error.
    let usage = run(ws.path(), &["inference", "add", "--id", "x"]);
    assert_eq!(usage.code, Some(2), "{}", usage.stderr);
    // `select` needs a session; an inactive session is not found.
    let usage = run(ws.path(), &["inference", "select", "--id", "default"]);
    assert_eq!(usage.code, Some(2), "{}", usage.stderr);
    let gone = run(ws.path(), &["inference", "clear", "--session", "nosuch"]);
    assert_eq!(gone.code, Some(65), "{}", gone.stderr);
    assert_eq!(gone.error_code(), "gait.op.not_found");
}

#[test]
fn mode_add_list_show_with_repeated_flags() {
    let ws = workspace();
    let add = run(
        ws.path(),
        &[
            "mode", "add", "--id", "review", "--allow", "read", "--allow", "grep", "--deny", "shell:rm *",
            "--steer", "review only", "--description", "Review mode",
        ],
    );
    assert_eq!(add.code, Some(0), "{}", add.stderr);
    assert_eq!(
        add.json(),
        json!({"name": "review", "allow": ["read", "grep"], "deny": ["shell:rm *"], "action": "added"})
    );
    let show = run(ws.path(), &["mode", "show", "--id", "review"]);
    let shown = show.json();
    assert_eq!(shown["steer"], "review only");
    assert_eq!(shown["description"], "Review mode");
    assert_eq!(shown["allow"], json!(["read", "grep"]));
    let list = run(ws.path(), &["mode", "list"]);
    let rows = list.json();
    assert_eq!(rows["items"].as_array().unwrap().len(), 1, "{rows}");
    assert_eq!(rows["items"][0]["name"], "review");
    let missing = run(ws.path(), &["mode", "show", "--id", "nosuch"]);
    assert_eq!(missing.code, Some(65));
    assert_eq!(missing.error_code(), "gait.op.not_found");
    let gone = run(ws.path(), &["mode", "select", "--id", "review", "--session", "nosuch"]);
    assert_eq!(gone.code, Some(65), "{}", gone.stderr);
}

#[test]
fn config_set_get_with_id_and_value() {
    let ws = workspace();
    let set = run(ws.path(), &["config", "set", "--id", "model", "--value", "anthropic/claude-opus-4"]);
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    let get = run(ws.path(), &["config", "get", "--id", "model"]);
    assert_eq!(get.json(), json!({"key": "model", "value": "anthropic/claude-opus-4"}));

    // A JSON value is stored as JSON, not as a string.
    let set = run(ws.path(), &["config", "set", "--id", "max_iter", "--value", "50"]);
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    let raw = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();
    let stored: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(stored["max_iter"], 50);

    let missing = run(ws.path(), &["config", "get", "--id", "nosuch"]);
    assert_eq!(missing.code, Some(65));
    assert_eq!(missing.error_code(), "gait.op.not_found");
    let usage = run(ws.path(), &["config", "set", "--id", "model"]);
    assert_eq!(usage.code, Some(2), "{}", usage.stderr);
}

#[test]
fn daemons_items_lists_and_stop_needs_a_known_daemon() {
    let ws = workspace();
    let bare = run(ws.path(), &["daemons"]);
    assert_eq!(bare.code, Some(0), "{}", bare.stderr);
    assert_eq!(bare.json(), json!({"items": []}));
    let all = run(ws.path(), &["daemons", "list", "--all"]);
    assert_eq!(all.json(), json!({"items": []}));
    let stop = run(ws.path(), &["daemons", "stop", "--id", "ghost"]);
    assert_eq!(stop.code, Some(65));
    assert_eq!(stop.error_code(), "gait.op.not_found");
    assert!(stop.stderr.contains("not found"), "{}", stop.stderr);
}

#[test]
fn extension_lifecycle_for_a_skill() {
    let ws = workspace();
    let reg = run(
        ws.path(),
        &["register", "--kind", "skill", "--id", "tdd", "--description", "Test first", "--content", "Write the test."],
    );
    assert_eq!(reg.code, Some(0), "{}", reg.stderr);
    let out = reg.json();
    assert_eq!(out["kind"], "skill");
    assert_eq!(out["name"], "tdd");
    assert_eq!(out["scope"], "global");
    assert_eq!(out["action"], "registered");
    assert!(out["writtenTo"].as_str().unwrap().ends_with("SKILL.md"), "{out}");
    assert!(ws.path().join("skills/tdd/SKILL.md").exists());

    let en = run(ws.path(), &["enable", "--kind", "skill", "--id", "tdd"]);
    assert_eq!(en.json(), json!({"kind": "skill", "name": "tdd", "action": "enabled"}));
    let dis = run(ws.path(), &["disable", "--kind", "skill", "--id", "tdd"]);
    assert_eq!(dis.json(), json!({"kind": "skill", "name": "tdd", "action": "disabled"}));

    // Loading into a session that is not there is not found.
    let load = run(ws.path(), &["load", "--kind", "skill", "--id", "tdd", "--session", "nosuch"]);
    assert_eq!(load.code, Some(65), "{}", load.stderr);
    let unload = run(ws.path(), &["unload", "--kind", "skill", "--id", "tdd", "--session", "nosuch"]);
    assert_eq!(unload.code, Some(65), "{}", unload.stderr);

    let un = run(ws.path(), &["unregister", "--kind", "skill", "--id", "tdd"]);
    assert_eq!(un.code, Some(0), "{}", un.stderr);
    assert_eq!(un.json()["action"], "unregistered");
    assert!(!ws.path().join("skills/tdd").exists());
    let again = run(ws.path(), &["unregister", "--kind", "skill", "--id", "tdd"]);
    assert_eq!(again.code, Some(65), "{}", again.stderr);
}

#[test]
fn register_mcp_with_command_args_and_env() {
    let ws = workspace();
    let reg = run(
        ws.path(),
        &[
            "register", "--kind", "mcp", "--id", "slack", "--command", "npx", "--args=-y @pkg/slack",
            "--env", "TOKEN=abc", "--env", "REGION=eu",
        ],
    );
    assert_eq!(reg.code, Some(0), "{}", reg.stderr);
    let entry: Value =
        serde_json::from_str(&std::fs::read_to_string(ws.path().join("mcp/slack.json")).unwrap()).unwrap();
    assert_eq!(entry["command"], "npx");
    assert_eq!(entry["args"], json!(["-y", "@pkg/slack"]));
    assert_eq!(entry["env"], json!({"TOKEN": "abc", "REGION": "eu"}));
    assert_eq!(entry["enabled"], false);
}

#[test]
fn extension_errors() {
    let ws = workspace();
    // The compat spellings are gone, and the kind is checked.
    let spawn = run(ws.path(), &["register", "--kind", "mcp", "--id", "x", "--spawn", "node a.js"]);
    assert_eq!(spawn.code, Some(2), "{}", spawn.stderr);
    let connect = run(ws.path(), &["register", "--kind", "mcp", "--id", "x", "--connect", "http://h"]);
    assert_eq!(connect.code, Some(2), "{}", connect.stderr);
    let kind = run(ws.path(), &["enable", "--kind", "widget", "--id", "x"]);
    assert_eq!(kind.code, Some(2), "{}", kind.stderr);
    let positional = run(ws.path(), &["enable", "mcp", "x"]);
    assert_ne!(positional.code, Some(0));
    // Enabling what was never registered is not found.
    let missing = run(ws.path(), &["enable", "--kind", "mcp", "--id", "nosuch"]);
    assert_eq!(missing.code, Some(65), "{}", missing.stderr);
    assert_eq!(missing.error_code(), "gait.op.not_found");
    assert!(missing.stderr.contains("not found"), "{}", missing.stderr);
}

#[test]
fn old_verb_enums_are_gone_and_workflow_is_the_extra() {
    use op_registry::cli::command;
    let registry = gait_bin::ops::registry();
    for name in [
        "inference list", "inference show", "inference add", "inference select", "inference clear",
        "mode list", "mode show", "mode add", "mode select", "mode clear", "config get", "config set",
        "daemons list", "daemons stop", "register", "unregister", "enable", "disable", "load", "unload",
    ] {
        let op = name.replace(' ', "_");
        assert!(registry.get(&op).is_some(), "missing op {op}");
    }
    // `workflow` is mounted as a clap command, not an op.
    assert!(registry.get("workflow").is_none());
    #[cfg(feature = "workflow")]
    assert!(command(&registry).get_subcommands().any(|c| c.get_name() == "workflow"));
    #[cfg(not(feature = "workflow"))]
    assert!(!command(&registry).get_subcommands().any(|c| c.get_name() == "workflow"));
}

#[cfg(feature = "workflow")]
#[test]
fn workflow_runs_through_the_extra_with_the_global_gait_dir() {
    let ws = workspace();
    let list = run(ws.path(), &["workflow", "list"]);
    assert_eq!(list.code, Some(0), "{}", list.stderr);
    // The global before the extra works too.
    let out = gait().arg("--gait-dir").arg(ws.path()).args(["workflow", "list"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}
