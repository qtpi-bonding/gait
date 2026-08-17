//! State half of the REPL view. Owns the pending interaction queue, the block accumulator, and the proto→view event application. Rendering lives in ui/renderer.rs.

use crate::ui::timer::Timer;
use gait_core::proto::gait::log::v1 as pb;
use std::io::{IsTerminal, Write};
use std::time::Instant;

// Dirty-queue item — the setState->build analog
pub(crate) enum ReplItem {
    Block {
        content: String,
        block_num: usize,
    },
    Spacing,
    ToolResult {
        ok: bool,
        name: String,
        elapsed_ms: Option<u64>,
        output: String,
    },
    Error {
        message: String,
    },
    TurnSummary {
        elapsed_ms: u64,
        output_tokens: u64,
    },
    /// `structured_done` human-readable message — rendered with a ✓│ gutter
    /// instead of a block number so it's visually distinct from streamed prose.
    DoneMessage {
        content: String,
    },
    /// `structured_done` JSON result — rendered in a box after the message.
    StructuredResult {
        json: String,
    },
}

// Interaction pending (one-shot per turn)
pub(crate) enum Pending {
    None,
    Clarify(Vec<pb::ClarifyItem>),
    Inform(Vec<pb::InformItem>),
    Permission(Vec<pb::PermissionItem>),
}

// ViewSignal
pub enum ViewSignal {
    Continue,
    TurnDone(Vec<String>),
}

// ReplState — the Cubit
pub struct ReplState {
    pub(crate) pending: Vec<ReplItem>,
    pub(crate) all_blocks: Vec<String>,
    pub(crate) block_num: usize,
    pub(crate) line_buf: String,
    pub(crate) block_buf: String,
    pub(crate) in_fence: bool,
    pub(crate) pending_tool: Option<String>,
    pub(crate) interaction_pending: Pending,
    pub(crate) timer: Option<Timer>,
    /// Shared with `ReplRenderer` — the timer tick uses `try_lock` and skips
    /// if `paint` holds it; this prevents the spinner erase from landing
    /// inside a block.
    pub(crate) paint_lock: Option<std::sync::Arc<std::sync::Mutex<()>>>,
    pub(crate) turn_start: Option<Instant>,
    /// Set when a `structured_done` ToolCall event is seen — lets `TurnResult`
    /// know to emit a `StructuredResult` pending item for the renderer.
    pub(crate) saw_structured_done: bool,
}

impl Default for ReplState {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplState {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            all_blocks: Vec::new(),
            block_num: 0,
            line_buf: String::new(),
            block_buf: String::new(),
            in_fence: false,
            pending_tool: Option::None,
            interaction_pending: Pending::None,
            timer: Option::None,
            paint_lock: None,
            turn_start: None,
            saw_structured_done: false,
        }
    }

    pub fn begin_turn(&mut self) {
        let now = Instant::now();
        *self = Self::new();
        self.turn_start = Some(now);
        let t = Timer::start(now);
        self.paint_lock = Some(t.paint_lock());
        self.timer = Some(t);
    }

    /// Start a turn with the stable context row attached to the spinner
    /// overlay. The overlay is drawn once here; subsequent renderer/timer
    /// redraws use the same centralized Timer state.
    pub fn begin_turn_with_status(&mut self, status: &str) {
        self.begin_turn();
        // The fd-capture REPL harness is intentionally non-terminal. Keep its
        // output linear so ANSI cursor movement cannot hide captured content;
        // the real terminal gets the managed two-line overlay below.
        if !std::io::stderr().is_terminal() {
            return;
        }
        if let Some(t) = &self.timer {
            t.set_context(status);
            let _guard = self.paint_lock.as_ref().and_then(|l| l.lock().ok());
            if let Some(start) = self.turn_start {
                t.draw_overlay(start);
                let _ = std::io::stderr().flush();
            }
        }
    }

    pub(crate) fn erase_overlay(&self) {
        if !std::io::stderr().is_terminal() {
            return;
        }
        if let Some(t) = &self.timer {
            t.erase_overlay();
        }
    }

    pub(crate) fn redraw_overlay(&self) {
        if !std::io::stderr().is_terminal() {
            return;
        }
        if let (Some(t), Some(start)) = (&self.timer, self.turn_start) {
            t.draw_overlay(start);
        }
    }

    pub fn push_text(&mut self, content: &str) {
        self.line_buf.push_str(content);
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].to_string();
            self.line_buf = self.line_buf[pos + 1..].to_string();
            self.push_line(&line);
        }
    }

    fn push_line(&mut self, line: &str) {
        let is_fence_marker = line.trim_start().starts_with("```");
        if self.in_fence {
            self.append_to_block(line);
            if is_fence_marker {
                self.in_fence = false;
                self.complete_block(false);
            }
        } else if line.is_empty() {
            self.complete_block(true);
        } else {
            self.append_to_block(line);
            if is_fence_marker {
                self.in_fence = true;
            }
        }
    }

    fn append_to_block(&mut self, line: &str) {
        if !self.block_buf.is_empty() {
            self.block_buf.push('\n');
        }
        self.block_buf.push_str(line);
    }

    fn complete_block(&mut self, with_spacing: bool) {
        if !self.block_buf.is_empty() {
            let block = std::mem::take(&mut self.block_buf);
            self.block_num += 1;
            self.all_blocks.push(block.clone());
            self.pending.push(ReplItem::Block {
                content: block,
                block_num: self.block_num,
            });
        }
        if with_spacing {
            self.pending.push(ReplItem::Spacing);
        }
    }

    pub fn flush_buf(&mut self) {
        if !self.line_buf.is_empty() {
            let partial = std::mem::take(&mut self.line_buf);
            self.append_to_block(&partial);
        }
        self.complete_block(false);
        self.in_fence = false;
    }

    pub fn apply_event(&mut self, ev: &pb::Event) -> ViewSignal {
        let Some(pb::event::Body::Payload(p)) = &ev.body else {
            return ViewSignal::Continue;
        };
        match &p.kind {
            Some(pb::payload::Kind::AssistantText(_)) => ViewSignal::Continue,

            Some(pb::payload::Kind::ToolCall(tc)) => {
                if tc.name == gait_core::tools::structured_done::DONE_TOOL_NAME {
                    self.saw_structured_done = true;
                }
                let args = tc.input.as_ref().and_then(|s| serde_json::to_value(s).ok());
                let target = args
                    .as_ref()
                    .and_then(|a| crate::ui::renderer::tool_target(&tc.name, a))
                    .map(|t| format!(" {}", t))
                    .unwrap_or_default();
                let display = format!("{}{}", tc.name, target);
                self.pending_tool = Some(display.clone());
                // Show the tool name in the status line — no separate spinner line.
                if let Some(t) = &self.timer {
                    t.set_activity(&display);
                }
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::ToolResult(tr)) => {
                let elapsed_ms = tr
                    .duration
                    .as_ref()
                    .map(|d| (d.seconds as u64) * 1000 + (d.nanos as u64) / 1_000_000);
                let ok = tr.process.as_ref().is_none_or(|pr| pr.exit_code == 0);
                let name = self.pending_tool.take().unwrap_or_else(|| "?".to_string());
                // Revert status line to "thinking…" while LLM processes the result.
                if let Some(t) = &self.timer {
                    t.set_activity("thinking…");
                }
                self.pending.push(ReplItem::ToolResult {
                    ok,
                    name,
                    elapsed_ms,
                    output: tr.output.clone(),
                });
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Permissions(pm)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Permission(pm.items.clone());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Clarify(c)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Clarify(c.items.clone());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Inform(inf)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Inform(inf.items.clone());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::TurnResult(tc)) => {
                self.flush_buf();
                // Stop the status line and emit the final turn summary.
                if let Some(mut t) = self.timer.take() {
                    t.stop();
                }
                let elapsed_ms = self
                    .turn_start
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let output_tokens = tc.usage.as_ref().map(|u| u.output).unwrap_or(0);
                self.pending.push(ReplItem::TurnSummary {
                    elapsed_ms,
                    output_tokens,
                });
                // If structured_done was called this turn, emit the result for rendering.
                if self.saw_structured_done {
                    if let Some(pb::turn_result::Outcome::Done(done)) = &tc.outcome {
                        if let Some(pb::turn_done::Output::Structured(out)) = &done.output {
                            // Render the human-readable message with a ✓│ gutter (DoneMessage),
                            // visually distinct from numbered streamed blocks. If the model also
                            // streamed the same text as deltas, it will appear twice — that's
                            // acceptable given the different visual treatment.
                            if !out.message.is_empty() {
                                self.pending.push(ReplItem::DoneMessage {
                                    content: out.message.clone(),
                                });
                            }
                            // JSON result → non-numbered box.
                            if let Some(json) = out
                                .result
                                .as_ref()
                                .and_then(|v| serde_json::to_value(v).ok())
                                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                            {
                                self.pending.push(ReplItem::StructuredResult { json });
                            }
                        }
                    }
                }
                let blocks = if tc.blocks.is_empty() {
                    self.all_blocks.clone()
                } else {
                    tc.blocks.clone()
                };
                ViewSignal::TurnDone(blocks)
            }
            Some(pb::payload::Kind::Error(e)) => {
                self.flush_buf();
                self.pending.push(ReplItem::Error {
                    message: e.message.clone(),
                });
                ViewSignal::Continue
            }
            _ => ViewSignal::Continue,
        }
    }

    pub fn has_interaction(&self) -> bool {
        !matches!(self.interaction_pending, Pending::None)
    }

    #[cfg(test)]
    pub(crate) fn peek_pending(&self) -> &Pending {
        &self.interaction_pending
    }

    pub(crate) fn take_pending(&mut self) -> Pending {
        std::mem::replace(&mut self.interaction_pending, Pending::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::events;

    fn turn_result_body(
        message: &str,
        result: Option<serde_json::Value>,
        blocks: Vec<String>,
        output_tokens: u64,
    ) -> pb::event::Body {
        events::turn_result(pb::TurnResult {
            turn_id: "ui-turn".into(),
            usage: Some(pb::TurnTokenUsage {
                output: output_tokens,
                ..Default::default()
            }),
            blocks,
            outcome: Some(pb::turn_result::Outcome::Done(pb::TurnDone {
                output: Some(pb::turn_done::Output::Structured(pb::StructuredDone {
                    message: message.into(),
                    result: events::to_value_opt(&result),
                })),
            })),
            ..Default::default()
        })
    }

    #[test]
    fn view_signal_variants_are_distinct() {
        assert!(!matches!(ViewSignal::Continue, ViewSignal::TurnDone(_)));
        assert!(matches!(
            ViewSignal::TurnDone(vec![]),
            ViewSignal::TurnDone(_)
        ));
    }

    #[test]
    fn pending_starts_none() {
        let state = ReplState::new();
        assert!(matches!(state.interaction_pending, Pending::None));
    }

    #[test]
    fn pending_items_empty_initially() {
        let state = ReplState::new();
        assert!(state.pending.is_empty());
    }

    #[test]
    fn push_text_single_newline_produces_block() {
        let mut state = ReplState::new();
        state.push_text("hello world\n");
        state.push_text("\n"); // blank line -> complete_block + Spacing
        assert!(state
            .pending
            .iter()
            .any(|p| matches!(p, ReplItem::Block { .. })));
        assert!(state.pending.iter().any(|p| matches!(p, ReplItem::Spacing)));
    }

    #[test]
    fn flush_buf_drains_partial_content() {
        let mut state = ReplState::new();
        state.push_text("partial");
        assert!(state.pending.is_empty(), "not flushed yet — no newline");
        state.flush_buf();
        assert!(state
            .pending
            .iter()
            .any(|p| matches!(p, ReplItem::Block { .. })));
    }

    #[test]
    fn fence_block_treated_as_single_block() {
        let mut state = ReplState::new();
        state.push_text("```\ncode line\n```\n");
        let blocks: Vec<_> = state
            .pending
            .iter()
            .filter(|p| matches!(p, ReplItem::Block { .. }))
            .collect();
        assert_eq!(blocks.len(), 1, "fence should produce exactly one block");
    }

    #[test]
    fn has_interaction_false_initially() {
        let state = ReplState::new();
        assert!(!state.has_interaction());
    }

    #[test]
    fn clarify_kind_maps_correctly() {
        use gait_core::proto::envelope;
        use gait_core::proto::events;
        let mut state = ReplState::new();
        let body = events::clarify(&[
            pb::ClarifyItem {
                id: "1".into(),
                content: "c1".into(),
                why: "w1".into(),
                kind: pb::ClarifyKind::Is as i32,
                options: vec![],
            },
            pb::ClarifyItem {
                id: "2".into(),
                content: "c2".into(),
                why: "w2".into(),
                kind: pb::ClarifyKind::Ought as i32,
                options: vec![],
            },
        ]);
        let ev = envelope::mint(
            body,
            None,
            envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        state.apply_event(&ev);
        if let Pending::Clarify(items) = &state.interaction_pending {
            assert_eq!(items[0].kind, pb::ClarifyKind::Is as i32);
            assert_eq!(items[1].kind, pb::ClarifyKind::Ought as i32);
        } else {
            panic!("expected Pending::Clarify");
        }
    }

    #[test]
    fn structured_done_message_emitted_as_done_message() {
        use gait_core::proto::envelope;
        let mut state = ReplState::new();
        state.saw_structured_done = true;
        let body = turn_result_body("task complete", None, vec![], 0);
        let ev = envelope::mint(body, None, envelope::EventScope::Turn { run: 0, turn: 0 });
        state.apply_event(&ev);
        assert!(
            state
                .pending
                .iter()
                .any(|p| matches!(p, ReplItem::DoneMessage { .. })),
            "structured_done message must become a DoneMessage item"
        );
        assert!(
            !state
                .pending
                .iter()
                .any(|p| matches!(p, ReplItem::StructuredResult { .. })),
            "no StructuredResult when result is None"
        );
    }

    #[test]
    fn structured_done_result_emitted_as_json_only_box() {
        use gait_core::proto::envelope;
        let mut state = ReplState::new();
        state.saw_structured_done = true;
        let result_value = serde_json::json!({ "count": 3 });
        let body = turn_result_body("", Some(result_value), vec![], 0);
        let ev = envelope::mint(body, None, envelope::EventScope::Turn { run: 0, turn: 0 });
        state.apply_event(&ev);
        let has_sr = state
            .pending
            .iter()
            .any(|p| matches!(p, ReplItem::StructuredResult { .. }));
        assert!(
            has_sr,
            "StructuredResult must be emitted when result is Some"
        );
        let block_contents: Vec<_> = state
            .pending
            .iter()
            .filter_map(|p| {
                if let ReplItem::Block { content, .. } = p {
                    Some(content.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            block_contents.is_empty(),
            "no Block when message is empty; got: {:?}",
            block_contents
        );
    }

    #[test]
    fn apply_event_turn_result_returns_done() {
        let mut state = ReplState::new();
        let body = turn_result_body("done", None, vec!["block one".into()], 0);
        let ev = gait_core::proto::envelope::mint(
            body,
            None,
            gait_core::proto::envelope::EventScope::Turn { run: 0, turn: 0 },
        );
        match state.apply_event(&ev) {
            ViewSignal::TurnDone(blocks) => assert_eq!(blocks, vec!["block one".to_string()]),
            _ => panic!("expected TurnDone"),
        }
    }

    #[test]
    fn apply_event_tool_call_sets_pending_tool() {
        use gait_core::proto::events;
        let mut state = ReplState::new();
        let body = events::tool_call(
            "call-1".into(),
            "read".into(),
            &serde_json::json!({"file_path": "a.rs"}),
        );
        let ev = gait_core::proto::envelope::mint(
            body,
            None,
            gait_core::proto::envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        let signal = state.apply_event(&ev);
        assert!(matches!(signal, ViewSignal::Continue));
        // Tool indication is via the timer status line, not a pending item.
        assert!(state.pending.is_empty());
        assert!(state.pending_tool.is_some());
    }

    #[test]
    fn apply_event_tool_result_pushes_pending_item() {
        use gait_core::proto::events;
        let mut state = ReplState::new();
        state.pending_tool = Some("read".into());
        let duration = pbjson_types::Duration {
            seconds: 1,
            nanos: 500_000_000,
        };
        let body = events::tool_result(
            "call-1".into(),
            "line one\nline two".into(),
            Some(duration),
            None,
        );
        let ev = gait_core::proto::envelope::mint(
            body,
            None,
            gait_core::proto::envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        let signal = state.apply_event(&ev);
        assert!(matches!(signal, ViewSignal::Continue));
        assert!(state
            .pending
            .iter()
            .any(|p| matches!(p, ReplItem::ToolResult { .. })));
        assert!(
            state.pending_tool.is_none(),
            "pending_tool should be consumed"
        );
    }

    #[test]
    fn apply_event_assistant_text_is_noop() {
        use gait_core::proto::events;
        let mut state = ReplState::new();
        state.all_blocks.push("prior".into());
        let before_blocks = state.all_blocks.clone();
        let body = events::assistant_text("full accumulated text".into());
        let ev = gait_core::proto::envelope::mint(
            body,
            None,
            gait_core::proto::envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        let signal = state.apply_event(&ev);
        assert!(matches!(signal, ViewSignal::Continue));
        assert_eq!(state.line_buf, "");
        assert_eq!(state.all_blocks, before_blocks);
    }

    #[test]
    fn push_text_paragraph_populates_all_blocks() {
        let mut state = ReplState::new();
        state.push_text("hello world\n");
        state.push_text("\n");
        assert_eq!(state.all_blocks.len(), 1);
        assert_eq!(state.all_blocks[0], "hello world");
    }

    #[test]
    fn blocks_numbered_sequentially() {
        let mut state = ReplState::new();
        state.push_text("para one\n\n");
        state.push_text("para two\n\n");
        assert_eq!(state.all_blocks.len(), 2);
        assert_eq!(state.block_num, 2);
    }
}
