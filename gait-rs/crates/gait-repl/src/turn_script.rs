//! Frame-scripting primitives shared by both REPL flavors' mock-brain e2e
//! test harnesses: `dev_harness.rs` (the rich REPL's harness, additionally
//! gated on `rich-tui` since it drives the themed `ReplBuilder`) and
//! `plain.rs`'s own `run_plain_script_fresh` (no `rich-tui` dependency at
//! all). Extracted so both flavors script a mock-brain conversation through
//! one implementation instead of two copies drifting apart — pure
//! protocol/fixture code, no `ReplBuilder`/`Theme` here, so this module
//! compiles under `dev-harness` alone with `rich-tui` off.
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

/// Write `mock_frames.<n>.bin` (one per non-slash-command turn, numbered by
/// non-slash-turn index — slash-command turns are short-circuited inside the
/// REPL source before the brain is ever invoked, so the mock's internal
/// step counter increments once per non-slash turn only) plus the `mock`
/// inference `gait.jsonc` points at.
pub(crate) fn write_numbered_frames(gait_dir: &Path, turns: &[Turn]) -> anyhow::Result<()> {
    std::fs::create_dir_all(gait_dir.join("inferences/mock"))?;
    std::fs::write(
        gait_dir.join("inferences/mock/inference.textproto"),
        "model: \"mock/model\"\ncompaction_threshold_tokens: 128000\n",
    )?;
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

/// Spins up a throwaway git-inited temp gait-dir configured for the `mock`
/// inference, points `GAIT_BRAIN_EXE` at `mock_brain_exe`, and isolates
/// credential discovery from the developer's real auth store — the same
/// single directory serves as both the gait-dir and the codebase.
/// `dev_harness::run_script_fresh`'s only consumer (hence `rich-tui`-gated
/// here too): it drives with `sandbox_on: Some(false)` explicitly, which is
/// the only thing that makes a shared directory safe (the turn engine's
/// in-tree guard would otherwise refuse it). `plain::run_plain_script_fresh`
/// cannot reuse this — the plain REPL's real entry point always passes
/// `sandbox_on: None` (defaults `true`) — so it uses the sibling-directory
/// `setup_fresh_gait_dir_and_codebase` below instead.
#[cfg(feature = "rich-tui")]
pub(crate) fn setup_fresh_gait_dir(mock_brain_exe: &Path) -> anyhow::Result<tempfile::TempDir> {
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

    Ok(ws)
}

/// Like `setup_fresh_gait_dir`, but returns *separate* sibling directories
/// for the gait-dir and the codebase (`<tempdir>/gait`, `<tempdir>/codebase`)
/// instead of one shared directory for both. The turn engine's in-tree
/// safety guard (`embed.rs`'s `gait_dir_inside_codebase`) refuses a sandboxed
/// turn whose gait-dir sits inside its own codebase; `dev_harness`'s rich-
/// REPL fixture sidesteps that by driving with `sandbox_on: Some(false)`
/// explicitly (see `run_script`), but the plain REPL's real entry point
/// (`run_plain_loop`) always passes `sandbox_on: None` (which the turn
/// runner defaults to `true`) — so `plain::run_plain_script_fresh`'s own
/// fixture needs the non-nested layout a real deployment would have, not the
/// single shared directory `setup_fresh_gait_dir` hands back.
pub(crate) fn setup_fresh_gait_dir_and_codebase(
    mock_brain_exe: &Path,
) -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf)> {
    let ws = tempfile::TempDir::new()?;
    let codebase = ws.path().join("codebase");
    let gait_dir = ws.path().join("gait");
    std::fs::create_dir_all(&codebase)?;
    std::fs::create_dir_all(&gait_dir)?;

    let repo = git2::Repository::init(&codebase)?;
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

    // The gait-dir is its own git repo too (the `.active` worktree machinery
    // in `gait_state::git::trace` opens it directly, independent of the
    // codebase's repo) — the same bootstrap step `gait init`'s
    // `Repos::production().traces.init_workspace()` performs.
    git2::Repository::init(&gait_dir)?;
    std::fs::create_dir_all(gait_dir.join(".active"))?;
    std::fs::create_dir_all(gait_dir.join("workflow-runs"))?;
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#)?;

    // Keep credential discovery hermetic — same rationale as
    // `setup_fresh_gait_dir`.
    let auth_home = ws.path().join("gait-home");
    std::fs::create_dir_all(&auth_home)?;
    std::env::set_var("GAIT_HOME", &auth_home);
    std::env::set_var("GAIT_BRAIN_EXE", mock_brain_exe);

    Ok((ws, gait_dir, codebase))
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
