//! Drives the real REPL turn-engine path (`gait_turn_runner::run_turn_source`,
//! the same call `chat.rs` makes) through a scripted, multi-turn conversation
//! — input lines via `ScriptedReader`, canned brain responses via `mock_brain`
//! — and returns the annotated transcript (see `ui::annotate`) of what would
//! have rendered. `run_script` drives an existing gait-dir; `run_script_fresh`
//! additionally spins one up in a temp dir.
use std::fmt;
use std::path::Path;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use gait_core::proto::gait::log::v1 as pb;
use gait_core::proto::gait::sandbox::v1::{signal, Signal, StreamForward, Terminated};
use gait_core::proto::{envelope, events};

#[derive(Debug, Deserialize)]
pub struct Script {
    pub turns: Vec<Turn>,
}

#[derive(Debug, Deserialize)]
pub struct Turn {
    pub input: String,
    #[serde(default)]
    pub frames: Vec<Frame>,
}

/// Hand-written single-key YAML map visitor for `Frame`. The plan's YAML form
/// (`- delta: "..."`, `- terminated_ok: "..."`, `- tool_call: { name: "read",
/// args: {...} }`, `- turn_done: { message: "..." }`) is a single-key map per element with no discriminator
/// field. `serde_yaml`'s enum deserializer (`#[serde(rename_all =
/// "snake_case")]`) refuses bare mappings without a `!Tag`, so a manual
/// visitor is the only way to match the plan's YAML verbatim. The string
/// field names match the plan exactly.
#[derive(Debug)]
pub enum Frame {
    Delta(String),
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        ok: bool,
        name: String,
    },
    TurnDone {
        message: String,
        result: Option<serde_json::Value>,
    },
    TerminatedOk(String),
    TerminatedFail(String),
}

impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Frame;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(
                    "a single-key frame map (delta|tool_call|tool_result|turn_done|terminated_ok|terminated_fail)",
                )
            }
            fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Frame, M::Error> {
                let k: String = m
                    .next_key()?
                    .ok_or_else(|| serde::de::Error::custom("empty frame"))?;
                match k.as_str() {
                    "delta" => Ok(Frame::Delta(m.next_value()?)),
                    "terminated_ok" => Ok(Frame::TerminatedOk(m.next_value()?)),
                    "terminated_fail" => Ok(Frame::TerminatedFail(m.next_value()?)),
                    "tool_call" => {
                        #[derive(Deserialize)]
                        struct In {
                            name: String,
                            args: serde_json::Value,
                        }
                        let i: In = m.next_value()?;
                        Ok(Frame::ToolCall {
                            name: i.name,
                            args: i.args,
                        })
                    }
                    "tool_result" => {
                        #[derive(Deserialize)]
                        struct In {
                            ok: bool,
                            name: String,
                        }
                        let i: In = m.next_value()?;
                        Ok(Frame::ToolResult {
                            ok: i.ok,
                            name: i.name,
                        })
                    }
                    "turn_done" => {
                        #[derive(Deserialize)]
                        struct In {
                            #[serde(default)]
                            message: String,
                            #[serde(default)]
                            result: Option<serde_json::Value>,
                        }
                        let i: In = m.next_value()?;
                        Ok(Frame::TurnDone {
                            message: i.message,
                            result: i.result,
                        })
                    }
                    other => Err(serde::de::Error::unknown_variant(
                        other,
                        &[
                            "delta",
                            "tool_call",
                            "tool_result",
                            "turn_done",
                            "terminated_ok",
                            "terminated_fail",
                        ],
                    )),
                }
            }
        }
        d.deserialize_map(V)
    }
}

fn frame_bytes(sig: &Signal) -> Vec<u8> {
    use prost::Message;
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

fn event_signal(body: pb::event::Body) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(pb::StreamItem {
                item: Some(pb::stream_item::Item::Event(envelope::mint(
                    body,
                    None,
                    envelope::EventScope::Step {
                        run: 0,
                        turn: 0,
                        step: 0,
                    },
                ))),
            }),
        })),
    }
}

fn append_event_signal(body: pb::event::Body, turn: u32) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(
            gait_core::proto::gait::sandbox::v1::Append {
                event: Some(envelope::mint(
                    body,
                    None,
                    envelope::EventScope::Step {
                        run: 0,
                        turn,
                        step: 0,
                    },
                )),
            },
        )),
    }
}

fn delta_signal(text: &str) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(pb::StreamItem {
                item: Some(pb::stream_item::Item::Delta(pb::TextDelta {
                    content: text.into(),
                })),
            }),
        })),
    }
}

fn terminated_signal(msg: &str, failed: bool) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(gait_core::proto::gait::sandbox::v1::FinalizedTurn {
                outcome: if failed {
                    gait_core::proto::gait::sandbox::v1::FinalizedTurnOutcome::Failed as i32
                } else {
                    gait_core::proto::gait::sandbox::v1::FinalizedTurnOutcome::TextDone as i32
                },
                message: msg.into(),
                ..Default::default()
            }),
            failed,
        })),
    }
}

fn frame_to_signal(frame: &Frame) -> Signal {
    match frame {
        Frame::Delta(text) => delta_signal(text),
        Frame::ToolCall { name, args } => event_signal(events::tool_call(
            "dev-harness-call".into(),
            name.clone(),
            args,
        )),
        Frame::ToolResult { ok, name } => event_signal(events::tool_result(
            "dev-harness-call".into(),
            if *ok {
                format!("{name}: ok")
            } else {
                format!("{name}: failed")
            },
            None,
            None,
        )),
        Frame::TurnDone { message, result } => event_signal(events::turn_result(pb::TurnResult {
            turn_id: "dev-harness-turn".into(),
            outcome: Some(pb::turn_result::Outcome::Done(pb::TurnDone {
                output: Some(pb::turn_done::Output::Structured(pb::StructuredDone {
                    message: message.clone(),
                    result: events::to_value_opt(result),
                })),
            })),
            ..Default::default()
        })),
        Frame::TerminatedOk(msg) => terminated_signal(msg, false),
        Frame::TerminatedFail(msg) => terminated_signal(msg, true),
    }
}

fn write_numbered_frames(gait_dir: &Path, turns: &[Turn]) -> anyhow::Result<()> {
    std::fs::create_dir_all(gait_dir.join("inferences/mock"))?;
    std::fs::write(
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\ncompaction_threshold_tokens: 128000\n",
    )?;
    // Slash-command turns (those whose `input` starts with `/`) are
    // short-circuited inside `ReplSource::next()` BEFORE the brain is
    // ever invoked (the REPL writes the cmd-handler response straight to
    // the shared builder and `continue`s). So `mock_brain` only sees the
    // non-slash turns, in order, and its internal `mock_step` counter
    // increments exactly once per non-slash turn. Number the brain's
    // `mock_frames.<N>.bin` files by *non-slash turn index*, not by raw
    // script-turn index, so turn N's `mock_frames.<N>.bin` matches what
    // the Nth brain invocation will look up.
    for (n, turn) in turns
        .iter()
        .filter(|t| !t.input.trim_start().starts_with('/'))
        .enumerate()
    {
        let mut blob = Vec::new();
        for frame in &turn.frames {
            // A real brain records at least one step-scoped event before its
            // terminal signal.  The tiny mock has no model loop, so provide
            // the same lifecycle marker; otherwise the spine quite correctly
            // rejects a Done result with zero steps.
            if matches!(frame, Frame::TerminatedOk(_) | Frame::TerminatedFail(_)) {
                blob.extend_from_slice(&frame_bytes(&append_event_signal(
                    events::assistant_text("mock step".into()),
                    n as u32,
                )));
            }
            blob.extend_from_slice(&frame_bytes(&frame_to_signal(frame)));
        }
        std::fs::write(gait_dir.join(format!("mock_frames.{n}.bin")), blob)?;
    }
    Ok(())
}

/// Drives `script` through the real interactive REPL path against an
/// already-set-up `gait_dir` (git-inited, `GAIT_BRAIN_EXE` already pointed at
/// `mock_brain`) and returns the annotated transcript of everything that
/// would have rendered.
///
/// NOTE: this future is `!Send` — `ReplSource` uses `#[async_trait(?Send)]`
/// (it holds an `Rc<RefCell<ReplShared>>`, the same single-threaded sharing
/// `chat.rs`'s real REPL loop uses). Callers must `.await` it directly on
/// whatever task already owns it — a plain `#[tokio::test]` (single-threaded
/// by default) or `chat.rs`'s own call site both do this correctly. Do NOT
/// `tokio::spawn` it or call it from a multi-threaded-runtime context that
/// requires `Send` futures — that won't compile, and isn't a bug to "fix" by
/// smuggling in `Send` bounds; it's inherent to the REPL's shared-state model.
pub async fn run_script(
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    script: &Script,
) -> anyhow::Result<String> {
    write_numbered_frames(gait_dir, &script.turns)?;

    let theme = crate::theme::Theme::dark();
    let inputs: Vec<&str> = script.turns.iter().map(|t| t.input.as_str()).collect();
    let builder = crate::ui::ReplBuilder::new(
        Box::new(crate::repl::ScriptedReader::lines(&inputs)),
        theme.clone(),
    );
    let shared: crate::repl::Shared =
        std::rc::Rc::new(std::cell::RefCell::new(crate::repl::ReplShared {
            builder,
            status_text: String::new(),
            turn_done_blocks: None,
            control: None,
            sandbox_on: false,
        }));

    let source = crate::repl::ReplSource {
        shared: shared.clone(),
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        session_id: session_id.to_string(),
        agent_path: None,
        repos: gait_core::git::Repos::production(),
        shell_history_path: gait_dir
            .join("tmp")
            .join(format!("shell-{session_id}.jsonl")),
        model_override: None,
        last_blocks: Vec::new(),
        ran_turn: false,
    };
    let mut sink = crate::repl::ReplSink {
        shared: shared.clone(),
    };

    let capture = crate::ui::capture::Capture::begin();
    let outcome = gait_turn_runner::run_turn_source(
        gait_turn_runner::TurnRunOpts {
            run_id: format!("repl-drive-{session_id}"),
            session_id: session_id.to_string(),
            continue_session: session_id.to_string(),
            gait_dir: gait_dir.to_path_buf(),
            codebase: codebase.to_path_buf(),
            args: Vec::new(),
            sandbox_on: Some(false),
            cascade_group_arg: None,
            cascade: None,
            profile: None,
            fs_delegate: None,
            allow_intree: false,
        },
        Box::new(source),
        &mut sink,
    )
    .await;
    let bytes = capture.end();
    if let Err(e) = outcome {
        anyhow::bail!(
            "run_turn_source failed: {e}; capture={}",
            String::from_utf8_lossy(&bytes)
        );
    }
    if String::from_utf8_lossy(&bytes).contains("turn failed") {
        let events = gait_core::session::Ingestor::from_session(gait_dir, session_id)
            .events()
            .unwrap_or_default();
        let coords: Vec<_> = events
            .iter()
            .filter(|e| e.step.is_some())
            .map(|e| (e.run, e.turn, e.step))
            .collect();
        anyhow::bail!(
            "run_turn_source reported a turn failure; coords={coords:?}; capture={}",
            String::from_utf8_lossy(&bytes),
        );
    }

    Ok(crate::ui::annotate::annotate(&bytes, &theme))
}

/// Convenience wrapper: spins up a throwaway git-inited temp gait-dir, points
/// `GAIT_BRAIN_EXE` at `mock_brain_exe`, and calls `run_script`. Returns the
/// `TempDir` too — it deletes on drop, so the caller must hold it until done.
pub async fn run_script_fresh(
    script: &Script,
    mock_brain_exe: &Path,
) -> anyhow::Result<(tempfile::TempDir, String)> {
    let ws = tempfile::TempDir::new()?;
    let gait_dir = ws.path().to_path_buf();

    let repo = git2::Repository::init(&gait_dir)?;
    let mut cfg = repo.config()?;
    cfg.set_str("user.name", "repl-drive")?;
    cfg.set_str("user.email", "repl-drive@test")?;
    let sig = git2::Signature::now("repl-drive", "repl-drive@test")?;
    let tree_id = repo.index()?.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])?;
    if let Ok(mut b) = repo.find_branch("master", git2::BranchType::Local) {
        let _ = b.rename("main", false);
    }

    std::fs::create_dir_all(gait_dir.join(".active"))?;
    std::fs::create_dir_all(gait_dir.join("workflow-runs"))?;
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#)?;

    // Keep credential discovery hermetic. The mock still follows the normal
    // auth-resolution path, but it must not inspect the developer's global
    // auth store (which may be unavailable in a restricted test runner).
    let auth_home = ws.path().join("gait-home");
    std::fs::create_dir_all(&auth_home)?;
    std::env::set_var("GAIT_HOME", &auth_home);
    std::env::set_var("GAIT_BRAIN_EXE", mock_brain_exe);

    let session_id = gait_core::session::new_session_id();
    let transcript = run_script(&gait_dir, &gait_dir, &session_id, script).await?;
    Ok((ws, transcript))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_yaml_deserializes_the_spec_example() {
        let yaml = r#"
turns:
  - input: "what does foo.rs do?"
    frames:
      - delta: "It defines the Foo struct.\n"
      - terminated_ok: "done"
  - input: "/sandbox off"
  - input: "now add a docstring"
    frames:
      - tool_call: { name: "read", args: { path: "foo.rs" } }
      - tool_result: { ok: true, name: "read" }
      - delta: "Added.\n"
      - turn_done: { message: "done", result: { changed: true } }
      - terminated_ok: "done"
"#;
        let script: Script = serde_yaml::from_str(yaml).expect("valid script");
        assert_eq!(script.turns.len(), 3);
        assert_eq!(script.turns[0].frames.len(), 2);
        assert!(script.turns[1].frames.is_empty());
        assert!(matches!(script.turns[2].frames[0], Frame::ToolCall { .. }));
        assert!(matches!(script.turns[2].frames[3], Frame::TurnDone { .. }));
    }
}
