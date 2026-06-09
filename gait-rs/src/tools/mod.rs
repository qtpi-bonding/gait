pub mod read;
pub mod write;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod shell;
pub mod output;
pub mod codesearch;
pub mod webfetch;
pub mod load;
pub mod spawn_agent;
pub mod list_agents;
pub mod clarify;
pub mod inform;
pub mod annotate;

pub use read::ReadTool;
pub use write::WriteTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use shell::ShellTool;
pub use output::OutputTool;
pub use codesearch::CodesearchTool;
pub use webfetch::WebfetchTool;
pub use load::LoadTool;
pub use spawn_agent::SpawnAgentTool;
pub use list_agents::ListAgentsTool;
pub use clarify::ClarifyTool;
pub use inform::InformTool;
pub use annotate::AnnotateTool;

use std::path::PathBuf;
use crate::types::Tool;
use crate::config::{McpToolDef, SkillDef};

/// Generate a JSON Schema for type T and strip the `$schema` and `title` meta-keys
/// that schemars adds by default. Anthropic's `input_schema` field accepts a plain
/// JSON Schema object — the extra meta-keys cause 400 errors against the live API.
pub fn make_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(T);
    let mut v = serde_json::to_value(&schema).expect("schema serialization");
    if let Some(obj) = v.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
    }
    v
}

/// Inject an optional filter field into an existing JSON Schema `properties` map.
/// `Parse` adds a structured `parse` array field (rg/sd pipeline, applied in-process).
/// `Curate` adds a `curate_prompt` string field (LLM annotation subagent).
/// `None` returns the schema unchanged.
pub fn inject_filter(mut schema: serde_json::Value, filter: crate::types::OutputFilter) -> serde_json::Value {
    use crate::types::OutputFilter;
    match filter {
        OutputFilter::None => return schema,
        OutputFilter::Parse => {
            if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                props.insert("parse".into(), serde_json::json!({
                    "type": "array",
                    "description": "Optional pipeline of filter operations applied in-process before \
                                    the result reaches the model. Operations run in order. \
                                    {\"rg\": \"<pattern>\"} keeps only lines matching the regex \
                                    (linear-time finite automaton, no backtracking, no ReDoS). \
                                    {\"sd\": [\"<from>\", \"<to>\"]} replaces all regex matches of \
                                    <from> with <to> (multiline mode).",
                    "items": {
                        "type": "object",
                        "description": "One operation: {\"rg\": \"pattern\"} or {\"sd\": [\"from\", \"to\"]}"
                    }
                }));
            }
        }
        OutputFilter::Curate => {
            if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                props.insert("curate_prompt".into(), serde_json::json!({
                    "type": "string",
                    "description": "Route this tool's output through a 1-turn annotation subagent \
                                    before the model sees it. Describe what counts as relevant. \
                                    Returns {relevant: [...], irrelevant: [...]}."
                }));
            }
        }
    }
    schema
}

/// Names of all built-in tools. Used for allow-list validation without constructing tool instances.
pub fn builtin_tool_names() -> &'static [&'static str] {
    &["read", "write", "edit", "glob", "grep", "ls", "shell", "output",
      "codesearch", "webfetch", "load", "spawn_agent", "list_agents", "clarify", "inform"]
}

/// Returns all built-in tools.
pub fn builtin_tools(
    codebase_path: PathBuf,
    session_id: String,
    workspace: PathBuf,
    mcp_tools: Vec<McpToolDef>,
    skills: Vec<SkillDef>,
    repos: crate::git::Repos,
) -> Vec<Box<dyn Tool>> {
    vec![
        ReadTool::new(codebase_path.clone()).boxed(),
        WriteTool::new(codebase_path.clone()).boxed(),
        EditTool::new(codebase_path.clone()).boxed(),
        GlobTool::new(codebase_path.clone()).boxed(),
        GrepTool::new(codebase_path.clone()).boxed(),
        LsTool::new(codebase_path.clone()).boxed(),
        ShellTool::new(codebase_path.clone(), workspace.clone()).boxed(),
        OutputTool::new().boxed(),
        CodesearchTool::new(codebase_path.clone()).boxed(),
        WebfetchTool::new().boxed(),
        LoadTool::new(mcp_tools, skills).boxed(),
        SpawnAgentTool::new(session_id.clone(), workspace.clone(), codebase_path.clone(), repos.clone()).boxed(),
        ListAgentsTool::new(workspace.clone()).boxed(),
        ClarifyTool::new().boxed(),
        InformTool::new().boxed(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builtin_tools_returns_fifteen() {
        let tools = builtin_tools(
            PathBuf::from("/tmp"),
            "stest".into(),
            PathBuf::from("/tmp"),
            vec![], vec![],
            crate::git::Repos::production(),
        );
        assert_eq!(tools.len(), 15);
    }

    #[test]
    fn all_tool_names_unique() {
        let tools = builtin_tools(
            PathBuf::from("/tmp"), "stest".into(), PathBuf::from("/tmp"), vec![], vec![],
            crate::git::Repos::production(),
        );
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate tool names found");
    }

    #[test]
    fn spawn_agent_and_list_agents_present() {
        let tools = builtin_tools(
            PathBuf::from("/tmp"), "stest".into(), PathBuf::from("/tmp"), vec![], vec![],
            crate::git::Repos::production(),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"spawn_agent"), "missing spawn_agent: {:?}", names);
        assert!(names.contains(&"list_agents"), "missing list_agents: {:?}", names);
    }

    #[test]
    fn load_meta_tool_present() {
        let tools = builtin_tools(
            PathBuf::from("/tmp"), "stest".into(), PathBuf::from("/tmp"), vec![], vec![],
            crate::git::Repos::production(),
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"load"), "missing load: {:?}", names);
    }

    #[test]
    fn inject_filter_parse_adds_array_field() {
        use crate::types::OutputFilter;
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "cmd": { "type": "string" } }
        });
        let result = inject_filter(schema, OutputFilter::Parse);
        let parse_field = &result["properties"]["parse"];
        assert!(parse_field.is_object(), "parse field missing: {result}");
        assert_eq!(parse_field["type"].as_str().unwrap(), "array");
    }

    #[test]
    fn inject_filter_curate_adds_curate_prompt_field() {
        use crate::types::OutputFilter;
        let schema = serde_json::json!({ "type": "object", "properties": {} });
        let result = inject_filter(schema, OutputFilter::Curate);
        assert!(result["properties"]["curate_prompt"].is_object());
        assert_eq!(result["properties"]["curate_prompt"]["type"].as_str().unwrap(), "string");
    }

    #[test]
    fn inject_filter_none_is_noop() {
        use crate::types::OutputFilter;
        let schema = serde_json::json!({ "type": "object", "properties": {} });
        let result = inject_filter(schema, OutputFilter::None);
        let props = result["properties"].as_object().unwrap();
        assert!(!props.contains_key("parse"));
        assert!(!props.contains_key("curate_prompt"));
    }
}
