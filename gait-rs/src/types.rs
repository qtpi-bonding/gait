use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Mirrors Vercel AI SDK's CoreMessage union
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    User { content: MessageContent },
    Assistant { content: AssistantContent },
    Tool { content: Vec<ToolResult> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContent {
    Text(String),
    Parts(Vec<AssistantPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AssistantPart {
    Text { text: String },
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(rename = "type")]
    pub kind: ToolResultKind,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolResultKind {
    ToolResult,
}

// Tool definition — description + JSON schema + async executor
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema object
    pub execute: Box<dyn Fn(serde_json::Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>> + Send + Sync>,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl Usage {
    pub fn add(&self, other: &Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
        }
    }
}

// Emitted per-step by stream_text, mirrors onStepFinish payload
#[derive(Debug, Clone)]
pub struct StepFinish {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_message_roundtrip() {
        let msg = Message::User { content: MessageContent::Text("hello".into()) };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        if let Message::User { content: MessageContent::Text(s) } = back {
            assert_eq!(s, "hello");
        } else {
            panic!("roundtrip failed");
        }
    }

    #[test]
    fn assistant_with_tool_call_roundtrip() {
        let msg = Message::Assistant {
            content: AssistantContent::Parts(vec![
                AssistantPart::ToolCall(ToolCall {
                    tool_call_id: "id1".into(),
                    tool_name: "read".into(),
                    args: serde_json::json!({ "path": "/foo" }),
                }),
            ]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = back {
            assert_eq!(parts.len(), 1);
        } else {
            panic!("roundtrip failed");
        }
    }

    #[test]
    fn usage_add() {
        let a = Usage { input_tokens: 10, output_tokens: 20 };
        let b = Usage { input_tokens: 5, output_tokens: 3 };
        let c = a.add(&b);
        assert_eq!(c.input_tokens, 15);
        assert_eq!(c.output_tokens, 23);
    }
}
