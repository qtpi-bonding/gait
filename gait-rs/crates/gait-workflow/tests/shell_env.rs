//! Task 3 (js-workflows): env-cleared shell + hooks, typed step options,
//! TurnExecutor. These tests are written BEFORE the implementation (step 1)
//! and must fail on the pre-task code.

use std::collections::BTreeMap;

use gait_workflow::exec_shell::{exec_shell_opts, NodeContext};
use gait_workflow::hooks::run_hook;
use gait_workflow::step::{build_shell_env, ShellOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use gait_workflow::turn_exec::{TurnExecutor, TurnRequest};

/// A stand-in for a provider credential: if this reaches a shell step or a
/// hook, the env clearing is broken.
const FAKE_PROVIDER_KEY: &str = "FAKE_PROVIDER_KEY_JS3";

fn set_fake_key() {
    // Unique name shared by no other test in this binary; set for the process
    // so the "gait's own environment" being leaked is real, not simulated.
    // Set once and never removed: tests run in parallel in one process and
    // removing the var races with other tests still reading it.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var(FAKE_PROVIDER_KEY, "sk-super-secret") });
}

fn ctx(ws: &std::path::Path) -> NodeContext<'static> {
    let ws: &'static std::path::Path = Box::leak(Box::new(ws.to_path_buf()));
    NodeContext {
        workflow_id: "wf1",
        session_id: "run1",
        node_id: "sh1",
        args: "a b",
        gait_dir: ws,
        codebase: ws,
        run_worktree: ws,
    }
}

#[tokio::test]
async fn shell_step_does_not_see_provider_key() {
    set_fake_key();
    let dir = tempfile::TempDir::new().unwrap();
    let opts = ShellOpts {
        id: "sh1".into(),
        argv: None,
        script: Some(format!("printenv {FAKE_PROVIDER_KEY}")),
        env: BTreeMap::new(),
        timeout: None,
        output_schema: None,
        retry: None,
        on_fail: None,
        hooks: None,
    };
    let res = exec_shell_opts(&opts, &ctx(dir.path()), &[]).await.unwrap();
    assert_eq!(
        res.output.trim(),
        "",
        "provider key must not reach a shell step"
    );
}

#[tokio::test]
async fn shell_step_sees_well_known_vars_as_env() {
    let dir = tempfile::TempDir::new().unwrap();
    let opts = ShellOpts {
        id: "sh1".into(),
        argv: None,
        script: Some(
            "printenv WORKFLOW_ID; printenv SESSION_ID; printenv CODEBASE; \
             printenv NODE_ID; printenv ARGS"
                .into(),
        ),
        env: BTreeMap::new(),
        timeout: None,
        output_schema: None,
        retry: None,
        on_fail: None,
        hooks: None,
    };
    let res = exec_shell_opts(&opts, &ctx(dir.path()), &[]).await.unwrap();
    let lines: Vec<&str> = res.output.lines().collect();
    assert_eq!(lines.first(), Some(&"wf1"), "WORKFLOW_ID");
    assert_eq!(lines.get(1), Some(&"run1"), "SESSION_ID");
    assert_eq!(
        lines.get(3),
        Some(&"sh1"),
        "NODE_ID must be set as a real env var"
    );
    assert_eq!(lines.get(4), Some(&"a b"), "ARGS");
}

#[tokio::test]
async fn hook_does_not_see_provider_key() {
    set_fake_key();
    let dir = tempfile::TempDir::new().unwrap();
    // Build the hook env exactly the way the runner now does: cleared env +
    // allowlist/passthrough/well-known, no free ride from gait's environment.
    let env = build_shell_env(
        &[
            ("WORKFLOW_ID".to_string(), "wf1".to_string()),
            ("SESSION_ID".to_string(), "run1".to_string()),
            ("NODE_ID".to_string(), "h1".to_string()),
            ("CODEBASE".to_string(), dir.path().display().to_string()),
            ("WORKSPACE".to_string(), dir.path().display().to_string()),
            ("ARGS".to_string(), String::new()),
        ]
        .into_iter()
        .collect(),
        &BTreeMap::new(),
        &[],
    )
    .unwrap();
    let (exit, out) = run_hook(
        &format!("echo \"[${{{FAKE_PROVIDER_KEY}}}]\""),
        &env,
        5_000,
        Some(dir.path()),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(out.trim(), "[]", "provider key must not reach a hook");
}

#[test]
fn passthrough_list_adds_named_vars() {
    set_fake_key();
    let well_known: BTreeMap<String, String> = [("WORKFLOW_ID".to_string(), "wf1".to_string())]
        .into_iter()
        .collect();
    let env = build_shell_env(
        &well_known,
        &BTreeMap::new(),
        &[FAKE_PROVIDER_KEY.to_string()],
    )
    .unwrap();
    assert_eq!(
        env.get(FAKE_PROVIDER_KEY).map(String::as_str),
        Some("sk-super-secret"),
        "an explicitly passthrough-listed var must be forwarded"
    );
    assert_eq!(env.get("WORKFLOW_ID").map(String::as_str), Some("wf1"));
    // PATH must still be present so commands resolve.
    assert!(env.contains_key("PATH"));
}

#[tokio::test]
async fn gait_prefixed_script_env_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let opts = ShellOpts {
        id: "sh1".into(),
        argv: None,
        script: Some("true".into()),
        env: [("GAIT_EVIL".to_string(), "1".to_string())]
            .into_iter()
            .collect(),
        timeout: None,
        output_schema: None,
        retry: None,
        on_fail: None,
        hooks: None,
    };
    let err = exec_shell_opts(&opts, &ctx(dir.path()), &[])
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("GAIT_EVIL"),
        "a GAIT_-prefixed user env key must be rejected, got: {err}"
    );
}

#[tokio::test]
async fn argv_runs_without_a_shell() {
    let dir = tempfile::TempDir::new().unwrap();
    let opts = ShellOpts {
        id: "sh1".into(),
        argv: Some(vec!["/bin/echo".into(), "a; touch injected_js3".into()]),
        script: None,
        env: BTreeMap::new(),
        timeout: None,
        output_schema: None,
        retry: None,
        on_fail: None,
        hooks: None,
    };
    let res = exec_shell_opts(&opts, &ctx(dir.path()), &[]).await.unwrap();
    assert_eq!(
        res.output.trim_end(),
        "a; touch injected_js3",
        "metacharacters must be a literal argv element, not shell syntax"
    );
    assert!(
        !dir.path().join("injected_js3").exists(),
        "no second command may run via argv"
    );
    assert_eq!(res.exit, Some(0));
}

#[tokio::test]
async fn scripted_turn_executor_returns_scripted_outcomes() {
    let mut fake = ScriptedTurnExecutor::new();
    fake.script_text("first turn output").session_id("sess-js3");
    fake.script_failure("model exploded");
    fake.script_json(serde_json::json!({ "ok": true }));

    let req1 = TurnRequest {
        id: "n1".into(),
        prompt: "do one".into(),
        ..Default::default()
    };
    let out1 = fake.run_turn(req1).await.unwrap();
    assert_eq!(out1.output, "first turn output");
    assert_eq!(out1.session_id.as_deref(), Some("sess-js3"));

    let req2 = TurnRequest {
        id: "n2".into(),
        prompt: "do two".into(),
        ..Default::default()
    };
    let err = fake.run_turn(req2).await.unwrap_err().to_string();
    assert!(err.contains("model exploded"), "got: {err}");

    let req3 = TurnRequest {
        id: "n3".into(),
        prompt: "do three".into(),
        ..Default::default()
    };
    let out3 = fake.run_turn(req3).await.unwrap();
    assert_eq!(out3.json, Some(serde_json::json!({ "ok": true })));

    // Exhausting the script is a hard error, not a silent empty output.
    let req4 = TurnRequest {
        id: "n4".into(),
        prompt: "do four".into(),
        ..Default::default()
    };
    assert!(fake.run_turn(req4).await.is_err());
}
