use crate::gait::log::v1 as pb;
use prost::Message;
use prost_reflect::{text_format::FormatOptions, DescriptorPool, DynamicMessage};
use std::sync::OnceLock;

static INFERENCE_DESCRIPTOR: OnceLock<prost_reflect::MessageDescriptor> = OnceLock::new();

fn descriptor() -> prost_reflect::MessageDescriptor {
    INFERENCE_DESCRIPTOR
        .get_or_init(|| {
            DescriptorPool::decode(crate::DESCRIPTOR)
                .expect("embedded descriptor")
                .get_message_by_name("gait.log.v1.Inference")
                .expect("Inference descriptor")
        })
        .clone()
}

fn parse_span(debug: &str) -> Option<(usize, usize)> {
    let (_, rest) = debug.rsplit_once("span: ")?;
    let start = rest
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let end = rest.find("..")?;
    let end = rest[end + 2..]
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((start, end))
}

fn location(text: &str, start: usize) -> Option<(usize, usize, &str)> {
    if start > text.len() || !text.is_char_boundary(start) {
        return None;
    }
    let before = &text[..start];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit_once('\n').map_or_else(
        || before.chars().count() + 1,
        |(_, current)| current.chars().count() + 1,
    );
    let source = text.split('\n').nth(line - 1)?.trim_end_matches('\r');
    Some((line, column, source))
}

/// Parse an Inference protobuf text format document, including its standard headers.
pub fn parse_inference_text(text: &str) -> Result<pb::Inference, String> {
    let dm = DynamicMessage::parse_text_format(descriptor(), text).map_err(|e| {
        let position = parse_span(&format!("{e:?}")).and_then(|(start, _)| location(text, start));
        match position {
            Some((line, column, source)) => {
                format!("line {line}, column {column}: {e}; source: {source}")
            }
            None => format!("location unavailable: {e}"),
        }
    })?;
    pb::Inference::decode(dm.encode_to_vec().as_slice()).map_err(|e| e.to_string())
}

/// Render an Inference protobuf message in pretty protobuf text format.
pub fn inference_to_text(inference: &pb::Inference) -> String {
    let mut without_name = inference.clone();
    without_name.name.clear();
    let dm = DynamicMessage::decode(descriptor(), without_name.encode_to_vec().as_slice())
        .expect("Inference matches descriptor");
    let mut body = dm.to_text_format_with_options(&FormatOptions::new().pretty(true));
    if !body.ends_with('\n') {
        body.push('\n');
    }
    format!("# proto-file: proto/log.proto\n# proto-message: gait.log.v1.Inference\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(provider_params: pb::llm_params::ProviderParams) -> pb::Inference {
        pb::Inference {
            name: "ignored-directory-name".into(),
            model: "provider/model".into(),
            llm_params: Some(pb::LlmParams {
                max_tokens: Some(100),
                temperature: Some(0.5),
                top_p: Some(0.9),
                provider_params: Some(provider_params),
            }),
            compaction_threshold_tokens: 1234,
        }
    }

    fn assert_round_trip(value: pb::Inference) {
        assert_eq!(parse_inference_text(&inference_to_text(&value)).unwrap(), {
            let mut expected = value;
            expected.name.clear();
            expected
        });
    }

    #[test]
    fn unknown_field_on_second_line_reports_line_two_column_one() {
        let error = parse_inference_text("model: \"x\"\nunknown_field: 1\n").unwrap_err();
        assert!(error.contains("line 2, column 1"), "{error}");
        assert!(error.contains("source: unknown_field: 1"), "{error}");
    }

    #[test]
    fn user_text_containing_span_marker_does_not_hijack_location() {
        let error = parse_inference_text("model: \"x\"\nspan: 9\n").unwrap_err();
        assert!(error.contains("line 2, column 1"), "{error}");
    }

    #[test]
    fn wrong_type_reports_exact_position() {
        let error = parse_inference_text("model: \"x\"\ncompaction_threshold_tokens: \"bad\"\n")
            .unwrap_err();
        assert!(error.contains("line 2, column 30"), "{error}");
    }

    #[test]
    fn non_ascii_text_does_not_shift_column() {
        let error = parse_inference_text("model: \"é\" @\n").unwrap_err();
        assert!(error.contains("line 1, column 12"), "{error}");
    }

    #[test]
    fn crlf_source_line_is_reported_without_carriage_return() {
        let error = parse_inference_text("model: \"x\"\r\nunknown_field: 1\r\n").unwrap_err();
        assert!(error.contains("line 2, column 1"), "{error}");
        assert!(error.contains("source: unknown_field: 1"), "{error}");
        assert!(!error.contains("unknown_field: 1\r"), "{error}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(parse_inference_text("not_a_field: 1\n").is_err());
    }

    #[test]
    fn openai_round_trip_all_fields() {
        assert_round_trip(base(pb::llm_params::ProviderParams::Openai(
            pb::OpenAiLlmParams {
                reasoning_effort: Some("high".into()),
                frequency_penalty: Some(0.1),
                presence_penalty: Some(0.2),
                seed: Some(42),
                stop: vec!["one".into(), "two".into()],
                strict_tools: Some(true),
            },
        )));
    }

    #[test]
    fn anthropic_round_trip_all_fields() {
        assert_round_trip(base(pb::llm_params::ProviderParams::Anthropic(
            pb::AnthropicLlmParams {
                top_k: Some(12),
                effort: Some("high".into()),
                thinking: Some(pb::AnthropicThinking {
                    r#type: pb::AnthropicThinkingType::Enabled as i32,
                    budget_tokens: Some(2048),
                    display: pb::AnthropicThinkingDisplay::Summarized as i32,
                }),
                strict_tools: Some(true),
            },
        )));
    }

    #[test]
    fn gemini_round_trip_budget_and_level() {
        for depth in [
            pb::gemini_thinking::Depth::Budget(4096),
            pb::gemini_thinking::Depth::Level(pb::GeminiThinkingLevel::High as i32),
        ] {
            assert_round_trip(base(pb::llm_params::ProviderParams::Gemini(
                pb::GeminiLlmParams {
                    top_k: Some(8),
                    candidate_count: Some(2),
                    stop_sequences: vec!["stop".into()],
                    seed: Some(7),
                    frequency_penalty: Some(0.1),
                    presence_penalty: Some(0.2),
                    thinking: Some(pb::GeminiThinking {
                        include_thoughts: Some(true),
                        depth: Some(depth),
                    }),
                },
            )));
        }
    }

    #[test]
    fn local_round_trip_all_fields() {
        assert_round_trip(base(pb::llm_params::ProviderParams::Local(
            pb::LocalLlmParams {
                model_path: "/models/model.gguf".into(),
                context_tokens: Some(8192),
                gpu_layers: Some(32),
            },
        )));
    }

    #[test]
    fn headers_and_name_omission() {
        let text = inference_to_text(&pb::Inference {
            name: "secret".into(),
            ..Default::default()
        });
        assert!(text.starts_with(
            "# proto-file: proto/log.proto\n# proto-message: gait.log.v1.Inference\n"
        ));
        assert!(!text.contains("name:"));
    }
}
