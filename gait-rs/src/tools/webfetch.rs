use crate::types::{Tool, ExecutionMode};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024; // 5MB
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum FetchFormat {
    Text,
    Markdown,
    Html,
}

impl Default for FetchFormat {
    fn default() -> Self { FetchFormat::Markdown }
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

impl WebfetchTool {
    pub fn new() -> Self {
        let parameters = crate::tools::inject_filter(
            crate::tools::make_schema::<WebfetchParams>(),
            crate::types::OutputFilter::Curate,
        );
        Self { parameters }
    }
}

/// Strip all HTML tags and decode entities. Uses regex for char-boundary-safe processing.
pub fn strip_html(html: &str) -> String {
    // Remove block elements with content (no backreferences — regex crate doesn't support them)
    let script  = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let style   = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap();
    let noscript = regex::Regex::new(r"(?is)<noscript[^>]*>.*?</noscript>").unwrap();
    let iframe  = regex::Regex::new(r"(?is)<iframe[^>]*>.*?</iframe>").unwrap();
    let s = script.replace_all(html, "");
    let s = style.replace_all(&s, "");
    let s = noscript.replace_all(&s, "");
    let s = iframe.replace_all(&s, "");
    // Convert <br> variants to newline
    let br = regex::Regex::new(r"(?i)<br\s*/?>").unwrap();
    let s = br.replace_all(&s, "\n");
    // Convert closing block elements to newlines
    let p_end = regex::Regex::new(r"(?i)</(p|h[1-6])>").unwrap();
    let s = p_end.replace_all(&s, "\n\n");
    let block_end = regex::Regex::new(r"(?i)</(div|li|tr)>").unwrap();
    let s = block_end.replace_all(&s, "\n");
    // Strip all remaining tags
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let s = tags.replace_all(&s, "");
    // Decode common entities
    let s = s
        .replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ")
        .replace("&mdash;", "—").replace("&ndash;", "–");
    // Collapse excess whitespace
    let multi_nl = regex::Regex::new(r"\n{3,}").unwrap();
    let s = multi_nl.replace_all(&s, "\n\n");
    let spaces = regex::Regex::new(r"[ \t]+").unwrap();
    let s = spaces.replace_all(&s, " ");
    s.trim().to_string()
}

/// Convert HTML to markdown. Uses regex for char-boundary-safe processing.
pub fn html_to_markdown(html: &str) -> String {
    // Remove script/style blocks (no backreferences — regex crate doesn't support them)
    let script = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let style  = regex::Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap();
    let s = script.replace_all(html, "").into_owned();
    let s = style.replace_all(&s, "").into_owned();

    // Headings h1-h6
    let h = regex::Regex::new(r"(?is)<h([1-6])[^>]*>(.*?)</h[1-6]>").unwrap();
    let s = h.replace_all(&s, |caps: &regex::Captures| {
        let level: usize = caps[1].parse().unwrap_or(1);
        format!("\n{} {}\n", "#".repeat(level), caps[2].trim())
    }).into_owned();

    // Strong/bold
    let strong = regex::Regex::new(r"(?is)<strong[^>]*>(.*?)</strong>").unwrap();
    let s = strong.replace_all(&s, |caps: &regex::Captures| format!("**{}**", &caps[1])).into_owned();
    let b = regex::Regex::new(r"(?is)<b[^>]*>(.*?)</b>").unwrap();
    let s = b.replace_all(&s, |caps: &regex::Captures| format!("**{}**", &caps[1])).into_owned();

    // Em/italic
    let em = regex::Regex::new(r"(?is)<em[^>]*>(.*?)</em>").unwrap();
    let s = em.replace_all(&s, |caps: &regex::Captures| format!("*{}*", &caps[1])).into_owned();
    let i_tag = regex::Regex::new(r"(?is)<i[^>]*>(.*?)</i>").unwrap();
    let s = i_tag.replace_all(&s, |caps: &regex::Captures| format!("*{}*", &caps[1])).into_owned();

    // Code
    let code = regex::Regex::new(r"(?is)<code[^>]*>(.*?)</code>").unwrap();
    let s = code.replace_all(&s, |caps: &regex::Captures| format!("`{}`", &caps[1])).into_owned();

    // Links
    let a = regex::Regex::new(r#"(?is)<a[^>]*href="([^"]*)"[^>]*>(.*?)</a>"#).unwrap();
    let s = a.replace_all(&s, |caps: &regex::Captures| {
        format!("[{}]({})", &caps[2], &caps[1])
    }).into_owned();

    // br → newline
    let br = regex::Regex::new(r"(?i)<br\s*/?>").unwrap();
    let s = br.replace_all(&s, "\n").into_owned();

    // li items
    let li = regex::Regex::new(r"(?i)<li[^>]*>").unwrap();
    let s = li.replace_all(&s, "- ").into_owned();

    // Block endings → newlines
    let p_end = regex::Regex::new(r"(?i)</(p|h[1-6])>").unwrap();
    let s = p_end.replace_all(&s, "\n\n").into_owned();
    let block_end = regex::Regex::new(r"(?i)</(div|li|tr)>").unwrap();
    let s = block_end.replace_all(&s, "\n").into_owned();

    // Strip remaining tags
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let s = tags.replace_all(&s, "").into_owned();

    // Entities
    let s = s
        .replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ");

    // Collapse excessive blank lines
    let multi_nl = regex::Regex::new(r"\n{3,}").unwrap();
    let s = multi_nl.replace_all(&s, "\n\n");
    s.trim().to_string()
}

#[async_trait::async_trait]
impl Tool for WebfetchTool {
    fn name(&self) -> &str { "webfetch" }
    fn description(&self) -> &str {
        "Fetch content from a URL and return it as text or markdown."
    }
    fn parameters(&self) -> &Value { &self.parameters }
    fn execution_mode(&self) -> ExecutionMode { ExecutionMode::Parallel }

    fn output_filter(&self) -> crate::types::OutputFilter {
        crate::types::OutputFilter::Curate
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let params: WebfetchParams = serde_json::from_value(args)?;

        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            anyhow::bail!("URL must start with http:// or https://");
        }

        let timeout_secs = params.timeout
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
            .build()?;

        let initial = client.get(&params.url)
            .header("Accept", accept)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send().await
            .map_err(|e| anyhow::anyhow!("Request failed: {}", e))?;

        // Retry with honest UA on Cloudflare challenge
        let response = if initial.status() == 403
            && initial.headers().get("cf-mitigated").is_some()
        {
            client.get(&params.url)
                .header("Accept", accept)
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("User-Agent", "gait")
                .send().await
                .map_err(|e| anyhow::anyhow!("Retry request failed: {}", e))?
        } else {
            initial
        };

        if !response.status().is_success() {
            anyhow::bail!("Request failed with status {}: {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or(""));
        }

        if let Some(len) = response.content_length() {
            if len as usize > MAX_RESPONSE_BYTES {
                anyhow::bail!("Response too large (exceeds 5MB limit)");
            }
        }

        let content_type = response.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let is_html = content_type.contains("text/html");

        let bytes = response.bytes().await?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            anyhow::bail!("Response too large (exceeds 5MB limit)");
        }
        let content = String::from_utf8_lossy(&bytes).into_owned();

        let output = match format {
            FetchFormat::Markdown => if is_html { html_to_markdown(&content) } else { content },
            FetchFormat::Text     => if is_html { strip_html(&content) } else { content },
            FetchFormat::Html     => content,
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
        assert!(result.contains("[click here](https://example.com)"), "got: {}", result);
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
        let result = tool.execute(serde_json::json!({ "url": "ftp://example.com/file" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("http"));
    }

    #[tokio::test]
    async fn rejects_file_url() {
        let tool = WebfetchTool::new();
        let result = tool.execute(serde_json::json!({ "url": "file:///etc/passwd" })).await;
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
        assert_eq!(params["properties"]["curate_prompt"]["type"].as_str().unwrap(), "string");
    }
}
