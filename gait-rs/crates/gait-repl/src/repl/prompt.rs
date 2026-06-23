//! reedline prompt: left `>`, right session context, open-node-aware indicator.
use reedline::{Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus};
use std::borrow::Cow;

pub struct GaitPrompt {
    pub model: String,
    pub session_short: String,
    pub branch: String,
    pub open_node: Option<String>,
}

impl Prompt for GaitPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        if self.branch.is_empty() {
            Cow::Owned(format!("{} · {}", self.model, self.session_short))
        } else {
            Cow::Owned(format!(
                "{} · {} · {}",
                self.model, self.session_short, self.branch
            ))
        }
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        match &self.open_node {
            // The degenerate chat node is always "open"; don't clutter for it.
            Some(node) if node != "chat" => Cow::Owned(format!("\u{21bb} {node} \u{203a} ")),
            _ => Cow::Borrowed("> "),
        }
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("\u{2502} ")
    }
    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({}reverse-search: {}) ",
            prefix, history_search.term
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{Prompt, PromptEditMode};

    fn p(open: Option<&str>) -> GaitPrompt {
        GaitPrompt {
            model: "m3".into(),
            session_short: "ab12".into(),
            branch: "main".into(),
            open_node: open.map(String::from),
        }
    }

    #[test]
    fn indicator_plain_when_no_open_node() {
        let prompt = p(None);
        let got = prompt.render_prompt_indicator(PromptEditMode::Default);
        assert_eq!(got.as_ref(), "> ");
    }

    #[test]
    fn indicator_shows_resuming_node() {
        let prompt = p(Some("fix"));
        let got = prompt.render_prompt_indicator(PromptEditMode::Default);
        assert!(got.contains("fix"), "got {got:?}");
    }

    #[test]
    fn right_prompt_shows_model_and_branch() {
        let prompt = p(None);
        let got = prompt.render_prompt_right();
        assert!(got.contains("m3") && got.contains("main"), "got {got:?}");
    }
}
