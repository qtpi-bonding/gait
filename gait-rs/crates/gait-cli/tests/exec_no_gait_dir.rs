// crates/gait-cli/tests/exec_no_gait_dir.rs
// NOTE: this test requires a mock brain (via CARGO_BIN_EXE_mock_brain env hook) and
// a real gait binary as current_exe(). It is ignored until that infrastructure is wired
// up (see spine_ephemeral.rs for the correct integration test path).
#[tokio::test]
#[ignore]
async fn run_one_off_resolves_named_inference_from_gait_dir() {
    // (See below for `full_one_shot_runs_one_sealed_turn`, an unrelated,
    // un-ignored test against `cmd_run` — this test exercises the ephemeral
    // `-e` one-off path (`run_one_off`), a structurally distinct code path.)
    // The stub brain is selected by the same env hook the spine tests use.
    std::env::set_var("ANTHROPIC_API_KEY", "test");
    let cwd = std::env::current_dir().unwrap();
    let gait_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(gait_dir.path().join("inferences/test")).unwrap();
    std::fs::write(
        gait_dir.path().join("inferences/test/inference.textproto"),
        "model: \"anthropic/claude-haiku-4-5-20251001\"\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    let r = gait_cli::cli::exec::run_one_off(
        "untar a.tar",
        Some("test".into()),
        gait_dir.path(),
        &cwd,
        None,
    )
    .await;
    assert!(matches!(
        r.unwrap(),
        gait_cli::cli::exec::OneOff::Cmd(_) | gait_cli::cli::exec::OneOff::Output(_)
    ));
}

// ── full_one_shot_runs_one_sealed_turn ──────────────────────────────────────
//
// Characterization test (see the gait-min-composition plan's Task 1):
// drives `gait_cli::cli::run::cmd_run` — the real `gait run --task ...`
// entry point — end to end against a fresh git-inited temp gait-dir, with
// `GAIT_BRAIN_EXE` pointed at the `mock_brain` dev-harness binary wired up
// in `Cargo.toml` (requires `--features dev-harness`; see that file's
// `[[bin]]`/`[features]` entries). Asserts the resulting session log holds
// exactly one `UserText` event and one completed `TurnResult`, read back via
// `gait_core::session::Ingestor`, the same pattern
// `gait-repl/src/dev_harness.rs::run_script` uses.
//
// `cmd_run` calls `std::process::exit` on several non-happy-path branches
// (see `cli/run.rs`); this test only exercises the happy path, which never
// reaches those.
mod full_one_shot {
    use std::path::PathBuf;

    use gait_core::proto::envelope;
    use gait_core::proto::gait::log::v1 as pb;
    use gait_core::proto::gait::sandbox::v1::{signal, Append, FinalizedTurn, Signal};
    use prost::Message;

    fn init_git_repo(dir: &std::path::Path) {
        let repo = git2::Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        repo.find_branch("master", git2::BranchType::Local)
            .and_then(|mut b| {
                b.rename("main", false)?;
                Ok(())
            })
            .ok();
    }

    // [version u8][len u32 BE][len bytes] — the mock brain's frame-blob wire
    // format (see `gait-testkit/src/mock_brain.rs::read_one_frame`).
    fn frame(sig: &Signal) -> Vec<u8> {
        let b = sig.encode_to_vec();
        let mut f = vec![1u8];
        f.extend_from_slice(&(b.len() as u32).to_be_bytes());
        f.extend_from_slice(&b);
        f
    }

    fn append_assistant(msg: &str) -> Signal {
        let event = envelope::mint(
            gait_core::proto::events::assistant_text(msg.into()),
            None,
            envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        Signal {
            corr_id: 0,
            body: Some(signal::Body::Append(Append { event: Some(event) })),
        }
    }

    fn terminated_ok(msg: &str) -> Signal {
        use gait_core::proto::gait::sandbox::v1::{FinalizedTurnOutcome, Terminated};
        Signal {
            corr_id: 0,
            body: Some(signal::Body::Terminated(Terminated {
                turn: Some(FinalizedTurn {
                    outcome: FinalizedTurnOutcome::TextDone as i32,
                    message: msg.into(),
                    ..Default::default()
                }),
                failed: false,
            })),
        }
    }

    fn write_frames(gait_dir: &std::path::Path, signals: &[Signal]) {
        std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
        std::fs::write(
            gait_dir.join("inferences/mock/inference.textproto"),
            "model: \"mock/model\"\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        let mut blob = Vec::new();
        for s in signals {
            blob.extend_from_slice(&frame(s));
        }
        std::fs::write(gait_dir.join("mock_frames.bin"), blob).unwrap();
    }

    #[tokio::test]
    async fn full_one_shot_runs_one_sealed_turn() {
        let ws = tempfile::tempdir().unwrap();
        let gait_dir = ws.path().to_path_buf();
        init_git_repo(&gait_dir);
        std::fs::create_dir_all(gait_dir.join(".active")).unwrap();
        std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();
        std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();

        write_frames(
            &gait_dir,
            &[append_assistant("done"), terminated_ok("done")],
        );

        // Keep credential discovery hermetic — same reasoning as
        // `dev_harness.rs::run_script_fresh`.
        let auth_home = ws.path().join("gait-home");
        std::fs::create_dir_all(&auth_home).unwrap();
        std::env::set_var("GAIT_HOME", &auth_home);
        std::env::set_var(
            "GAIT_BRAIN_EXE",
            PathBuf::from(env!("CARGO_BIN_EXE_mock_brain")),
        );

        let session_id = "s00000001".to_string();
        let argv: Vec<String> = vec![
            "--session".to_string(),
            session_id.clone(),
            "--task".to_string(),
            "hello from the test".to_string(),
            "--allow-intree".to_string(),
        ];

        gait_cli::cli::run::cmd_run(&argv, gait_dir.clone(), gait_dir.clone())
            .await
            .expect("cmd_run succeeds against the mock brain");

        let events = gait_core::session::Ingestor::from_session(&gait_dir, &session_id)
            .events()
            .expect("read session events");

        let user_texts = events
            .iter()
            .filter(|e| {
                matches!(
                    gait_core::proto::event_kind(e),
                    Some(pb::payload::Kind::UserText(_))
                )
            })
            .count();
        assert_eq!(
            user_texts, 1,
            "exactly one user_turn (UserText) event must be recorded: {events:?}"
        );

        let turn_results: Vec<&pb::TurnResult> = events
            .iter()
            .filter_map(|e| match gait_core::proto::event_kind(e) {
                Some(pb::payload::Kind::TurnResult(tr)) => Some(tr),
                _ => None,
            })
            .collect();
        assert_eq!(
            turn_results.len(),
            1,
            "exactly one completed TurnResult must be recorded: {events:?}"
        );
        assert!(
            matches!(
                turn_results[0].outcome,
                Some(pb::turn_result::Outcome::Done(_))
            ),
            "the recorded TurnResult must be a completed Done outcome: {:?}",
            turn_results[0]
        );
    }
}
