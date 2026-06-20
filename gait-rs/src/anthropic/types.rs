// gait-rs/src/anthropic/types.rs

use serde::{Deserialize, Serialize};

// ── Cache control ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".into(),
        }
    }
}

// ── Request ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnthropicThinkingRequest {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicOutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<AnthropicSystem>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinkingRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<AnthropicOutputConfig>,
}

/// System prompt: either a plain string or an array of content blocks (needed for cache_control).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AnthropicSystem {
    Text(String),
    Blocks(Vec<AnthropicSystemBlock>),
}

#[derive(Debug, Serialize)]
pub struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl AnthropicSystemBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: text.into(),
            cache_control: None,
        }
    }
    pub fn text_cached(text: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: text.into(),
            cache_control: Some(CacheControl::ephemeral()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String, // "user" | "assistant"
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
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    // Send-side: pass captured thinking blocks back verbatim in multi-turn.
    // Anthropic requires the exact signature to be present.
    Thinking {
        thinking: String,
        signature: String,
    },
}

impl AnthropicBlock {
    /// Set cache_control on this block, regardless of variant.
    pub fn with_cache(mut self) -> Self {
        match &mut self {
            AnthropicBlock::Text { cache_control, .. } => {
                *cache_control = Some(CacheControl::ephemeral())
            }
            AnthropicBlock::ToolResult { cache_control, .. } => {
                *cache_control = Some(CacheControl::ephemeral())
            }
            AnthropicBlock::ToolUse { .. } => {}
            AnthropicBlock::Thinking { .. } => {}
        }
        self
    }
}

#[derive(Debug, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

// ── SSE Events ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicEvent {
    MessageStart {
        message: MessageStartData,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaData,
        usage: Option<DeltaUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: AnthropicErrorData,
    },
}

#[derive(Debug, Deserialize)]
pub struct MessageStartData {
    pub usage: Option<DeltaUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
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
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicErrorData {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

// ── Conversion: crate::types::Message → AnthropicMessage ─────────────────────

use crate::types::{AssistantContent, AssistantPart, Message, MessageContent};

pub fn to_anthropic_messages(messages: &[Message]) -> Vec<AnthropicMessage> {
    messages
        .iter()
        .map(|m| match m {
            Message::User { content, .. } => AnthropicMessage {
                role: "user".into(),
                content: match content {
                    MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
                    MessageContent::Parts(parts) => AnthropicContent::Blocks(
                        parts
                            .iter()
                            .map(|p| match p {
                                crate::types::ContentPart::Text { text } => AnthropicBlock::Text {
                                    text: text.clone(),
                                    cache_control: None,
                                },
                            })
                            .collect(),
                    ),
                },
            },
            Message::Assistant { content, .. } => AnthropicMessage {
                role: "assistant".into(),
                content: match content {
                    AssistantContent::Text(t) => AnthropicContent::Text(t.clone()),
                    AssistantContent::Parts(parts) => AnthropicContent::Blocks(
                        parts
                            .iter()
                            .map(|p| match p {
                                AssistantPart::Text { text } => AnthropicBlock::Text {
                                    text: text.clone(),
                                    cache_control: None,
                                },
                                AssistantPart::ToolCall(tc) => AnthropicBlock::ToolUse {
                                    id: tc.tool_call_id.clone(),
                                    name: tc.tool_name.clone(),
                                    // Backstop: `tool_use.input` must be a JSON object. Coerce any
                                    // non-object (e.g. a malformed value from an older transcript) so
                                    // a poisoned history can't 400 the request. See provider::coerce_tool_input.
                                    input: crate::provider::coerce_tool_input(tc.args.clone()),
                                },
                                AssistantPart::Thinking { content, signature } => {
                                    AnthropicBlock::Thinking {
                                        thinking: content.clone(),
                                        signature: signature.clone().unwrap_or_default(),
                                    }
                                }
                            })
                            .collect(),
                    ),
                },
            },
            Message::ToolResult { content, .. } => AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Blocks(
                    content
                        .iter()
                        .map(|tr| AnthropicBlock::ToolResult {
                            tool_use_id: tr.tool_call_id.clone(),
                            content: tr.result.to_string(),
                            cache_control: None,
                        })
                        .collect(),
                ),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_request_no_tools() {
        let req = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".into(),
            max_tokens: 4096,
            system: Some(AnthropicSystem::Text("You are helpful.".into())),
            messages: vec![AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Text("hello".into()),
            }],
            tools: vec![],
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
            output_config: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(json["stream"], true);
        assert!(json.get("tools").is_none());
        assert_eq!(json["system"], "You are helpful.");
    }

    #[test]
    fn serialize_cached_system_as_blocks() {
        let req = AnthropicRequest {
            model: "claude-3".into(),
            max_tokens: 1024,
            system: Some(AnthropicSystem::Blocks(vec![
                AnthropicSystemBlock::text_cached("You are helpful."),
            ])),
            messages: vec![],
            tools: vec![],
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
            output_config: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        let sys = &json["system"];
        assert!(sys.is_array());
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "You are helpful.");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn serialize_tool_with_cache_control() {
        let tool = AnthropicTool {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral()),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn serialize_tool_without_cache_control_omits_field() {
        let tool = AnthropicTool {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
            cache_control: None,
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert!(json.get("cache_control").is_none());
    }

    #[test]
    fn deserialize_text_delta_event() {
        let raw = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
        let event: AnthropicEvent = serde_json::from_str(raw).unwrap();
        if let AnthropicEvent::ContentBlockDelta {
            delta: ContentDelta::TextDelta { text },
            ..
        } = event
        {
            assert_eq!(text, "hello");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn deserialize_tool_use_block() {
        let raw = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"id1","name":"read"}}"#;
        let event: AnthropicEvent = serde_json::from_str(raw).unwrap();
        if let AnthropicEvent::ContentBlockStart {
            content_block: ContentBlock::ToolUse { id, name },
            ..
        } = event
        {
            assert_eq!(id, "id1");
            assert_eq!(name, "read");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn malformed_tool_use_input_is_coerced_to_object() {
        use crate::types::{AssistantContent, AssistantPart, ToolCall};
        // Simulate a poisoned history: a tool_use whose input was stored as a bare
        // JSON string (the truncated-buffer bug). It must serialize as an object,
        // otherwise the provider 400s on `tool_use.input` and the session dies.
        let messages = vec![Message::Assistant {
            id: String::new(),
            content: AssistantContent::Parts(vec![AssistantPart::ToolCall(ToolCall {
                tool_call_id: "tc1".into(),
                tool_name: "edit".into(),
                args: serde_json::Value::String("{\"edits\": ".into()),
            })]),
        }];
        let converted = to_anthropic_messages(&messages);
        let AnthropicContent::Blocks(blocks) = &converted[0].content else {
            panic!("expected blocks")
        };
        let AnthropicBlock::ToolUse { input, .. } = &blocks[0] else {
            panic!("expected tool_use")
        };
        assert!(
            input.is_object(),
            "tool_use.input must be an object, got {input:?}"
        );
        assert_eq!(*input, serde_json::json!({}));
    }

    #[test]
    fn tool_message_converts_to_user_with_tool_result_blocks() {
        use crate::types::{ToolResult, ToolResultKind};
        let messages = vec![Message::ToolResult {
            id: String::new(),
            content: vec![ToolResult {
                kind: ToolResultKind::ToolResult,
                tool_call_id: "id1".into(),
                result: serde_json::json!("file contents"),
            }],
        }];
        let converted = to_anthropic_messages(&messages);
        assert_eq!(converted[0].role, "user");
        if let AnthropicContent::Blocks(blocks) = &converted[0].content {
            assert!(matches!(
                blocks[0],
                AnthropicBlock::ToolResult {
                    cache_control: None,
                    ..
                }
            ));
        } else {
            panic!("expected blocks");
        }
    }
}
