//! Task 4 (js-workflows): the JS script surface (`gait.wf.*`) driven through
//! `run_script`. These tests are written BEFORE the implementation (the red
//! step): they fail to compile until `gait_workflow::script::run_script`
//! exists, and are the spec for it.

use std::path::Path;
use std::sync::Arc;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use tempfile::TempDir;

/// A temp gait_dir with a git repo (main branch, one empty commit) — the
/// same setup the lazy-run tests use.
fn init_git_repo(gait_dir: &Path) {
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(gait_dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t.com")
        .output()
        .unwrap();
}

/// Write `source` to `<tmp>/wf.js` and run it with the given run_id.
/// The turn executor is scripted per-call by the caller (the same Arc is
/// captured in the opts). Returns the raw Result so each test asserts its
/// own flavor of success/failure.
async fn run_js(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    exec: Arc<ScriptedTurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, source).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: exec,
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
    )
    .await
}

/// Same, but with `args` passed through to the script.
async fn run_js_with_args(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    args: Vec<String>,
    exec: Arc<ScriptedTurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, source).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args,
            force_rerun_nodes: vec![],
            force: false,
            turns: exec,
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
    )
    .await
}

/// Read the run log's events.
fn log_events(gait_dir: &Path, run_id: &str) -> Vec<workflow_event::Kind> {
    WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id)
        .unwrap()
        .read_events()
        .unwrap()
        .into_iter()
        .map(|ev| ev.kind.expect("event must have a kind"))
        .collect()
}

/// Error text of a failed run.
fn err_text(res: anyhow::Result<WorkflowOutcome>) -> String {
    match res {
        Ok(_) => panic!("expected Err"),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test]
async fn shell_then_turn_runs_in_order() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let mut exec = ScriptedTurnExecutor::new();
    exec.script_text("done");
    let source = r#"
        const cfg = await gait.wf.config({ name: "t" });
        const a = await gait.wf.shell({ id: "a", argv: ["echo", "hi"] });
        const b = await gait.wf.turn({ id: "b", prompt: "say " + a.output });
    "#;
    let res = run_js(&tmp, "ord-001", source, Arc::new(exec)).await;
    assert!(matches!(res, Ok(WorkflowOutcome::Completed)));
    let kinds: Vec<String> = log_events(tmp.path(), "ord-001")
        .iter()
        .map(|k| match k {
            workflow_event::Kind::WorkflowStart(_) => "workflow_start".to_string(),
            workflow_event::Kind::NodeStart(ns) => format!("start:{}", ns.node_id),
            workflow_event::Kind::NodeComplete(nc) => format!("complete:{}", nc.node_id),
            workflow_event::Kind::WorkflowComplete(_) => "workflow_complete".to_string(),
            other => format!("other:{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "workflow_start".to_string(),
            "start:a".to_string(),
            "complete:a".to_string(),
            "start:b".to_string(),
            "complete:b".to_string(),
            "workflow_complete".to_string(),
        ]
    );
    for k in log_events(tmp.path(), "ord-001") {
        if let workflow_event::Kind::NodeComplete(nc) = &k {
            if nc.node_id == "a" {
                assert!(nc.output.contains("hi"), "shell output: {}", nc.output);
            }
            if nc.node_id == "b" {
                assert_eq!(nc.output, "done");
            }
        }
    }
}

#[tokio::test]
async fn config_must_be_awaited_first() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let no_exec = Arc::new(ScriptedTurnExecutor::new());

    let early = run_js(
        &tmp,
        "cfg-001",
        r#"await gait.wf.shell({ id: "early", argv: ["true"] });"#,
        no_exec.clone(),
    )
    .await;
    assert!(err_text(early).contains("config"));

    let twice = run_js(
        &tmp,
        "cfg-002",
        r#"
            await gait.wf.config({ name: "t" });
            await gait.wf.config({ name: "t2" });
        "#,
        no_exec,
    )
    .await;
    assert!(err_text(twice).contains("config"));
}

#[tokio::test]
async fn unknown_key_names_step_and_field() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.shell({ id: "x", argv: ["true"], bogus: 1 });
    "#;
    let res = run_js(
        &tmp,
        "unk-001",
        source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    let text = err_text(res);
    assert!(text.contains("x"), "step id in error: {text}");
    assert!(text.contains("bogus"), "field name in error: {text}");
}

#[tokio::test]
async fn missing_id_rejected() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.shell({ argv: ["true"] });
    "#;
    let res = run_js(
        &tmp,
        "no-id-001",
        source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    assert!(err_text(res).contains("id"));
}

#[tokio::test]
async fn duplicate_id_in_one_run_rejected() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.shell({ id: "dup", argv: ["true"] });
        await gait.wf.shell({ id: "dup", argv: ["true"] });
    "#;
    let res = run_js(
        &tmp,
        "dup-001",
        source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    let text = err_text(res);
    assert!(text.contains("dup"), "id in error: {text}");
    assert!(text.contains("duplicate"), "duplicate in error: {text}");
}

#[tokio::test]
async fn shell_injection_argv_script_env_sh() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER-file");
    let marker_str = marker.to_string_lossy().to_string();
    // JSON-escape the hostile value into the script source so the JS string
    // is exactly the hostile text with no interpretation surprises.
    let hostile = "'; touch MARKER; '".replace("MARKER", &marker_str);
    let hostile_js = serde_json::to_string(&hostile).unwrap();
    let no_exec = Arc::new(ScriptedTurnExecutor::new());

    // (1) argv form.
    let argv_src = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        const s = await gait.wf.shell({{ id: "inj", argv: ["echo", {hostile_js}] }});
    "#
    );
    let res = run_js(&tmp, "inj-argv", &argv_src, no_exec.clone()).await;
    match res {
        Ok(_) => {
            for k in log_events(tmp.path(), "inj-argv") {
                if let workflow_event::Kind::NodeComplete(nc) = &k {
                    if nc.node_id == "inj" {
                        assert!(
                            nc.output.contains(&hostile),
                            "argv output must contain the literal hostile text: {}",
                            nc.output
                        );
                    }
                }
            }
        }
        Err(e) => panic!("argv run must succeed: {e:#}"),
    }
    assert!(!marker.exists(), "argv form must not execute the injection");

    // (2) script form with env.
    let script_src = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{ id: "inj2", script: "echo \"$OUT\"", env: {{ OUT: {hostile_js} }} }});
    "#
    );
    let res = run_js(&tmp, "inj-script", &script_src, no_exec.clone()).await;
    match res {
        Ok(_) => {
            for k in log_events(tmp.path(), "inj-script") {
                if let workflow_event::Kind::NodeComplete(nc) = &k {
                    if nc.node_id == "inj2" {
                        assert!(
                            nc.output.contains(&hostile),
                            "script output must contain the literal hostile text: {}",
                            nc.output
                        );
                    }
                }
            }
        }
        Err(e) => panic!("script run must succeed: {e:#}"),
    }
    assert!(
        !marker.exists(),
        "script form must not execute the injection"
    );

    // (3) the sh`` tagged template form.
    let sh_src = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        const hostile = {hostile_js};
        await gait.wf.shell({{ id: "inj3", ...gait.wf.sh`echo ${{hostile}}` }});
    "#
    );
    let res = run_js(&tmp, "inj-sh", &sh_src, no_exec.clone()).await;
    match res {
        Ok(_) => {
            for k in log_events(tmp.path(), "inj-sh") {
                if let workflow_event::Kind::NodeComplete(nc) = &k {
                    if nc.node_id == "inj3" {
                        assert!(
                            nc.output.contains(&hostile),
                            "sh`` output must contain the literal hostile text: {}",
                            nc.output
                        );
                    }
                }
            }
        }
        Err(e) => panic!("sh`` run must succeed: {e:#}"),
    }
    assert!(!marker.exists(), "sh`` form must not execute the injection");

    // (4) GAIT_ env names are reserved.
    let gait_env_src = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.shell({ id: "inj4", argv: ["true"], env: { GAIT_X: "1" } });
    "#;
    let res = run_js(&tmp, "inj-gait", gait_env_src, no_exec).await;
    assert!(err_text(res).contains("GAIT"));
}

#[tokio::test]
async fn date_now_and_math_random_throw() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        const msgs = [];
        try { Date.now(); } catch (e) { msgs.push(e.message); }
        try { new Date(); } catch (e) { msgs.push(e.message); }
        try { Math.random(); } catch (e) { msgs.push(e.message); }
        const zero = new Date(0).getTime();
        throw new Error(msgs.join("|") + "|" + zero);
    "#;
    let res = run_js(
        &tmp,
        "date-001",
        source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    let text = err_text(res);
    let count = text.matches("use gait.wf.now() / gait.wf.random()").count();
    assert_eq!(count, 3, "one hint per blocked call: {text}");
    assert!(
        text.ends_with("|0"),
        "new Date(0).getTime() must still be 0: {text}"
    );
}

#[tokio::test]
async fn node_start_type_is_turn_and_carries_session_id() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let mut exec = ScriptedTurnExecutor::new();
    exec.script_text("first out").session_id("sess-1");
    exec.script_text("second out");
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.turn({ id: "t1", prompt: "p1" });
        await gait.wf.turn({ id: "t2", prompt: "p2", continueSession: "prev-sess" });
    "#;
    let script_text = source.to_string();
    let res = run_js(&tmp, "turn-sess-001", source, Arc::new(exec)).await;
    assert!(matches!(res, Ok(WorkflowOutcome::Completed)));
    let events = log_events(tmp.path(), "turn-sess-001");
    for k in &events {
        if let workflow_event::Kind::NodeStart(ns) = k {
            assert_eq!(ns.node_type, "turn", "node_type for {}", ns.node_id);
            if ns.node_id == "t1" {
                assert_eq!(ns.session_id, "");
            }
            if ns.node_id == "t2" {
                assert_eq!(ns.session_id, "prev-sess");
            }
        }
        if let workflow_event::Kind::NodeComplete(nc) = k {
            if nc.node_id == "t1" {
                assert_eq!(nc.session_id, "sess-1");
            }
        }
    }
    for k in &events {
        if let workflow_event::Kind::WorkflowStart(ws) = k {
            assert!(!ws.script_path.is_empty());
            assert_eq!(ws.script_sha256.len(), 64);
            assert!(
                ws.script_sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "sha must be hex"
            );
            assert_eq!(ws.script_source, script_text);
        }
    }
}

#[tokio::test]
async fn run_object_has_run_id_args_codebase_and_workspace_before_worktree_exists() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        throw new Error(JSON.stringify(gait.wf.run));
    "#;
    let run_id = "runobj-001";
    let res = run_js_with_args(
        &tmp,
        run_id,
        source,
        vec!["a".to_string(), "b".to_string()],
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    let text = err_text(res);
    let start = text.find('{').expect("JSON object in error text");
    let end = text.rfind('}').expect("JSON object in error text");
    let run: serde_json::Value = serde_json::from_str(&text[start..=end]).unwrap();
    assert_eq!(run["runId"], "runobj-001");
    assert_eq!(run["args"], serde_json::json!(["a", "b"]));
    assert_eq!(
        Path::new(run["codebase"].as_str().unwrap()),
        tmp.path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf())
    );
    let workspace = run["workspace"].as_str().unwrap();
    assert!(!workspace.is_empty());
    let expected_suffix = format!("workflow-runs/{run_id}/worktree");
    assert!(
        workspace.ends_with(&expected_suffix),
        "workspace {workspace} must end in {expected_suffix}"
    );
    assert!(
        !tmp.path()
            .join("workflow-runs")
            .join(run_id)
            .join("worktree")
            .exists(),
        "workspace dir must not exist before any node runs"
    );
}

#[tokio::test]
async fn step_cap_refuses_run_past_the_limit() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t", maxSteps: 2 });
        await gait.wf.shell({ id: "s1", argv: ["true"] });
        await gait.wf.shell({ id: "s2", argv: ["true"] });
        await gait.wf.shell({ id: "s3", argv: ["true"] });
    "#;
    let res = run_js(
        &tmp,
        "cap-001",
        source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    assert!(err_text(res).contains("step cap"));
    let events = log_events(tmp.path(), "cap-001");
    let mut completed = vec![];
    let mut started_s3 = false;
    for k in &events {
        if let workflow_event::Kind::NodeComplete(nc) = k {
            completed.push(nc.node_id.clone());
        }
        if let workflow_event::Kind::NodeStart(ns) = k {
            if ns.node_id == "s3" {
                started_s3 = true;
            }
        }
    }
    assert!(completed.contains(&"s1".to_string()));
    assert!(completed.contains(&"s2".to_string()));
    assert!(!completed.contains(&"s3".to_string()));
    assert!(!started_s3, "s3 must never start past the cap");
}

#[tokio::test]
async fn oversized_script_is_refused_at_start() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    // 1 MiB + 1 bytes: a comment padded with spaces.
    let mut source = String::from("// ");
    let pad = 1_048_577usize.saturating_sub(source.len());
    source.push_str(&" ".repeat(pad));
    assert_eq!(source.len(), 1_048_577);
    let run_id = "big-001";
    let res = run_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(err_text(res).contains("too large"));
    assert!(
        !tmp.path().join("workflow-runs").join(run_id).exists(),
        "an oversized script must be refused before any run log is created"
    );
}

#[tokio::test]
async fn parallel_steps_run_concurrently_and_all_complete() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let mut exec = ScriptedTurnExecutor::new();
    exec.script_text("three");
    let source = r#"
        await gait.wf.config({ name: "t" });
        await Promise.all([
            gait.wf.shell({ id: "p1", script: "echo one" }),
            gait.wf.shell({ id: "p2", script: "echo two" }),
            gait.wf.turn({ id: "p3", prompt: "x" }),
        ]);
    "#;
    let run_id = "par-001";
    let res = run_js(&tmp, run_id, source, Arc::new(exec)).await;
    assert!(matches!(res, Ok(WorkflowOutcome::Completed)));

    let events = log_events(tmp.path(), run_id);
    let mut node_starts = 0;
    let mut complete_outputs: Vec<(String, String)> = vec![];
    for k in &events {
        if let workflow_event::Kind::NodeStart(_) = k {
            node_starts += 1;
        }
        if let workflow_event::Kind::NodeComplete(nc) = k {
            complete_outputs.push((nc.node_id.clone(), nc.output.clone()));
        }
    }
    assert_eq!(node_starts, 3, "exactly three NodeStart events");
    let all = complete_outputs
        .iter()
        .map(|(_, out)| out.as_str())
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(all.contains("one"), "p1 output present: {all}");
    assert!(all.contains("two"), "p2 output present: {all}");
    assert!(all.contains("three"), "p3 output present: {all}");
    let ids: Vec<&str> = complete_outputs.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"p1"));
    assert!(ids.contains(&"p2"));
    assert!(ids.contains(&"p3"));

    // Real overlap: two concurrent `sleep 1` shells must finish in well
    // under the 2 seconds a serial execution would take.
    let sleep_src = r#"
        await gait.wf.config({ name: "t" });
        await Promise.all([
            gait.wf.shell({ id: "sl1", script: "sleep 1" }),
            gait.wf.shell({ id: "sl2", script: "sleep 1" }),
        ]);
    "#;
    let run_id2 = "par-002";
    let no_exec = Arc::new(ScriptedTurnExecutor::new());
    let started2 = std::time::Instant::now();
    let res2 = run_js(&tmp, run_id2, sleep_src, no_exec).await;
    let elapsed2 = started2.elapsed();
    assert!(matches!(res2, Ok(WorkflowOutcome::Completed)));
    assert!(
        elapsed2.as_secs_f64() < 1.9,
        "two sleep-1 shells must overlap, took {elapsed2:?}"
    );
}

#[tokio::test]
async fn prelude_does_not_leak_host_or_date_constructor() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let no_exec = Arc::new(ScriptedTurnExecutor::new());

    let leak_src = r#"
        await gait.wf.config({ name: "t" });
        throw new Error(
            JSON.stringify([
                typeof __host,
                typeof __run,
                typeof __RealDate,
                typeof MSG_SUFFIX,
            ])
        );
    "#;
    let res = run_js(&tmp, "leak-001", leak_src, no_exec.clone()).await;
    let text = err_text(res);
    assert!(
        text.contains(r#"["undefined","undefined","undefined","undefined"]"#),
        "host internals must not leak into the script: {text}"
    );

    let ctor_src = r#"
        await gait.wf.config({ name: "t" });
        const msgs = [];
        try { new (Date.prototype.constructor)(); } catch (e) { msgs.push(e.message); }
        try { new (new Date(0).constructor)(); } catch (e) { msgs.push(e.message); }
        throw new Error(msgs.join(" | "));
    "#;
    let res2 = run_js(&tmp, "leak-002", ctor_src, no_exec).await;
    let text2 = err_text(res2);
    let count = text2
        .matches("use gait.wf.now() / gait.wf.random()")
        .count();
    assert_eq!(count, 2, "constructor bypass must be closed: {text2}");
}
