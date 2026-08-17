//! reedline prompt: right shows agent + model, indicator is accent-colored.
use reedline::{Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus};
use std::borrow::Cow;

pub struct GaitPrompt {
    /// Agent file stem, e.g. "default", "coding".
    pub agent: String,
    /// Short model name, e.g. "haiku", "m3" (everything after the first `/`).
    pub model: String,
    pub branch: String,
    pub open_node: Option<String>,
    /// Pre-built accent-colored indicator string (avoids storing the theme).
    pub indicator: String,
}

impl Prompt for GaitPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        // Stable context is rendered on its own row above the editor. A right
        // prompt shares the edit row and disappears when the buffer is long.
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        match &self.open_node {
            Some(node) if node != "chat" => Cow::Owned(format!("\u{21bb} {node} \u{203a} ")),
            _ => Cow::Borrowed(&self.indicator),
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
            agent: "default".into(),
            model: "m3".into(),
            branch: "main".into(),
            open_node: open.map(String::from),
            indicator: "> ".into(),
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
    fn right_prompt_is_empty_so_long_input_cannot_overwrite_status() {
        let prompt = p(None);
        let got = prompt.render_prompt_right();
        assert_eq!(got.as_ref(), "");
    }
}
