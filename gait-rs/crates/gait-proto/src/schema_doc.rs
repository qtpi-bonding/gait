use prost_reflect::{Cardinality, DescriptorPool, Kind};
use std::sync::OnceLock;

static DESCRIPTOR_POOL: OnceLock<DescriptorPool> = OnceLock::new();

fn pool() -> &'static DescriptorPool {
    DESCRIPTOR_POOL
        .get_or_init(|| DescriptorPool::decode(crate::DESCRIPTOR).expect("embedded descriptor"))
}

/// The source-level cardinality of a protobuf field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Optional,
    Repeated,
    Singular,
}

/// Documentation and type information for one protobuf field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDoc {
    pub name: String,
    pub ty: String,
    pub label: Label,
    pub oneof: Option<String>,
    pub doc: String,
}

/// Documentation and fields for one protobuf message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDoc {
    pub full_name: String,
    pub doc: String,
    pub fields: Vec<FieldDoc>,
}

fn comment(file: &prost_types::FileDescriptorProto, path: &[i32], trailing: bool) -> String {
    let comments = file
        .source_code_info
        .as_ref()
        .and_then(|info| info.location.iter().find(|location| location.path == path))
        .and_then(|location| {
            if trailing {
                location
                    .trailing_comments
                    .as_deref()
                    .or(location.leading_comments.as_deref())
            } else {
                location.leading_comments.as_deref()
            }
        })
        .unwrap_or("");
    comments
        .lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn scalar_name(kind: &Kind) -> Option<&'static str> {
    Some(match kind {
        Kind::Double => "double",
        Kind::Float => "float",
        Kind::Int32 => "int32",
        Kind::Int64 => "int64",
        Kind::Uint32 => "uint32",
        Kind::Uint64 => "uint64",
        Kind::Sint32 => "sint32",
        Kind::Sint64 => "sint64",
        Kind::Fixed32 => "fixed32",
        Kind::Fixed64 => "fixed64",
        Kind::Sfixed32 => "sfixed32",
        Kind::Sfixed64 => "sfixed64",
        Kind::Bool => "bool",
        Kind::String => "string",
        Kind::Bytes => "bytes",
        Kind::Message(_) | Kind::Enum(_) => return None,
    })
}

fn field_type(field: &prost_reflect::FieldDescriptor) -> String {
    let kind = field.kind();
    scalar_name(&kind)
        .map(str::to_owned)
        .unwrap_or_else(|| match kind {
            Kind::Message(message) => message.name().to_owned(),
            Kind::Enum(enumeration) => enumeration.name().to_owned(),
            _ => unreachable!(),
        })
}

/// Describe a message from the embedded descriptor set, including its comments.
pub fn message_doc(full_name: &str) -> Option<MessageDoc> {
    let message = pool().get_message_by_name(full_name)?;
    let file = message.parent_file_descriptor_proto();
    let message_path = message.path();
    let doc = comment(file, message_path, false);

    let mut fields = message
        .descriptor_proto()
        .field
        .iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let field = message.get_field_by_name(raw.name())?;
            let label = if raw.proto3_optional() {
                Label::Optional
            } else if field.cardinality() == Cardinality::Repeated {
                Label::Repeated
            } else {
                Label::Singular
            };
            let oneof = if raw.proto3_optional() {
                None
            } else {
                field
                    .containing_oneof()
                    .map(|oneof| oneof.name().to_owned())
            };
            let mut path = message_path.to_vec();
            path.extend([2, index as i32]);
            Some(FieldDoc {
                name: raw.name().to_owned(),
                ty: field_type(&field),
                label,
                oneof,
                doc: comment(file, &path, true),
            })
        })
        .collect::<Vec<_>>();
    fields.shrink_to_fit();

    Some(MessageDoc {
        full_name: message.full_name().to_owned(),
        doc,
        fields,
    })
}

/// Return enum value names in their declaration order.
pub fn enum_values(full_name: &str) -> Option<Vec<String>> {
    let enumeration = pool().get_enum_by_name(full_name)?;
    Some(
        enumeration
            .enum_descriptor_proto()
            .value
            .iter()
            .map(|value| value.name().to_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_inference() {
        let doc = message_doc("gait.log.v1.Inference").unwrap();
        assert_eq!(
            doc.fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["name", "model", "llm_params", "compaction_threshold_tokens"]
        );
        assert_eq!(doc.fields[1].doc, "provider/model-id to invoke");
        assert_eq!(doc.fields[3].doc, "token budget before compaction");
        assert_eq!(doc.fields[2].label, Label::Optional);
        assert_eq!(doc.fields[2].ty, "LlmParams");
    }

    #[test]
    fn documents_oneofs_repeated_fields_and_enum() {
        let doc = message_doc("gait.log.v1.LlmParams").unwrap();
        for name in ["anthropic", "openai", "gemini", "local"] {
            assert_eq!(
                doc.fields
                    .iter()
                    .find(|field| field.name == name)
                    .unwrap()
                    .oneof
                    .as_deref(),
                Some("provider_params")
            );
        }
        let max_tokens = doc
            .fields
            .iter()
            .find(|field| field.name == "max_tokens")
            .unwrap();
        assert_eq!(max_tokens.oneof, None);
        assert_eq!(max_tokens.label, Label::Optional);

        let mode = message_doc("gait.log.v1.Mode").unwrap();
        assert!(mode.doc.starts_with("A permission-and-steering overlay"));
        assert_eq!(
            mode.fields
                .iter()
                .find(|field| field.name == "allow")
                .unwrap()
                .label,
            Label::Repeated
        );
        assert_eq!(
            message_doc("gait.log.v1.OpenAILlmParams")
                .unwrap()
                .fields
                .iter()
                .find(|field| field.name == "stop")
                .unwrap()
                .label,
            Label::Repeated
        );

        let thinking = message_doc("gait.log.v1.AnthropicThinking").unwrap();
        assert_eq!(thinking.fields[0].ty, "AnthropicThinkingType");
        let values = enum_values("gait.log.v1.AnthropicThinkingType").unwrap();
        assert_eq!(values[1], "ANTHROPIC_THINKING_TYPE_ADAPTIVE");
    }

    #[test]
    fn unknown_message_is_absent() {
        assert!(message_doc("nope").is_none());
    }
}
