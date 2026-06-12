use crate::proto::gait::log::v1 as pb;
use crate::session::transcript::{self as tx, ClarifyItem, InformItem, PermissionItem};
use crate::ui::timer::Timer;

// Dirty-queue item — the setState->build analog
pub(crate) enum PendingItem {
    Block { content: String, block_num: usize },
    Spacing,
    ToolCall { display: String },
    ToolResult { ok: bool, name: String, elapsed_ms: Option<u64>, output: String, had_spinner: bool },
    Error { message: String },
}

// Interaction pending (one-shot per turn)
pub(crate) enum Pending {
    None,
    Clarify(Vec<ClarifyItem>),
    Inform(Vec<InformItem>),
    Permission(Vec<PermissionItem>),
}

// ViewSignal
pub enum ViewSignal {
    Continue,
    TurnDone(Vec<String>),
}

// Proto -> transcript converters (moved from renderer.rs)
pub(crate) fn perm_to_tx(p: &pb::PermissionItem) -> tx::PermissionItem {
    tx::PermissionItem {
        id: p.id.clone(),
        cmd: p.cmd.clone(),
        reason: p.reason.clone(),
        patterns: p.patterns.clone(),
        tool_name: p.tool_name.clone(),
        tool_args: p.tool_args.clone()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null),
    }
}

pub(crate) fn clarify_to_tx(c: &pb::ClarifyItem) -> tx::ClarifyItem {
    tx::ClarifyItem {
        id: c.id.clone(),
        content: c.content.clone(),
        why: c.why.clone(),
        kind: match pb::ClarifyKind::try_from(c.kind) {
            Ok(pb::ClarifyKind::Is) => "is".to_string(),
            Ok(pb::ClarifyKind::Ought) => "ought".to_string(),
            _ => String::new(),
        },
    }
}

pub(crate) fn inform_to_tx(i: &pb::InformItem) -> tx::InformItem {
    tx::InformItem {
        id: i.id.clone(),
        content: i.content.clone(),
        context: i.context.clone(),
    }
}

// ReplState — the Cubit
pub struct ReplState {
    pub(crate) pending: Vec<PendingItem>,
    pub(crate) all_blocks: Vec<String>,
    pub(crate) block_num: usize,
    pub(crate) line_buf: String,
    pub(crate) block_buf: String,
    pub(crate) in_fence: bool,
    pub(crate) pending_tool: Option<String>,
    pub(crate) interaction_pending: Pending,
    pub(crate) timer: Option<Timer>,
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
        }
    }

    pub fn begin_turn(&mut self) {
        *self = Self::new();
        self.timer = Some(Timer::start());
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
            if is_fence_marker { self.in_fence = true; }
        }
    }

    fn append_to_block(&mut self, line: &str) {
        if !self.block_buf.is_empty() { self.block_buf.push('\n'); }
        self.block_buf.push_str(line);
    }

    fn complete_block(&mut self, with_spacing: bool) {
        if !self.block_buf.is_empty() {
            let block = std::mem::take(&mut self.block_buf);
            self.block_num += 1;
            self.all_blocks.push(block.clone());
            self.pending.push(PendingItem::Block { content: block, block_num: self.block_num });
        }
        if with_spacing {
            self.pending.push(PendingItem::Spacing);
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
        if let Some(mut t) = self.timer.take() { t.stop(); }

        let Some(pb::event::Body::Payload(p)) = &ev.body else {
            return ViewSignal::Continue;
        };
        match &p.kind {
            Some(pb::payload::Kind::AssistantText(_)) => ViewSignal::Continue,

            Some(pb::payload::Kind::ToolCall(tc)) => {
                let args = tc.input.as_ref()
                    .and_then(|s| serde_json::to_value(s).ok());
                let target = args.as_ref()
                    .and_then(|a| crate::ui::renderer::tool_target(&tc.name, a))
                    .map(|t| format!(" {}", t))
                    .unwrap_or_default();
                let display = format!("{}{}", tc.name, target);
                self.pending_tool = Some(display.clone());
                self.pending.push(PendingItem::ToolCall { display });
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::ToolResult(tr)) => {
                let elapsed_ms = tr.duration.as_ref()
                    .map(|d| (d.seconds as u64) * 1000 + (d.nanos as u64) / 1_000_000);
                let ok = tr.process.as_ref().map_or(true, |pr| pr.exit_code == 0);
                let had_spinner = self.pending_tool.is_some();
                let name = self.pending_tool.take().unwrap_or_else(|| "?".to_string());
                self.pending.push(PendingItem::ToolResult { ok, name, elapsed_ms, output: tr.output.clone(), had_spinner });
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Permissions(pm)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Permission(pm.items.iter().map(perm_to_tx).collect());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Clarify(c)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Clarify(c.items.iter().map(clarify_to_tx).collect());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Inform(inf)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Inform(inf.items.iter().map(inform_to_tx).collect());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::TurnComplete(tc)) => {
                self.flush_buf();
                let blocks = if tc.blocks.is_empty() {
                    self.all_blocks.clone()
                } else {
                    tc.blocks.clone()
                };
                ViewSignal::TurnDone(blocks)
            }
            Some(pb::payload::Kind::Error(e)) => {
                self.flush_buf();
                self.pending.push(PendingItem::Error { message: e.message.clone() });
                ViewSignal::Continue
            }
            _ => ViewSignal::Continue,
        }
    }

    pub fn has_interaction(&self) -> bool {
        !matches!(self.interaction_pending, Pending::None)
    }

    pub(crate) fn take_pending(&mut self) -> Pending {
        std::mem::replace(&mut self.interaction_pending, Pending::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_signal_variants_are_distinct() {
        assert!(!matches!(ViewSignal::Continue, ViewSignal::TurnDone(_)));
        assert!(matches!(ViewSignal::TurnDone(vec![]), ViewSignal::TurnDone(_)));
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
        state.push_text("\n");   // blank line -> complete_block + Spacing
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::Block { .. })));
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::Spacing)));
    }

    #[test]
    fn flush_buf_drains_partial_content() {
        let mut state = ReplState::new();
        state.push_text("partial");
        assert!(state.pending.is_empty(), "not flushed yet — no newline");
        state.flush_buf();
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::Block { .. })));
    }

    #[test]
    fn fence_block_treated_as_single_block() {
        let mut state = ReplState::new();
        state.push_text("```\ncode line\n```\n");
        let blocks: Vec<_> = state.pending.iter().filter(|p| matches!(p, PendingItem::Block { .. })).collect();
        assert_eq!(blocks.len(), 1, "fence should produce exactly one block");
    }

    #[test]
    fn has_interaction_false_initially() {
        let state = ReplState::new();
        assert!(!state.has_interaction());
    }

    #[test]
    fn clarify_kind_maps_correctly() {
        use crate::proto::envelope;
        use crate::proto::events;
        use crate::session::transcript::ClarifyItem as txCI;
        let mut state = ReplState::new();
        let body = events::clarify(&[
            txCI { id: "1".into(), content: "c1".into(), why: "w1".into(), kind: "is".into() },
            txCI { id: "2".into(), content: "c2".into(), why: "w2".into(), kind: "ought".into() },
        ]);
        let ev = envelope::mint(body, None);
        state.apply_event(&ev);
        if let Pending::Clarify(items) = &state.interaction_pending {
            assert_eq!(items[0].kind, "is");
            assert_eq!(items[1].kind, "ought");
        } else {
            panic!("expected Pending::Clarify");
        }
    }
    #[test]
    fn apply_event_permissions_sets_pending() {
        use crate::proto::events;
        use crate::session::transcript::PermissionItem as txPI;
        let mut state = ReplState::new();
        let body = events::permissions(&[txPI {
            id: "tc-1".into(),
            cmd: "cargo test".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["cargo *".into(), "cargo test".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "cargo test"}),
        }]);
        let ev = crate::proto::envelope::mint(body, None);
        state.apply_event(&ev);
        if let Pending::Permission(items) = &state.interaction_pending {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].cmd, "cargo test");
            assert_eq!(items[0].tool_name, "shell");
            assert_eq!(items[0].tool_args["cmd"], "cargo test");
        } else {
            panic!("expected Pending::Permission");
        }
    }

    #[test]
    fn apply_event_turn_complete_returns_done() {
        use crate::proto::events;
        use crate::session::transcript::{TurnUsage, TurnOutput};
        let mut state = ReplState::new();
        let body = events::turn_complete(
            &TurnUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 },
            "h".into(), false, None, vec!["block one".into()],
            &TurnOutput { message: "done".into(), result: None },
        );
        let ev = crate::proto::envelope::mint(body, None);
        match state.apply_event(&ev) {
            ViewSignal::TurnDone(blocks) => assert_eq!(blocks, vec!["block one".to_string()]),
            _ => panic!("expected TurnDone"),
        }
    }

    #[test]
    fn apply_event_tool_call_pushes_pending_item() {
        use crate::proto::events;
        let mut state = ReplState::new();
        let body = events::tool_call("call-1".into(), "read".into(), &serde_json::json!({"file_path": "a.rs"}));
        let ev = crate::proto::envelope::mint(body, None);
        let signal = state.apply_event(&ev);
        assert!(matches!(signal, ViewSignal::Continue));
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::ToolCall { .. })));
    }

    #[test]
    fn apply_event_tool_result_pushes_pending_item() {
        use crate::proto::events;
        let mut state = ReplState::new();
        state.pending_tool = Some("read".into());
        let duration = pbjson_types::Duration { seconds: 1, nanos: 500_000_000 };
        let body = events::tool_result("call-1".into(), "line one\nline two".into(), Some(duration), None);
        let ev = crate::proto::envelope::mint(body, None);
        let signal = state.apply_event(&ev);
        assert!(matches!(signal, ViewSignal::Continue));
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::ToolResult { .. })));
        assert!(state.pending_tool.is_none(), "pending_tool should be consumed");
    }

    #[test]
    fn apply_event_assistant_text_is_noop() {
        use crate::proto::events;
        let mut state = ReplState::new();
        state.all_blocks.push("prior".into());
        let before_blocks = state.all_blocks.clone();
        let body = events::assistant_text("full accumulated text".into());
        let ev = crate::proto::envelope::mint(body, None);
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
