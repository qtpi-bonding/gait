use crate::provider::{Provider, ProviderError, StreamChunk};
use crate::types::{Message, ToolDef, ToolCall, ToolResult, ToolResultKind, StepFinish, Usage};
use futures_util::StreamExt;

pub struct StreamTextResult {
    pub text: String,
    pub steps: Vec<StepFinish>,
    pub usage: Usage,
}

pub async fn stream_text(
    provider: &dyn Provider,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<StreamTextResult, ProviderError> {
    stream_text_with_callback(provider, system, messages, tools, |_| {}).await
}

pub async fn stream_text_with_callback<F>(
    provider: &dyn Provider,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    mut on_token: F,
) -> Result<StreamTextResult, ProviderError>
where
    F: FnMut(&str),
{
    let mut all_messages: Vec<Message> = messages.to_vec();
    let mut all_steps: Vec<StepFinish> = vec![];
    let mut total_usage = Usage::default();

    // Agentic loop: run until no tool calls remain (mirrors maxSteps in TS)
    for _ in 0..50 {
        let mut chunk_stream = provider.stream(system, &all_messages, tools).await?;

        let mut step_text = String::new();
        let mut step_tool_calls: Vec<ToolCall> = vec![];
        let mut step_usage = Usage::default();
        let mut pending_complete: Vec<ToolCall> = vec![];

        while let Some(chunk) = chunk_stream.next().await {
            match chunk? {
                StreamChunk::Text(t) => {
                    on_token(&t);
                    step_text.push_str(&t);
                }
                StreamChunk::ToolCallComplete { tool_call_id, tool_name, args } => {
                    pending_complete.push(ToolCall { tool_call_id, tool_name, args });
                }
                StreamChunk::StepUsage(u) => {
                    step_usage = step_usage.add(&u);
                    total_usage = total_usage.add(&u);
                }
                StreamChunk::Done => break,
                _ => {}
            }
        }

        step_tool_calls = pending_complete.clone();

        // No tool calls — we're done
        if step_tool_calls.is_empty() {
            all_steps.push(StepFinish {
                text: step_text.clone(),
                tool_calls: vec![],
                tool_results: vec![],
                usage: step_usage,
            });
            return Ok(StreamTextResult {
                text: step_text,
                steps: all_steps,
                usage: total_usage,
            });
        }

        // Execute all tool calls
        let mut tool_results: Vec<ToolResult> = vec![];
        for tc in &step_tool_calls {
            let tool = tools.iter().find(|t| t.name == tc.tool_name);
            let result = match tool {
                Some(t) => (t.execute)(tc.args.clone()).await
                    .unwrap_or_else(|e| serde_json::Value::String(format!("error: {e}"))),
                None => serde_json::Value::String(format!("unknown tool: {}", tc.tool_name)),
            };
            tool_results.push(ToolResult {
                kind: ToolResultKind::ToolResult,
                tool_call_id: tc.tool_call_id.clone(),
                result,
            });
        }

        // Append assistant turn with tool calls + tool results turn to messages
        use crate::types::{AssistantContent, AssistantPart};
        let mut assistant_parts: Vec<AssistantPart> = vec![];
        if !step_text.is_empty() {
            assistant_parts.push(AssistantPart::Text { text: step_text.clone() });
        }
        for tc in &step_tool_calls {
            assistant_parts.push(AssistantPart::ToolCall(tc.clone()));
        }
        all_messages.push(Message::Assistant {
            content: AssistantContent::Parts(assistant_parts),
        });
        all_messages.push(Message::Tool { content: tool_results.clone() });

        all_steps.push(StepFinish {
            text: step_text,
            tool_calls: step_tool_calls,
            tool_results,
            usage: step_usage,
        });
    }

    Err(ProviderError::Other("max steps (50) exceeded".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChunkStream, Provider, StreamChunk};
    use crate::types::{Message, ToolDef};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use futures_util::stream;

    struct MockProvider {
        // First call returns a tool call, second call returns text
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ChunkStream, ProviderError> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                // First call: emit a tool call
                let chunks: Vec<Result<StreamChunk, ProviderError>> = vec![
                    Ok(StreamChunk::ToolCallComplete {
                        tool_call_id: "id1".into(),
                        tool_name: "echo".into(),
                        args: serde_json::json!({ "msg": "hi" }),
                    }),
                    Ok(StreamChunk::Done),
                ];
                Ok(Box::pin(stream::iter(chunks)))
            } else {
                // Second call: emit text response
                let chunks: Vec<Result<StreamChunk, ProviderError>> = vec![
                    Ok(StreamChunk::Text("done".into())),
                    Ok(StreamChunk::Done),
                ];
                Ok(Box::pin(stream::iter(chunks)))
            }
        }
        fn model_id(&self) -> &str { "mock" }
    }

    #[tokio::test]
    async fn executes_tool_and_loops() {
        let provider = MockProvider { call_count: Arc::new(AtomicUsize::new(0)) };
        let call_count = provider.call_count.clone();

        let tool = ToolDef {
            name: "echo".into(),
            description: "echoes msg".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|args| Box::pin(async move {
                Ok(args["msg"].clone())
            })),
        };

        let result = stream_text(
            &provider,
            "",
            &[Message::User { content: crate::types::MessageContent::Text("go".into()) }],
            &[tool],
        ).await.unwrap();

        assert_eq!(result.text, "done");
        assert_eq!(call_count.load(Ordering::SeqCst), 2); // two LLM calls
        assert_eq!(result.steps.len(), 2);
        assert_eq!(result.steps[0].tool_calls[0].tool_name, "echo");
    }

    #[tokio::test]
    async fn returns_text_immediately_when_no_tools() {
        let provider = MockProvider { call_count: Arc::new(AtomicUsize::new(1)) }; // skip tool-call branch
        let result = stream_text(
            &provider,
            "",
            &[Message::User { content: crate::types::MessageContent::Text("hi".into()) }],
            &[],
        ).await.unwrap();
        assert_eq!(result.text, "done");
    }
}
