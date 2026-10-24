use super::*;
use crate::proto::{events, wire};
use identity_keyfile::testkit::TempAgentDir;
use tempfile::TempDir;

fn write_events(dir: &std::path::Path, bodies: Vec<event::Body>) -> std::path::PathBuf {
    let path = dir.join("log.jsonl");
    for b in bodies {
        wire::append(&path, b).unwrap();
    }
    path
}

/// Build in-memory proto events from bodies (seq is irrelevant for these
/// assertions; each is minted as a root).
fn mint_events(bodies: Vec<event::Body>) -> Vec<Event> {
    bodies
        .into_iter()
        .map(|body| scoped_mint(body, None, 0, 0))
        .collect()
}

fn scoped_mint(body: event::Body, prev: Option<&Event>, turn: u32, step: u32) -> Event {
    crate::proto::envelope::mint(
        body,
        prev,
        crate::proto::envelope::EventScope::Step { run: 0, turn, step },
    )
}

fn durable_turn(usage: pb::TurnTokenUsage, blocks: Vec<String>) -> event::Body {
    events::turn_result(pb::TurnResult {
        turn_id: uuid::Uuid::new_v4().to_string(),
        usage: Some(usage),
        blocks,
        outcome: Some(pb::turn_result::Outcome::Pause(pb::TurnPause::default())),
        ..Default::default()
    })
}

#[test]
fn empty_transcript_returns_empty() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    let msgs = messages_from_path(&path).unwrap();
    assert!(msgs.is_empty());
}

#[test]
fn coalesce_defers_interstitial_message_until_after_tool_result() {
    let raw = vec![
        Message::Assistant {
            content: AssistantContent::Parts(vec![AssistantPart::ToolCall(ToolCall {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                args: serde_json::json!({}),
            })]),
            id: "call-event".into(),
        },
        Message::User {
            content: MessageContent::Text("error during llm: timed out".into()),
            id: "error-event".into(),
            attribution: Vec::new(),
        },
        Message::ToolResult {
            content: vec![ToolResult {
                kind: ToolResultKind::ToolResult,
                tool_call_id: "call-1".into(),
                result: serde_json::Value::String("timed out".into()),
            }],
            id: "result-event".into(),
        },
    ];

    let messages = coalesce(raw);

    assert!(matches!(messages[0], Message::Assistant { .. }));
    assert!(matches!(messages[1], Message::ToolResult { .. }));
    assert!(matches!(messages[2], Message::User { .. }));
}

#[test]
fn user_and_assistant_text_reconstructed() {
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            events::session_start("s1".into(), None, None, false),
            events::user_text("hello".into()),
            events::assistant_text("hi there".into()),
        ],
    );
    let msgs = messages_from_path(&path).unwrap();
    assert_eq!(msgs.len(), 2); // session_start is skipped
    assert!(
        matches!(&msgs[0], Message::User { content: MessageContent::Text(t), .. } if t == "hello")
    );
    assert!(
        matches!(&msgs[1], Message::Assistant { content: AssistantContent::Text(t), .. } if t == "hi there")
    );
}

#[test]
fn tool_call_and_result_paired() {
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            events::tool_call(
                "id1".into(),
                "read".into(),
                &serde_json::json!({"file_path":"a.rs"}),
            ),
            events::tool_result("id1".into(), "contents".into(), None, None),
        ],
    );
    let msgs = messages_from_path(&path).unwrap();
    assert_eq!(msgs.len(), 2);
    if let Message::Assistant {
        content: AssistantContent::Parts(parts),
        ..
    } = &msgs[0]
    {
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], AssistantPart::ToolCall(tc) if tc.tool_call_id == "id1"));
    } else {
        panic!("expected assistant parts");
    }
    assert!(matches!(&msgs[1], Message::ToolResult { .. }));
}

#[test]
fn sidecar_event_between_call_and_result_does_not_drop_result() {
    // The shell tool emits a Cwd event BETWEEN a tool_call and its tool_result
    // (hooks can emit HookRun similarly). These are non-context sidecar events
    // and must NOT break the tool-call→tool-result run. If they reset the
    // coalescing phase, the result is dropped as an orphan on resume, leaving
    // a dangling tool_use that providers reject ("tool call result does not
    // follow tool call").
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            events::tool_call(
                "c1".into(),
                "shell".into(),
                &serde_json::json!({"command":"cd sub && ls"}),
            ),
            events::cwd("/ws/sub"),
            events::tool_result("c1".into(), "out1".into(), None, None),
            events::tool_call(
                "c2".into(),
                "shell".into(),
                &serde_json::json!({"command":"git status"}),
            ),
            events::hook_run(
                "post_tool",
                "echo hi",
                0,
                "hi",
                &crate::types::HookContext {
                    tool_call_id: "c2".into(),
                    ..Default::default()
                },
            ),
            events::tool_result("c2".into(), "out2".into(), None, None),
        ],
    );
    let msgs = messages_from_path(&path).unwrap();
    let tool_msgs = msgs
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_msgs, 2,
        "both tool_results must survive interleaved Cwd/HookRun sidecar events; got {:?}",
        msgs
    );
}

#[test]
fn llm_call_events_are_transparent_to_tool_run_coalescing() {
    // LlmCallStarted/LlmCallCompleted must never appear between a ToolCall
    // and its ToolResult in the current step loop, but the fold must not
    // silently break if they do (see retrace/mod.rs:685-690 for the same
    // guarantee already required of Cwd/HookRun).
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            events::tool_call(
                "c1".into(),
                "shell".into(),
                &serde_json::json!({"command":"ls"}),
            ),
            events::llm_call_started("test-model".into()),
            events::llm_call_completed(
                crate::proto::gait::log::v1::StepTokenUsage::default(),
                0,
                false,
            ),
            events::tool_result("c1".into(), "out1".into(), None, None),
        ],
    );
    let msgs = messages_from_path(&path).unwrap();
    let tool_msgs = msgs
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_msgs, 1,
        "tool_result must survive an interleaved LlmCallStarted/Completed pair; got {:?}",
        msgs
    );
    assert!(
        !msgs.iter().any(|m| matches!(
            m,
            Message::User {
                content: MessageContent::Text(_),
                ..
            }
        ) || matches!(
            m,
            Message::Assistant {
                content: AssistantContent::Text(_),
                ..
            }
        )),
        "LlmCallStarted/Completed must not surface as their own message; got {:?}",
        msgs
    );
}

#[test]
fn parallel_tool_calls_grouped() {
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            events::tool_call("id1".into(), "read".into(), &serde_json::json!({})),
            events::tool_call("id2".into(), "ls".into(), &serde_json::json!({})),
            events::tool_result("id1".into(), "r1".into(), None, None),
            events::tool_result("id2".into(), "r2".into(), None, None),
        ],
    );
    let msgs = messages_from_path(&path).unwrap();
    assert_eq!(msgs.len(), 2);
    if let Message::Assistant {
        content: AssistantContent::Parts(parts),
        ..
    } = &msgs[0]
    {
        assert_eq!(parts.len(), 2);
    } else {
        panic!("expected 2 tool calls in one assistant message");
    }
    if let Message::ToolResult { content, .. } = &msgs[1] {
        assert_eq!(content.len(), 2);
    } else {
        panic!("expected tool message with 2 results");
    }
}

#[test]
fn permissions_excluded_from_context_reconstruction() {
    let bodies = vec![
        events::user_text("do x".into()),
        events::permissions(&[pb::PermissionItem {
            tool_args: crate::proto::events::to_struct(&serde_json::Value::Null),
            ..Default::default()
        }]),
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "tc1".into(),
            decision: pb::Decision::Allow as i32,
            text: None,
            scope: pb::Scope::Unspecified as i32,
        }]),
    ];
    let messages = messages(&mint_events(bodies)).unwrap();
    assert_eq!(
        messages.len(),
        1,
        "only UserTurn should appear; got: {} messages",
        messages.len()
    );
    assert!(matches!(&messages[0], Message::User { .. }));
}

#[test]
fn compaction_breakpoint_discards_earlier_events() {
    // This test exercises the `ActiveMessages` Summary-checkpoint behaviour:
    // a Summary event resets the message context to the synthesized summary
    // baseline. `ActiveRawMessages` has the same positional checkpoint
    // semantics; this asserts the normalized projection directly.
    use std::collections::HashMap;
    let dir = TempDir::new().unwrap();
    let usage = pb::TurnTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let cu = pb::CompactionTokenUsage {
        input: 10,
        output: 5,
        cache_read: 0,
        cache_write: 0,
    };
    let path = write_events(
        dir.path(),
        vec![
            events::user_text("old".into()),
            events::assistant_text("old response".into()),
            durable_turn(usage, vec![]),
            events::summary("summary of old stuff".into(), cu, Some(1)),
            events::user_text("new".into()),
        ],
    );
    let all = read_events(&path).unwrap();
    let msgs = run_projection(&ActiveMessages, &all, &HashMap::new()).msgs;
    // Should have: compaction summary + "new" user turn
    assert_eq!(msgs.len(), 2, "got: {:?}", msgs.len());
    assert!(
        matches!(&msgs[0], Message::User { content: MessageContent::Text(t), .. } if t.contains("summary of old stuff"))
    );
    assert!(
        matches!(&msgs[1], Message::User { content: MessageContent::Text(t), .. } if t == "new")
    );
}

#[test]
fn enabled_loadables_seeds_from_config_and_replays_toggles() {
    use std::collections::HashSet;
    let config_enabled: HashSet<String> = ["tdd".into()].into();
    let bodies = vec![
        events::toggle("mcp", "slack_send", true),
        events::toggle("skill", "tdd", false),
    ];
    let seed = AgentState {
        skills: config_enabled,
        mcp: HashSet::new(),
        ..Default::default()
    };
    let st = active_agent(&mint_events(bodies), seed);
    let enabled = st.enabled_loadables();
    assert!(
        enabled.contains("slack_send"),
        "slack should be enabled after load"
    );
    assert!(
        !enabled.contains("tdd"),
        "tdd should be disabled after unload (LWW per entity)"
    );
}

#[test]
fn both_projections_derive_from_one_event_log() {
    let bodies = vec![
        events::user_text("hi".into()),
        events::toggle("mcp", "slack_send", true),
    ];
    let proto_events = mint_events(bodies);
    let msgs = messages(&proto_events).unwrap();
    assert!(
        matches!(&msgs[0], Message::User { content: MessageContent::Text(t), .. } if t == "hi")
    );
    let enabled = active_agent(&proto_events, AgentState::default()).enabled_loadables();
    assert!(enabled.contains("slack_send"));
}

#[test]
fn active_agent_checkpoint_returns_last_agent() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    // Write two Agent events — active_agent_checkpoint should return the second.
    wire::append(
        &path,
        wire::body(pb::payload::Kind::Agent(pb::Agent {
            inference: Some(pb::Inference {
                name: "a".into(),
                model: "model-a".into(),
                llm_params: None,
                compaction_threshold_tokens: 128_000,
            }),
            tools: vec![],
            mcp_servers: vec![],
            skills: vec![],
            plugins: vec![],
            permissions: Some(pb::PermissionPolicy {
                allow: vec![],
                deny: vec![],
            }),
            instructions: String::new(),
            through_seq: 0,
        })),
    )
    .unwrap();
    wire::append(
        &path,
        wire::body(pb::payload::Kind::Agent(pb::Agent {
            inference: Some(pb::Inference {
                name: "b".into(),
                model: "model-b".into(),
                llm_params: None,
                compaction_threshold_tokens: 128_000,
            }),
            tools: vec![],
            mcp_servers: vec![],
            skills: vec![],
            plugins: vec![],
            permissions: Some(pb::PermissionPolicy {
                allow: vec![],
                deny: vec![],
            }),
            instructions: String::new(),
            through_seq: 0,
        })),
    )
    .unwrap();

    let all_events = read_events(&path).unwrap();
    let r = active_agent_checkpoint(&all_events).expect("an agent");
    assert_eq!(r.inference.unwrap().model, "model-b");
}

#[test]
fn active_agent_checkpoint_returns_none_for_no_agent() {
    let bodies = vec![
        events::user_text("hi".into()),
        events::assistant_text("hello".into()),
    ];
    let proto_events = mint_events(bodies);
    assert!(active_agent_checkpoint(&proto_events).is_none());
}

#[test]
fn active_mode_folds_lww_and_empty_clears() {
    let evs = mint_events(vec![
        events::mode("plan", vec!["write".into()], vec![], "s", "", None),
        events::mode("code", vec![], vec![], "", "", None),
    ]);
    assert_eq!(active_mode(&evs, &HashMap::new()).unwrap().name, "code");

    let cleared = mint_events(vec![
        events::mode("plan", vec![], vec![], "", "", None),
        events::mode("", vec![], vec![], "", "", None),
    ]);
    assert!(active_mode(&cleared, &HashMap::new()).is_none());
}

#[test]
fn active_mode_survives_agent_rebaseline() {
    // finding #1 regression: an Agent checkpoint between Mode events must NOT
    // clear the active mode (it would if `mode` were a field on AgentState).
    let evs = mint_events(vec![
        events::mode("plan", vec!["write".into()], vec![], "s", "", None),
        wire::body(payload::Kind::Agent(pb::Agent {
            ..Default::default()
        })), // drift re-emit
    ]);
    assert_eq!(
        active_mode(&evs, &HashMap::new()).unwrap().name,
        "plan",
        "Agent re-baseline must not clear mode",
    );
}

// ── AgentState + conversions (T1 + T2) ──────────────────────────────────────

#[test]
fn agent_state_from_pb_agent_maps_enabled_sets() {
    let a = pb::Agent {
        inference: Some(pb::Inference {
            name: "x".into(),
            model: "x/y".into(),
            llm_params: None,
            compaction_threshold_tokens: 128_000,
        }),
        instructions: "sys".into(),
        tools: vec!["read".into(), "edit".into()],
        mcp_servers: vec!["slack".into()],
        skills: vec!["tdd".into()],
        plugins: vec![],
        permissions: Some(pb::PermissionPolicy {
            allow: vec!["read".into()],
            deny: vec!["shell:rm *".into()],
        }),
        through_seq: 0,
    };
    let st = AgentState::from(&a);
    assert_eq!(st.model, "x/y");
    assert_eq!(st.instructions, "sys");
    assert!(st.skills.contains("tdd"));
    assert!(st.mcp.contains("slack"));
    assert!(st.tools.contains("read") && st.tools.contains("edit"));
    assert!(st.allow.contains("read"));
    assert!(st.deny.contains("shell:rm *"));
}

// ── ActiveAgent projection (T3 + T4) ────────────────────────────────────────

#[test]
fn active_agent_folds_load_toolcall_like_loadables() {
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        events::toggle("mcp", "slack", true),
        events::toggle("mcp", "slack", false),
        events::toggle("skill", "tdd", true),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    let names = st.enabled_loadables();
    assert!(!names.contains("slack"), "LWW: slack toggled off");
    assert!(names.contains("tdd"), "tdd toggled on");
}

#[test]
fn active_agent_checkpoint_resets_baseline() {
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        events::toggle("skill", "old", true),
        wire::body(payload::Kind::Agent(pb::Agent {
            inference: Some(pb::Inference {
                name: "m2".into(),
                model: "m2".into(),
                llm_params: None,
                compaction_threshold_tokens: 128_000,
            }),
            instructions: "sys2".into(),
            tools: vec![],
            mcp_servers: vec!["slack".into()],
            skills: vec![],
            plugins: vec![],
            permissions: Some(pb::PermissionPolicy {
                allow: vec![],
                deny: vec![],
            }),
            through_seq: 0,
        })),
        events::toggle("skill", "tdd", true),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert_eq!(st.model, "m2", "checkpoint reset the model");
    let names = st.enabled_loadables();
    assert!(
        !names.contains("old"),
        "pre-checkpoint load wiped by Agent reset"
    );
    assert!(names.contains("slack"), "checkpoint's mcp present");
    assert!(names.contains("tdd"), "post-checkpoint load folded");
}

#[test]
fn active_agent_honors_mutations_redacted_load_dropped() {
    use std::collections::HashMap;
    let seed = AgentState::default();
    let one = mint_events(vec![events::toggle("mcp", "slack", true)]);
    let mut muts = HashMap::new();
    muts.insert(one[0].id.clone(), MutationState::Redacted { reason: None });
    let st = run_projection(&ActiveAgent { seed }, &one, &muts);
    assert!(
        !st.enabled_loadables().contains("slack"),
        "redacted load disabled"
    );
}

#[test]
fn active_agent_projection_checkpoint_is_last_write_wins() {
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        wire::body(payload::Kind::Agent(pb::Agent {
            inference: Some(pb::Inference {
                name: "m1".into(),
                model: "m1".into(),
                llm_params: None,
                compaction_threshold_tokens: 128_000,
            }),
            ..Default::default()
        })),
        wire::body(payload::Kind::Agent(pb::Agent {
            inference: Some(pb::Inference {
                name: "m2".into(),
                model: "m2".into(),
                llm_params: None,
                compaction_threshold_tokens: 128_000,
            }),
            ..Default::default()
        })),
    ]);
    let got = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert_eq!(got.model, "m2", "latest Agent checkpoint wins");
}

#[test]
fn active_agent_and_messages_projections_agree_over_the_same_event_set() {
    // Was `fold_returns_all_three_projections`, driven through the dead
    // `fold()` wrapper (deleted -- zero real callers, see the plan). Same
    // event set, same assertions, now via the real per-projection entry
    // points `messages()`/`ActiveAgent` that production code actually calls.
    use std::collections::HashMap;
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            wire::body(pb::payload::Kind::Agent(pb::Agent {
                inference: Some(pb::Inference {
                    name: "gpt".into(),
                    model: "gpt-fold".into(),
                    llm_params: None,
                    compaction_threshold_tokens: 128_000,
                }),
                tools: vec![],
                mcp_servers: vec![],
                skills: vec![],
                plugins: vec![],
                permissions: Some(pb::PermissionPolicy {
                    allow: vec![],
                    deny: vec![],
                }),
                instructions: String::new(),
                through_seq: 0,
            })),
            events::user_text("hi".into()),
            events::toggle("skill", "tdd", true),
        ],
    );
    let all_events = read_events(&path).unwrap();
    let seed = AgentState::default();

    // messages: user turn (no summary-only events, user_turn becomes a Message)
    assert!(!messages(&all_events).unwrap().is_empty());
    let agent = run_projection(&ActiveAgent { seed }, &all_events, &HashMap::new());
    // agent.model: the checkpoint we wrote
    assert_eq!(agent.model, "gpt-fold");
    // enabled_loadables: tdd was toggled on
    assert!(agent.enabled_loadables().contains("tdd"));
}

#[test]
fn active_agent_and_permissions_projections_agree_over_the_same_event_set() {
    // Was `fold_runs_all_projections_single_pass` -- see the note above.
    use std::collections::HashMap;
    let evs = mint_events(vec![events::toggle("mcp", "slack", true)]);
    let seed = AgentState::default();
    let agent = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(agent.enabled_loadables().contains("slack"));
    let allow_state = run_projection(
        &ActivePermissions {
            seed: Default::default(),
        },
        &evs,
        &HashMap::new(),
    );
    assert!(
        allow_state.session.is_empty() && allow_state.once.is_empty(),
        "permission stub yields no grants yet"
    );
}

#[test]
fn active_messages_coalesces_consecutive_tool_calls_and_results() {
    use std::collections::HashMap;
    // Two consecutive tool_calls then two consecutive tool_results must render
    // as ONE assistant(Parts[2]) message followed by ONE tool message[2].
    let evs = mint_events(vec![
        events::tool_call(
            "c1".into(),
            "shell".into(),
            &serde_json::json!({"cmd":"ls"}),
        ),
        events::tool_call(
            "c2".into(),
            "shell".into(),
            &serde_json::json!({"cmd":"pwd"}),
        ),
        events::tool_result("c1".into(), "a".into(), None, None),
        events::tool_result("c2".into(), "b".into(), None, None),
    ]);
    let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
    assert_eq!(msgs.len(), 2, "one assistant parts msg + one tool msg");
    match &msgs[0] {
        Message::Assistant {
            content: AssistantContent::Parts(parts),
            ..
        } => assert_eq!(
            parts.len(),
            2,
            "two tool calls coalesced into one assistant message"
        ),
        other => panic!("expected Assistant Parts, got {other:?}"),
    }
    match &msgs[1] {
        Message::ToolResult { content, .. } => assert_eq!(
            content.len(),
            2,
            "two tool results coalesced into one tool message"
        ),
        other => panic!("expected Tool message, got {other:?}"),
    }
}

#[test]
fn active_messages_resets_on_compaction() {
    use std::collections::HashMap;
    // A user turn before a compaction must be cleared; only the summary +
    // anything after survive.
    let cu = pb::CompactionTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let evs = mint_events(vec![
        events::user_text("before".into()),
        events::summary("the summary".into(), cu, Some(1)),
        events::user_text("after".into()),
    ]);
    let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
    assert_eq!(
        msgs.len(),
        2,
        "summary + post-compaction turn (pre-compaction cleared)"
    );
    assert!(matches!(&msgs[0],
        Message::User { content: MessageContent::Text(t), .. } if t.contains("the summary")));
    assert!(matches!(&msgs[1],
        Message::User { content: MessageContent::Text(t), .. } if t == "after"));
}

#[test]
fn active_messages_two_compactions_second_wins() {
    use std::collections::HashMap;
    // Two Compaction events in one log. Each resets the message context to its
    // own summary; the SECOND summary replaces the first (positional reset, C1).
    let cu = pb::CompactionTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let evs = mint_events(vec![
        events::user_text("before-first".into()),
        events::summary("first summary".into(), cu, Some(1)),
        events::user_text("between".into()),
        events::summary("second summary".into(), cu, Some(2)),
        events::user_text("after-second".into()),
    ]);
    let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
    // Only the second summary + the turn after it survive; the first summary and
    // everything before the second compaction are gone.
    assert_eq!(
        msgs.len(),
        2,
        "second summary + post-second turn, got: {msgs:?}"
    );
    assert!(
        matches!(&msgs[0],
        Message::User { content: MessageContent::Text(t), .. }
        if t.contains("second summary") && !t.contains("first summary")),
        "second compaction's summary replaces the first"
    );
    assert!(matches!(&msgs[1],
        Message::User { content: MessageContent::Text(t), .. } if t == "after-second"));
}

#[test]
fn active_raw_messages_checkpoint_keeps_only_latest_summary_and_later_pairs() {
    use std::collections::HashMap;
    let usage = pb::CompactionTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let evs = mint_events(vec![
        events::user_text("old".into()),
        events::tool_call("old-call".into(), "read".into(), &serde_json::Value::Null),
        events::tool_result("old-call".into(), "old result".into(), None, None),
        events::summary("checkpoint".into(), usage, Some(3)),
        events::tool_call("new-call".into(), "read".into(), &serde_json::Value::Null),
        events::tool_result("new-call".into(), "new result".into(), None, None),
    ]);
    let raw = run_projection(&ActiveRawMessages::default(), &evs, &HashMap::new());
    assert_eq!(raw.base.len(), 3, "summary plus the later complete pair");
    assert!(
        matches!(&raw.base[0], Message::User { content: MessageContent::Text(text), .. } if text.contains("checkpoint"))
    );
    assert!(matches!(&raw.base[1], Message::Assistant { .. }));
    assert!(matches!(&raw.base[2], Message::ToolResult { .. }));
}

#[test]
fn permissions_session_persists_once_expires_on_run_complete() {
    use std::collections::{HashMap, HashSet};
    let evs = mint_events(vec![
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "1".into(),
            decision: pb::Decision::Allow as i32,
            text: Some("shell:cargo test".into()),
            scope: pb::Scope::Session as i32,
        }]),
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "2".into(),
            decision: pb::Decision::Allow as i32,
            text: Some("shell:ls".into()),
            scope: pb::Scope::Once as i32,
        }]),
        events::run_complete(pb::RunComplete::default()),
    ]);
    let st = run_projection(
        &ActivePermissions {
            seed: HashSet::new(),
        },
        &evs,
        &HashMap::new(),
    );
    assert!(
        st.session.contains("shell:cargo test"),
        "session grant persists past RunComplete"
    );
    assert!(
        !st.once.contains("shell:ls"),
        "once grant expires at RunComplete"
    );
}

#[test]
fn permissions_session_deny_revokes_but_ledger_keeps_both() {
    use std::collections::{HashMap, HashSet};
    let evs = mint_events(vec![
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "1".into(),
            decision: pb::Decision::Allow as i32,
            text: Some("shell:rm".into()),
            scope: pb::Scope::Session as i32,
        }]),
        events::permissions_reply(&[pb::PermissionsReplyItem {
            id: "2".into(),
            decision: pb::Decision::Deny as i32,
            text: Some("shell:rm".into()),
            scope: pb::Scope::Session as i32,
        }]),
    ]);
    // Append-only ledger: both the approve and the deny remain on disk.
    assert_eq!(evs.len(), 2, "approve and deny both stay in the log");
    let st = run_projection(
        &ActivePermissions {
            seed: HashSet::new(),
        },
        &evs,
        &HashMap::new(),
    );
    assert!(
        !st.session.contains("shell:rm"),
        "forward deny revokes from the derived set"
    );
}

#[test]
fn permissions_session_deny_populates_deny_session_bucket() {
    // The Deny-side mirror of `permissions_projection_effective_allow_includes_session_grant`
    // below: a session-scoped Reject must land in `deny_session` so
    // `grants::effective_deny_session` can union it into the gate's `deny`
    // list — an identical request should never be asked about again.
    use std::collections::{HashMap, HashSet};
    let evs = mint_events(vec![events::permissions_reply(&[
        pb::PermissionsReplyItem {
            id: "1".into(),
            decision: pb::Decision::Deny as i32,
            text: Some("shell:curl *".into()),
            scope: pb::Scope::Session as i32,
        },
    ])]);
    let st = run_projection(
        &ActivePermissions {
            seed: HashSet::new(),
        },
        &evs,
        &HashMap::new(),
    );
    assert!(
        st.deny_session.contains("shell:curl *"),
        "session-scoped reject must land in deny_session"
    );
    assert!(
        !st.session.contains("shell:curl *"),
        "a bare reject with no prior allow must not also appear in the allow-side set"
    );
}

#[test]
fn permissions_projection_effective_allow_includes_session_grant() {
    // Was `fold_effective_allow_includes_session_grant`, driven through the
    // dead `fold()` wrapper (deleted). `effective_allow` was just
    // `ActivePermissions`'s session ∪ once union, computed directly here.
    use std::collections::HashMap;
    let evs = mint_events(vec![events::permissions_reply(&[
        pb::PermissionsReplyItem {
            id: "1".into(),
            decision: pb::Decision::Allow as i32,
            text: Some("shell:git status".into()),
            scope: pb::Scope::Session as i32,
        },
    ])]);
    let allow_state = run_projection(
        &ActivePermissions {
            seed: Default::default(),
        },
        &evs,
        &HashMap::new(),
    );
    let effective_allow: std::collections::HashSet<String> = allow_state
        .session
        .union(&allow_state.once)
        .cloned()
        .collect();
    assert!(
        effective_allow.contains("shell:git status"),
        "session grants must flatten into effective_allow"
    );
}

#[test]
fn effective_kinds_skips_redacted_and_keeps_others() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    // An enable tool_call (will be redacted) + an assistant_text (kept).
    let tc = wire::append(
        &path,
        events::tool_call(
            "tc1".into(),
            "enable".into(),
            &serde_json::json!({"name":"slack","enabled":true}),
        ),
    )
    .unwrap();
    wire::append(&path, redact_body(&tc.id)).unwrap();
    wire::append(&path, events::assistant_text("hi".into())).unwrap();

    let all = read_events(&path).unwrap();
    let mut muts = active_mutations(&all);
    apply_cascade(&mut muts, &all);

    let kinds: Vec<_> = effective_kinds(&all, &muts).collect();
    // redacted enable must NOT appear:
    assert!(
        kinds
            .iter()
            .all(|(_, k)| !matches!(k, payload::Kind::ToolCall(tc) if tc.name == "enable")),
        "redacted enable must not appear in effective kinds"
    );
    // the non-redacted assistant_text MUST appear:
    assert!(
        kinds
            .iter()
            .any(|(_, k)| matches!(k, payload::Kind::AssistantText(_))),
        "non-redacted event must appear in effective kinds"
    );
}

// --- Phase 4 mutation tests ---

use crate::proto::gait::log::v1::{Redact, Replace};

fn redact_body(target: &str) -> event::Body {
    event::Body::Redact(Redact {
        target: target.into(),
        reason: None,
    })
}

fn replace_body(target: &str, new_payload: pb::Payload) -> event::Body {
    event::Body::Replace(Replace {
        target: target.into(),
        replacement: Some(new_payload),
        reason: None,
    })
}

#[test]
fn redact_removes_event_from_messages() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    // Write a UserTurn, then a Redact targeting it.
    let user_event = wire::append(&path, events::user_text("hello".into())).unwrap();
    wire::append(&path, redact_body(&user_event.id)).unwrap();

    let all = read_events(&path).unwrap();
    let mut muts = active_mutations(&all);
    apply_cascade(&mut muts, &all);
    let msgs = run_projection(&ActiveMessages, &all, &muts).msgs;

    assert!(
        msgs.is_empty(),
        "redacted UserTurn should not appear in messages"
    );
}

#[test]
fn replace_substitutes_payload_in_messages() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    let user_event = wire::append(&path, events::user_text("old text".into())).unwrap();
    let new_payload = pb::Payload {
        kind: Some(payload::Kind::UserText(pb::UserText {
            content: "new text".into(),
            ..Default::default()
        })),
    };
    wire::append(&path, replace_body(&user_event.id, new_payload)).unwrap();

    let all = read_events(&path).unwrap();
    let mut muts = active_mutations(&all);
    apply_cascade(&mut muts, &all);
    let msgs = run_projection(&ActiveMessages, &all, &muts).msgs;

    assert_eq!(msgs.len(), 1);
    assert!(
        matches!(&msgs[0], Message::User { content: MessageContent::Text(t), .. } if t == "new text"),
        "replacement payload should be used"
    );
}

#[test]
fn later_redact_overrides_replace() {
    // Replace first, then Redact on same target — Redact has higher seq so it wins.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    let user_event = wire::append(&path, events::user_text("original".into())).unwrap();
    let new_payload = pb::Payload {
        kind: Some(payload::Kind::UserText(pb::UserText {
            content: "replaced".into(),
            ..Default::default()
        })),
    };
    wire::append(&path, replace_body(&user_event.id, new_payload)).unwrap();
    wire::append(&path, redact_body(&user_event.id)).unwrap(); // later → wins

    let all = read_events(&path).unwrap();
    let muts = active_mutations(&all);
    // LWW winner for user_event.id should be Redacted.
    assert!(
        matches!(
            muts.get(&user_event.id),
            Some(MutationState::Redacted { .. })
        ),
        "Redact (higher seq) must win over earlier Replace"
    );
}

#[test]
fn cascade_redact_tool_call_removes_paired_result() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    let tc_event = wire::append(
        &path,
        events::tool_call("cid1".into(), "read".into(), &serde_json::json!({})),
    )
    .unwrap();
    wire::append(
        &path,
        events::tool_result("cid1".into(), "output".into(), None, None),
    )
    .unwrap();
    wire::append(&path, redact_body(&tc_event.id)).unwrap(); // Redact the ToolCall

    let all = read_events(&path).unwrap();
    let mut muts = active_mutations(&all);
    apply_cascade(&mut muts, &all);

    // Both ToolCall and ToolResult should be Redacted.
    assert_eq!(muts.len(), 2, "cascade should produce 2 Redacts");
    assert!(
        muts.values()
            .all(|m| matches!(m, MutationState::Redacted { .. }))
    );

    let msgs = run_projection(&ActiveMessages, &all, &muts).msgs;
    assert!(msgs.is_empty(), "both paired events redacted → no messages");
}

#[test]
fn cascade_redact_overrides_orphan_replace() {
    // Redact ToolCall, Replace ToolResult — cascade Redact should win over the Replace.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    let tc_event = wire::append(
        &path,
        events::tool_call("cid2".into(), "shell".into(), &serde_json::json!({})),
    )
    .unwrap();
    let tr_event = wire::append(
        &path,
        events::tool_result("cid2".into(), "output".into(), None, None),
    )
    .unwrap();
    wire::append(&path, redact_body(&tc_event.id)).unwrap(); // Redact ToolCall
    // Replace ToolResult (same seq-space, but cascade must override).
    let new_tr = pb::Payload {
        kind: Some(payload::Kind::ToolResult(pb::ToolResult {
            call_id: "cid2".into(),
            output: "new output".into(),
            checkpoint: None,
            ..Default::default()
        })),
    };
    wire::append(&path, replace_body(&tr_event.id, new_tr)).unwrap();

    let all = read_events(&path).unwrap();
    let mut muts = active_mutations(&all);
    // Before cascade: ToolCall=Redacted, ToolResult=Replaced.
    apply_cascade(&mut muts, &all);
    // After cascade: ToolResult must become Redacted (cascade overrides Replace).
    assert!(
        matches!(muts.get(&tr_event.id), Some(MutationState::Redacted { .. })),
        "cascade Redact must override orphan Replace on ToolResult"
    );
}

#[test]
fn active_mutations_and_messages_agree_on_a_redacted_event() {
    // Was `fold_mutations_included_in_result`, driven through the dead
    // `fold()` wrapper (deleted). `mutations` was just `active_mutations`'s
    // own converged set; `messages` is the real, live rendering path.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    let e = wire::append(&path, events::user_text("hi".into())).unwrap();
    wire::append(&path, redact_body(&e.id)).unwrap();

    let all = read_events(&path).unwrap();
    let mut mutations = active_mutations(&all);
    apply_cascade(&mut mutations, &all);

    assert!(!mutations.is_empty(), "must populate mutations");
    assert!(
        messages(&all).unwrap().is_empty(),
        "redacted event must not appear in messages"
    );
}
#[test]
fn loadables_projection_full_session_and_mutation_aware() {
    use std::collections::HashMap;
    // A load, then a compaction AFTER it — the projection must NOT reset (full session).
    let cu = pb::CompactionTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let with_comp = mint_events(vec![
        events::toggle("mcp", "slack", true),
        events::summary("summary".into(), cu, Some(1)),
    ]);
    let p = ActiveAgent {
        seed: AgentState::default(),
    };
    let got: AgentState = run_projection(&p, &with_comp, &HashMap::new());
    assert!(
        got.enabled_loadables().contains("slack"),
        "load before compaction must stay enabled (full session)"
    );

    // Now redact that load → projection must drop it (honors mutations).
    let one = mint_events(vec![events::toggle("mcp", "slack", true)]);
    let mut muts = HashMap::new();
    muts.insert(one[0].id.clone(), MutationState::Redacted { reason: None });
    let got2: AgentState = run_projection(&p, &one, &muts);
    assert!(
        !got2.enabled_loadables().contains("slack"),
        "redacted/undone load must be disabled"
    );
}
#[test]
fn last_turn_blocks_returns_last_complete() {
    // Two durable TurnResult events with different blocks; the helper returns
    // the second (last) one's blocks, in order.
    let usage = pb::TurnTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let first = vec!["alpha".to_string(), "beta".to_string()];
    let second = vec![
        "gamma".to_string(),
        "delta".to_string(),
        "epsilon".to_string(),
    ];
    let evs = mint_events(vec![
        durable_turn(usage, first.clone()),
        durable_turn(usage, second.clone()),
    ]);
    let got = last_turn_blocks(&evs);
    assert_eq!(got, second, "last TurnResult wins");
}

#[test]
fn last_turn_blocks_empty_when_none() {
    // No TurnResult events in the log → empty Vec.
    let evs = mint_events(vec![
        events::user_text("hi".into()),
        events::assistant_text("hello".into()),
    ]);
    assert!(last_turn_blocks(&evs).is_empty());
}

#[test]
fn active_agent_folds_toggle_per_kind() {
    use crate::proto::gait::log::v1::ToggleKind;
    use std::collections::HashMap;
    let seed = AgentState {
        model: "m".into(),
        ..AgentState::default()
    };
    let evs = mint_events(vec![
        wire::body(payload::Kind::Toggle(pb::Toggle {
            kind: ToggleKind::Skill as i32,
            name: "tdd".into(),
            enabled: true,
        })),
        wire::body(payload::Kind::Toggle(pb::Toggle {
            kind: ToggleKind::Mcp as i32,
            name: "slack".into(),
            enabled: true,
        })),
        wire::body(payload::Kind::Toggle(pb::Toggle {
            kind: ToggleKind::Skill as i32,
            name: "tdd".into(),
            enabled: false,
        })),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(!st.skills.contains("tdd"), "skill toggled off");
    assert!(st.mcp.contains("slack"), "mcp toggled on in mcp set");
    assert!(
        !st.skills.contains("slack"),
        "mcp toggle did NOT leak into skills"
    );
    assert_eq!(st.model, "m", "inference selection is folded separately");
}

#[test]
fn active_agent_folds_registry_per_kind() {
    use crate::proto::gait::log::v1::ToggleKind;
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Skill as i32,
            name: "tdd".into(),
            listed: true,
        })),
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Mcp as i32,
            name: "slack".into(),
            listed: true,
        })),
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Plugin as i32,
            name: "wasmy".into(),
            listed: true,
        })),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(st.session_skills().contains("tdd"), "skill listed");
    assert!(st.session_mcp().contains("slack"), "mcp listed");
    assert!(st.session_plugins().contains("wasmy"), "plugin listed");
    // Registry never touches the layer-3 live-load sets.
    assert!(st.skills.is_empty());
    assert!(st.mcp.is_empty());
    assert!(st.plugins.is_empty());
}

#[test]
fn active_agent_folds_registry_unlisted_removes() {
    use crate::proto::gait::log::v1::ToggleKind;
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Skill as i32,
            name: "tdd".into(),
            listed: true,
        })),
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Skill as i32,
            name: "tdd".into(),
            listed: false,
        })),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(
        !st.session_skills().contains("tdd"),
        "unlisted removes from session whitelist"
    );
}

#[test]
fn active_agent_registry_guards_tool_and_unspecified() {
    use crate::proto::gait::log::v1::ToggleKind;
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Tool as i32,
            name: "bogus-tool".into(),
            listed: true,
        })),
        wire::body(payload::Kind::Registry(pb::Registry {
            kind: ToggleKind::Unspecified as i32,
            name: "bogus-unspecified".into(),
            listed: true,
        })),
    ]);
    // Must not panic, and must not mutate any set.
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(st.session_skills().is_empty());
    assert!(st.session_mcp().is_empty());
    assert!(st.session_plugins().is_empty());
}

#[test]
fn message_accept_folds_as_or_set() {
    use std::collections::HashMap;
    let seed = AgentState::default();
    let evs = mint_events(vec![
        wire::body(payload::Kind::MessageAccept(pb::MessageAccept {
            sender: b"sender_a".to_vec(),
            granted: true,
        })),
        wire::body(payload::Kind::MessageAccept(pb::MessageAccept {
            sender: b"sender_b".to_vec(),
            granted: true,
        })),
        wire::body(payload::Kind::MessageAccept(pb::MessageAccept {
            sender: b"sender_a".to_vec(),
            granted: false, // revoke
        })),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(!st.accepted_senders.contains(&b"sender_a".to_vec()[..]));
    assert!(st.accepted_senders.contains(&b"sender_b".to_vec()[..]));
}

#[test]
fn accepts_sender_allows_spawn_relations_without_explicit_grant() {
    let empty_accepted: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    assert!(
        super::accepts_sender(&empty_accepted, b"agent:parent_id", true),
        "spawn-hierarchy relation must be accepted by default"
    );
    assert!(
        !super::accepts_sender(&empty_accepted, b"agent:stranger", false),
        "non-relation with no explicit grant must be rejected"
    );
}

#[test]
fn accepts_sender_honors_explicit_grant_for_non_relations() {
    let mut accepted = std::collections::HashSet::new();
    accepted.insert(b"agent:stranger".to_vec());
    assert!(
        super::accepts_sender(&accepted, b"agent:stranger", false),
        "explicit MessageAccept grant must override the default reject for non-relations"
    );
}

#[test]
fn rebaseline_agent_overlays_skills_and_mcp_from_state() {
    use std::collections::HashSet;
    let live = pb::Agent {
        inference: Some(pb::Inference {
            name: "m1".into(),
            model: "m1".into(),
            llm_params: None,
            compaction_threshold_tokens: 128_000,
        }),
        instructions: "sys".into(),
        tools: vec!["read".into()],
        mcp_servers: vec!["old-mcp".into()],
        skills: vec!["old-skill".into()],
        plugins: vec![],
        permissions: Some(pb::PermissionPolicy {
            allow: vec![],
            deny: vec![],
        }),
        through_seq: 0,
    };
    let st = AgentState {
        model: "m2".into(),
        instructions: String::new(),
        tools: HashSet::new(),
        skills: ["new-skill".to_string()].into(),
        mcp: ["new-mcp".to_string()].into(),
        plugins: HashSet::new(),
        allow: HashSet::new(),
        deny: HashSet::new(),
        inference: None,
        session_skills: HashSet::new(),
        session_mcp: HashSet::new(),
        session_plugins: HashSet::new(),
        accepted_senders: HashSet::new(),
    };
    let got = rebaseline_agent(live.clone(), &st);
    assert_eq!(
        got.inference.unwrap().model,
        "m1",
        "inference is carried by the checkpoint"
    );
    assert_eq!(got.instructions, "sys", "instructions are static");
    assert_eq!(got.tools, vec!["read".to_string()], "tools are static");
    assert_eq!(
        got.skills,
        vec!["new-skill".to_string()],
        "skills overlaid from state"
    );
    assert_eq!(
        got.mcp_servers,
        vec!["new-mcp".to_string()],
        "mcp overlaid from state"
    );
    let perms = got.permissions.as_ref().expect("permissions preserved");
    assert!(perms.allow.is_empty());
}

#[test]
fn plugin_toggle_folds_into_agent_state() {
    let events = vec![
        events::toggle("plugin", "myfmt", true),
        events::toggle("plugin", "other", true),
        events::toggle("plugin", "myfmt", false),
    ];
    let evs = mint_events(events);
    let state = active_agent(&evs, AgentState::default());
    assert!(!state.plugins.contains("myfmt"), "LWW: myfmt toggled off");
    assert!(state.plugins.contains("other"), "other toggled on");
}

#[test]
fn plugin_enabled_appears_in_enabled_loadables() {
    let events = vec![events::toggle("plugin", "myfmt", true)];
    let evs = mint_events(events);
    let state = active_agent(&evs, AgentState::default());
    let enabled = state.enabled_loadables();
    assert!(enabled.contains("myfmt"), "plugin in enabled_loadables");
}

#[test]
fn total_usage_sums_turn_result_events() {
    let usage1 = pb::TurnTokenUsage {
        input: 100,
        output: 20,
        cache_read: 5,
        cache_write: 2,
    };
    let usage2 = pb::TurnTokenUsage {
        input: 50,
        output: 10,
        cache_read: 3,
        cache_write: 1,
    };
    let evs = mint_events(vec![
        durable_turn(usage1, vec![]),
        durable_turn(usage2, vec![]),
    ]);
    let total = total_usage(&evs);
    assert_eq!(total.input, 150, "input should sum to 150");
    assert_eq!(total.output, 30, "output should sum to 30");
    assert_eq!(total.cache_read, 8, "cache_read should sum to 8");
    assert_eq!(total.cache_write, 3, "cache_write should sum to 3");
}

#[test]

fn fold_replaces_tool_result_in_messages() {
    // Replace-only path: a Replace targeting a ToolResult must flow through

    // effective_kind() → ActiveMessages::apply and the messages output must

    // reflect the replacement (not the original payload).

    let dir = TempDir::new().unwrap();

    let path = dir.path().join("tx.jsonl");

    // One UserTurn, one ToolCall, one ToolResult.

    wire::append(&path, events::user_text("do it".into())).unwrap();

    wire::append(
        &path,
        events::tool_call("cid-r".into(), "read".into(), &serde_json::json!({})),
    )
    .unwrap();

    let tr_event = wire::append(
        &path,
        events::tool_result("cid-r".into(), "original output".into(), None, None),
    )
    .unwrap();

    // Replace the ToolResult's payload — output becomes "replaced output".

    let new_payload = pb::Payload {
        kind: Some(payload::Kind::ToolResult(pb::ToolResult {
            call_id: "cid-r".into(),

            output: "replaced output".into(),

            checkpoint: None,
            ..Default::default()
        })),
    };

    wire::append(&path, replace_body(&tr_event.id, new_payload)).unwrap();

    let all = read_events(&path).unwrap();

    let msgs = messages(&all).unwrap();

    // Exactly one ToolResult message should appear, and its content must be

    // the replacement string — not the original.

    let tool_results: Vec<_> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => Some(content.as_slice()),

            _ => None,
        })
        .flat_map(|s| s.iter())
        .collect();

    assert_eq!(
        tool_results.len(),
        1,
        "expected exactly one ToolResult, got messages: {:?}",
        msgs
    );

    let rendered = match &tool_results[0].result {
        serde_json::Value::String(s) => s.clone(),

        other => serde_json::to_string(other).unwrap_or_default(),
    };

    assert_eq!(
        rendered, "replaced output",
        "Replace on ToolResult must flow through effective_kind → ActiveMessages::apply"
    );
}

#[test]

fn cascade_replace_tool_result_then_redact_call_hides_both() {
    // Cascade + Replace interaction end-to-end through fold():

    // a Replace targeting a ToolResult, followed by a Redact targeting the

    // paired ToolCall, must cascade-Redact the ToolResult — the Replace is

    // promoted to Redact, so no ToolResult entry survives in messages.

    let dir = TempDir::new().unwrap();

    let path = dir.path().join("tx.jsonl");

    wire::append(&path, events::user_text("do it".into())).unwrap();

    let tc_event = wire::append(
        &path,
        events::tool_call("cid-c".into(), "shell".into(), &serde_json::json!({})),
    )
    .unwrap();

    let tr_event = wire::append(
        &path,
        events::tool_result("cid-c".into(), "some output".into(), None, None),
    )
    .unwrap();

    // Replace the ToolResult first.

    let new_payload = pb::Payload {
        kind: Some(payload::Kind::ToolResult(pb::ToolResult {
            call_id: "cid-c".into(),

            output: "replaced output".into(),

            checkpoint: None,
            ..Default::default()
        })),
    };

    wire::append(&path, replace_body(&tr_event.id, new_payload)).unwrap();

    // Then redact the paired ToolCall — cascade must promote the Replace to

    // Redact on the ToolResult as well.

    wire::append(&path, redact_body(&tc_event.id)).unwrap();

    let all = read_events(&path).unwrap();

    let msgs = messages(&all).unwrap();

    // No ToolResult entry should remain in messages — cascade promoted the

    // Replace to Redact, so the result is dropped. (The ToolCall is also

    // redacted directly, so even without coalescing nothing would render.)

    let has_tool = msgs.iter().any(|m| matches!(m, Message::ToolResult { .. }));

    assert!(
        !has_tool,
        "cascade must hide the ToolResult too, got: {:?}",
        msgs
    );
}

// --- Task 4: ActiveRawMessages + coalesce + transcript + outgoing_for_step ---

#[test]
fn raw_messages_carry_event_ids() {
    use std::collections::HashMap;
    let evs = vec![
        scoped_mint(events::user_text("hello".into()), None, 0, 0),
        scoped_mint(events::assistant_text("hi".into()), None, 0, 0),
    ];
    let raw = run_projection(&ActiveRawMessages::default(), &evs, &HashMap::new());
    assert_eq!(raw.base.len(), 2);
    assert_eq!(raw.base[0].id(), evs[0].id.as_str());
    assert_eq!(raw.base[1].id(), evs[1].id.as_str());
}

/// Agent identity fixture, minted at `now` so that events stamped at `now`
/// fall inside its device key window. Keep the fixture alive while using `ctx`.
fn agent_fixture(now: i64) -> (TempAgentDir, gait_identity::IdentityContext) {
    let agent = TempAgentDir::new(now, 3_600);
    let ctx = gait_identity::IdentityContext::new(agent.path()).unwrap();
    (agent, ctx)
}

fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Attach a real direct signature to `ev`, made for `audience` at `now`.
/// The event timestamp is pinned to `now`, so verification places the event
/// inside the device window. The content hash covers the body only, so this
/// does not disturb it.
fn direct_signed(ctx: &gait_identity::IdentityContext, ev: &mut Event, audience: &str, now: i64) {
    ev.ts = (now as u64) * 1_000;
    let provenance = gait_identity::sign_direct_event(ctx, ev, audience, 60, &|| now).unwrap();
    ev.provenance = vec![provenance];
}

/// A log opened by `SessionStart`, which names the session that signatures
/// must be made for.
fn session_log(session_id: &str) -> Vec<Event> {
    vec![scoped_mint(
        events::session_start(session_id.into(), None, None, false),
        None,
        0,
        0,
    )]
}

/// A raw projection that verifies signatures with `ctx`'s agent directory.
fn verifying_projection(ctx: &gait_identity::IdentityContext) -> ActiveRawMessages {
    ActiveRawMessages {
        verifier: Some(Arc::new(
            gait_identity::DirectVerifier::from_context(ctx).unwrap(),
        )),
        ..ActiveRawMessages::default()
    }
}

#[test]
fn signed_event_from_another_session_log_is_unverified() {
    let now = wall_clock();
    let (_agent, ctx) = agent_fixture(now);

    let mut msg = scoped_mint(events::incoming_message("from elsewhere".into()), None, 0, 0);
    direct_signed(&ctx, &mut msg, "other-session", now);

    let mut in_other_log = session_log("other-session");
    in_other_log.push(msg.clone());
    let raw = run_projection(&verifying_projection(&ctx), &in_other_log, &HashMap::new());
    let Message::User { attribution, .. } = &raw.base[0] else {
        panic!("expected a folded user message");
    };
    assert_eq!(
        attribution[0].trust,
        Trust::Verified,
        "control: the same signature verifies in the session it was made for"
    );

    let mut in_room_log = session_log("room-session");
    in_room_log.push(msg);
    let raw = run_projection(&verifying_projection(&ctx), &in_room_log, &HashMap::new());
    let Message::User { attribution, .. } = &raw.base[0] else {
        panic!("a signed message must still be folded, not dropped");
    };
    assert_eq!(attribution[0].trust, Trust::Unverified);
    assert_eq!(attribution[0].author_id, "unverified");
}

#[test]
fn active_raw_messages_omits_rejected_incoming_message_claim() {
    use std::collections::HashMap;
    let body = crate::proto::wire::body(crate::proto::gait::log::v1::payload::Kind::IncomingMessage(
        pb::IncomingMessage {
            content: "do not show this".into(),
            rejected_provenance_claim: true,
        },
    ));
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");
    wire::append(&path, body).unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1, "rejected input remains in the durable log");
    let raw = run_projection(&ActiveRawMessages::default(), &events, &HashMap::new());
    assert!(
        raw.base.is_empty(),
        "malformed claimed provenance must not reach the agent"
    );
}

#[test]
fn active_raw_messages_omits_incoming_message_without_provenance() {
    use std::collections::HashMap;
    let body = events::incoming_message("missing attribution must not reach the agent".into());
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");
    wire::append(&path, body).unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1, "malformed input remains in the durable log");
    let raw = run_projection(&ActiveRawMessages::default(), &events, &HashMap::new());
    assert!(
        raw.base.is_empty(),
        "IncomingMessage requires acceptable provenance to reach the agent"
    );
}

#[test]
fn valid_direct_provenance_keeps_uniform_attribution() {
    let now = wall_clock();
    let (_agent, ctx) = agent_fixture(now);
    let mut msg = scoped_mint(events::incoming_message("hello".into()), None, 0, 0);
    direct_signed(&ctx, &mut msg, "room-session", now);
    msg.provenance[0].context = "room-general".into();

    let mut evs = session_log("room-session");
    evs.push(msg);
    let raw = run_projection(&verifying_projection(&ctx), &evs, &HashMap::new());
    let Message::User { attribution, .. } = &raw.base[0] else {
        panic!("expected accepted user message");
    };
    assert_eq!(attribution.len(), 1);
    assert_eq!(attribution[0].trust, Trust::Verified);
    assert_eq!(attribution[0].author_id, ctx.agent_id().to_string());
    assert_eq!(attribution[0].context, "room-general");
}

#[test]
fn signed_event_without_verifier_is_unverified_not_dropped() {
    let now = wall_clock();
    let (_agent, ctx) = agent_fixture(now);
    let mut msg = scoped_mint(events::incoming_message("signed, no verifier".into()), None, 0, 0);
    direct_signed(&ctx, &mut msg, "room-session", now);

    let mut evs = session_log("room-session");
    evs.push(msg);
    let raw = run_projection(&ActiveRawMessages::default(), &evs, &HashMap::new());
    assert_eq!(raw.base.len(), 1, "a signed message with no verifier is still folded");
    let Message::User {
        content: MessageContent::Text(text),
        attribution,
        ..
    } = &raw.base[0]
    else {
        panic!("expected a user message");
    };
    assert_eq!(text, "signed, no verifier", "the folded message carries its own text");
    assert_eq!(attribution[0].trust, Trust::Unverified);
    assert_eq!(attribution[0].author_id, "unverified");
}

#[test]
fn forged_verified_author_id_with_signed_message_is_not_trusted() {
    let now = wall_clock();
    let (_agent, ctx) = agent_fixture(now);
    let forged = "agent-forged";

    // Signature made for a different session, plus a claimed author id.
    let mut bad = scoped_mint(events::incoming_message("forged bad".into()), None, 0, 0);
    direct_signed(&ctx, &mut bad, "other-session", now);
    bad.provenance[0].verified_author_id = Some(forged.into());

    // Valid signature for this session, with the same forged claimed author id.
    let mut good = scoped_mint(events::incoming_message("forged good".into()), None, 0, 0);
    direct_signed(&ctx, &mut good, "room-session", now);
    good.provenance[0].verified_author_id = Some(forged.into());

    let mut evs = session_log("room-session");
    evs.push(bad);
    evs.push(good);
    let raw = run_projection(&verifying_projection(&ctx), &evs, &HashMap::new());
    assert_eq!(raw.base.len(), 2);

    let Message::User { attribution: bad_attr, .. } = &raw.base[0] else {
        panic!("expected the forged-but-unsigned-for-this-session message to fold");
    };
    assert_eq!(bad_attr[0].trust, Trust::Unverified);
    assert_eq!(bad_attr[0].author_id, "unverified", "the stored id must not be trusted");

    let Message::User { attribution: good_attr, .. } = &raw.base[1] else {
        panic!("expected the validly signed message to fold");
    };
    assert_eq!(good_attr[0].trust, Trust::Verified);
    assert_eq!(
        good_attr[0].author_id,
        ctx.agent_id().to_string(),
        "a verified signature yields the principal id, never the stored id"
    );
    assert_ne!(good_attr[0].author_id, forged);
}

#[test]
fn coalesce_merges_consecutive_tool_calls() {
    use std::collections::HashMap;
    let evs = vec![
        scoped_mint(
            events::tool_call("c1".into(), "read".into(), &serde_json::json!({})),
            None,
            0,
            0,
        ),
        scoped_mint(
            events::tool_call("c2".into(), "write".into(), &serde_json::json!({})),
            None,
            0,
            0,
        ),
        scoped_mint(
            events::tool_result("c1".into(), "r1".into(), None, None),
            None,
            0,
            0,
        ),
        scoped_mint(
            events::tool_result("c2".into(), "r2".into(), None, None),
            None,
            0,
            0,
        ),
    ];
    let raw = run_projection(&ActiveRawMessages::default(), &evs, &HashMap::new());
    let coalesced = coalesce(raw.base);
    assert_eq!(coalesced.len(), 2);
    if let Message::Assistant {
        content: AssistantContent::Parts(parts),
        ..
    } = &coalesced[0]
    {
        assert_eq!(
            parts.len(),
            2,
            "two tool calls merged into one assistant message"
        );
    } else {
        panic!("expected merged assistant message");
    }
    if let Message::ToolResult { content, .. } = &coalesced[1] {
        assert_eq!(content.len(), 2, "two tool results merged");
    } else {
        panic!("expected merged ToolResult message");
    }
}

#[test]
fn inject_appears_in_base() {
    // Design A: Inject goes to base, not a per-turn HashMap
    use std::collections::HashMap;
    let ev = scoped_mint(events::inject("user", "injected".into()), None, 1, 0);
    let raw = run_projection(
        &ActiveRawMessages::default(),
        std::slice::from_ref(&ev),
        &HashMap::new(),
    );
    assert_eq!(raw.base.len(), 1, "Inject must go into base (Design A)");
    assert_eq!(raw.base[0].id(), ev.id.as_str());
}

#[test]
fn outgoing_for_step_includes_injected_messages() {
    let user_ev = scoped_mint(events::user_text("hello".into()), None, 0, 0);
    let inject_ev = scoped_mint(
        events::inject("user", "context from RAG".into()),
        None,
        0,
        0,
    );
    let evs = vec![user_ev, inject_ev];
    let out = outgoing_for_step(&evs, 0).unwrap();
    assert_eq!(out.len(), 2);
    let texts: Vec<&str> = out
        .iter()
        .filter_map(|m| match m {
            Message::User {
                content: MessageContent::Text(t),
                ..
            } => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"hello"));
    assert!(texts.contains(&"context from RAG"));
}

#[test]
fn outgoing_for_step_appends_newly_folded_message_in_log_order() {
    let evs = vec![
        scoped_mint(events::user_text("first".into()), None, 0, 0),
        scoped_mint(events::assistant_text("answer".into()), None, 0, 0),
        scoped_mint(events::inject("user", "new context".into()), None, 0, 1),
    ];
    let out = outgoing_for_step(&evs, 0).unwrap();
    let texts: Vec<&str> = out
        .iter()
        .filter_map(|m| match m {
            Message::User {
                content: MessageContent::Text(text),
                ..
            }
            | Message::Assistant {
                content: AssistantContent::Text(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first", "answer", "new context"]);
}

#[test]
fn active_unlock_folds_once_grants_and_clears_on_run_complete() {
    use std::path::PathBuf;

    let unlock_item = pb::SandboxUnlockItem {
        id: "u1".to_string(),
        tool_name: "write".to_string(),
        resource: "/external/path/foo.txt".to_string(),
        dimension: "write".to_string(),
        preview: None,
    };
    let reply_item = pb::SandboxUnlockReplyItem {
        id: "u1".to_string(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Once as i32,
    };

    let evs = mint_events(vec![
        events::sandbox_unlock(&[unlock_item]),
        events::sandbox_unlock_reply(&[reply_item]),
    ]);

    let result = active_unlock_state(&evs);
    assert!(
        result
            .once
            .contains(&PathBuf::from("/external/path/foo.txt")),
        "once grant should be in active_unlock.once"
    );
    assert!(result.session.is_empty());
}

#[test]
fn active_unlock_session_grant_survives_run_complete() {
    use std::path::PathBuf;

    let unlock_item = pb::SandboxUnlockItem {
        id: "u2".to_string(),
        tool_name: "write".to_string(),
        resource: "/external/dir".to_string(),
        dimension: "write".to_string(),
        preview: None,
    };
    let reply_item = pb::SandboxUnlockReplyItem {
        id: "u2".to_string(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Session as i32,
    };
    let evs = mint_events(vec![
        events::sandbox_unlock(&[unlock_item]),
        events::sandbox_unlock_reply(&[reply_item]),
        events::run_complete(pb::RunComplete::default()),
    ]);

    let result = active_unlock_state(&evs);
    // session grant survives RunComplete
    assert!(result.session.contains(&PathBuf::from("/external/dir")));
    // once was cleared by RunComplete
    assert!(result.once.is_empty());
}

#[test]
fn daemon_update_renders_as_user_message() {
    // The ActiveMessages projection folds a DaemonUpdate event into a user
    // message formatted as "[daemon:<name>] <status> — exit <code> — log: <path>".
    let bodies = vec![events::daemon_update(
        "build".into(),
        0,
        "done",
        "/tmp/build.log".into(),
        "build@1".into(),
    )];
    let msgs = messages(&mint_events(bodies)).unwrap();
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        Message::User {
            content: MessageContent::Text(t),
            ..
        } => {
            assert!(t.contains("[daemon:build]"), "got: {t}");
            assert!(t.contains("exit 0"), "got: {t}");
            assert!(t.contains("/tmp/build.log"), "got: {t}");
        }
        other => panic!("expected User message, got {other:?}"),
    }
}

#[test]
fn daemon_update_raw_messages_includes_user_message() {
    // The ActiveRawMessages projection (raw transcript, no coalescing) must
    // also render DaemonUpdate as a User message so the transcript survives
    // mutation flows that route through the raw projection.
    use std::collections::HashMap;
    let evs = mint_events(vec![events::daemon_update(
        "svc-x".into(),
        124,
        "killed",
        "/var/log/svc.log".into(),
        "svc-x@1".into(),
    )]);
    let raw = run_projection(&ActiveRawMessages::default(), &evs, &HashMap::new());
    assert_eq!(raw.base.len(), 1);
    match &raw.base[0] {
        Message::User {
            content: MessageContent::Text(t),
            ..
        } => {
            assert!(t.contains("[daemon:svc-x]"), "got: {t}");
            assert!(t.contains("killed"), "got: {t}");
            assert!(t.contains("exit 124"), "got: {t}");
        }
        other => panic!("expected User message, got {other:?}"),
    }
}

#[test]
fn full_tool_history_survives_a_resumed_turn() {
    // Regression guard for a real incident: a session ran a 73-step turn
    // (many tool_call/tool_result pairs), paused via structured_done, and was
    // resumed with a fresh `gait run --session <id> --task <reply>` process —
    // a brand-new process attaching to the existing log, not a continuation of
    // any in-memory state. The agent reported "I lost the tool history you
    // sent me" after resuming. Investigation found no windowing/truncation
    // anywhere in the fold path — `transcript()` always folds the ENTIRE
    // on-disk log — so this pins that: a large first turn's tool calls/results
    // must all still be present in the transcript built for the resumed turn,
    // across the RunComplete + new UserText boundary a fresh CLI dispatch
    // produces.
    const STEPS: usize = 73;
    let dir = TempDir::new().unwrap();
    let mut bodies = vec![events::user_text("Implement Task 2 ONLY: ...".into())];
    for i in 0..STEPS {
        bodies.push(events::tool_call(
            format!("call{i}"),
            "shell".into(),
            &serde_json::json!({"cmd": format!("step {i}")}),
        ));
        bodies.push(events::tool_result(
            format!("call{i}"),
            format!("output {i}"),
            None,
            None,
        ));
    }
    bodies.push(durable_turn(
        pb::TurnTokenUsage {
            input: 790_939,
            output: 11_067,
            cache_read: 7_463_100,
            cache_write: 0,
        },
        vec![],
    ));
    // The resumed dispatch: a brand-new process attaches to the same session
    // and appends the user's reply as plain UserText (this is the
    // `--task <reply>` "literal one-shot node" path, not a PermissionsReply
    // resume — see `turn::resume::apply_if_pending`'s `did_resume = false`
    // branch, which is what actually runs here).
    bodies.push(events::user_text(
        "Answer to your question: proceed with Option 1/C ...".into(),
    ));

    let path = write_events(dir.path(), bodies);
    let msgs = messages_from_path(&path).unwrap();

    let tool_result_count = msgs
        .iter()
        .filter(|m| matches!(m, Message::ToolResult { .. }))
        .count();
    assert_eq!(
        tool_result_count, STEPS,
        "all {STEPS} tool results from the first (pre-resume) turn must survive into \
         the transcript folded for the resumed turn; got {tool_result_count}"
    );

    // Spot-check the very first and very last tool calls from turn 0 are
    // still addressable (not just present in aggregate).
    let has_call = |id: &str| {
        msgs.iter().any(|m| {
            matches!(m, Message::Assistant { content: AssistantContent::Parts(parts), .. }
                if parts.iter().any(|p| matches!(p, AssistantPart::ToolCall(tc) if tc.tool_call_id == id)))
        })
    };
    assert!(
        has_call("call0"),
        "first turn-0 tool call must survive resume"
    );
    assert!(
        has_call(&format!("call{}", STEPS - 1)),
        "last turn-0 tool call must survive resume"
    );

    // The resumed turn's reply must appear too — this is not just "turn 0
    // untouched", the fold must include everything.
    assert!(msgs.iter().any(|m| matches!(m, Message::User {
        content: MessageContent::Text(t), ..
    } if t.starts_with("Answer to your question"))));
}

fn agent_update_body(session_id: &str, summary: &str) -> event::Body {
    crate::proto::gait::log::v1::event::Body::Payload(crate::proto::gait::log::v1::Payload {
        kind: Some(crate::proto::gait::log::v1::payload::Kind::AgentUpdate(
            crate::proto::gait::log::v1::AgentUpdate {
                session_id: session_id.into(),
                status: crate::proto::gait::log::v1::AgentStatus::Done as i32,
                summary: summary.into(),
                items: None,
                idempotency_key: "s1234abcd-evt1".into(),
            },
        )),
    })
}

fn agent_update_text(raw: &RawMessagesState, index: usize) -> String {
    match &raw.base[index] {
        Message::User {
            content: MessageContent::Text(t),
            ..
        } => t.clone(),
        other => panic!("expected a User message, got {other:?}"),
    }
}

#[test]
fn active_raw_messages_agent_update_uses_provenance_not_session_id() {
    let now = wall_clock();
    let (_agent, ctx) = agent_fixture(now);
    let mut ev = scoped_mint(agent_update_body("s_old_field_ignored", "tests pass"), None, 0, 0);
    direct_signed(&ctx, &mut ev, "room-session", now);

    let mut evs = session_log("room-session");
    evs.push(ev);
    let raw = run_projection(&verifying_projection(&ctx), &evs, &HashMap::new());

    let t = agent_update_text(&raw, 0);
    let principal = ctx.agent_id().to_string();
    assert!(
        t.contains(&format!("[agent:{principal}]")) && !t.contains("s_old_field_ignored"),
        "rendered text must use the verified principal id, got: {t}"
    );
}

#[test]
fn daemon_authored_attested_message_renders_verified_with_its_principal_id() {
    // A daemon-side agent update: signed for the parent session, and also
    // carrying the principal id in verified_author_id, as ingress does.
    // Both are present, so the signature decides and the stored id is ignored.
    let now = wall_clock();
    let (_agent, ctx) = agent_fixture(now);
    let principal = ctx.agent_id().to_string();
    let mut ev = scoped_mint(agent_update_body("daemon-claimed", "build finished"), None, 0, 0);
    direct_signed(&ctx, &mut ev, "room-session", now);
    ev.provenance[0].verified_author_id = Some(principal.clone());

    let mut evs = session_log("room-session");
    evs.push(ev);
    let raw = run_projection(&verifying_projection(&ctx), &evs, &HashMap::new());

    let t = agent_update_text(&raw, 0);
    assert!(
        t.starts_with(&format!("[agent:{principal}] status: done")),
        "a verified daemon-side update renders its principal id, got: {t}"
    );
    assert!(!t.contains("daemon-claimed"), "the session field must not leak, got: {t}");
}

#[test]
fn a_resumed_session_folds_a_verified_incoming_message_with_attribution() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");
    let verified_provenance = pb::Provenance {
        verified_author_id: Some("agent-2".into()),
        context: "room-general".into(),
        ..Default::default()
    };
    wire::append_with_given_provenances(
        &path,
        events::incoming_message("hello from the room".into()),
        vec![verified_provenance],
        crate::proto::envelope::EventScope::Session,
    )
    .unwrap();

    // Simulates a session resume: fresh read from disk, fresh fold -- not
    // reusing any in-memory state from whatever wrote the event.
    let all_events = read_events(&path).unwrap();
    let msgs = messages(&all_events).unwrap();

    let crate::types::Message::User { attribution, .. } = &msgs[0] else {
        panic!("expected a User message");
    };
    assert_eq!(attribution.len(), 1);
    assert_eq!(
        attribution[0].trust,
        crate::types::Trust::Verified,
        "an ingress-verified message must fold as Verified on resume, not Unverified"
    );
    assert_eq!(attribution[0].author_id, "agent-2");
    assert_eq!(attribution[0].context, "room-general");
}

#[test]
fn a_resumed_session_folds_an_attested_system_message_as_verified() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");
    let verified_provenance = pb::Provenance {
        verified_author_id: Some("system".into()),
        context: "room-general".into(),
        ..Default::default()
    };
    wire::append_with_given_provenances(
        &path,
        events::incoming_message("agent joined the room".into()),
        vec![verified_provenance],
        crate::proto::envelope::EventScope::Session,
    )
    .unwrap();

    let all_events = read_events(&path).unwrap();
    let msgs = messages(&all_events).unwrap();

    let crate::types::Message::User { attribution, .. } = &msgs[0] else {
        panic!("expected a User message");
    };
    assert_eq!(
        attribution[0].trust,
        crate::types::Trust::Verified,
        "an ingress-attested author id folds as Verified; there is no separate system trust level"
    );
    assert_eq!(attribution[0].author_id, "system");
}
