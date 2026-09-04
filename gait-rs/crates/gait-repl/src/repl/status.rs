//! Stable, non-editable context displayed immediately above the Reedline input.
//!
//! Keep this formatting separate from `Prompt`: Reedline's right prompt shares
//! the input row and is therefore allowed to disappear when the edit buffer is
//! long.  These rows are ordinary terminal output and remain in scrollback.

#[cfg(feature = "rich-tui")]
use gait_core::proto::gait::log::v1 as pb;

#[cfg(feature = "rich-tui")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReplStatus {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub mode: Option<String>,
    pub codebase: Option<String>,
    pub branch: Option<String>,
}

#[cfg(feature = "rich-tui")]
impl ReplStatus {
    pub(crate) fn render(&self) -> String {
        [
            self.agent.as_deref(),
            self.model.as_deref(),
            self.reasoning.as_deref(),
            self.mode.as_deref(),
            self.codebase.as_deref(),
            self.branch.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

/// Extract the configured provider-specific effort/think level. Missing
/// provider settings intentionally returns `None`: provider defaults must not
/// be presented as an invented "medium" setting.
#[cfg(feature = "rich-tui")]
pub(crate) fn reasoning_label(inference: &pb::Inference) -> Option<String> {
    let params = inference.llm_params.as_ref()?;
    match params.provider_params.as_ref()? {
        pb::llm_params::ProviderParams::Anthropic(a) => a.effort.clone(),
        pb::llm_params::ProviderParams::Openai(o) => o.reasoning_effort.clone(),
        pb::llm_params::ProviderParams::Gemini(g) => {
            g.thinking.as_ref().and_then(|t| match t.depth.as_ref()? {
                pb::gemini_thinking::Depth::Level(level) => Some(
                    match pb::GeminiThinkingLevel::try_from(*level).ok()? {
                        pb::GeminiThinkingLevel::Minimal => "minimal",
                        pb::GeminiThinkingLevel::Low => "low",
                        pb::GeminiThinkingLevel::Medium => "medium",
                        pb::GeminiThinkingLevel::High => "high",
                        _ => return None,
                    }
                    .to_string(),
                ),
                pb::gemini_thinking::Depth::Budget(budget) => Some(format!("budget:{budget}")),
            })
        }
        pb::llm_params::ProviderParams::Local(_) => None,
    }
}

pub(crate) fn short_model(model: &str) -> String {
    model
        .split_once('/')
        .map(|(_, id)| id)
        .unwrap_or(model)
        .to_string()
}

#[cfg(all(test, feature = "rich-tui"))]
mod tests {
    use super::*;

    fn inference(params: pb::llm_params::ProviderParams) -> pb::Inference {
        pb::Inference {
            model: "provider/model".into(),
            llm_params: Some(pb::LlmParams {
                provider_params: Some(params),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn status_omits_unconfigured_optional_values() {
        let status = ReplStatus {
            agent: Some("default".into()),
            model: Some("gpt-5".into()),
            codebase: Some("~/project".into()),
            ..Default::default()
        };
        assert_eq!(status.render(), "default · gpt-5 · ~/project");
    }

    #[test]
    fn status_preserves_reasoning_and_mode_order() {
        let status = ReplStatus {
            agent: Some("coding".into()),
            model: Some("gpt-5".into()),
            reasoning: Some("xhigh".into()),
            mode: Some("review".into()),
            codebase: Some("~/project".into()),
            branch: Some("main".into()),
        };
        assert_eq!(
            status.render(),
            "coding · gpt-5 · xhigh · review · ~/project · main"
        );
    }

    #[test]
    fn reasoning_comes_from_openai_effort() {
        let got = reasoning_label(&inference(pb::llm_params::ProviderParams::Openai(
            pb::OpenAiLlmParams {
                reasoning_effort: Some("xhigh".into()),
                ..Default::default()
            },
        )));
        assert_eq!(got.as_deref(), Some("xhigh"));
    }

    #[test]
    fn missing_reasoning_is_not_reported_as_medium() {
        let got = reasoning_label(&inference(pb::llm_params::ProviderParams::Openai(
            pb::OpenAiLlmParams::default(),
        )));
        assert_eq!(got, None);
    }

    #[test]
    fn reasoning_comes_from_gemini_level_or_budget() {
        let level = inference(pb::llm_params::ProviderParams::Gemini(
            pb::GeminiLlmParams {
                thinking: Some(pb::GeminiThinking {
                    depth: Some(pb::gemini_thinking::Depth::Level(
                        pb::GeminiThinkingLevel::Medium as i32,
                    )),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ));
        assert_eq!(reasoning_label(&level).as_deref(), Some("medium"));

        let budget = inference(pb::llm_params::ProviderParams::Gemini(
            pb::GeminiLlmParams {
                thinking: Some(pb::GeminiThinking {
                    depth: Some(pb::gemini_thinking::Depth::Budget(4096)),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ));
        assert_eq!(reasoning_label(&budget).as_deref(), Some("budget:4096"));
    }
}
