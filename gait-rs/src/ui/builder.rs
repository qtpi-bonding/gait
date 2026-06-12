use rustyline::Editor;
use rustyline::history::DefaultHistory;
use rustyline::error::ReadlineError;

use crate::repl_helper::ReplHelper;
use crate::proto::gait::log::v1 as pb;
use crate::ui::state::{ReplState, ViewSignal};
use crate::ui::renderer::ReplRenderer;
use crate::theme::Theme;
use crate::session::transcript::{
    ClarifyItem, ClarifyReplyItem, InformItem, InformReplyItem,
    PermissionItem,
};
use crate::theme::{RESET, DIM_FG};
use crate::proto::events;
use crate::ui::state::Pending;

pub struct InteractionOutcome {
    pub event_body: pb::event::Body,
    pub respawn: bool,
}

pub struct ReplBuilder {
    pub rl: Editor<ReplHelper, DefaultHistory>,
    state: ReplState,
    renderer: ReplRenderer,
}

impl ReplBuilder {
    pub fn new(rl: Editor<ReplHelper, DefaultHistory>, theme: Theme) -> Self {
        Self {
            rl,
            state: ReplState::new(),
            renderer: ReplRenderer::new(theme),
        }
    }

    pub fn begin_turn(&mut self) {
        self.state.begin_turn();
    }

    pub fn handle_stream_item(&mut self, item: pb::StreamItem) -> ViewSignal {
        use pb::stream_item::Item;
        let signal = match item.item {
            Some(Item::Event(ev)) => self.state.apply_event(&ev),
            Some(Item::Delta(d)) => {
                self.state.push_text(&d.content);
                ViewSignal::Continue
            }
            None => ViewSignal::Continue,
        };
        self.renderer.paint(&mut self.state);
        signal
    }

    pub fn flush(&mut self) {
        self.state.flush_buf();
        self.renderer.paint(&mut self.state);
    }

    pub fn has_interaction(&self) -> bool {
        self.state.has_interaction()
    }

    pub fn take_turn_blocks(&mut self) -> Vec<String> {
        std::mem::take(&mut self.state.all_blocks)
    }

    pub fn readline(&mut self, prompt: &str) -> Result<String, ReadlineError> {
        self.rl.readline(prompt)
    }

    pub fn add_history_entry(&mut self, line: &str) {
        let _ = self.rl.add_history_entry(line);
    }
}

impl ReplBuilder {
    pub fn run_interaction(
        &mut self,
        gait_dir: &std::path::Path,
        session_id: &str,
        agent_path: Option<&std::path::Path>,
    ) -> anyhow::Result<InteractionOutcome> {
        match self.state.take_pending() {
            Pending::Clarify(items)    => self.run_clarify(items),
            Pending::Inform(items)     => self.run_inform(items),
            Pending::Permission(items) => self.run_permission(items, gait_dir, session_id, agent_path),
            Pending::None => anyhow::bail!("run_interaction called with no pending interaction"),
        }
    }

    fn run_clarify(&mut self, items: Vec<ClarifyItem>) -> anyhow::Result<InteractionOutcome> {
        let prompt = format!("{}>{} ", self.renderer.theme().accent.fg(), RESET);
        let mut replies = Vec::new();
        for item in &items {
            eprintln!();
            eprintln!("{}? [{}] {}{}", self.renderer.theme().accent.fg(), item.kind, item.content, RESET);
            eprintln!("  {}why: {}{}", DIM_FG, item.why, RESET);
            let input = self.rl.readline(&prompt).unwrap_or_default().trim().to_string();
            replies.push(ClarifyReplyItem { id: item.id.clone(), content: input, reasoning: None });
        }
        Ok(InteractionOutcome { event_body: events::clarify_reply(&replies), respawn: true })
    }

    fn run_inform(&mut self, items: Vec<InformItem>) -> anyhow::Result<InteractionOutcome> {
        let prompt = format!("{}>{} ", self.renderer.theme().accent.fg(), RESET);
        let mut replies = Vec::new();
        for item in &items {
            eprintln!();
            eprintln!("{}> {}{}", self.renderer.theme().accent.fg(), item.content, RESET);
            if let Some(ctx) = &item.context {
                eprintln!("  {}context: {}{}", DIM_FG, ctx, RESET);
            }
            eprintln!("  {}(Enter to continue, or type a correction){}", DIM_FG, RESET);
            let input = self.rl.readline(&prompt).unwrap_or_default().trim().to_string();
            let content = if input.is_empty() { "understood".into() } else { input };
            replies.push(InformReplyItem { id: item.id.clone(), content });
        }
        Ok(InteractionOutcome { event_body: events::inform_reply(&replies), respawn: true })
    }

    fn run_permission(
        &mut self,
        _items: Vec<PermissionItem>,
        _gait_dir: &std::path::Path,
        _session_id: &str,
        _agent_path: Option<&std::path::Path>,
    ) -> anyhow::Result<InteractionOutcome> {
        anyhow::bail!("run_permission not yet implemented — see Task 5")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_outcome_fields() {
        let outcome = InteractionOutcome {
            event_body: crate::proto::events::inform_reply(&[] as &[crate::session::transcript::InformReplyItem]),
            respawn: false,
        };
        assert!(!outcome.respawn);
    }

    #[test]
    fn has_interaction_false_initially() {
        let state = crate::ui::state::ReplState::new();
        assert!(!state.has_interaction());
    }

    #[test]
    fn clarify_reply_event_has_correct_kind() {
        use crate::session::transcript::ClarifyReplyItem;
        use crate::proto::gait::log::v1::payload;
        let replies = vec![ClarifyReplyItem { id: "q1".into(), content: "answer".into(), reasoning: None }];
        let body = crate::proto::events::clarify_reply(&replies);
        if let crate::proto::gait::log::v1::event::Body::Payload(p) = body {
            assert!(matches!(p.kind, Some(payload::Kind::ClarifyReply(_))));
        } else { panic!("expected Payload body"); }
    }

    #[test]
    fn inform_reply_event_has_correct_kind() {
        use crate::session::transcript::InformReplyItem;
        use crate::proto::gait::log::v1::payload;
        let replies = vec![InformReplyItem { id: "i1".into(), content: "ok".into() }];
        let body = crate::proto::events::inform_reply(&replies);
        if let crate::proto::gait::log::v1::event::Body::Payload(p) = body {
            assert!(matches!(p.kind, Some(payload::Kind::InformReply(_))));
        } else { panic!("expected Payload body"); }
    }
}
