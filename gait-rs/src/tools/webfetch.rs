use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::LazyLock;

static WF_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script[^>]*>.*?</script>").expect("valid regex literal"));
static WF_STYLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style[^>]*>.*?</style>").expect("valid regex literal"));
static WF_NOSCRIPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<noscript[^>]*>.*?</noscript>").expect("valid regex literal")
});
static WF_IFRAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<iframe[^>]*>.*?</iframe>").expect("valid regex literal"));
static WF_BR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").expect("valid regex literal"));
static WF_P_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</(p|h[1-6])>").expect("valid regex literal"));
static WF_BLOCK_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)</(div|li|tr)>").expect("valid regex literal"));
static WF_TAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid regex literal"));
static WF_MULTI_NL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid regex literal"));
static WF_SPACES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+").expect("valid regex literal"));
static WF_H: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<h([1-6])[^>]*>(.*?)</h[1-6]>").expect("valid regex literal")
});
static WF_STRONG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<strong[^>]*>(.*?)</strong>").expect("valid regex literal"));
static WF_B: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<b[^>]*>(.*?)</b>").expect("valid regex literal"));
static WF_EM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<em[^>]*>(.*?)</em>").expect("valid regex literal"));
static WF_I: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<i[^>]*>(.*?)</i>").expect("valid regex literal"));
static WF_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<code[^>]*>(.*?)</code>").expect("valid regex literal"));
static WF_A: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).expect("valid regex literal")
});
static WF_LI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<li[^>]*>").expect("valid regex literal"));

const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024; // 5MB
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
enum FetchFormat {
    Text,
    #[default]
    Markdown,
    Html,
}

#[derive(Deserialize, JsonSchema)]
struct WebfetchParams {
    /// The URL to fetch content from (must start with http:// or https://)
    url: String,
    /// The format to return content in: text (plain), markdown, or html. Defaults to markdown.
    format: Option<FetchFormat>,
    /// Timeout in seconds (max 120). Defaults to 30.
    timeout: Option<u64>,
}

pub struct WebfetchTool {
    parameters: Value,
}

impl Default for WebfetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebfetchTool {
    pub fn new() -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<WebfetchParams>(),
            crate::types::OutputFilter::Both,
        );
        Self { parameters }
    }
}

/// Strip all HTML tags and decode entities. Uses regex for char-boundary-safe processing.
pub fn strip_html(html: &str) -> String {
    // Remove block elements with content (no backreferences — regex crate doesn't support them)
    let s = WF_SCRIPT.replace_all(html, "");
    let s = WF_STYLE.replace_all(&s, "");
    let s = WF_NOSCRIPT.replace_all(&s, "");
    let s = WF_IFRAME.replace_all(&s, "");
    // Convert <br> variants to newline
    let s = WF_BR.replace_all(&s, "\n");
    // Convert closing block elements to newlines
    let s = WF_P_END.replace_all(&s, "\n\n");
    let s = WF_BLOCK_END.replace_all(&s, "\n");
    // Strip all remaining tags
    let s = WF_TAGS.replace_all(&s, "");
    // Decode common entities
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–");
    // Collapse excess whitespace
    let s = WF_MULTI_NL.replace_all(&s, "\n\n");
    let s = WF_SPACES.replace_all(&s, " ");
    s.trim().to_string()
}

/// Convert HTML to markdown. Uses regex for char-boundary-safe processing.
pub fn html_to_markdown(html: &str) -> String {
    // Remove script/style blocks (no backreferences — regex crate doesn't support them)
    let s = WF_SCRIPT.replace_all(html, "").into_owned();
    let s = WF_STYLE.replace_all(&s, "").into_owned();

    // Headings h1-h6
    let s = WF_H
        .replace_all(&s, |caps: &regex::Captures| {
            let level: usize = caps[1].parse().unwrap_or(1);
            format!("\n{} {}\n", "#".repeat(level), caps[2].trim())
        })
        .into_owned();

    // Strong/bold
    let s = WF_STRONG
        .replace_all(&s, |caps: &regex::Captures| format!("**{}**", &caps[1]))
        .into_owned();
    let s = WF_B
        .replace_all(&s, |caps: &regex::Captures| format!("**{}**", &caps[1]))
        .into_owned();

    // Em/italic
    let s = WF_EM
        .replace_all(&s, |caps: &regex::Captures| format!("*{}*", &caps[1]))
        .into_owned();
    let s = WF_I
        .replace_all(&s, |caps: &regex::Captures| format!("*{}*", &caps[1]))
        .into_owned();

    // Code
    let s = WF_CODE
        .replace_all(&s, |caps: &regex::Captures| format!("`{}`", &caps[1]))
        .into_owned();

    // Links
    let s = WF_A
        .replace_all(&s, |caps: &regex::Captures| {
            format!("[{}]({})", &caps[2], &caps[1])
        })
        .into_owned();

    // br → newline
    let s = WF_BR.replace_all(&s, "\n").into_owned();

    // li items
    let s = WF_LI.replace_all(&s, "- ").into_owned();

    // Block endings → newlines
    let s = WF_P_END.replace_all(&s, "\n\n").into_owned();
    let s = WF_BLOCK_END.replace_all(&s, "\n").into_owned();

    // Strip remaining tags
    let s = WF_TAGS.replace_all(&s, "").into_owned();

    // Entities
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Collapse excessive blank lines
    let s = WF_MULTI_NL.replace_all(&s, "\n\n");
    s.trim().to_string()
}

#[async_trait::async_trait]
impl Tool for WebfetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn description(&self) -> &str {
        "Fetch content from a URL and return it as text or markdown."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Curate
    }

    async fn execute(&self, args: Value, _rec: &crate::Recorder) -> Result<Value, ToolError> {
        let params: WebfetchParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;

        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            return Err(crate::tools::ToolError::Execute(
                "URL must start with http:// or https://".to_string(),
            ));
        }

        let timeout_secs = params
            .timeout
            .map(|t| t.min(MAX_TIMEOUT_SECS))
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let format = params.format.unwrap_or_default();

        let accept = match format {
            FetchFormat::Markdown => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
            FetchFormat::Text    => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            FetchFormat::Html    => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, */*;q=0.1",
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
            .build()
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;

        let initial = client
            .get(&params.url)
            .header("Accept", accept)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|e| {
                crate::tools::ToolError::Internal(anyhow::anyhow!("Request failed: {}", e))
            })?;

        // Retry with honest UA on Cloudflare challenge
        let response = if initial.status() == 403 && initial.headers().get("cf-mitigated").is_some()
        {
            client
                .get(&params.url)
                .header("Accept", accept)
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("User-Agent", "gait")
                .send()
                .await
                .map_err(|e| {
                    crate::tools::ToolError::Internal(anyhow::anyhow!(
                        "Retry request failed: {}",
                        e
                    ))
                })?
        } else {
            initial
        };

        if !response.status().is_success() {
            return Err(crate::tools::ToolError::Execute(format!(
                "Request failed with status {}: {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("")
            )));
        }

        if let Some(len) = response.content_length() {
            if len as usize > MAX_RESPONSE_BYTES {
                return Err(crate::tools::ToolError::Execute(
                    "Response too large (exceeds 5MB limit)".to_string(),
                ));
            }
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let is_html = content_type.contains("text/html");

        let bytes = response
            .bytes()
            .await
            .map_err(|e| crate::tools::ToolError::Internal(e.into()))?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(crate::tools::ToolError::Execute(
                "Response too large (exceeds 5MB limit)".to_string(),
            ));
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();

        let output = match format {
            FetchFormat::Markdown => {
                if is_html {
                    html_to_markdown(&content)
                } else {
                    content
                }
            }
            FetchFormat::Text => {
                if is_html {
                    strip_html(&content)
                } else {
                    content
                }
            }
            FetchFormat::Html => content,
        };

        Ok(Value::String(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags() {
        let html = "<p>Hello <b>world</b>!</p>";
        let result = strip_html(html);
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn strip_html_removes_script_blocks() {
        let html = "<p>text</p><script>alert('xss')</script><p>more</p>";
        let result = strip_html(html);
        assert!(!result.contains("alert"), "got: {}", result);
        assert!(result.contains("text"), "got: {}", result);
        assert!(result.contains("more"), "got: {}", result);
    }

    #[test]
    fn strip_html_decodes_entities() {
        let html = "<p>a &amp; b &lt; c &gt; d</p>";
        let result = strip_html(html);
        assert!(result.contains("a & b"), "got: {}", result);
        assert!(result.contains("< c"), "got: {}", result);
    }

    #[test]
    fn strip_html_handles_non_ascii() {
        let html = "<p>Héllo wörld — café</p><script>x()</script>";
        let result = strip_html(html);
        assert!(!result.contains("x()"), "script not removed: {}", result);
        assert!(result.contains("Héllo"), "content lost: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_headings() {
        let html = "<h1>Title</h1><h2>Subtitle</h2>";
        let result = html_to_markdown(html);
        assert!(result.contains("# Title"), "got: {}", result);
        assert!(result.contains("## Subtitle"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_bold_italic() {
        let html = "<strong>bold</strong> and <em>italic</em>";
        let result = html_to_markdown(html);
        assert!(result.contains("**bold**"), "got: {}", result);
        assert!(result.contains("*italic*"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_converts_links() {
        let html = r#"<a href="https://example.com">click here</a>"#;
        let result = html_to_markdown(html);
        assert!(
            result.contains("[click here](https://example.com)"),
            "got: {}",
            result
        );
    }

    #[test]
    fn html_to_markdown_converts_code() {
        let html = "<code>let x = 1;</code>";
        let result = html_to_markdown(html);
        assert!(result.contains("`let x = 1;`"), "got: {}", result);
    }

    #[test]
    fn html_to_markdown_handles_non_ascii() {
        let html = "<h1>Héllo</h1><p>wörld — café</p>";
        let result = html_to_markdown(html);
        assert!(result.contains("# Héllo"), "got: {}", result);
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let tool = WebfetchTool::new();
        let result = tool
            .execute(
                serde_json::json!({ "url": "ftp://example.com/file" }),
                &crate::Recorder::detached(),
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http"));
    }

    #[tokio::test]
    async fn rejects_file_url() {
        let tool = WebfetchTool::new();
        let result = tool
            .execute(
                serde_json::json!({ "url": "file:///etc/passwd" }),
                &crate::Recorder::detached(),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn name_is_webfetch() {
        let tool = WebfetchTool::new();
        assert_eq!(tool.name(), "webfetch");
    }

    #[test]
    fn schema_has_curate_prompt_field() {
        let tool = WebfetchTool::new();
        let params = tool.parameters();
        assert_eq!(
            params["properties"]["curate_prompt"]["type"]
                .as_str()
                .unwrap(),
            "string"
        );
    }
}
