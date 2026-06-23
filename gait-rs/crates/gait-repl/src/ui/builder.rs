use reedline::{DefaultPrompt, DefaultPromptSegment, Prompt, Reedline, Signal};

use crate::theme::Theme;
use crate::theme::{DIM_FG, RESET};
use crate::ui::renderer::ReplRenderer;
use crate::ui::state::Pending;
use crate::ui::state::{ReplState, ViewSignal};
use gait_core::proto::events;
use gait_core::proto::gait::log::v1 as pb;

pub struct InteractionOutcome {
    pub event_body: pb::event::Body,
    pub respawn: bool,
}

pub struct ReplBuilder {
    pub rl: Reedline,
    state: ReplState,
    renderer: ReplRenderer,
}

impl ReplBuilder {
    pub fn new(rl: Reedline, theme: Theme) -> Self {
        // Keybindings are configured at build time in chat.rs::build_line_editor.
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
        if let Some(mut t) = self.state.timer.take() {
            t.stop();
        }
        self.state.flush_buf();
        self.renderer.paint(&mut self.state);
    }

    pub fn has_interaction(&self) -> bool {
        self.state.has_interaction()
    }

    pub fn take_turn_blocks(&mut self) -> Vec<String> {
        std::mem::take(&mut self.state.all_blocks)
    }

    pub fn readline(&mut self, prompt: &dyn Prompt) -> std::io::Result<Signal> {
        self.rl.read_line(prompt)
    }

    fn make_prompt(&self, prefix: &str) -> DefaultPrompt {
        DefaultPrompt::new(
            DefaultPromptSegment::Basic(prefix.to_string()),
            DefaultPromptSegment::Empty,
        )
    }

    fn read_line_raw(&mut self, prefix: &str) -> String {
        let prompt = self.make_prompt(prefix);
        match self.rl.read_line(&prompt) {
            Ok(Signal::Success(s)) => {
                eprint!("{}", RESET);
                s.trim().to_string()
            }
            _ => {
                eprint!("{}", RESET);
                String::new()
            }
        }
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
            Pending::Clarify(items) => self.run_clarify(items),
            Pending::Inform(items) => self.run_inform(items),
            Pending::Permission(items) => {
                self.run_permission(items, gait_dir, session_id, agent_path)
            }
            Pending::None => anyhow::bail!("run_interaction called with no pending interaction"),
        }
    }

    fn run_clarify(&mut self, items: Vec<pb::ClarifyItem>) -> anyhow::Result<InteractionOutcome> {
        let prompt_str = format!("{}>{} ", self.renderer.theme().accent.fg(), RESET,);
        let mut replies = Vec::new();

        let bar = "\u{2500}".repeat(56);
        eprint!(
            "\r\n{}  \u{2500}\u{2500} clarify {}{}\r\n",
            DIM_FG, bar, RESET
        );

        for item in &items {
            eprint!("\r\n");
            // Content through the normal block renderer — gets a block number, is replyable.
            self.state.push_text(&item.content);
            self.state.flush_buf();
            self.renderer.paint(&mut self.state);
            // Secondary dim line — not part of the block.
            eprint!("{}     why: {}{}\r\n", DIM_FG, item.why, RESET);
            // Optional lettered options.
            if !item.options.is_empty() {
                eprint!("\r\n");
                for (i, opt) in item.options.iter().enumerate() {
                    let letter = (b'a' + i as u8) as char;
                    eprint!("{}     ({}) {}{}\r\n", DIM_FG, letter, opt, RESET);
                }
            }
            let raw = self.read_line_raw(&prompt_str);
            let content = if !item.options.is_empty() {
                parse_option_input(&raw, &item.options).unwrap_or(raw)
            } else {
                raw
            };
            replies.push(pb::ClarifyReplyItem {
                id: item.id.clone(),
                content,
                reasoning: None,
            });
        }
        Ok(InteractionOutcome {
            event_body: events::clarify_reply(&replies),
            respawn: true,
        })
    }

    fn run_inform(&mut self, items: Vec<pb::InformItem>) -> anyhow::Result<InteractionOutcome> {
        let prompt_str = format!("{}>{} ", self.renderer.theme().accent.fg(), RESET,);
        let mut replies = Vec::new();

        let bar = "\u{2500}".repeat(57);
        eprint!(
            "\r\n{}  \u{2500}\u{2500} inform {}{}\r\n",
            DIM_FG, bar, RESET
        );

        for item in &items {
            eprint!("\r\n");
            // Content through the normal block renderer — gets a block number, is replyable.
            self.state.push_text(&item.content);
            self.state.flush_buf();
            self.renderer.paint(&mut self.state);
            // Optional context dim line — not part of the block.
            if let Some(ctx) = &item.context {
                eprint!("{}     context: {}{}\r\n", DIM_FG, ctx, RESET);
            }
            let raw = self.read_line_raw(&prompt_str);
            let content = if raw.is_empty() {
                "understood".into()
            } else {
                raw
            };
            replies.push(pb::InformReplyItem {
                id: item.id.clone(),
                content,
            });
        }
        Ok(InteractionOutcome {
            event_body: events::inform_reply(&replies),
            respawn: true,
        })
    }

    fn run_permission(
        &mut self,
        items: Vec<pb::PermissionItem>,
        gait_dir: &std::path::Path,
        session_id: &str,
        agent_path: Option<&std::path::Path>,
    ) -> anyhow::Result<InteractionOutcome> {
        use crate::ui::permission::parse_permission_response;
        use gait_core::permission::PermissionChoice;
        let prompt_str = format!("  {}>{} ", self.renderer.theme().accent.fg(), RESET,);
        let mut reply_items: Vec<pb::PermissionsReplyItem> = vec![];
        let mut cancelled = false;
        let mut bulk: Option<BulkScope> = None;

        for (idx, item) in items.iter().enumerate() {
            eprint!("\r\n");
            if items.len() > 1 {
                eprint!(
                    "{}⟳ {}  {}({} of {}){}\r\n",
                    self.renderer.theme().accent.fg(),
                    item.cmd,
                    DIM_FG,
                    idx + 1,
                    items.len(),
                    RESET
                );
            } else {
                eprint!(
                    "{}⟳ {}{}\r\n",
                    self.renderer.theme().accent.fg(),
                    item.cmd,
                    RESET
                );
            }
            for line in crate::ui::permission::preview_block(item.preview.as_deref()) {
                eprint!("{}{}{}\r\n", DIM_FG, line, RESET);
            }
            eprint!("\r\n");
            let exact = item.patterns.last().cloned().unwrap_or_default();
            eprint!("  {}[0] {}  (all){}\r\n", DIM_FG, exact, RESET);
            for (i, pat) in item.patterns.iter().rev().enumerate() {
                eprint!("  {}[{}] {}{}\r\n", DIM_FG, i + 1, pat, RESET);
            }
            eprint!("\r\n");
            eprint!(
                "  {}0 = all · number = once · s = session · a = agent · r = reject · esc = abort{}\r\n",
                DIM_FG, RESET
            );

            let broadest = || item.patterns.last().cloned().unwrap_or_default();

            let choice = if cancelled {
                PermissionChoice::Deny
            } else if let Some(ref scope) = bulk {
                scope.to_choice(broadest())
            } else {
                loop {
                    let pmt = self.make_prompt(&prompt_str);
                    match self.rl.read_line(&pmt) {
                        Ok(Signal::Success(input)) => {
                            eprint!("{}", RESET);
                            let trimmed = input.trim().to_string();
                            if let Some(scope) = parse_bulk_scope(&trimmed) {
                                let choice = scope.to_choice(broadest());
                                bulk = Some(scope);
                                break choice;
                            }
                            match parse_permission_response(&trimmed, &item.patterns) {
                                Some(c) => break c,
                                None => eprint!("  unrecognized — e.g. 0, 1, s, s1, 1a, r\r\n"),
                            }
                        }
                        Ok(Signal::CtrlC) | Ok(Signal::CtrlD) | Err(_) => {
                            eprint!("{}", RESET);
                            cancelled = true;
                            break PermissionChoice::Deny;
                        }
                        Ok(_) => {
                            eprint!("{}", RESET);
                            cancelled = true;
                            break PermissionChoice::Deny;
                        }
                    }
                }
            };

            if let Err(e) =
                gait_core::permission::save_choice(&choice, gait_dir, session_id, agent_path)
            {
                eprint!("[gait] warning: could not save permission: {e}\r\n");
            }

            let perm = match &choice {
                gait_core::permission::PermissionChoice::Once(p) => pb::PermissionsReplyItem {
                    id: item.id.clone(),
                    decision: pb::Decision::Allow as i32,
                    text: Some(p.clone()),
                    scope: pb::Scope::Once as i32,
                },
                gait_core::permission::PermissionChoice::Session(p) => pb::PermissionsReplyItem {
                    id: item.id.clone(),
                    decision: pb::Decision::Allow as i32,
                    text: Some(p.clone()),
                    scope: pb::Scope::Session as i32,
                },
                gait_core::permission::PermissionChoice::Agent(p) => pb::PermissionsReplyItem {
                    id: item.id.clone(),
                    decision: pb::Decision::Allow as i32,
                    text: Some(p.clone()),
                    scope: pb::Scope::Agent as i32,
                },
                gait_core::permission::PermissionChoice::Global(p) => pb::PermissionsReplyItem {
                    id: item.id.clone(),
                    decision: pb::Decision::Allow as i32,
                    text: Some(p.clone()),
                    scope: pb::Scope::Global as i32,
                },
                gait_core::permission::PermissionChoice::Deny => pb::PermissionsReplyItem {
                    id: item.id.clone(),
                    decision: pb::Decision::Deny as i32,
                    text: None,
                    scope: pb::Scope::Unspecified as i32,
                },
            };
            reply_items.push(perm);
        }

        Ok(InteractionOutcome {
            event_body: events::permissions_reply("user".into(), &reply_items),
            respawn: !cancelled,
        })
    }
}

enum BulkScope {
    Once,
    Session,
    Agent,
}

impl BulkScope {
    fn to_choice(&self, pattern: String) -> gait_core::permission::PermissionChoice {
        use gait_core::permission::PermissionChoice;
        match self {
            BulkScope::Once => PermissionChoice::Once(pattern),
            BulkScope::Session => PermissionChoice::Session(pattern),
            BulkScope::Agent => PermissionChoice::Agent(pattern),
        }
    }
}

/// Parse `0`, `s0`, `0s`, `a0`, `0a` → BulkScope. Anything else → None.
fn parse_bulk_scope(input: &str) -> Option<BulkScope> {
    let s = input.trim().to_lowercase();
    let mut has_zero = false;
    let mut letter: Option<char> = None;
    for c in s.chars() {
        match c {
            '0' if !has_zero => has_zero = true,
            'o' | 's' | 'a' if letter.is_none() => letter = Some(c),
            _ => return None,
        }
    }
    if !has_zero {
        return None;
    }
    Some(match letter {
        Some('s') => BulkScope::Session,
        Some('a') => BulkScope::Agent,
        _ => BulkScope::Once,
    })
}

/// Parse a parenthesised letter list "(a)", "(a,b)", "( a , b )" against
/// `options`. Returns `Some(joined)` on full match, `None` for free-text fallback.
pub(crate) fn parse_option_input(input: &str, options: &[String]) -> Option<String> {
    let s = input.trim();
    if !(s.starts_with('(') && s.ends_with(')')) {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    if inner.trim().is_empty() {
        return None;
    }
    let letters: Vec<char> = inner
        .split(',')
        .map(|p| p.trim().chars().next())
        .collect::<Option<Vec<_>>>()?;
    let selected: Vec<String> = letters
        .iter()
        .map(|&l| {
            let idx = l.to_ascii_lowercase() as usize;
            let a = b'a' as usize;
            if idx < a {
                return None;
            }
            options.get(idx - a).cloned()
        })
        .collect::<Option<Vec<_>>>()?;
    if selected.is_empty() {
        return None;
    }
    Some(selected.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_outcome_fields() {
        let outcome = InteractionOutcome {
            event_body: gait_core::proto::events::inform_reply(&[] as &[pb::InformReplyItem]),
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
        use gait_core::proto::gait::log::v1::payload;
        let replies = vec![pb::ClarifyReplyItem {
            id: "q1".into(),
            content: "answer".into(),
            reasoning: None,
        }];
        let body = gait_core::proto::events::clarify_reply(&replies);
        if let pb::event::Body::Payload(p) = body {
            assert!(matches!(p.kind, Some(payload::Kind::ClarifyReply(_))));
        } else {
            panic!("expected Payload body");
        }
    }

    #[test]
    fn inform_reply_event_has_correct_kind() {
        use gait_core::proto::gait::log::v1::payload;
        let replies = vec![pb::InformReplyItem {
            id: "i1".into(),
            content: "ok".into(),
        }];
        let body = gait_core::proto::events::inform_reply(&replies);
        if let pb::event::Body::Payload(p) = body {
            assert!(matches!(p.kind, Some(payload::Kind::InformReply(_))));
        } else {
            panic!("expected Payload body");
        }
    }
    #[test]
    fn permissions_reply_event_has_correct_kind() {
        use gait_core::proto::gait::log::v1::payload;
        let replies = vec![pb::PermissionsReplyItem {
            id: "tc-1".into(),
            decision: pb::Decision::Deny as i32,
            text: None,
            scope: pb::Scope::Unspecified as i32,
        }];
        let body = gait_core::proto::events::permissions_reply("user".into(), &replies);
        if let pb::event::Body::Payload(p) = body {
            assert!(matches!(p.kind, Some(payload::Kind::PermissionsReply(_))));
        } else {
            panic!("expected Payload body");
        }
    }

    #[test]
    fn parse_option_input_single_letter() {
        let opts = vec!["JWT tokens".to_string(), "Sessions".to_string()];
        assert_eq!(
            parse_option_input("(a)", &opts),
            Some("JWT tokens".to_string())
        );
        assert_eq!(
            parse_option_input("(b)", &opts),
            Some("Sessions".to_string())
        );
    }

    #[test]
    fn parse_option_input_multi_select() {
        let opts = vec![
            "JWT tokens".to_string(),
            "Sessions".to_string(),
            "OAuth".to_string(),
        ];
        assert_eq!(
            parse_option_input("(a, b)", &opts),
            Some("JWT tokens; Sessions".to_string()),
        );
        assert_eq!(
            parse_option_input("(a,c)", &opts),
            Some("JWT tokens; OAuth".to_string()),
        );
    }

    #[test]
    fn parse_option_input_whitespace_tolerance() {
        let opts = vec!["JWT".to_string(), "Sessions".to_string()];
        assert_eq!(
            parse_option_input("( a , b )", &opts),
            Some("JWT; Sessions".to_string())
        );
        assert_eq!(
            parse_option_input("  (a)  ", &opts),
            Some("JWT".to_string())
        );
    }

    #[test]
    fn parse_option_input_free_text_fallback() {
        let opts = vec!["JWT".to_string(), "Sessions".to_string()];
        // out-of-range letter
        assert_eq!(parse_option_input("(z)", &opts), None);
        // no parens
        assert_eq!(parse_option_input("JWT please", &opts), None);
        // empty parens
        assert_eq!(parse_option_input("()", &opts), None);
    }
}
