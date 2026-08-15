//! Adapter from Gait's provider contract to an in-process local model engine.
//!
//! Native inference intentionally does not appear here: this module is tested
//! against `gait_local::FakeGenerator`, so provider translation stays portable.

use crate::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
use async_stream::try_stream;
use futures_util::StreamExt;
use gait_local::{
    GenerationChunk, GenerationMessage, GenerationRequest, GenerationTool, GenerationToolCall,
    TextGenerator,
};
use gait_types::types::{AssistantContent, AssistantPart, Message, MessageContent, Tool, Usage};
use std::sync::Arc;

pub struct LocalProvider {
    model: String,
    generator: Arc<dyn TextGenerator>,
}

impl LocalProvider {
    pub fn new(model: String, generator: Arc<dyn TextGenerator>) -> Self {
        Self { model, generator }
    }
}

fn text_content(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                gait_types::types::ContentPart::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn assistant_content(content: &AssistantContent) -> (String, Vec<GenerationToolCall>) {
    match content {
        AssistantContent::Text(text) => (text.clone(), vec![]),
        AssistantContent::Parts(parts) => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for part in parts {
                match part {
                    AssistantPart::Text { text: delta } => text.push_str(delta),
                    AssistantPart::ToolCall(call) => calls.push(GenerationToolCall {
                        id: call.tool_call_id.clone(),
                        name: call.tool_name.clone(),
                        arguments: call.args.to_string(),
                    }),
                    AssistantPart::Thinking { .. } => {}
                }
            }
            (text, calls)
        }
    }
}

/// Pure translation boundary used by local-provider tests.
pub fn build_request(
    system: &str,
    messages: &[Message],
    tools: &[Box<dyn Tool>],
    params: &gait_types::config::LlmParamsConfig,
    tail_context: Option<&str>,
) -> GenerationRequest {
    let mut out = Vec::new();
    for message in messages {
        match message {
            Message::User {
                content,
                attribution,
                ..
            } => out.push(GenerationMessage::User {
                content: format!(
                    "{}{}",
                    gait_types::types::render_attribution_prefix(attribution),
                    text_content(content)
                ),
            }),
            Message::Assistant { content, .. } => {
                let (content, tool_calls) = assistant_content(content);
                out.push(GenerationMessage::Assistant {
                    content,
                    tool_calls,
                });
            }
            Message::ToolResult { content, .. } => {
                for result in content {
                    out.push(GenerationMessage::ToolResult {
                        call_id: result.tool_call_id.clone(),
                        // Gait's persisted ToolResult does not retain the tool
                        // name. The local chat renderer therefore receives an
                        // empty name here; it uses the call id as the stable key.
                        name: String::new(),
                        content: result.result.to_string(),
                    });
                }
            }
        }
    }
    if let Some(tail) = tail_context {
        out.push(GenerationMessage::User {
            content: tail.to_string(),
        });
    }
    GenerationRequest {
        system: system.to_string(),
        messages: out,
        tools: tools
            .iter()
            .map(|tool| GenerationTool {
                name: tool.name().to_string(),
                description: gait_types::types::render_description(tool.as_ref()),
                parameters: tool.parameters().clone(),
            })
            .collect(),
        max_tokens: params.max_tokens,
        temperature: params.temperature,
        top_p: params.top_p,
    }
}

#[async_trait::async_trait]
impl Provider for LocalProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Box<dyn Tool>],
        params: &gait_types::config::LlmParamsConfig,
        tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        let request = build_request(system, messages, tools, params, tail_context);
        let stream = self
            .generator
            .generate(request)
            .await
            .map_err(|error| ProviderError::Other(error.to_string()))?;
        Ok(Box::pin(try_stream! {
            tokio::pin!(stream);
            while let Some(chunk) = stream.next().await {
                match chunk.map_err(|error| ProviderError::Other(error.to_string()))? {
                    GenerationChunk::Text(text) => yield StreamChunk::Text(text),
                    GenerationChunk::ToolCall(call) => {
                        let args = crate::provider::coerce_tool_input(
                            serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null),
                        );
                        yield StreamChunk::ToolCallComplete {
                            tool_call_id: call.id,
                            tool_name: call.name,
                            args,
                        };
                    }
                    GenerationChunk::Usage { input_tokens, output_tokens } => {
                        yield StreamChunk::StepUsage(Usage {
                            input_tokens,
                            output_tokens,
                            ..Default::default()
                        });
                    }
                    GenerationChunk::Done => yield StreamChunk::Done,
                }
            }
        }))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_local::{FakeGenerator, GenerationChunk, GenerationToolCall};
    use gait_types::types::{AssistantContent, AssistantPart, Message, ToolCall};

    #[test]
    fn request_preserves_history_tail_and_sampling_bounds() {
        let messages = vec![
            Message::User {
                content: MessageContent::Text("hello".into()),
                id: "u".into(),
                attribution: None,
            },
            Message::Assistant {
                content: AssistantContent::Parts(vec![
                    AssistantPart::Text {
                        text: "thinking".into(),
                    },
                    AssistantPart::ToolCall(ToolCall {
                        tool_call_id: "call-1".into(),
                        tool_name: "read".into(),
                        args: serde_json::json!({"path":"a"}),
                    }),
                ]),
                id: "a".into(),
            },
        ];
        let request = build_request(
            "system",
            &messages,
            &[],
            &gait_types::config::LlmParamsConfig {
                max_tokens: Some(123),
                temperature: Some(0.2),
                top_p: Some(0.9),
                ..Default::default()
            },
            Some("tail"),
        );
        assert_eq!(request.system, "system");
        assert_eq!(request.max_tokens, Some(123));
        assert_eq!(request.messages.len(), 3);
        assert!(
            matches!(&request.messages[1], GenerationMessage::Assistant { tool_calls, .. } if tool_calls[0].name == "read")
        );
        assert!(
            matches!(&request.messages[2], GenerationMessage::User { content } if content == "tail")
        );
    }

    #[tokio::test]
    async fn provider_maps_text_tool_usage_and_done_without_a_native_model() {
        let provider = LocalProvider::new(
            "qwen3-coder".into(),
            Arc::new(FakeGenerator {
                chunks: vec![
                    Ok(GenerationChunk::Text("hi".into())),
                    Ok(GenerationChunk::ToolCall(GenerationToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: r#"{"path":"x"}"#.into(),
                    })),
                    Ok(GenerationChunk::Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    }),
                    Ok(GenerationChunk::Done),
                ],
            }),
        );
        let chunks = provider
            .stream("", &[], &[], &Default::default(), None)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(&chunks[0], Ok(StreamChunk::Text(text)) if text == "hi"));
        assert!(
            matches!(&chunks[1], Ok(StreamChunk::ToolCallComplete { tool_name, args, .. }) if tool_name == "read" && args == &serde_json::json!({"path":"x"}))
        );
        assert!(matches!(
            &chunks[2],
            Ok(StreamChunk::StepUsage(Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..
            }))
        ));
        assert!(matches!(&chunks[3], Ok(StreamChunk::Done)));
    }

    #[test]
    fn request_prepends_attribution_prefix_to_user_message() {
        let messages = vec![Message::User {
            content: MessageContent::Text("hello".into()),
            id: "u".into(),
            attribution: Some(gait_types::types::Attribution {
                sender: "ed25519:deadbeef".into(),
                verified: false,
            }),
        }];
        let request = build_request("system", &messages, &[], &Default::default(), None);
        assert_eq!(request.messages.len(), 1);
        let content = match &request.messages[0] {
            GenerationMessage::User { content } => content,
            _ => panic!("Expected User message"),
        };
        assert!(
            content.starts_with("[from: ed25519:deadbeef, unverified]\n"),
            "Content should start with attribution prefix, got: {}",
            content
        );
        assert!(
            content.ends_with("hello"),
            "Content should end with original message, got: {}",
            content
        );
    }
}
