pub mod read;
pub mod write;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod shell;

pub use read::ReadTool;
pub use write::WriteTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use shell::ShellTool;

use std::path::PathBuf;
use crate::types::Tool;

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

/// Returns all seven built-in tools configured for the given codebase root path.
pub fn builtin_tools(codebase_path: PathBuf) -> Vec<Box<dyn Tool>> {
    vec![
        ReadTool::new(codebase_path.clone()).boxed(),
        WriteTool::new(codebase_path.clone()).boxed(),
        EditTool::new(codebase_path.clone()).boxed(),
        GlobTool::new(codebase_path.clone()).boxed(),
        GrepTool::new(codebase_path.clone()).boxed(),
        LsTool::new(codebase_path.clone()).boxed(),
        ShellTool::new(codebase_path).boxed(),
    ]
}
