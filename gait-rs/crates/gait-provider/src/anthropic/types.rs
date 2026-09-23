// gait-rs/src/anthropic/types.rs

use serde::{Deserialize, Serialize};

// ── Cache control ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
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

/// Controls which tool(s) the model may call.
/// Wire shape: `{"type": "auto"}`, `{"type": "any"}`, `{"type": "tool", "name": "X"}`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolChoice {
    /// Model decides whether to call a tool (Anthropic default when tools are present).
    Auto,
    /// Model must call at least one of the provided tools (prevents hallucinated names).
    Any,
    /// Model must call exactly this named tool.
    Tool { name: String },
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
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
    pub fn text_cached(text: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: text.into(),
            cache_control: Some(CacheControl::ephemeral()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AnthropicMessage {
    pub role: String, // "user" | "assistant"
    pub content: AnthropicContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub strict: Option<bool>,
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
        #[allow(dead_code)] // wire-format mirror; not consumed
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
        #[allow(dead_code)] // wire-format mirror; not consumed
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    Thinking {
        #[allow(dead_code)] // wire-format mirror; not consumed
        thinking: String,
        #[serde(default)]
        #[allow(dead_code)] // wire-format mirror; not consumed
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
    #[allow(dead_code)] // wire-format mirror; not consumed
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
    #[allow(dead_code)] // wire-format mirror; not consumed
    pub kind: String,
    pub message: String,
}

// ── Conversion: gait_types::types::Message → AnthropicMessage ─────────────────────

use gait_types::types::{AssistantContent, AssistantPart, Message, MessageContent};

pub fn to_anthropic_messages(messages: &[Message]) -> Vec<AnthropicMessage> {
    let collected: Vec<AnthropicMessage> = messages
        .iter()
        .map(|m| match m {
            Message::User {
                content,
                attribution,
                ..
            } => AnthropicMessage {
                role: "user".into(),
                content: match content {
                    MessageContent::Text(t) => {
                        AnthropicContent::Text(gait_types::types::render_user_text(attribution, t))
                    }
                    MessageContent::Parts(parts) => {
                        let mut blocks: Vec<AnthropicBlock> = Vec::new();
                        if !attribution.is_empty() {
                            // Parts are already separate, structurally-distinct
                            // Anthropic content blocks (not concatenated at the
                            // text level), so the marker only needs to convey
                            // the bounded/escaped attribution itself -- an
                            // empty body is fine, the real text follows as its
                            // own block(s) below.
                            let marker =
                                gait_types::types::render_attributed_messages(attribution, &[String::new()])
                                    .unwrap_or_default();
                            blocks.push(AnthropicBlock::Text {
                                text: marker,
                                cache_control: None,
                            });
                        }
                        blocks.extend(parts.iter().map(|p| match p {
                            gait_types::types::ContentPart::Text { text } => AnthropicBlock::Text {
                                text: text.clone(),
                                cache_control: None,
                            },
                        }));
                        AnthropicContent::Blocks(blocks)
                    }
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
        .collect();
    // Merge consecutive messages with the same role. The Anthropic API rejects
    // adjacent same-role turns with HTTP 400. The most common trigger after
    // Part 2's live-turn poll: a ToolResult (role "user") immediately followed
    // by a daemon_update event folded to a User message (also role "user").
    // We merge by accumulating content blocks — purely structural, no text concat.
    merge_consecutive_same_role(collected)
}

/// Collapse runs of adjacent same-role messages into one by concatenating their
/// content blocks. A solitary Text message is preserved as `AnthropicContent::Text`
/// (wire-shape parity with the unmerged path); any merged group becomes `Blocks`.
fn merge_consecutive_same_role(msgs: Vec<AnthropicMessage>) -> Vec<AnthropicMessage> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(msgs.len());
    let mut idx = 0usize;
    while idx < msgs.len() {
        let cur = msgs[idx].clone();
        idx += 1;
        let mut combined = content_into_blocks(&cur.content);
        while idx < msgs.len() && msgs[idx].role == cur.role {
            combined.extend(content_into_blocks(&msgs[idx].content));
            idx += 1;
        }
        // Preserve Text shape for solo non-merged messages (unchanged wire output).
        let content = if combined.len() == 1
            && matches!(
                combined[0],
                AnthropicBlock::Text {
                    cache_control: None,
                    ..
                }
            )
            && matches!(cur.content, AnthropicContent::Text(_))
        {
            let AnthropicBlock::Text { text, .. } = combined.into_iter().next().unwrap() else {
                unreachable!()
            };
            AnthropicContent::Text(text)
        } else {
            AnthropicContent::Blocks(combined)
        };
        out.push(AnthropicMessage {
            role: cur.role,
            content,
        });
    }
    out
}

/// Lift `AnthropicContent` into a flat block list so `merge_consecutive_same_role`
/// can concatenate blocks from adjacent messages uniformly.
fn content_into_blocks(c: &AnthropicContent) -> Vec<AnthropicBlock> {
    match c {
        AnthropicContent::Text(s) => {
            vec![AnthropicBlock::Text {
                text: s.clone(),
                cache_control: None,
            }]
        }
        AnthropicContent::Blocks(v) => v.to_vec(),
    }
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
            tool_choice: None,
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
            tool_choice: None,
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
            strict: None,
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
            strict: None,
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
        use gait_types::types::{AssistantContent, AssistantPart, ToolCall};
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
        use gait_types::types::{ToolResult, ToolResultKind};
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

    #[test]
    fn tool_choice_any_serializes_correctly() {
        let v = serde_json::to_value(AnthropicToolChoice::Any).unwrap();
        assert_eq!(v, serde_json::json!({"type": "any"}));
    }

    #[test]
    fn tool_choice_auto_serializes_correctly() {
        let v = serde_json::to_value(AnthropicToolChoice::Auto).unwrap();
        assert_eq!(v, serde_json::json!({"type": "auto"}));
    }

    #[test]
    fn tool_choice_named_tool_serializes_correctly() {
        let v = serde_json::to_value(AnthropicToolChoice::Tool {
            name: "read_file".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({"type": "tool", "name": "read_file"}));
    }

    #[test]
    fn request_with_tool_choice_any_serializes_field() {
        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 1024,
            system: None,
            messages: vec![],
            tools: vec![],
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
            output_config: None,
            tool_choice: Some(AnthropicToolChoice::Any),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["tool_choice"], serde_json::json!({"type": "any"}));
    }

    #[test]
    fn request_without_tool_choice_omits_field() {
        let req = AnthropicRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 1024,
            system: None,
            messages: vec![],
            tools: vec![],
            stream: true,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking: None,
            output_config: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("tool_choice").is_none());
    }

    #[test]
    fn tool_result_followed_by_user_merges_into_one_message() {
        use gait_types::types::{
            AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind,
        };
        // Sequence: Assistant(tool_use) → ToolResult → User("injection")
        // Without same-role merge: ToolResult and User both emit role="user" → 400.
        // With merge: they collapse into one role="user" message with two blocks.
        let messages = vec![
            Message::Assistant {
                id: String::new(),
                content: AssistantContent::Parts(vec![AssistantPart::ToolCall(ToolCall {
                    tool_call_id: "tc1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                })]),
            },
            Message::ToolResult {
                id: String::new(),
                content: vec![ToolResult {
                    kind: ToolResultKind::ToolResult,
                    tool_call_id: "tc1".into(),
                    result: serde_json::json!("ok"),
                }],
            },
            Message::User {
                id: String::new(),
                content: gait_types::types::MessageContent::Text(
                    "[daemon:build] done — exit 0 — log: /tmp/build.log".into(),
                ),
                attribution: Vec::new(),
            },
        ];
        let converted = to_anthropic_messages(&messages);
        // Expect exactly 2 messages: one assistant, one user.
        assert_eq!(
            converted.len(),
            2,
            "expected 2 API messages (assistant + merged user), got {}: {converted:?}",
            converted.len()
        );
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        // The user message must be Blocks containing both the tool_result and the text.
        let AnthropicContent::Blocks(blocks) = &converted[1].content else {
            panic!(
                "expected Blocks for merged user message, got {:?}",
                converted[1].content
            );
        };
        assert_eq!(
            blocks.len(),
            2,
            "expected 2 blocks (tool_result + text), got {}",
            blocks.len()
        );
        assert!(
            matches!(blocks[0], AnthropicBlock::ToolResult { .. }),
            "first block should be ToolResult, got {:?}",
            blocks[0]
        );
        assert!(
            matches!(blocks[1], AnthropicBlock::Text { .. }),
            "second block should be Text, got {:?}",
            blocks[1]
        );
    }

    #[test]
    fn user_message_with_attribution_wraps_text_in_room_message_tag() {
        let attribution = vec![gait_types::types::Attribution {
            author_id: "agent-2".into(),
            context: "room-general".into(),
            trust: gait_types::types::Trust::Unverified,
        }];
        let messages = vec![Message::User {
            id: String::new(),
            content: MessageContent::Text("Hello from peer".into()),
            attribution: attribution.clone(),
        }];
        let converted = to_anthropic_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        if let AnthropicContent::Text(text) = &converted[0].content {
            assert_eq!(
                text,
                "<incoming-message from=\"agent-2\" context=\"room-general\" trust=\"unverified\">Hello from peer</incoming-message>\n"
            );
        } else {
            panic!("expected Text content, got {:?}", converted[0].content);
        }
    }

    #[test]
    fn user_message_with_attribution_inserts_marker_block_in_parts() {
        let attribution = vec![gait_types::types::Attribution {
            author_id: "agent-2".into(),
            context: "room-general".into(),
            trust: gait_types::types::Trust::Unverified,
        }];
        let messages = vec![Message::User {
            id: String::new(),
            content: MessageContent::Parts(vec![
                gait_types::types::ContentPart::Text {
                    text: "First part".into(),
                },
                gait_types::types::ContentPart::Text {
                    text: "Second part".into(),
                },
            ]),
            attribution: attribution.clone(),
        }];
        let converted = to_anthropic_messages(&messages);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        if let AnthropicContent::Blocks(blocks) = &converted[0].content {
            assert_eq!(
                blocks.len(),
                3,
                "expected 3 blocks (attribution marker + 2 parts), got {}",
                blocks.len()
            );
            // First block should be the attribution marker
            if let AnthropicBlock::Text {
                text,
                cache_control,
            } = &blocks[0]
            {
                assert!(
                    text.contains("from=\"agent-2\""),
                    "first block should contain the attribution marker, got: {text:?}"
                );
                assert!(
                    cache_control.is_none(),
                    "cache_control should be None, got {:?}",
                    cache_control
                );
            } else {
                panic!("first block should be Text, got {:?}", blocks[0]);
            }
            // Second and third blocks should be the original parts
            if let AnthropicBlock::Text { text, .. } = &blocks[1] {
                assert_eq!(text, "First part");
            } else {
                panic!("second block should be Text, got {:?}", blocks[1]);
            }
            if let AnthropicBlock::Text { text, .. } = &blocks[2] {
                assert_eq!(text, "Second part");
            } else {
                panic!("third block should be Text, got {:?}", blocks[2]);
            }
        } else {
            panic!("expected Blocks content, got {:?}", converted[0].content);
        }
    }
}
