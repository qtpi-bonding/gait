//! Rendering half of the REPL view. Reads ReplState produced by ui/state.rs,
//! transforms items via the pure view layer in ui/view.rs, and writes the result
//! to stderr. No business logic here.

use crate::theme::{Theme, RESET};
use crate::ui::state::ReplState;
use crate::ui::view::{clean_repl_item, ReplViewItem};
use std::io::Write;
use termimad::crossterm::style::Color as CColor;
use termimad::MadSkin;

/// Display width of the `NNN│ ` / `  ✓│ ` gutter all continuation lines align under.
const GUTTER: usize = 5;

/// Extract the most meaningful argument from a tool call to show beside the tool name.
pub(crate) fn tool_target(name: &str, args: &serde_json::Value) -> Option<String> {
    let s = |key: &str| -> Option<&str> { args.get(key).and_then(|v| v.as_str()) };
    let basename = |p: &str| -> String {
        std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p)
            .to_string()
    };
    let trunc = |t: &str, max: usize| -> String {
        if t.len() > max {
            format!("{}…", &t[..max])
        } else {
            t.to_string()
        }
    };
    match name {
        "read" | "edit" | "write" => s("file_path").map(basename),
        "grep" => {
            let pat = s("pattern")?;
            Some(match s("path") {
                Some(p) => format!("{} in {}", trunc(pat, 40), basename(p)),
                None => trunc(pat, 50),
            })
        }
        "glob" => s("pattern").map(|p| trunc(p, 50)),
        "ls" => s("path").map(basename),
        "shell" => s("cmd").map(|c| trunc(c, 60)),
        "codesearch" => s("query").map(|q| trunc(q, 50)),
        "webfetch" => s("url").map(|u| trunc(u, 80)),
        "spawn_agent" => s("task").map(|t| trunc(t, 50)),
        _ => None,
    }
}

fn to_ccolor(r: u8, g: u8, b: u8) -> CColor {
    if crate::theme::supports_truecolor() {
        CColor::Rgb { r, g, b }
    } else {
        CColor::AnsiValue(crate::theme::rgb_to_ansi256(r, g, b))
    }
}

fn themed_skin(theme: &Theme) -> MadSkin {
    let _accent = to_ccolor(theme.accent.0, theme.accent.1, theme.accent.2);
    let muted = to_ccolor(theme.muted.0, theme.muted.1, theme.muted.2);
    let md_heading = to_ccolor(theme.md_heading.0, theme.md_heading.1, theme.md_heading.2);
    let md_code = to_ccolor(theme.md_code.0, theme.md_code.1, theme.md_code.2);
    let md_strong = to_ccolor(theme.md_strong.0, theme.md_strong.1, theme.md_strong.2);
    let md_emph = to_ccolor(theme.md_emph.0, theme.md_emph.1, theme.md_emph.2);
    let _md_link = to_ccolor(theme.md_link.0, theme.md_link.1, theme.md_link.2);

    let mut skin = MadSkin::default();
    skin.inline_code.set_fgbg(md_code, CColor::Reset);
    skin.code_block
        .compound_style
        .set_fgbg(md_code, CColor::Reset);
    skin.bold.set_fg(md_strong);
    skin.italic.set_fg(md_emph);
    skin.set_headers_fg(md_heading);
    skin.quote_mark = termimad::StyledChar::from_fg_char(muted, '▐');
    skin.horizontal_rule = termimad::StyledChar::from_fg_char(muted, '─');
    // Links have no direct skin slot in termimad; keep default.
    skin
}

/// A chunk of markdown: plain text or a fenced code block.
enum MdChunk {
    Text(String),
    Code { lang: String, code: String },
}

/// Split raw markdown into alternating text/code chunks.
/// Fences are ` ```lang ` … ` ``` ` on their own lines.
fn split_markdown(src: &str) -> Vec<MdChunk> {
    let mut out = Vec::new();
    let mut text_buf = String::new();
    let mut inside = false;
    let mut lang = String::new();
    let mut code_buf = String::new();

    for line in src.lines() {
        if inside {
            if line.trim() == "```" {
                // End of code block
                if !text_buf.is_empty() {
                    out.push(MdChunk::Text(std::mem::take(&mut text_buf)));
                }
                out.push(MdChunk::Code {
                    lang: std::mem::take(&mut lang),
                    code: std::mem::take(&mut code_buf),
                });
                inside = false;
            } else {
                if !code_buf.is_empty() {
                    code_buf.push('\n');
                }
                code_buf.push_str(line);
            }
        } else if let Some(rest) = line.trim_start().strip_prefix("```") {
            // Start of code block
            if !text_buf.is_empty() {
                out.push(MdChunk::Text(std::mem::take(&mut text_buf)));
            }
            inside = true;
            lang = rest.trim().to_string();
        } else {
            if !text_buf.is_empty() {
                text_buf.push('\n');
            }
            text_buf.push_str(line);
        }
    }

    // Remaining text after the last code block
    if !text_buf.is_empty() {
        out.push(MdChunk::Text(text_buf));
    }
    // If we ended while still inside a code block, treat remainder as plain text
    if inside && !code_buf.is_empty() {
        out.push(MdChunk::Text(code_buf));
    }

    out
}

/// Stateless terminal painter — the StatelessWidget in the Flutter/Bloc analogy.
pub struct ReplRenderer {
    pub theme: Theme,
    skin: MadSkin,
}

impl ReplRenderer {
    pub fn new(theme: Theme) -> Self {
        let skin = themed_skin(&theme);
        Self { theme, skin }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn skin(&self) -> &MadSkin {
        &self.skin
    }

    /// Drain all pending items from state and paint them to stderr.
    ///
    /// The shared `paint_lock` is held for the full drain so the timer thread
    /// cannot land a status-line erase (`\r\x1b[2K`) inside a block.
    ///
    /// All output uses `\r\n` instead of bare `\n` because the Esc-key watcher
    /// enables terminal raw mode during turns. In raw mode `\n` is a bare line
    /// feed (no carriage return), so without `\r` each line would start at the
    /// column where the previous line ended rather than column 0.
    pub fn paint(&self, state: &mut ReplState) -> Vec<ReplViewItem> {
        if state.pending.is_empty() {
            return Vec::new();
        }
        let _guard = state.paint_lock.as_ref().and_then(|l| l.lock().ok());
        // The timer owns a two-line transient overlay while a turn is active.
        // Move it out of the way before appending permanent output, then put
        // it back below the newly rendered items.
        state.erase_overlay();
        eprint!("\r\x1b[2K");
        let pending = std::mem::take(&mut state.pending);
        let view_items: Vec<ReplViewItem> = pending.into_iter().flat_map(clean_repl_item).collect();
        for v in &view_items {
            self.paint_view_item(v);
        }
        state.redraw_overlay();
        let _ = std::io::stderr().flush();
        view_items
    }

    fn paint_view_item(&self, v: &ReplViewItem) {
        match v {
            ReplViewItem::Block { num, content } => {
                self.paint_block(*num, content);
            }
            ReplViewItem::DoneMessage { content } => {
                self.paint_done_message(content);
            }
            ReplViewItem::ToolResult {
                ok,
                name,
                elapsed_ms,
            } => {
                let (color, mark) = if *ok {
                    (self.theme.success.fg(), '\u{2713}')
                } else {
                    (self.theme.danger.fg(), '\u{2717}')
                };
                let timing = elapsed_ms
                    .map(|ms| format!(" \u{b7} {}ms", ms))
                    .unwrap_or_default();
                eprint!("  {} {}{}{}{}\r\n", color, mark, name, timing, RESET);
            }
            ReplViewItem::SnippetLine { text } => {
                eprint!("{}     {}{}\r\n", self.theme.muted.fg(), text, RESET);
            }
            ReplViewItem::Error { message } => {
                eprint!("{}[error] {}{}\r\n", self.theme.danger.fg(), message, RESET);
            }
            ReplViewItem::TurnSummary {
                elapsed_ms,
                output_tokens,
            } => {
                let elapsed = if *elapsed_ms < 60_000 {
                    format!("{}s", elapsed_ms / 1000)
                } else {
                    format!("{}m {}s", elapsed_ms / 60_000, (elapsed_ms % 60_000) / 1000)
                };
                let tokens = if *output_tokens >= 1000 {
                    format!("{:.1}k", *output_tokens as f64 / 1000.0)
                } else {
                    format!("{}", output_tokens)
                };
                eprint!(
                    "{}  \u{b7} {} \u{b7} \u{2193} {} tokens{}\r\n",
                    self.theme.muted.fg(),
                    elapsed,
                    tokens,
                    RESET
                );
            }
            ReplViewItem::StructuredBox { json } => {
                self.paint_structured_result(json);
            }
            ReplViewItem::Spacing => {
                eprint!("\r\n");
            }
        }
    }

    /// Render a numbered assistant block through termimad so markdown (bold, code,
    /// headers, etc.) is formatted. The gutter (`NNN│`) appears on the first
    /// rendered line; continuation lines get a plain `     ` indent to align under
    /// the content column.
    ///
    /// Fenced code blocks (```lang … ```) are intercepted and highlighted with
    /// syntect using the theme's syntax palette.
    fn paint_block(&self, num: usize, content: &str) {
        let term_w = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        let content_w = term_w.saturating_sub(GUTTER);
        let rendered_lines = self.render_markdown_lines(content, content_w);

        let mut is_first = true;
        for line in &rendered_lines {
            if is_first {
                eprint!(
                    "{}{:>3}\u{2502}{} {}\r\n",
                    self.theme.muted.fg(),
                    num,
                    RESET,
                    line
                );
                is_first = false;
            } else {
                eprint!("     {}\r\n", line);
            }
        }
        if is_first {
            // empty render — just the gutter
            eprint!("{}{:>3}\u{2502}{}\r\n", self.theme.muted.fg(), num, RESET);
        }
    }

    /// Render a structured completion message through the same Markdown path as
    /// ordinary assistant prose, retaining the distinct completion gutter.
    fn paint_done_message(&self, content: &str) {
        let term_w = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        let content_w = term_w.saturating_sub(GUTTER);
        let rendered_lines = self.render_markdown_lines(content, content_w);

        let mut lines = rendered_lines.iter();
        if let Some(first) = lines.next() {
            eprint!(
                "{}  \u{2713}\u{2502}{} {}\r\n",
                self.theme.success.fg(),
                RESET,
                first
            );
        } else {
            eprint!("{}  \u{2713}\u{2502}{}\r\n", self.theme.success.fg(), RESET);
        }
        for line in lines {
            eprint!("     {}\r\n", line);
        }
    }

    /// Convert Markdown to terminal-ready lines without painting them. Both
    /// ordinary assistant blocks and structured completion messages use this
    /// path so headings, lists, emphasis, and fenced code behave identically.
    fn render_markdown_lines(&self, content: &str, content_w: usize) -> Vec<String> {
        let mut rendered_lines = Vec::new();
        for chunk in split_markdown(content) {
            match chunk {
                MdChunk::Text(text) => {
                    let text = self.skin.text(&text, Some(content_w));
                    let rendered = format!("{text}");
                    rendered_lines.extend(rendered.lines().map(str::to_string));
                }
                MdChunk::Code { lang, code } => {
                    // Code blocks get a 5-char line-number gutter ("  NN│ ") so
                    // content width is reduced by 5. Long lines are truncated by
                    // syntect before highlighting.
                    let code_w = content_w.saturating_sub(5);
                    if let Some(hl) =
                        crate::syntax::highlight_block(&code, &lang, &self.theme, Some(code_w))
                    {
                        for (i, line) in hl.lines().enumerate() {
                            let n = format!("{:>3}", i + 1);
                            let gutter =
                                format!("{}{}\u{2502}{} ", self.theme.muted.fg(), n, RESET);
                            rendered_lines.push(format!("{gutter}{line}"));
                        }
                    } else {
                        // Fallback to termimad's plain code-block styling.
                        let block = format!("```{lang}\n{code}\n```");
                        let text = self.skin.text(&block, Some(content_w));
                        let rendered = format!("{text}");
                        rendered_lines.extend(rendered.lines().map(str::to_string));
                    }
                }
            }
        }
        rendered_lines
    }

    fn paint_structured_result(&self, json: &str) {
        let usable = termimad::terminal_size().0 as usize;
        let usable = usable.saturating_sub(4).min(60);
        // Prefix widths: "  ┌─ result " = 12, "  └" = 3 — subtract each so both lines = usable.
        let top_bar = "\u{2500}".repeat(usable.saturating_sub(12));
        let bot_bar = "\u{2500}".repeat(usable.saturating_sub(3));
        eprint!(
            "{}  \u{2500}\u{2500} result {}{}\r\n",
            self.theme.muted.fg(),
            top_bar,
            RESET
        );
        for line in json.lines() {
            eprint!("    {}{}{}\r\n", self.theme.accent.fg(), line, RESET);
        }
        eprint!(
            "{}  \u{2500}{}{}\r\n",
            self.theme.muted.fg(),
            bot_bar,
            RESET
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use crate::ui::state::ReplItem;

    #[test]
    fn paint_drains_pending() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        state.pending.push(ReplItem::Block {
            content: "hello world".into(),
            block_num: 1,
        });
        state.pending.push(ReplItem::Spacing);
        r.paint(&mut state);
        assert!(
            state.pending.is_empty(),
            "paint must drain all pending items"
        );
    }

    #[test]
    fn paint_empty_state_is_noop() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        r.paint(&mut state);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn paint_returns_the_block_it_rendered() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        state.pending.push(ReplItem::Block {
            content: "Hello!".into(),
            block_num: 1,
        });
        let painted = r.paint(&mut state);
        assert!(
            painted.iter().any(
                |v| matches!(v, ReplViewItem::Block { content, .. } if content.contains("Hello!"))
            ),
            "paint must return the Block it rendered: {painted:?}"
        );
    }

    #[test]
    fn paint_takes_ref_self_not_mut() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        state.pending.push(ReplItem::Block {
            content: "a".into(),
            block_num: 1,
        });
        r.paint(&mut state);
        state.pending.push(ReplItem::Block {
            content: "b".into(),
            block_num: 2,
        });
        r.paint(&mut state);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn structured_done_message_uses_markdown_renderer() {
        let r = ReplRenderer::new(Theme::dark());
        let rendered = r
            .render_markdown_lines("## Result\n\n- **formatted**", 72)
            .join("\n");

        assert!(rendered.contains("Result"));
        assert!(rendered.contains("formatted"));
        assert!(
            !rendered.contains("##"),
            "heading markup leaked: {rendered:?}"
        );
        assert!(
            !rendered.contains("**"),
            "emphasis markup leaked: {rendered:?}"
        );
    }

    #[test]
    fn tool_target_extracts_per_tool() {
        let args = serde_json::json!({"file_path": "/tmp/src/types.rs"});
        assert_eq!(tool_target("read", &args).as_deref(), Some("types.rs"));
        assert_eq!(tool_target("edit", &args).as_deref(), Some("types.rs"));
        assert_eq!(tool_target("write", &args).as_deref(), Some("types.rs"));

        let args = serde_json::json!({"pattern": "fn foo", "path": "src/lib.rs"});
        assert_eq!(
            tool_target("grep", &args).as_deref(),
            Some("fn foo in lib.rs")
        );

        let args = serde_json::json!({"pattern": "**/*.rs"});
        assert_eq!(tool_target("glob", &args).as_deref(), Some("**/*.rs"));

        let args = serde_json::json!({"cmd": "cargo test"});
        assert_eq!(tool_target("shell", &args).as_deref(), Some("cargo test"));

        let args = serde_json::json!({"query": "impl Trait"});
        assert_eq!(
            tool_target("codesearch", &args).as_deref(),
            Some("impl Trait")
        );

        let args = serde_json::json!({"url": "https://example.com/page"});
        assert_eq!(
            tool_target("webfetch", &args).as_deref(),
            Some("https://example.com/page")
        );

        assert_eq!(
            tool_target("structured_done", &serde_json::json!({"message": "hi"})),
            None
        );
    }
}
