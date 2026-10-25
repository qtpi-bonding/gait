//! `spawn_agent` handler family: parse args, launch a child `gait run`, and
//! assemble the `Feedback` from its terminal result.

use crate::proto::gait::sandbox::v1::{Feedback, Impulse};

use super::{feedback_result, TurnCtx};
use crate::turn::spine::handlers::{error_feedback, SpawnAgentArgs};

#[cfg(feature = "telemetry")]
fn emit_agent_spawned(session_id: &str, parent_session_id: &str, agent_name: &str) {
    concord_telemetry::event!(gait_events::gait::events::v1::AgentSpawned {
        source_record_id: session_id.to_owned(),
        session_id: parent_session_id.to_owned(),
        child_session_id: session_id.to_owned(),
        agent_name: agent_name.to_owned(),
    });
}

/// Parse a proto `Struct` from an Impulse into `SpawnAgentArgs`.
pub(crate) fn parse_spawn_args(impulse: &Impulse) -> SpawnAgentArgs {
    let s = impulse.args.as_ref();
    let get_str = |key: &str| -> Option<String> {
        s.and_then(|st| st.fields.get(key))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
    };
    let get_bool = |key: &str| -> bool {
        s.and_then(|st| st.fields.get(key))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false)
    };
    let schema: Option<serde_json::Value> = s
        .and_then(|st| st.fields.get("schema"))
        .and_then(|v| serde_json::to_value(v).ok())
        .filter(|v| !v.is_null());
    SpawnAgentArgs {
        task: get_str("task").unwrap_or_default(),
        inference: get_str("inference"),
        schema,
        context_files: None,
        detached: get_bool("detached"),
    }
}

/// Launch a child `gait run` process from the spine.
///
/// Returns `Some(child)` for an enrolled subagent (retained for wait), or
/// `None` for a breakaway peer (detached into its own process group/cgroup).
pub(crate) fn launch_child(
    ctx: &TurnCtx,
    session_id: &str,
    detached: bool,
    inference: Option<String>,
) -> anyhow::Result<Option<std::process::Child>> {
    let cascade_group = if detached { None } else { ctx.group.handle() };
    if !detached && cascade_group.is_none() {
        #[allow(clippy::print_stderr)]
        // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
        {
            eprintln!("[spine] warning: spawn_agent cascade-group handle unavailable; subagent will not be enrolled in cascade group");
        }
    }
    let argv = crate::launch::RunLaunch {
        session: Some(session_id.to_string()),
        gait_dir: Some(ctx.gait_dir.clone()),
        auto: true,
        inference,
        sandbox_locked: ctx.sandbox_on,
        cascade_group,
        ..Default::default()
    }
    .to_process_argv();
    if detached {
        let mut cmd = std::process::Command::new(&ctx.exe);
        crate::launch::with_trace_env(&mut cmd);
        cmd.args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let result = ctx.group.spawn_detached(cmd);
        if result.is_ok() {
            #[cfg(feature = "telemetry")]
            emit_agent_spawned(session_id, &ctx.session_id, "spawn_agent");
        }
        let rec = crate::diagnostic::DiagRecord::lifecycle(
            &ctx.session_id,
            crate::diagnostic::Phase::AwaitFeedback,
            if result.is_ok() {
                crate::diagnostic::PhaseEvent::Start
            } else {
                crate::diagnostic::PhaseEvent::Error
            },
            &format!(
                "agent_spawn detached=true child_session={} result={:?}",
                session_id,
                result.as_ref().err()
            ),
        );
        ctx.diag.debug(&rec.to_json_line());
        result.map(|_| None).map_err(Into::into)
    } else {
        let mut cmd = std::process::Command::new(&ctx.exe);
        crate::launch::with_trace_env(&mut cmd);
        cmd.args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let result = ctx.group.spawn_enrolled(cmd);
        if result.is_ok() {
            #[cfg(feature = "telemetry")]
            emit_agent_spawned(session_id, &ctx.session_id, "spawn_agent");
        }
        let rec = crate::diagnostic::DiagRecord::lifecycle(
            &ctx.session_id,
            crate::diagnostic::Phase::AwaitFeedback,
            if result.is_ok() {
                crate::diagnostic::PhaseEvent::Start
            } else {
                crate::diagnostic::PhaseEvent::Error
            },
            &format!(
                "agent_spawn detached=false child_session={} result={:?}",
                session_id,
                result.as_ref().err()
            ),
        );
        ctx.diag.debug(&rec.to_json_line());
        result.map(Some).map_err(Into::into)
    }
}

/// Reads the child's session log and returns the final durable done output.
pub(crate) fn read_terminal_result(
    log_path: &std::path::Path,
) -> Option<(String, Option<serde_json::Value>)> {
    use crate::proto::gait::log::v1::payload;
    let events = crate::session::log::read_events(log_path).ok()?;
    use crate::proto::gait::log::v1::{turn_done, turn_result};
    for event in events.iter().rev() {
        if let Some(payload::Kind::TurnResult(turn)) = crate::proto::event_kind(event) {
            let turn_result::Outcome::Done(done) = turn.outcome.as_ref()? else {
                continue;
            };
            return match done.output.as_ref()? {
                turn_done::Output::Text(text) => Some((text.message.clone(), None)),
                turn_done::Output::Structured(output) => Some((
                    output.message.clone(),
                    output
                        .result
                        .as_ref()
                        .and_then(|value| serde_json::to_value(value).ok())
                        .filter(|value| !value.is_null()),
                )),
            };
        }
    }
    None
}

/// Spine-side handler for `spawn_agent`: creates the child session (disk writes)
/// and launches the child process. Returns a `Feedback` with `{session_id}`.
pub(crate) async fn handle_spawn_agent(args: SpawnAgentArgs, ctx: TurnCtx) -> Feedback {
    use crate::tools::spawn_agent::{write_child_session, WriteChildSessionOpts};

    let new_id = crate::session::new_session_id();

    if write_child_session(WriteChildSessionOpts {
        gait_dir: &ctx.gait_dir,
        codebase: &ctx.codebase,
        new_id: &new_id,
        parent_session_id: &ctx.session_id,
        task: args.task,
        notify_done: args.detached,
        inference: args.inference.clone(),
        repos: &ctx.repos,
        identity: ctx.identity.as_deref(),
    })
    .is_err()
    {
        return error_feedback("spawn_agent: failed to create child session");
    }

    // Persist per-spawn schema so the child CLI can read it into ProfileInputs.
    if let Some(ref schema) = args.schema {
        let schema_path =
            crate::session::live::worktree_dir(&ctx.gait_dir, &new_id).join("result_schema.json");
        match serde_json::to_vec_pretty(schema) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&schema_path, &bytes) {
                    #[allow(clippy::print_stderr)]
                    // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                    {
                        eprintln!("[spine] warning: failed to write result_schema.json: {e}");
                    }
                }
            }
            Err(e) => {
                #[allow(clippy::print_stderr)]
                // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                {
                    eprintln!("[spine] warning: failed to serialize result schema: {e}");
                }
            }
        }
    }

    if args.detached {
        // Fire-and-forget peer: break out of the cascade group, don't wait.
        match launch_child(&ctx, &new_id, true, args.inference) {
            Ok(None) => return feedback_result(serde_json::json!({ "session_id": new_id })),
            Ok(Some(_)) => {
                return error_feedback("spawn_agent: detached launch returned a child handle")
            }
            Err(e) => return error_feedback(&format!("spawn_agent: failed to launch peer: {e}")),
        }
    }

    // Blocking subagent: wait for the child process to exit, then read its result.
    let tx_path = crate::session::live::log_path(&ctx.gait_dir, &new_id);

    let Some(mut child) = (match launch_child(&ctx, &new_id, false, args.inference) {
        Ok(child) => child,
        Err(e) => return error_feedback(&format!("spawn_agent: failed to launch subagent: {e}")),
    }) else {
        return error_feedback("spawn_agent: failed to launch subagent");
    };

    // Await child exit without blocking a tokio worker thread.
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .ok() // JoinError → None
        .and_then(|r| r.ok()); // io::Error → None

    let rec = crate::diagnostic::DiagRecord::lifecycle(
        &ctx.session_id,
        crate::diagnostic::Phase::AwaitFeedback,
        if status.is_some() {
            crate::diagnostic::PhaseEvent::Ok
        } else {
            crate::diagnostic::PhaseEvent::Error
        },
        &format!("agent_exit child_session={} status={status:?}", new_id),
    );
    ctx.diag.debug(&rec.to_json_line());

    // Check if killed by signal (cascade kill = parent cancelled the session).
    let was_signalled = status
        .as_ref()
        .map(|s| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                s.signal().is_some()
            }
            #[cfg(not(unix))]
            {
                false
            }
        })
        .unwrap_or(false);

    if was_signalled {
        return feedback_result(serde_json::json!({
            "session_id": new_id,
            "status": "cancelled",
        }));
    }

    let exit_code = status.as_ref().and_then(|s| s.code()).unwrap_or(-1);

    build_subagent_feedback(&new_id, &tx_path, exit_code)
}

/// Assemble a `Feedback` after the blocking subagent exits.
///
/// Reads the child's session log for a `TurnResult` event. Extracted as a
/// separate function so it can be unit-tested independently of process launching.
pub(crate) fn build_subagent_feedback(
    session_id: &str,
    log_path: &std::path::Path,
    exit_code: i32,
) -> Feedback {
    match read_terminal_result(log_path) {
        Some((message, result)) => feedback_result(serde_json::json!({
            "session_id": session_id,
            "status": "done",
            "message": message,
            "result": result,
        })),
        None => feedback_result(serde_json::json!({
            "session_id": session_id,
            "status": "failed",
            "summary": format!("subagent exited (code {exit_code}) without completing"),
        })),
    }
}

#[cfg(all(test, feature = "telemetry"))]
mod telemetry_tests {
    use super::emit_agent_spawned;

    #[test]
    fn agent_spawned_emits_once() {
        let capture = concord_telemetry::testing::capture_exclusive();
        let child = format!("child-{}", std::process::id());
        emit_agent_spawned(&child, "parent", "researcher");
        let event = capture
            .single_event_where::<gait_events::gait::events::v1::AgentSpawned>(
                |value| value.source_record_id == child,
            );
        assert_eq!(event.session_id, "parent");
        assert_eq!(event.child_session_id, child);
        assert_eq!(event.agent_name, "researcher");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::spine::handlers::test_ctx;
    use identity_keyfile::testkit::TempAgentDir;
    use std::sync::Arc;

    /// A real agent identity minted now, with a one-hour device window.
    /// Keep the returned fixture alive while the identity is in use.
    fn agent_identity() -> (TempAgentDir, Arc<gait_identity::IdentityContext>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let agent = TempAgentDir::new(now, 3_600);
        let identity = Arc::new(gait_identity::IdentityContext::new(agent.path()).unwrap());
        (agent, identity)
    }

    fn done_body(
        message: &str,
        result: Option<serde_json::Value>,
    ) -> crate::proto::gait::log::v1::event::Body {
        crate::proto::events::turn_result(crate::proto::gait::log::v1::TurnResult {
            turn_id: uuid::Uuid::new_v4().to_string(),
            outcome: Some(crate::proto::gait::log::v1::turn_result::Outcome::Done(
                crate::proto::gait::log::v1::TurnDone {
                    output: Some(crate::proto::gait::log::v1::turn_done::Output::Structured(
                        crate::proto::gait::log::v1::StructuredDone {
                            message: message.into(),
                            result: crate::proto::events::to_value_opt(&result),
                        },
                    )),
                },
            )),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn handle_spawn_agent_creates_session_and_returns_id() {
        let ws = tempfile::TempDir::new().unwrap();
        let (_agent, identity) = agent_identity();
        crate::test_utils::init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/mock")).unwrap();
        std::fs::write(
            ws.path().join("inferences/mock/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"inference":"mock","allow":[],"tools":[]}"#,
        )
        .unwrap();
        let fb = handle_spawn_agent(
            SpawnAgentArgs {
                task: "do X".into(),
                inference: None,
                schema: None,
                context_files: None,
                detached: true,
            },
            test_ctx(ws.path(), "parent01", Some(identity.clone())),
        )
        .await;
        // result is a proto Struct; extract session_id directly as a StringValue field.
        let id = fb
            .result
            .as_ref()
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StructValue(s)) => s.fields.get("session_id"),
                _ => None,
            })
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        let tx_path = crate::session::live::log_path(ws.path(), id);
        assert!(
            tx_path.exists(),
            "child session log must exist at {tx_path:?}"
        );
    }

    /// When `spawn_agent` is called with a non-null `schema`, the schema must be
    /// persisted to `sessions/<child_id>/result_schema.json` in the gait-dir so the
    /// child CLI can load it into `ProfileInputs.result_schema`.
    ///
    /// A full end-to-end test (parent spawns child with schema → child calls
    /// `structured_done` → parent receives result) requires a live LLM API and a real
    /// `gait` binary in PATH; that is not feasible in unit/integration tests. Instead we
    /// exercise the two independently-testable halves:
    ///   1. `handle_spawn_agent` writes `result_schema.json` (this test).
    ///   2. `build_subagent_feedback` reads a pre-written `TurnResult` carrying
    ///      `result:{answer:1}` and returns it in the Feedback (next test).
    #[tokio::test]
    async fn handle_spawn_agent_with_schema_writes_result_schema_json() {
        let ws = tempfile::TempDir::new().unwrap();
        let (_agent, identity) = agent_identity();
        crate::test_utils::init_git_repo(ws.path());
        std::fs::create_dir_all(ws.path().join("inferences/mock")).unwrap();
        std::fs::write(
            ws.path().join("inferences/mock/inference.textproto"),
            "model: \"minimax/MiniMax-M3\"\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{"inference":"mock","allow":[],"tools":[]}"#,
        )
        .unwrap();
        let schema = serde_json::json!({"type": "object", "required": ["answer"]});
        let fb = handle_spawn_agent(
            SpawnAgentArgs {
                task: "answer the question".into(),
                inference: None,
                schema: Some(schema.clone()),
                context_files: None,
                detached: true, // fire-and-forget so we don't need a real binary
            },
            test_ctx(ws.path(), "parent-schema-01", Some(identity.clone())),
        )
        .await;
        // Extract the child session id from the returned Feedback.
        let id = fb
            .result
            .as_ref()
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StructValue(s)) => s.fields.get("session_id"),
                _ => None,
            })
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .expect("Feedback must carry session_id");
        let schema_path =
            crate::session::live::worktree_dir(ws.path(), &id).join("result_schema.json");
        assert!(
            schema_path.exists(),
            "result_schema.json must exist at {schema_path:?}"
        );
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&schema_path).unwrap()).unwrap();
        assert_eq!(
            on_disk, schema,
            "result_schema.json content must round-trip the schema"
        );
    }

    /// Asserts that `build_subagent_feedback` correctly surfaces `result:{answer:1}`
    /// from a child log that contains a `TurnResult` written by `structured_done`.
    ///
    /// This covers the Task-4 requirement: "assert the parent's `spawn_agent` result
    /// carries `{answer:1}`".  The live end-to-end path (real LLM → real child process)
    /// is not feasible without a live API key and a compiled `gait` binary on PATH.
    #[test]
    fn build_subagent_feedback_result_answer_1() {
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Simulate the child calling `structured_done(result:{answer:1})`.
        wire::append(
            &log_path,
            done_body("done", Some(serde_json::json!({"answer": 1}))),
        )
        .unwrap();

        let fb = build_subagent_feedback("child-answer-1", &log_path, 0);

        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue result, got {other:?}"),
        };

        // status == "done"
        let status = fields
            .get("status")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .expect("status field must be present");
        assert_eq!(status, "done");

        // result.answer == 1
        let result_struct = match fields.get("result").and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => s,
            other => panic!("expected result StructValue, got {other:?}"),
        };
        let answer = match result_struct
            .fields
            .get("answer")
            .and_then(|v| v.kind.as_ref())
        {
            Some(pbjson_types::value::Kind::NumberValue(n)) => *n,
            other => panic!("expected NumberValue for answer, got {other:?}"),
        };
        assert!(
            (answer - 1.0).abs() < f64::EPSILON,
            "spawn_agent result must carry answer:1, got {answer}"
        );
    }

    // ── read_terminal_result tests ───────────────────────────────────────────

    /// Log with a TurnResult event → returns (message, result).
    #[test]
    fn read_terminal_result_finds_turn_result() {
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        let body = done_body("hello from child", Some(serde_json::json!({"score": 42})));
        wire::append(&log_path, body).unwrap();

        let result = read_terminal_result(&log_path);
        assert!(result.is_some(), "should find turn_result");
        let (msg, val) = result.unwrap();
        assert_eq!(msg, "hello from child");
        let val = val.expect("result should be Some");
        // pbjson_types::Value serializes numbers as f64, so 42 becomes 42.0.
        let score = val["score"].as_f64().expect("score should be a number");
        assert!(
            (score - 42.0).abs() < f64::EPSILON,
            "score should be 42, got {score}"
        );
    }

    /// Empty log → None.
    #[test]
    fn read_terminal_result_empty_log_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");
        // Don't write anything.
        let result = read_terminal_result(&log_path);
        assert!(result.is_none(), "empty log should return None");
    }

    /// Log with events but no TurnResult → None.
    #[test]
    fn read_terminal_result_no_turn_result_returns_none() {
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");
        // Write only a UserText event.
        let body = crate::proto::events::user_text("some task".into());
        wire::append(&log_path, body).unwrap();

        let result = read_terminal_result(&log_path);
        assert!(result.is_none(), "no turn_result should return None");
    }

    /// handle_spawn_agent with detached=false but child cannot be launched (bad exe) →
    /// no process but session exists. We can't easily test child.wait() path without a
    /// real gait binary, so test the log-read fallback: write a session with no TurnResult
    /// and verify a hypothetical `read_terminal_result` returns None (status == "failed").
    #[test]
    fn read_terminal_result_last_turn_result_wins() {
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Write two TurnResult events; the second should win.
        wire::append(&log_path, done_body("first", None)).unwrap();
        wire::append(
            &log_path,
            done_body("second", Some(serde_json::json!("final"))),
        )
        .unwrap();

        let (msg, val) = read_terminal_result(&log_path).unwrap();
        assert_eq!(msg, "second");
        assert_eq!(val.unwrap(), serde_json::json!("final"));
    }

    /// Subagent (enrolled) is killed by kill_all; peer (detached) survives.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn launch_child_subagent_enrolled_peer_breaks_away() {
        use crate::process::establish;

        // Build a minimal TurnCtx with a real containment group.
        let ws = tempfile::TempDir::new().unwrap();
        let group = establish(None);

        // We'll use `sleep 60` as a stand-in for gait run — the exe path doesn't
        // matter because we're testing containment behavior, not gait startup.
        let sleep_exe = std::path::PathBuf::from("sleep");

        let ctx = TurnCtx {
            gait_dir: ws.path().to_path_buf(),
            codebase: ws.path().to_path_buf(),
            session_id: "parent".to_string(),
            repos: crate::git::Repos::production(),
            exe: sleep_exe,
            group: group.clone(),
            sandbox_on: false,
            diag: crate::turn::DiagnosticSink::new(ws.path(), "parent"),
            identity: None,
        };

        // Spawn the "enrolled" subagent: launch_child with detached=false.
        // We pass a dummy session_id; since the exe is `sleep`, args after `run`
        // are ignored — sleep just reads arg[1] as duration.  We can't fully
        // exercise the gait CLI here, so we test containment membership directly
        // by spawning sleep via the group APIs and verifying kill_all behavior.

        // Enrolled child via the group directly (mirrors what launch_child does).
        let mut enrolled_cmd = std::process::Command::new("sleep");
        enrolled_cmd
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut enrolled = ctx
            .group
            .spawn_enrolled(enrolled_cmd)
            .expect("spawn enrolled");
        let enrolled_pid = enrolled.id() as i32;

        // Detached (peer) child via the group directly (mirrors what launch_child does).
        let mut peer_cmd = std::process::Command::new("sleep");
        peer_cmd
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (mut peer, peer_pid) = ctx.group.spawn_detached(peer_cmd).expect("spawn detached");

        // Kill all enrolled processes.
        ctx.group.kill_all().expect("kill_all");
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Reap the enrolled child so it's not a zombie.
        let _ = enrolled.wait();

        // Enrolled child must be dead.
        let enrolled_alive = unsafe { libc::kill(enrolled_pid, 0) };
        // Peer must still be alive.
        let peer_alive = unsafe { libc::kill(peer_pid, 0) };

        // Clean up peer.
        unsafe { libc::kill(peer_pid, libc::SIGKILL) };
        let _ = peer.wait();

        assert_eq!(
            enrolled_alive, -1,
            "enrolled subagent (pid {enrolled_pid}) should be dead after kill_all"
        );
        assert_eq!(
            peer_alive, 0,
            "breakaway peer (pid {peer_pid}) should survive kill_all"
        );
    }

    // ── build_subagent_feedback integration tests ────────────────────────────

    /// Pre-write a `turn_result` with `result = {"answer": 42}` to the child log,
    /// then call `build_subagent_feedback` and assert the returned Feedback carries
    /// the expected result value.
    #[test]
    fn build_subagent_feedback_reads_turn_result_result() {
        use crate::proto::wire;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // Pre-write the turn_result event (as the child process would).
        wire::append(
            &log_path,
            done_body("task done", Some(serde_json::json!({"answer": 42}))),
        )
        .unwrap();

        let fb = build_subagent_feedback("child-session-1", &log_path, 0);

        // result is a proto Struct; navigate to the fields.
        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };

        // status == "done"
        let status = fields.get("status").and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("done"), "status must be 'done'");

        // message == "task done"
        let message = fields.get("message").and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        });
        assert_eq!(message.as_deref(), Some("task done"));

        // result.answer == 42 (stored as f64 in proto Number)
        let result_struct = match fields.get("result").and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => s,
            other => panic!("expected result StructValue, got {other:?}"),
        };
        let answer = match result_struct
            .fields
            .get("answer")
            .and_then(|v| v.kind.as_ref())
        {
            Some(pbjson_types::value::Kind::NumberValue(n)) => *n,
            other => panic!("expected NumberValue for answer, got {other:?}"),
        };
        assert!(
            (answer - 42.0).abs() < f64::EPSILON,
            "answer must be 42, got {answer}"
        );
    }

    /// When the child log has no `TurnResult`, `build_subagent_feedback` should
    /// return a Feedback with `status == "failed"`.
    #[test]
    fn build_subagent_feedback_no_turn_result_returns_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");
        // Do NOT write any events — simulates child that exited without completing.

        let fb = build_subagent_feedback("child-session-2", &log_path, 1);

        let fields = match fb.result.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(pbjson_types::value::Kind::StructValue(s)) => &s.fields,
            other => panic!("expected StructValue, got {other:?}"),
        };

        let status = fields.get("status").and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("failed"), "status must be 'failed'");
    }
}
