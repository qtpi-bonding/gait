use crate::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
use crate::types::{Message, ToolDef, Usage, AssistantContent, AssistantPart, MessageContent};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
        ChatCompletionRequestToolMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionTool, ChatCompletionToolType, FunctionObject,
        CreateChatCompletionRequestArgs,
        ChatCompletionRequestUserMessageContent,
        ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestToolMessageContent, ChatCompletionStreamOptions,
    },
};
use async_openai::types::FunctionCall;
use futures_util::StreamExt;

pub struct OpenAIProvider {
    pub api_key: String,
    pub model: String,
    pub base_url: Option<String>,
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self { api_key, model, base_url }
    }

    fn build_client(&self) -> Client<OpenAIConfig> {
        let mut config = OpenAIConfig::new().with_api_key(&self.api_key);
        if let Some(url) = &self.base_url {
            config = config.with_api_base(url);
        }
        Client::with_config(config)
    }

    fn to_openai_messages(messages: &[Message]) -> Vec<ChatCompletionRequestMessage> {
        messages.iter().flat_map(|m| -> Vec<ChatCompletionRequestMessage> {
            match m {
                Message::User { content } => {
                    let text = match content {
                        MessageContent::Text(t) => t.clone(),
                        MessageContent::Parts(parts) => parts.iter()
                            .filter_map(|p| if let crate::types::ContentPart::Text { text } = p { Some(text.as_str()) } else { None })
                            .collect::<Vec<_>>().join(""),
                    };
                    vec![ChatCompletionRequestMessage::User(
                        ChatCompletionRequestUserMessage {
                            content: ChatCompletionRequestUserMessageContent::Text(text),
                            name: None,
                        }
                    )]
                }
                Message::Assistant { content } => {
                    let (text, tool_calls) = match content {
                        AssistantContent::Text(t) => (Some(t.clone()), vec![]),
                        AssistantContent::Parts(parts) => {
                            let text = parts.iter().find_map(|p| if let AssistantPart::Text { text } = p { Some(text.clone()) } else { None });
                            let tcs: Vec<_> = parts.iter().filter_map(|p| {
                                if let AssistantPart::ToolCall(tc) = p {
                                    Some(ChatCompletionMessageToolCall {
                                        id: tc.tool_call_id.clone(),
                                        r#type: ChatCompletionToolType::Function,
                                        function: FunctionCall {
                                            name: tc.tool_name.clone(),
                                            arguments: tc.args.to_string(),
                                        },
                                    })
                                } else { None }
                            }).collect();
                            (text, tcs)
                        }
                    };
                    // C2 fix: use builder to avoid missing-field compile errors across async-openai versions.
                    // async-openai 0.28.3 uses `pattern = "mutable"` — setters take &mut self and return &mut Self,
                    // so we call them as statements and never reassign.
                    let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
                    if let Some(t) = text { builder.content(t); }
                    if !tool_calls.is_empty() { builder.tool_calls(tool_calls); }
                    vec![ChatCompletionRequestMessage::Assistant(
                        builder.build().expect("assistant message build")
                    )]
                }
                Message::Tool { content } => {
                    content.iter().map(|tr| {
                        ChatCompletionRequestMessage::Tool(
                            ChatCompletionRequestToolMessage {
                                tool_call_id: tr.tool_call_id.clone(),
                                content: ChatCompletionRequestToolMessageContent::Text(
                                    tr.result.to_string()
                                ),
                            }
                        )
                    }).collect()
                }
            }
        }).collect()
    }
}

#[async_trait::async_trait]
impl Provider for OpenAIProvider {
    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ChunkStream, ProviderError> {
        let client = self.build_client();

        let mut all_messages = vec![];
        if !system.is_empty() {
            all_messages.push(ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessage { content: system.into(), name: None }
            ));
        }
        all_messages.extend(Self::to_openai_messages(messages));

        let oai_tools: Vec<ChatCompletionTool> = tools.iter().map(|t| ChatCompletionTool {
            r#type: ChatCompletionToolType::Function,
            function: FunctionObject {
                name: t.name.clone(),
                description: Some(t.description.clone()),
                parameters: Some(t.parameters.clone()),
                strict: None,
            },
        }).collect();

        // async-openai 0.28.3 uses `pattern = "mutable"` — setters take &mut self and return &mut Self,
        // so we must not chain them (the builder is a temporary that would be dropped). Use statements.
        let mut req_builder = CreateChatCompletionRequestArgs::default();
        req_builder.model(&self.model);
        req_builder.messages(all_messages);
        // C3 fix: request usage in the stream (many OAI-compatible gateways honor this)
        req_builder.stream_options(ChatCompletionStreamOptions { include_usage: true });
        if !oai_tools.is_empty() {
            req_builder.tools(oai_tools);
        }
        let req = req_builder.build().map_err(|e| ProviderError::Other(e.to_string()))?;

        let mut oai_stream = client.chat().create_stream(req).await
            .map_err(|e| ProviderError::Other(e.to_string()))?;

        let stream = async_stream::try_stream! {
            // C1 fix: key by index (u32), not by id string — id is only present on first chunk
            let mut tool_ids:   std::collections::HashMap<u32, String> = std::collections::HashMap::new();
            let mut tool_names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
            let mut tool_args:  std::collections::HashMap<u32, String> = std::collections::HashMap::new();

            while let Some(chunk) = oai_stream.next().await {
                let chunk = chunk.map_err(|e| ProviderError::Other(e.to_string()))?;
                for choice in &chunk.choices {
                    if let Some(delta) = &choice.delta.content {
                        yield StreamChunk::Text(delta.clone());
                    }
                    if let Some(tcs) = &choice.delta.tool_calls {
                        for tc_delta in tcs {
                            let idx = tc_delta.index;
                            // id and name only arrive on the first delta for this index
                            if let Some(id) = &tc_delta.id {
                                tool_ids.entry(idx).or_insert_with(|| id.clone());
                            }
                            if let Some(f) = &tc_delta.function {
                                if let Some(name) = &f.name {
                                    tool_names.entry(idx).or_insert_with(|| name.clone());
                                }
                                if let Some(args) = &f.arguments {
                                    tool_args.entry(idx).or_default().push_str(args);
                                    let id   = tool_ids.get(&idx).cloned().unwrap_or_default();
                                    let name = tool_names.get(&idx).cloned().unwrap_or_default();
                                    yield StreamChunk::ToolCallDelta {
                                        tool_call_id: id,
                                        tool_name: name,
                                        args_delta: args.clone(),
                                    };
                                }
                            }
                        }
                    }
                    if matches!(choice.finish_reason, Some(async_openai::types::FinishReason::ToolCalls) | Some(async_openai::types::FinishReason::Stop)) {
                        for (idx, args_str) in &tool_args {
                            let id   = tool_ids.get(idx).cloned().unwrap_or_default();
                            let name = tool_names.get(idx).cloned().unwrap_or_default();
                            let args = serde_json::from_str(args_str)
                                .unwrap_or(serde_json::Value::String(args_str.clone()));
                            yield StreamChunk::ToolCallComplete { tool_call_id: id, tool_name: name, args };
                        }
                        yield StreamChunk::Done;
                    }
                }
                if let Some(usage) = &chunk.usage {
                    yield StreamChunk::StepUsage(Usage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                    });
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}
