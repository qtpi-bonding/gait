use super::*;
use crate::proto::{events, wire};
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
        .map(|b| crate::proto::envelope::mint(b, None, 0, 0))
        .collect()
}

#[test]
fn empty_transcript_returns_empty() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    let msgs = messages_from_path(&path).unwrap();
    assert!(msgs.is_empty());
}

#[test]
fn user_and_assistant_text_reconstructed() {
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            events::session_start("s1".into(), None, None, false),
            events::user_text("hello".into(), None),
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
        events::user_text("do x".into(), None),
        events::permissions(&[pb::PermissionItem {
            tool_args: crate::proto::events::to_struct(&serde_json::Value::Null),
            ..Default::default()
        }]),
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "tc1".into(),
                decision: pb::Decision::Allow as i32,
                text: None,
                scope: pb::Scope::Unspecified as i32,
            }],
        ),
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
    // baseline. `transcript()` / `messages_from_path()` use `ActiveRawMessages`
    // (no Summary checkpoint — see Task 4), so we drive `ActiveMessages`
    // directly to assert the projection's own semantics.
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
    };
    let path = write_events(
        dir.path(),
        vec![
            events::user_text("old".into(), None),
            events::assistant_text("old response".into()),
            events::turn_complete(events::TurnCompleteParams {
                usage,
                message: "s".into(),
                ..Default::default()
            }),
            events::summary("summary of old stuff".into(), 1, cu),
            events::user_text("new".into(), None),
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
        events::user_text("hi".into(), None),
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
            model: "model-a".into(),
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
            llm_params: None,
        })),
    )
    .unwrap();
    wire::append(
        &path,
        wire::body(pb::payload::Kind::Agent(pb::Agent {
            model: "model-b".into(),
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
            llm_params: None,
        })),
    )
    .unwrap();

    let all_events = read_events(&path).unwrap();
    let r = active_agent_checkpoint(&all_events).expect("an agent");
    assert_eq!(r.model, "model-b");
}

#[test]
fn active_agent_checkpoint_returns_none_for_no_agent() {
    let bodies = vec![
        events::user_text("hi".into(), None),
        events::assistant_text("hello".into()),
    ];
    let proto_events = mint_events(bodies);
    assert!(active_agent_checkpoint(&proto_events).is_none());
}

#[test]
fn active_cwd_defaults_to_root() {
    let root = std::path::Path::new("/repo/root");
    let evs = mint_events(vec![events::assistant_text("no cwd here".into())]);
    assert_eq!(active_cwd(&evs, root), root.to_path_buf());
}

#[test]
fn active_cwd_last_write_wins() {
    let root = std::path::Path::new("/repo/root");
    let evs = mint_events(vec![events::cwd("/a"), events::cwd("/b")]);
    assert_eq!(active_cwd(&evs, root), std::path::PathBuf::from("/b"));
}

// ── AgentState + conversions (T1 + T2) ──────────────────────────────────────

#[test]
fn agent_state_from_pb_agent_maps_enabled_sets() {
    let a = pb::Agent {
        model: "x/y".into(),
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
        llm_params: None,
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

#[test]
fn agent_state_from_config_seeds_enabled_from_config() {
    use crate::config::{GaitConfig, McpServerDef, SkillDef};
    let cfg = GaitConfig {
        model: "anthropic/x".into(),
        mcp_servers: vec![
            McpServerDef {
                name: "slack".into(),
                command: None,
                args: None,
                env: None,
                url: None,
                spawn: None,
                connect: None,
                enabled: true,
                description: None,
            },
            McpServerDef {
                name: "off".into(),
                command: None,
                args: None,
                env: None,
                url: None,
                spawn: None,
                connect: None,
                enabled: false,
                description: None,
            },
        ],
        skills: vec![SkillDef {
            name: "tdd".into(),
            description: "d".into(),
            content: "c".into(),
            enabled: true,
        }],
        ..GaitConfig::default()
    };
    let st = AgentState::from_config(
        &cfg,
        "anthropic/x".into(),
        "sys".into(),
        vec!["read".into()],
        vec!["shell:rm *".into()],
    );
    assert!(st.mcp.contains("slack"), "enabled mcp seeded");
    assert!(!st.mcp.contains("off"), "disabled mcp NOT seeded");
    assert!(st.skills.contains("tdd"), "enabled skill seeded");
    assert_eq!(st.model, "anthropic/x");
    assert!(st.allow.contains("read") && st.deny.contains("shell:rm *"));
}

// ── ActiveAgent projection (T3 + T4) ────────────────────────────────────────

#[test]
fn active_agent_folds_load_toolcall_like_loadables() {
    use std::collections::HashMap;
    let seed = AgentState::from_config(
        &crate::config::GaitConfig::default(),
        "m".into(),
        "sys".into(),
        vec![],
        vec![],
    );
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
    let seed = AgentState::from_config(
        &crate::config::GaitConfig::default(),
        "m".into(),
        "sys".into(),
        vec![],
        vec![],
    );
    let evs = mint_events(vec![
        events::toggle("skill", "old", true),
        wire::body(payload::Kind::Agent(pb::Agent {
            model: "m2".into(),
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
            llm_params: None,
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
    let seed = AgentState::from_config(
        &crate::config::GaitConfig::default(),
        "m".into(),
        "sys".into(),
        vec![],
        vec![],
    );
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
            model: "m1".into(),
            ..Default::default()
        })),
        wire::body(payload::Kind::Agent(pb::Agent {
            model: "m2".into(),
            ..Default::default()
        })),
    ]);
    let got = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert_eq!(got.model, "m2", "latest Agent checkpoint wins");
}

#[test]
fn fold_returns_all_three_projections() {
    let dir = TempDir::new().unwrap();
    let path = write_events(
        dir.path(),
        vec![
            wire::body(pb::payload::Kind::Agent(pb::Agent {
                model: "gpt-fold".into(),
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
                llm_params: None,
            })),
            events::user_text("hi".into(), None),
            events::toggle("skill", "tdd", true),
        ],
    );
    let all_events = read_events(&path).unwrap();
    let seed = AgentState::from_config(
        &crate::config::GaitConfig::default(),
        "m".into(),
        "".into(),
        vec![],
        vec![],
    );
    let result = fold(&all_events, seed).unwrap();

    // messages: user turn (no summary-only events, user_turn becomes a Message)
    assert!(!result.messages.is_empty());
    // agent.model: the checkpoint we wrote
    assert_eq!(result.agent.model, "gpt-fold");
    // enabled_loadables: tdd was toggled on
    assert!(result.agent.enabled_loadables().contains("tdd"));
}

#[test]
fn fold_runs_all_projections_single_pass() {
    let evs = mint_events(vec![events::toggle("mcp", "slack", true)]);
    let seed = AgentState::from_config(
        &crate::config::GaitConfig::default(),
        "m".into(),
        "".into(),
        vec![],
        vec![],
    );
    let res = fold(&evs, seed).unwrap();
    assert!(res.agent.enabled_loadables().contains("slack"));
    assert!(
        res.effective_allow.is_empty(),
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
    };
    let evs = mint_events(vec![
        events::user_text("before".into(), None),
        events::summary("the summary".into(), 1, cu),
        events::user_text("after".into(), None),
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
    };
    let evs = mint_events(vec![
        events::user_text("before-first".into(), None),
        events::summary("first summary".into(), 1, cu),
        events::user_text("between".into(), None),
        events::summary("second summary".into(), 1, cu),
        events::user_text("after-second".into(), None),
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
fn permissions_session_persists_once_expires_on_turn_complete() {
    use std::collections::{HashMap, HashSet};
    let evs = mint_events(vec![
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "1".into(),
                decision: pb::Decision::Allow as i32,
                text: Some("shell:cargo test".into()),
                scope: pb::Scope::Session as i32,
            }],
        ),
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "2".into(),
                decision: pb::Decision::Allow as i32,
                text: Some("shell:ls".into()),
                scope: pb::Scope::Once as i32,
            }],
        ),
        wire::body(payload::Kind::TurnComplete(pb::TurnComplete::default())),
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
        "session grant persists past TurnComplete"
    );
    assert!(
        !st.once.contains("shell:ls"),
        "once grant expires at TurnComplete"
    );
}

#[test]
fn permissions_session_deny_revokes_but_ledger_keeps_both() {
    use std::collections::{HashMap, HashSet};
    let evs = mint_events(vec![
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "1".into(),
                decision: pb::Decision::Allow as i32,
                text: Some("shell:rm".into()),
                scope: pb::Scope::Session as i32,
            }],
        ),
        events::permissions_reply(
            "user".into(),
            &[pb::PermissionsReplyItem {
                id: "2".into(),
                decision: pb::Decision::Deny as i32,
                text: Some("shell:rm".into()),
                scope: pb::Scope::Session as i32,
            }],
        ),
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
fn fold_effective_allow_includes_session_grant() {
    let evs = mint_events(vec![events::permissions_reply(
        "user".into(),
        &[pb::PermissionsReplyItem {
            id: "1".into(),
            decision: pb::Decision::Allow as i32,
            text: Some("shell:git status".into()),
            scope: pb::Scope::Session as i32,
        }],
    )]);
    let res = fold(&evs, AgentState::default()).unwrap();
    assert!(
        res.effective_allow.contains("shell:git status"),
        "fold flattens session grants into effective_allow"
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
    let user_event = wire::append(&path, events::user_text("hello".into(), None)).unwrap();
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

    let user_event = wire::append(&path, events::user_text("old text".into(), None)).unwrap();
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

    let user_event = wire::append(&path, events::user_text("original".into(), None)).unwrap();
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
    assert!(muts
        .values()
        .all(|m| matches!(m, MutationState::Redacted { .. })));

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
fn fold_mutations_included_in_result() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("tx.jsonl");

    let e = wire::append(&path, events::user_text("hi".into(), None)).unwrap();
    wire::append(&path, redact_body(&e.id)).unwrap();

    let all = read_events(&path).unwrap();
    let result = fold(&all, AgentState::default()).unwrap();

    assert!(!result.mutations.is_empty(), "fold must populate mutations");
    assert!(
        result.messages.is_empty(),
        "redacted event must not appear in fold messages"
    );
}
#[test]
fn loadables_projection_full_session_and_mutation_aware() {
    use std::collections::HashMap;
    // A load, then a compaction AFTER it — the projection must NOT reset (full session).
    let cu = pb::CompactionTokenUsage {
        input: 1,
        output: 1,
    };
    let with_comp = mint_events(vec![
        events::toggle("mcp", "slack", true),
        events::summary("summary".into(), 1, cu),
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
    // Two TurnComplete events with different blocks; the helper must return
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
        events::turn_complete(events::TurnCompleteParams {
            usage,
            blocks: first.clone(),
            message: "ok".into(),
            ..Default::default()
        }),
        events::turn_complete(events::TurnCompleteParams {
            usage,
            blocks: second.clone(),
            message: "ok".into(),
            ..Default::default()
        }),
    ]);
    let got = last_turn_blocks(&evs);
    assert_eq!(got, second, "last TurnComplete wins");
}

#[test]
fn last_turn_blocks_empty_when_none() {
    // No TurnComplete events in the log → empty Vec.
    let evs = mint_events(vec![
        events::user_text("hi".into(), None),
        events::assistant_text("hello".into()),
    ]);
    assert!(last_turn_blocks(&evs).is_empty());
}

#[test]
fn active_agent_folds_toggle_per_kind_and_model_change() {
    use crate::proto::gait::log::v1::ToggleKind;
    use std::collections::HashMap;
    let seed = AgentState::from_config(
        &crate::config::GaitConfig::default(),
        "m".into(),
        "sys".into(),
        vec![],
        vec![],
    );
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
        wire::body(payload::Kind::ModelChange(pb::ModelChange {
            model: "m2".into(),
        })),
    ]);
    let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
    assert!(!st.skills.contains("tdd"), "skill toggled off");
    assert!(st.mcp.contains("slack"), "mcp toggled on in mcp set");
    assert!(
        !st.skills.contains("slack"),
        "mcp toggle did NOT leak into skills"
    );
    assert_eq!(st.model, "m2", "ModelChange LWW");
}

#[test]
fn rebaseline_agent_overlays_skills_and_mcp_from_state() {
    use std::collections::HashSet;
    let live = pb::Agent {
        model: "m1".into(),
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
        llm_params: None,
    };
    let st = AgentState {
        model: "m2".into(), // model from state is NOT used — static field from live
        instructions: String::new(),
        tools: HashSet::new(),
        skills: ["new-skill".to_string()].into(),
        mcp: ["new-mcp".to_string()].into(),
        plugins: HashSet::new(),
        allow: HashSet::new(),
        deny: HashSet::new(),
        llm_params: None,
    };
    let got = rebaseline_agent(live.clone(), &st);
    assert_eq!(got.model, "m1", "model is static — from live, not state");
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
fn total_usage_sums_turn_complete_events() {
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
        events::turn_complete(events::TurnCompleteParams {
            usage: usage1,
            message: "ok".into(),
            ..Default::default()
        }),
        events::turn_complete(events::TurnCompleteParams {
            usage: usage2,
            message: "ok".into(),
            ..Default::default()
        }),
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

    wire::append(&path, events::user_text("do it".into(), None)).unwrap();

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

            ..Default::default()
        })),
    };

    wire::append(&path, replace_body(&tr_event.id, new_payload)).unwrap();

    let all = read_events(&path).unwrap();

    let result = fold(&all, AgentState::default()).unwrap();

    // Exactly one ToolResult message should appear, and its content must be

    // the replacement string — not the original.

    let tool_results: Vec<_> = result
        .messages
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
        result.messages
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

    wire::append(&path, events::user_text("do it".into(), None)).unwrap();

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

            ..Default::default()
        })),
    };

    wire::append(&path, replace_body(&tr_event.id, new_payload)).unwrap();

    // Then redact the paired ToolCall — cascade must promote the Replace to

    // Redact on the ToolResult as well.

    wire::append(&path, redact_body(&tc_event.id)).unwrap();

    let all = read_events(&path).unwrap();

    let result = fold(&all, AgentState::default()).unwrap();

    // No ToolResult entry should remain in messages — cascade promoted the

    // Replace to Redact, so the result is dropped. (The ToolCall is also

    // redacted directly, so even without coalescing nothing would render.)

    let has_tool = result
        .messages
        .iter()
        .any(|m| matches!(m, Message::ToolResult { .. }));

    assert!(
        !has_tool,
        "cascade must hide the ToolResult too, got: {:?}",
        result.messages
    );
}

// --- Task 4: ActiveRawMessages + coalesce + transcript + outgoing_for_step ---

#[test]
fn raw_messages_carry_event_ids() {
    use crate::proto::envelope::mint;
    use std::collections::HashMap;
    let evs = vec![
        mint(events::user_text("hello".into(), None), None, 0, 0),
        mint(events::assistant_text("hi".into()), None, 0, 0),
    ];
    let raw = run_projection(&ActiveRawMessages, &evs, &HashMap::new());
    assert_eq!(raw.base.len(), 2);
    assert_eq!(raw.base[0].id(), evs[0].id.as_str());
    assert_eq!(raw.base[1].id(), evs[1].id.as_str());
}

#[test]
fn coalesce_merges_consecutive_tool_calls() {
    use crate::proto::envelope::mint;
    use std::collections::HashMap;
    let evs = vec![
        mint(
            events::tool_call("c1".into(), "read".into(), &serde_json::json!({})),
            None,
            0,
            0,
        ),
        mint(
            events::tool_call("c2".into(), "write".into(), &serde_json::json!({})),
            None,
            0,
            0,
        ),
        mint(
            events::tool_result("c1".into(), "r1".into(), None, None),
            None,
            0,
            0,
        ),
        mint(
            events::tool_result("c2".into(), "r2".into(), None, None),
            None,
            0,
            0,
        ),
    ];
    let raw = run_projection(&ActiveRawMessages, &evs, &HashMap::new());
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
    use crate::proto::envelope::mint;
    use std::collections::HashMap;
    let ev = mint(events::inject("user", "injected".into()), None, 1, 0);
    let raw = run_projection(
        &ActiveRawMessages,
        std::slice::from_ref(&ev),
        &HashMap::new(),
    );
    assert_eq!(raw.base.len(), 1, "Inject must go into base (Design A)");
    assert_eq!(raw.base[0].id(), ev.id.as_str());
}

#[test]
fn outgoing_for_step_includes_injected_messages() {
    use crate::proto::envelope::mint;
    let user_ev = mint(events::user_text("hello".into(), None), None, 0, 0);
    let inject_ev = mint(
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
fn active_unlock_folds_once_grants_and_clears_on_turn_complete() {
    use std::path::PathBuf;

    let unlock_item = pb::UnlockItem {
        id: "u1".to_string(),
        tool_name: "write".to_string(),
        resource: "/external/path/foo.txt".to_string(),
        dimension: "write".to_string(),
        preview: None,
    };
    let reply_item = pb::UnlockReplyItem {
        id: "u1".to_string(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Once as i32,
    };

    let evs = mint_events(vec![
        events::unlock(&[unlock_item]),
        events::unlock_reply("user".to_string(), &[reply_item]),
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
fn active_unlock_session_grant_survives_turn_complete() {
    use std::path::PathBuf;

    let unlock_item = pb::UnlockItem {
        id: "u2".to_string(),
        tool_name: "write".to_string(),
        resource: "/external/dir".to_string(),
        dimension: "write".to_string(),
        preview: None,
    };
    let reply_item = pb::UnlockReplyItem {
        id: "u2".to_string(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Session as i32,
    };
    let usage = pb::TurnTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };

    let evs = mint_events(vec![
        events::unlock(&[unlock_item]),
        events::unlock_reply("user".to_string(), &[reply_item]),
        events::turn_complete(events::TurnCompleteParams {
            usage,
            ..Default::default()
        }),
    ]);

    let result = active_unlock_state(&evs);
    // session grant survives TurnComplete
    assert!(result.session.contains(&PathBuf::from("/external/dir")));
    // once was cleared by TurnComplete
    assert!(result.once.is_empty());
}
