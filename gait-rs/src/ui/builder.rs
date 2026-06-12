use rustyline::Editor;
use rustyline::history::DefaultHistory;
use rustyline::error::ReadlineError;

use crate::repl_helper::ReplHelper;
use crate::proto::gait::log::v1 as pb;
use crate::ui::state::{ReplState, ViewSignal};
use crate::ui::renderer::ReplRenderer;
use crate::theme::Theme;

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
}
