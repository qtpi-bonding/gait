use crate::provider::{Provider, ProviderError};
use crate::types::{Message, Usage};
use crate::stream::stream_text;

pub struct GenerateResult {
    pub text: String,
    pub usage: Usage,
}

pub async fn generate_text(
    provider: &dyn Provider,
    system: &str,
    messages: &[Message],
) -> Result<GenerateResult, ProviderError> {
    let result = stream_text(provider, system, messages, &[]).await?;
    Ok(GenerateResult { text: result.text, usage: result.usage })
}
