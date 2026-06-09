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
pub mod curate;

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
pub use curate::CurateTool;

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
        SpawnAgentTool::new(session_id.clone(), workspace.clone(), codebase_path.clone()).boxed(),
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
        );
        assert_eq!(tools.len(), 15);
    }

    #[test]
    fn all_tool_names_unique() {
        let tools = builtin_tools(
            PathBuf::from("/tmp"), "stest".into(), PathBuf::from("/tmp"), vec![], vec![],
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
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"spawn_agent"), "missing spawn_agent: {:?}", names);
        assert!(names.contains(&"list_agents"), "missing list_agents: {:?}", names);
    }

    #[test]
    fn load_meta_tool_present() {
        let tools = builtin_tools(
            PathBuf::from("/tmp"), "stest".into(), PathBuf::from("/tmp"), vec![], vec![],
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"load"), "missing load: {:?}", names);
    }
}
