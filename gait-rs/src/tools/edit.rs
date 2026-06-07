use std::path::PathBuf;
use std::sync::Arc;
use crate::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("oldString not found in content")]
    NotFound,
    #[error("Found multiple matches for oldString. Provide more surrounding lines to identify the correct match.")]
    MultipleMatches,
    #[error("oldString and newString must be different")]
    SameStrings,
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 { return n; }
    if n == 0 { return m; }
    let mut matrix = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..=m { matrix[i][0] = i; }
    for j in 0..=n { matrix[0][j] = j; }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if a[i-1] == b[j-1] { 0 } else { 1 };
            matrix[i][j] = (matrix[i-1][j] + 1)
                .min(matrix[i][j-1] + 1)
                .min(matrix[i-1][j-1] + cost);
        }
    }
    matrix[m][n]
}

type Replacer = fn(&str, &str) -> Vec<String>;

fn simple_replacer(_content: &str, find: &str) -> Vec<String> {
    vec![find.to_string()]
}

fn line_trimmed_replacer(content: &str, find: &str) -> Vec<String> {
    let orig: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.last() == Some(&"") { search.pop(); }
    let slen = search.len();
    let mut results = vec![];
    for i in 0..=orig.len().saturating_sub(slen) {
        let matches = (0..slen).all(|j| orig[i+j].trim() == search[j].trim());
        if matches {
            let start: usize = (0..i).map(|k| orig[k].len() + 1).sum();
            let end = start + (0..slen).map(|k| orig[i+k].len() + if k < slen-1 { 1 } else { 0 }).sum::<usize>();
            results.push(content[start..end].to_string());
        }
    }
    results
}

fn block_anchor_replacer(content: &str, find: &str) -> Vec<String> {
    let orig: Vec<&str> = content.split('\n').collect();
    let mut search: Vec<&str> = find.split('\n').collect();
    if search.len() < 3 { return vec![]; }
    if search.last() == Some(&"") { search.pop(); }
    let slen = search.len();
    let first = search[0].trim();
    let last  = search[slen - 1].trim();

    let mut candidates: Vec<(usize, usize)> = vec![];
    for i in 0..orig.len() {
        if orig[i].trim() != first { continue; }
        for j in (i+2)..orig.len() {
            if orig[j].trim() == last { candidates.push((i, j)); break; }
        }
    }
    if candidates.is_empty() { return vec![]; }

    let compute_match = |si: usize, ei: usize| -> f64 {
        let lines_to_check = (slen - 2).min(ei - si - 1);
        if lines_to_check == 0 { return 1.0; }
        let mut sim = 0.0f64;
        for j in 1..slen-1 {
            if si + j >= ei { break; }
            let o = orig[si + j].trim();
            let s = search[j].trim();
            let max_len = o.len().max(s.len());
            if max_len == 0 { continue; }
            sim += (1.0 - levenshtein(o, s) as f64 / max_len as f64) / lines_to_check as f64;
        }
        sim
    };

    let extract = |si: usize, ei: usize| -> String {
        let start: usize = (0..si).map(|k| orig[k].len() + 1).sum();
        let end = start + (si..=ei).enumerate().map(|(idx, k)| orig[k].len() + if idx < ei - si { 1 } else { 0 }).sum::<usize>();
        content[start..end].to_string()
    };

    if candidates.len() == 1 {
        let (si, ei) = candidates[0];
        vec![extract(si, ei)]
    } else {
        let best = candidates.iter()
            .map(|&(si, ei)| (compute_match(si, ei), si, ei))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        match best {
            Some((sim, si, ei)) if sim >= 0.3 => vec![extract(si, ei)],
            _ => vec![],
        }
    }
}

fn whitespace_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let normalize = |t: &str| -> String {
        t.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let norm_find = normalize(find);
    let mut results = vec![];
    for line in content.split('\n') {
        if normalize(line) == norm_find {
            results.push(line.to_string());
        }
    }
    let find_lines: Vec<&str> = find.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    if find_lines.len() > 1 {
        for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
            let block = orig_lines[i..i+find_lines.len()].join("\n");
            if normalize(&block) == norm_find {
                results.push(block);
            }
        }
    }
    results
}

fn indentation_flexible_replacer(content: &str, find: &str) -> Vec<String> {
    let remove_indent = |text: &str| -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let non_empty: Vec<&str> = lines.iter().filter(|l| !l.trim().is_empty()).copied().collect();
        if non_empty.is_empty() { return text.to_string(); }
        let min_indent = non_empty.iter()
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        lines.iter().map(|l| {
            if l.trim().is_empty() { l.to_string() } else { l[min_indent..].to_string() }
        }).collect::<Vec<_>>().join("\n")
    };
    let norm_find = remove_indent(find);
    let orig_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    let mut results = vec![];
    for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
        let block = orig_lines[i..i+find_lines.len()].join("\n");
        if remove_indent(&block) == norm_find {
            results.push(block);
        }
    }
    results
}

fn escape_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let unescape = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n')  => out.push('\n'),
                    Some('t')  => out.push('\t'),
                    Some('r')  => out.push('\r'),
                    Some(c)    => out.push(c),
                    None       => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let unescaped = unescape(find);
    if unescaped == find { return vec![]; }
    let mut results = vec![];
    if content.contains(&unescaped as &str) {
        results.push(unescaped.clone());
    }
    let find_lines: Vec<&str> = unescaped.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
        let block = orig_lines[i..i+find_lines.len()].join("\n");
        if unescape(&block) == unescaped && !results.contains(&block) {
            results.push(block);
        }
    }
    results
}

fn trimmed_boundary_replacer(content: &str, find: &str) -> Vec<String> {
    let trimmed = find.trim();
    if trimmed == find { return vec![]; }
    let mut results = vec![];
    if content.contains(trimmed) {
        results.push(trimmed.to_string());
    }
    let find_lines: Vec<&str> = find.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    for i in 0..=orig_lines.len().saturating_sub(find_lines.len()) {
        let block = orig_lines[i..i+find_lines.len()].join("\n");
        if block.trim() == trimmed && !results.contains(&block) {
            results.push(block);
        }
    }
    results
}

fn context_aware_replacer(content: &str, find: &str) -> Vec<String> {
    let orig: Vec<&str> = content.split('\n').collect();
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 { return vec![]; }
    if find_lines.last() == Some(&"") { find_lines.pop(); }
    let flen = find_lines.len();
    let first = find_lines[0].trim();
    let last  = find_lines[flen - 1].trim();
    let mut results = vec![];

    for i in 0..orig.len() {
        if orig[i].trim() != first { continue; }
        for j in (i+2)..orig.len() {
            if orig[j].trim() != last { continue; }
            let block = &orig[i..=j];
            if block.len() == flen {
                let (matching, total) = (1..flen-1).fold((0usize, 0usize), |(m, t), k| {
                    let bl = block[k].trim();
                    let fl = find_lines[k].trim();
                    if bl.is_empty() && fl.is_empty() { (m, t) }
                    else { (m + if bl == fl { 1 } else { 0 }, t + 1) }
                });
                if total == 0 || matching * 2 >= total {
                    results.push(block.join("\n"));
                }
            }
            break;
        }
    }
    results
}

fn multi_occurrence_replacer(content: &str, find: &str) -> Vec<String> {
    let mut results = vec![];
    let mut start = 0;
    while let Some(idx) = content[start..].find(find) {
        results.push(find.to_string());
        start += idx + find.len();
    }
    results
}

/// Replace `old_str` with `new_str` in `content`. Tries 9 matching strategies from exact to fuzzy.
/// Returns `Err(EditError::NotFound)` if no match, `Err(EditError::MultipleMatches)` if ambiguous.
pub fn replace(content: &str, old_str: &str, new_str: &str, replace_all: bool) -> Result<String, EditError> {
    if old_str == new_str { return Err(EditError::SameStrings); }

    const REPLACERS: &[Replacer] = &[
        simple_replacer,
        line_trimmed_replacer,
        block_anchor_replacer,
        whitespace_normalized_replacer,
        indentation_flexible_replacer,
        escape_normalized_replacer,
        trimmed_boundary_replacer,
        context_aware_replacer,
        multi_occurrence_replacer,
    ];

    let mut not_found = true;

    for replacer in REPLACERS {
        for search in replacer(content, old_str) {
            let Some(first) = content.find(&search as &str) else { continue };
            not_found = false;

            if replace_all {
                return Ok(content.replace(&search as &str, new_str));
            }

            let last = content.rfind(&search as &str).unwrap();
            if first != last { continue; }

            let mut result = String::with_capacity(content.len() + new_str.len());
            result.push_str(&content[..first]);
            result.push_str(new_str);
            result.push_str(&content[first + search.len()..]);
            return Ok(result);
        }
    }

    if not_found { Err(EditError::NotFound) } else { Err(EditError::MultipleMatches) }
}

#[derive(Deserialize, JsonSchema)]
struct EditParams {
    /// Path to the file to modify (relative to codebase root, or absolute)
    file_path: String,
    /// The text to replace (must exist exactly once in the file, or use replace_all)
    old_string: String,
    /// The replacement text
    new_string: String,
    /// Replace all occurrences of old_string (default false)
    replace_all: Option<bool>,
}

pub struct EditTool {
    codebase_path: Arc<PathBuf>,
    parameters: Value,
}

impl EditTool {
    pub fn new(codebase_path: PathBuf) -> Self {
        let parameters = crate::tools::make_schema::<EditParams>();
        Self { codebase_path: Arc::new(codebase_path), parameters }
    }
}

#[async_trait::async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn description(&self) -> &str {
        "Edit a file by replacing one exact string with another. The old_string must uniquely identify the location to change."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: EditParams = serde_json::from_value(args)?;
        let replace_all = params.replace_all.unwrap_or(false);

        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.codebase_path.join(&params.file_path)
        };

        if params.old_string.is_empty() {
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&resolved, &params.new_string)?;
            return Ok(Value::String(format!("Created {} with {} bytes",
                resolved.display(), params.new_string.len())));
        }

        if params.old_string == params.new_string {
            anyhow::bail!("old_string and new_string must be different");
        }
        if !resolved.exists() {
            anyhow::bail!("File not found: {}", resolved.display());
        }
        if resolved.is_dir() {
            anyhow::bail!("Path is a directory, not a file: {}", resolved.display());
        }

        let old_content = std::fs::read_to_string(&resolved)?;
        let new_content = replace(&old_content, &params.old_string, &params.new_string, replace_all)
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        std::fs::write(&resolved, &new_content)?;
        let chars_changed = (new_content.len() as i64 - old_content.len() as i64).unsigned_abs() as usize;
        Ok(Value::String(format!("Edited {}: replaced {} chars", resolved.display(), chars_changed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn simple_exact_replace() {
        let result = replace("hello world", "world", "rust", false).unwrap();
        assert_eq!(result, "hello rust");
    }

    #[test]
    fn replace_not_found() {
        let err = replace("hello world", "missing", "x", false).unwrap_err();
        assert!(matches!(err, EditError::NotFound));
    }

    #[test]
    fn replace_multiple_matches_errors() {
        let err = replace("foo foo foo", "foo", "bar", false).unwrap_err();
        assert!(matches!(err, EditError::MultipleMatches));
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let result = replace("foo foo foo", "foo", "bar", true).unwrap();
        assert_eq!(result, "bar bar bar");
    }

    #[test]
    fn same_strings_errors() {
        let err = replace("hello", "hello", "hello", false).unwrap_err();
        assert!(matches!(err, EditError::SameStrings));
    }

    #[test]
    fn line_trimmed_replace_ignores_indentation() {
        let content = "  fn hello() {\n    println!(\"hi\");\n  }";
        let result = replace(content, "fn hello() {\n  println!(\"hi\");\n}", "fn world() {}", false).unwrap();
        assert!(result.contains("fn world()"));
    }

    #[test]
    fn indentation_flexible_replace() {
        let content = "    if x {\n        do_thing();\n    }";
        let result = replace(content, "if x {\n    do_thing();\n}", "if y {}", false).unwrap();
        assert!(result.contains("if y {}"));
    }

    #[tokio::test]
    async fn edit_tool_replaces_in_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}").unwrap();

        let tool = EditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "code.rs",
            "old_string": "hello",
            "new_string": "world"
        })).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"world\""));
        assert!(!content.contains("\"hello\""));
    }

    #[tokio::test]
    async fn edit_tool_creates_file_when_old_string_empty() {
        let dir = TempDir::new().unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        tool.execute(serde_json::json!({
            "file_path": "new.txt",
            "old_string": "",
            "new_string": "created content"
        })).await.unwrap();

        let content = std::fs::read_to_string(dir.path().join("new.txt")).unwrap();
        assert_eq!(content, "created content");
    }

    #[tokio::test]
    async fn edit_tool_returns_error_when_not_found() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "existing content").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        let result = tool.execute(serde_json::json!({
            "file_path": "f.txt",
            "old_string": "nonexistent",
            "new_string": "x"
        })).await;
        assert!(result.is_err());
    }
}
