use crate::tools::ToolError;
use crate::types::{ExecutionMode, Tool};
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

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
    /// When `Some`, only requests to these hosts are allowed. Enforced at the
    /// app level because the kernel SBPL can't do host-level filtering.
    network_allowlist: Option<Vec<String>>,
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
        Self {
            parameters,
            network_allowlist: None,
        }
    }

    pub fn with_network_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.network_allowlist = Some(hosts);
        self
    }
}

/// Check a URL's host against the egress allowlist. Returns the denial
/// message (`Some`) when the host is not permitted (a missing host is never permitted).
fn check_egress(allowlist: &[String], url: Option<&reqwest::Url>) -> Option<String> {
    let host = url
        .and_then(|u| u.host_str())
        .map(|h: &str| h.to_lowercase());
    let display = host.as_deref().unwrap_or("<no host>");
    let permitted = host.as_deref().is_some_and(|host| {
        allowlist.iter().any(|allowed| {
            let allowed = allowed.to_lowercase();
            allowed == host
                || (allowed.starts_with("*.")
                    && !allowed[2..].is_empty()
                    && host.ends_with(&allowed[1..]))
        })
    });
    (!permitted)
        .then(|| format!("egress denied: host '{display}' not in sandbox.network allowlist"))
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

    async fn execute(&self, args: Value, _rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: WebfetchParams = serde_json::from_value(args)
            .map_err(|e| crate::tools::ToolError::InvalidParams(e.to_string()))?;

        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            return Err(crate::tools::ToolError::Execute(
                "URL must start with http:// or https://".to_string(),
            ));
        }

        // App-level egress allowlist: when sandbox.network is in Hosts mode,
        // the kernel SBPL allows all outbound (can't filter by host). Check
        // the URL's host against the allowlist here to close the gap.
        if let Some(ref allowlist) = self.network_allowlist {
            let first_url = reqwest::Url::parse(&params.url).ok();
            if let Some(msg) = check_egress(allowlist, first_url.as_ref()) {
                return Err(crate::tools::ToolError::Execute(msg));
            }
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

        let mut builder = reqwest::Client::builder();
        let denied_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        if let Some(ref allowlist) = self.network_allowlist {
            let allowlist = allowlist.clone();
            let slot = Arc::clone(&denied_slot);
            let policy = reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                match check_egress(&allowlist, Some(attempt.url())) {
                    None => attempt.follow(),
                    Some(msg) => {
                        if let Ok(mut guard) = slot.lock() {
                            *guard = Some(msg);
                        }
                        attempt.stop()
                    }
                }
            });
            builder = builder.redirect(policy);
        } else {
            builder = builder.redirect(reqwest::redirect::Policy::default());
        }
        let client = builder
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

        if let Ok(guard) = denied_slot.lock() {
            if let Some(msg) = guard.as_ref() {
                return Err(crate::tools::ToolError::Execute(msg.clone()));
            }
        }

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

        if let Ok(guard) = denied_slot.lock() {
            if let Some(msg) = guard.as_ref() {
                return Err(crate::tools::ToolError::Execute(msg.clone()));
            }
        }

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

    fn access(&self) -> crate::types::ToolAccess {
        crate::types::ToolAccess {
            network: true,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny local HTTP stub. Binds to 127.0.0.1:0 and serves a fixed raw
    /// response for every connection. `hits` counts accepted connections so
    /// tests can assert a stub was never contacted.
    async fn spawn_stub(
        response: &'static str,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let resp = response;
                tokio::spawn(async move {
                    // Read until end of request headers (\r\n\r\n), then reply.
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut sock, &mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut sock).await;
                });
            }
        });
        port
    }

    fn hits_counter() -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0))
    }

    fn serve_200(body: &'static str) -> &'static str {
        // Leaked strings let us build the fixed raw response per call.
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        )
    }

    fn serve_redirect_to(location: String) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_boxed_str(),
        )
    }

    /// Guard: when an allowlist is set, a redirect to a non-listed host must
    /// not be followed. The target stub must never receive a request.
    #[tokio::test]
    async fn allowlist_blocks_redirect_to_unlisted_host() {
        let target_hits = hits_counter();
        let target_port = spawn_stub(serve_200("secret"), target_hits.clone()).await;

        let origin_hits = hits_counter();
        let origin_port = spawn_stub(
            serve_redirect_to(format!("http://localhost:{target_port}/")),
            origin_hits.clone(),
        )
        .await;

        let tool = WebfetchTool::new().with_network_allowlist(vec!["127.0.0.1".to_string()]);
        let result = tool
            .execute(
                serde_json::json!({ "url": format!("http://127.0.0.1:{origin_port}/") }),
                &crate::Recorder::detached(),
            )
            .await;

        let err = result.expect_err("redirect to unlisted host must be denied");
        assert!(
            err.to_string()
                .contains("egress denied: host 'localhost' not in sandbox.network allowlist"),
            "unexpected error: {err}"
        );
        assert_eq!(
            target_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "target stub must never be contacted"
        );
    }

    /// Guard: a redirect to a host that IS in the allowlist (same IP, here
    /// reached as 127.0.0.1) may be followed and must return the target body.
    #[tokio::test]
    async fn allowlist_permits_redirect_to_listed_host() {
        let target_hits = hits_counter();
        let target_port = spawn_stub(serve_200("ok-body"), target_hits.clone()).await;

        let origin_hits = hits_counter();
        let origin_port = spawn_stub(
            serve_redirect_to(format!("http://127.0.0.1:{target_port}/")),
            origin_hits.clone(),
        )
        .await;

        let tool = WebfetchTool::new().with_network_allowlist(vec!["127.0.0.1".to_string()]);
        let result = tool
            .execute(
                serde_json::json!({ "url": format!("http://127.0.0.1:{origin_port}/") }),
                &crate::Recorder::detached(),
            )
            .await
            .expect("redirect to allowed host should succeed");

        let out = result.as_str().expect("string output");
        assert!(out.contains("ok-body"), "got: {out}");
    }

    /// Guard: without an allowlist, redirects to any host are followed.
    #[tokio::test]
    async fn no_allowlist_follows_redirect() {
        let target_hits = hits_counter();
        let target_port = spawn_stub(serve_200("ok-body"), target_hits.clone()).await;

        let origin_hits = hits_counter();
        let origin_port = spawn_stub(
            serve_redirect_to(format!("http://localhost:{target_port}/")),
            origin_hits.clone(),
        )
        .await;

        let tool = WebfetchTool::new();
        let result = tool
            .execute(
                serde_json::json!({ "url": format!("http://127.0.0.1:{origin_port}/") }),
                &crate::Recorder::detached(),
            )
            .await
            .expect("no allowlist: redirect should be followed");

        let out = result.as_str().expect("string output");
        assert!(out.contains("ok-body"), "got: {out}");
    }

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

    #[tokio::test]
    async fn missing_url_is_invalid_params_not_internal() {
        let tool = WebfetchTool::new();
        let err = tool
            .execute(serde_json::json!({}), &crate::Recorder::detached())
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::tools::ToolError::InvalidParams(_)),
            "{err:?}"
        );
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

    #[tokio::test]
    async fn allowlist_blocks_unlisted_host() {
        let tool = WebfetchTool::new().with_network_allowlist(vec!["github.com".to_string()]);
        let result = tool
            .execute(
                serde_json::json!({ "url": "https://example.com/page" }),
                &crate::Recorder::detached(),
            )
            .await;
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("egress denied") && err.contains("example.com"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn allowlist_permits_listed_host_past_check() {
        let tool = WebfetchTool::new().with_network_allowlist(vec!["example.com".to_string()]);
        // The host passes the allowlist check; it then fails at the network
        // layer (no real connection in tests). We confirm the error is NOT
        // "egress denied".
        let result = tool
            .execute(
                serde_json::json!({ "url": "https://example.com/page" }),
                &crate::Recorder::detached(),
            )
            .await;
        // May succeed or fail at network, but must NOT be an allowlist error.
        if let Err(e) = result {
            assert!(
                !e.to_string().contains("egress denied"),
                "should not be blocked by allowlist: {e}"
            );
        }
    }

    #[tokio::test]
    async fn allowlist_wildcard_permits_subdomain() {
        let tool = WebfetchTool::new().with_network_allowlist(vec!["*.github.com".to_string()]);
        let result = tool
            .execute(
                serde_json::json!({ "url": "https://api.github.com/repos" }),
                &crate::Recorder::detached(),
            )
            .await;
        if let Err(e) = result {
            assert!(
                !e.to_string().contains("egress denied"),
                "wildcard should permit subdomain: {e}"
            );
        }
    }

    #[tokio::test]
    async fn no_allowlist_allows_any_host() {
        // Without an allowlist, no egress filtering is applied.
        let tool = WebfetchTool::new();
        let result = tool
            .execute(
                serde_json::json!({ "url": "https://example.com/page" }),
                &crate::Recorder::detached(),
            )
            .await;
        if let Err(e) = result {
            assert!(
                !e.to_string().contains("egress denied"),
                "no-allowlist tool should not block: {e}"
            );
        }
    }

    #[tokio::test]
    async fn allowlist_star_does_not_bypass() {
        // A bare "*" wildcard should NOT bypass the allowlist (fail-closed).
        let tool = WebfetchTool::new().with_network_allowlist(vec!["*".to_string()]);
        let result = tool
            .execute(
                serde_json::json!({ "url": "https://evil.com/" }),
                &crate::Recorder::detached(),
            )
            .await;
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("egress denied"),
            "bare * should not permit any host: {err}"
        );
    }
}
