use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const GLOB_LIMIT: usize = 100;

#[derive(Deserialize, JsonSchema)]
struct GlobParams {
    /// The glob pattern to match files against (e.g. '**/*.ts', 'src/**/*.json')
    pattern: String,
    /// Directory to search in (relative to codebase root). Omit to search entire codebase.
    path: Option<String>,
}

pub struct GlobTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl GlobTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<GlobParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "glob" }
    fn description(&self) -> &str {
        "Find files in the codebase matching a glob pattern. Results sorted by modification time (newest first)."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: GlobParams = serde_json::from_value(args)?;

        let search_dir = match &params.path {
            Some(p) if std::path::Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.codebase_path.join(p),
            None => (*self.codebase_path).clone(),
        };

        if !search_dir.exists() {
            anyhow::bail!("Directory not found: {}", search_dir.display());
        }

        let abs_pattern = format!("{}/{}", search_dir.display(), params.pattern);

        let mut files: Vec<(PathBuf, Option<std::time::SystemTime>)> = vec![];
        let mut truncated = false;

        let glob_results = ::glob::glob(&abs_pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?;

        for entry in glob_results {
            match entry {
                Ok(path) if path.is_file() => {
                    if files.len() >= GLOB_LIMIT { truncated = true; break; }
                    let mtime = path.metadata().ok().and_then(|m| m.modified().ok());
                    files.push((path, mtime));
                }
                _ => {}
            }
        }

        if files.is_empty() { return Ok(Value::String("No files found".into())); }

        files.sort_by(|a, b| b.1.cmp(&a.1));

        let mut lines: Vec<String> = files.iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        if truncated {
            lines.push(String::new());
            lines.push("(Results are truncated. Consider using a more specific path or pattern.)".into());
        }

        Ok(Value::String(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn finds_files_by_extension() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.ts"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();

        let tool = GlobTool::new(dir.path().to_path_buf());
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

        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "**/*.rs" })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("main.rs"), "got: {text}");
        assert!(!text.contains("README.md"), "got: {text}");
    }

    #[tokio::test]
    async fn returns_no_files_found_when_empty() {
        let dir = TempDir::new().unwrap();
        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({ "pattern": "*.xyz" })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "No files found");
    }

    #[tokio::test]
    async fn searches_in_subdir_when_path_given() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.rs"), "").unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();

        let tool = GlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "pattern": "*.rs",
            "path": "sub"
        })).await.unwrap();
        let text = result.as_str().unwrap();
        assert!(text.contains("deep.rs"), "got: {text}");
        assert!(!text.contains("root.rs"), "got: {text}");
    }
}
