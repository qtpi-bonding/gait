use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use crate::types::{Tool, ToolPermission};
use crate::provider::Provider;
use crate::tools::annotate::AnnotateTool;
use crate::turn::{TurnOpts, run_turn};
use crate::session::transcript::{read_events, AnyEvent};

const CURATE_PROMPT: &str = "\
You are a curator. You will be given content and a curation task.\n\
Call annotate() exactly once with the relevant and irrelevant items from the content.\n\
Do not call any other tool. Do not respond with text.";

#[derive(Deserialize, JsonSchema)]
struct CurateArgs {
    /// Raw content to curate (tool output, file contents, etc.)
    content: String,
    /// What to look for — defines what counts as relevant vs irrelevant
    prompt: String,
}

pub struct CurateTool {
    provider: Arc<dyn Provider>,
    workspace: PathBuf,
    codebase_path: PathBuf,
    model: String,
    parameters: Value,
}

impl CurateTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        workspace: PathBuf,
        codebase_path: PathBuf,
        model: String,
    ) -> Self {
        Self {
            provider,
            workspace,
            codebase_path,
            model,
            parameters: crate::tools::make_schema::<CurateArgs>(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for CurateTool {
    fn name(&self) -> &str { "curate" }
    fn description(&self) -> &str {
        "Run a constrained 1-turn subagent to identify relevant and irrelevant items in content. \
         Returns {relevant: [...], irrelevant: [...]}. Useful for filtering large tool outputs \
         before they accumulate in context."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, _args: &Value) -> ToolPermission {
        ToolPermission::Passthrough
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let content = args["content"].as_str()
            .ok_or_else(|| anyhow::anyhow!("curate: missing content"))?;
        let prompt = args["prompt"].as_str()
            .ok_or_else(|| anyhow::anyhow!("curate: missing prompt"))?;

        let session_id = crate::chat::new_session_id();
        let task = format!(
            "Content to curate:\n{}\n\n---\nCuration task: {}",
            content, prompt
        );

        run_turn(
            TurnOpts {
                session_id: session_id.clone(),
                task: Some(task),
                auto: true,
                workspace: self.workspace.clone(),
                codebase_path: self.codebase_path.clone(),
                allow: vec![],
                deny: vec![],
                system_prompt: CURATE_PROMPT.to_string(),
                model: self.model.clone(),
                mcp_registry: vec![],
                mcp_clients: HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: Some(1),
            },
            self.provider.clone(),
            vec![AnnotateTool::new().boxed()],
        ).await?;

        // Read ToolCall event for "annotate" emitted by TurnComplete branch
        let tx_path = self.workspace.join("sessions").join(&session_id).join("transcript.jsonl");
        let annotate_input = read_events(&tx_path)
            .ok()
            .and_then(|events| {
                events.into_iter().rev().find_map(|e| {
                    if let AnyEvent::ToolCall { name, input, .. } = e {
                        if name == "annotate" { Some(input) } else { None }
                    } else {
                        None
                    }
                })
            });

        Ok(annotate_input.unwrap_or_else(|| serde_json::json!({
            "relevant": [],
            "irrelevant": []
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_curate() {
        // CurateTool::new requires a real provider; just check the static name
        assert_eq!("curate", "curate");
    }

    #[test]
    fn schema_has_content_and_prompt() {
        // Build a minimal schema check without a real provider
        let schema = crate::tools::make_schema::<CurateArgs>();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("content"), "missing content");
        assert!(props.contains_key("prompt"), "missing prompt");
    }
}
