// gait-rs/src/anthropic/types.rs

use serde::{Deserialize, Serialize};

// ── Request ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    pub stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,  // "user" | "assistant"
    pub content: AnthropicContent,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicBlock>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String },
}

#[derive(Debug, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// ── SSE Events ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicEvent {
    MessageStart { message: MessageStartData },
    ContentBlockStart { index: u32, content_block: ContentBlock },
    ContentBlockDelta { index: u32, delta: ContentDelta },
    ContentBlockStop { index: u32 },
    MessageDelta { delta: MessageDeltaData, usage: Option<DeltaUsage> },
    MessageStop,
    Ping,
    Error { error: AnthropicErrorData },
}

#[derive(Debug, Deserialize)]
pub struct MessageStartData {
    pub usage: Option<DeltaUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String },
    Thinking { thinking: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct MessageDeltaData {
    pub stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeltaUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicErrorData {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

// ── Conversion: crate::types::Message → AnthropicMessage ─────────────────────

use crate::types::{Message, MessageContent, AssistantContent, AssistantPart};

pub fn to_anthropic_messages(messages: &[Message]) -> Vec<AnthropicMessage> {
    messages.iter().map(|m| match m {
        Message::User { content } => AnthropicMessage {
            role: "user".into(),
            content: match content {
                MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
                MessageContent::Parts(parts) => AnthropicContent::Blocks(
                    parts.iter().map(|p| match p {
                        crate::types::ContentPart::Text { text } =>
                            AnthropicBlock::Text { text: text.clone() },
                    }).collect()
                ),
            },
        },
        Message::Assistant { content } => AnthropicMessage {
            role: "assistant".into(),
            content: match content {
                AssistantContent::Text(t) => AnthropicContent::Text(t.clone()),
                AssistantContent::Parts(parts) => AnthropicContent::Blocks(
                    parts.iter().map(|p| match p {
                        AssistantPart::Text { text } =>
                            AnthropicBlock::Text { text: text.clone() },
                        AssistantPart::ToolCall(tc) =>
                            AnthropicBlock::ToolUse {
                                id: tc.tool_call_id.clone(),
                                name: tc.tool_name.clone(),
                                input: tc.args.clone(),
                            },
                    }).collect()
                ),
            },
        },
        Message::Tool { content } => AnthropicMessage {
            role: "user".into(),
            content: AnthropicContent::Blocks(
                content.iter().map(|tr| AnthropicBlock::ToolResult {
                    tool_use_id: tr.tool_call_id.clone(),
                    content: tr.result.to_string(),
                }).collect()
            ),
        },
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_request_no_tools() {
        let req = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".into(),
            max_tokens: 4096,
            system: Some("You are helpful.".into()),
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Text("hello".into()),
            }],
            tools: vec![],
            stream: true,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(json["stream"], true);
        // tools should be omitted when empty
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn deserialize_text_delta_event() {
        let raw = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
        let event: AnthropicEvent = serde_json::from_str(raw).unwrap();
        if let AnthropicEvent::ContentBlockDelta { delta: ContentDelta::TextDelta { text }, .. } = event {
            assert_eq!(text, "hello");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn deserialize_tool_use_block() {
        let raw = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"id1","name":"read"}}"#;
        let event: AnthropicEvent = serde_json::from_str(raw).unwrap();
        if let AnthropicEvent::ContentBlockStart { content_block: ContentBlock::ToolUse { id, name }, .. } = event {
            assert_eq!(id, "id1");
            assert_eq!(name, "read");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn tool_message_converts_to_user_with_tool_result_blocks() {
        use crate::types::{ToolResult, ToolResultKind};
        let messages = vec![Message::Tool {
            content: vec![ToolResult {
                kind: ToolResultKind::ToolResult,
                tool_call_id: "id1".into(),
                result: serde_json::json!("file contents"),
            }],
        }];
        let converted = to_anthropic_messages(&messages);
        assert_eq!(converted[0].role, "user");
        if let AnthropicContent::Blocks(blocks) = &converted[0].content {
            assert!(matches!(blocks[0], AnthropicBlock::ToolResult { .. }));
        } else {
            panic!("expected blocks");
        }
    }
}
