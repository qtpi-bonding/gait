use std::path::PathBuf;
use crate::types::{Tool, ExecutionMode};
use crate::tools::ToolCtx;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;


#[derive(Deserialize, JsonSchema)]
struct GlobParams {
    /// The glob pattern to match files against (e.g. '**/*.ts', 'src/**/*.json')
    pattern: String,
    /// Directory to search in (relative to codebase root). Omit to search entire codebase.
    path: Option<String>,
}

pub struct GlobTool {
    ctx: ToolCtx,
    parameters: Value,
}

impl GlobTool {
    pub fn new(ctx: ToolCtx) -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<GlobParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { ctx, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str {
        "Find files in the codebase matching a glob pattern. Results sorted by modification time (newest first)."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Parse
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: GlobParams = serde_json::from_value(args)?;

        let search_dir = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.codebase_path.join(p),
            None => self.ctx.codebase_path.clone(),
        };

        if !search_dir.exists() {
            anyhow::bail!("Directory not found: {}", search_dir.display());
        }

        let abs_pattern = format!("{}/{}", search_dir.display(), params.pattern);

        let mut files: Vec<(PathBuf, Option<std::time::SystemTime>)> = vec![];

        let glob_results = ::glob::glob(&abs_pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?;

        for entry in glob_results {
            match entry {
                Ok(path) if path.is_file() => {
                    let mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                    files.push((path, mtime));
                }
                _ => {}
            }
        }

        if files.is_empty() { return Ok(Value::String("No files found".into())); }

        files.sort_by(|a, b| b.1.cmp(&a.1));

        let lines: Vec<String> = files.iter()
            .map(|(p, _)| p.display().to_string())
            .collect();

        Ok(Value::String(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::path::PathBuf;
    use crate::tools::ToolCtx;

    fn make_ctx(dir: &TempDir) -> ToolCtx {
        ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "test".into())
    }

    #[tokio::test]
    async fn finds_files_by_extension() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.ts"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();

        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({ "pattern": "*.rs" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("a.rs"), "got: {text}");
        assert!(text.contains("c.rs"), "got: {text}");
        assert!(!text.contains("b.ts"), "got: {text}");
    }

    #[tokio::test]
    async fn finds_nested_files_with_globstar() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({ "pattern": "**/*.rs" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {text}");
        assert!(!text.contains("README.md"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_empty() {
        let dir = TempDir::new().unwrap();
        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({ "pattern": "*.xyz" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn searches_in_subdir_when_path_given() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.rs"), "").unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();

        let tool = GlobTool::new(make_ctx(&dir));
        let result = tool.execute(serde_json::json!({
            "pattern": "*.rs",
            "path": "sub"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("deep.rs"), "got: {text}");
        assert!(!text.contains("root.rs"), "got: {text}");
    }

    #[test]
    fn schema_has_parse_array_field() {
        let tool = GlobTool::new(ToolCtx::new(PathBuf::from("/tmp"), PathBuf::from("/tmp"), "test".into()));
        let params = tool.parameters();
        assert_eq!(params["properties"]["parse"]["type"].as_str().unwrap(), "array",
            "parse should be array type: {}", params);
    }
}
