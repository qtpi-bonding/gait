//! Rendering half of the REPL view. Reads ReplState produced by ui/state.rs,
//! transforms items via the pure view layer in ui/view.rs, and writes the result
//! to stderr. No business logic here.

use crate::theme::{Theme, DIM_FG, RESET};
use crate::ui::state::ReplState;
use crate::ui::view::{clean_repl_item, ReplViewItem};
use std::io::Write;
use termimad::crossterm::style::Color as CColor;
use termimad::MadSkin;

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
    let accent = to_ccolor(theme.accent.0, theme.accent.1, theme.accent.2);

    let code_block_color = if theme.is_dark {
        to_ccolor(0x00, 0x62, 0x80)
    } else {
        to_ccolor(0x00, 0xC3, 0xFF)
    };

    let mut skin = MadSkin::default();
    skin.inline_code.set_fgbg(accent, CColor::Reset);
    skin.code_block.set_fgbg(code_block_color, CColor::Reset);
    skin.quote_mark = termimad::StyledChar::from_fg_char(CColor::DarkGrey, '▐');
    skin.horizontal_rule = termimad::StyledChar::from_fg_char(CColor::DarkGrey, '─');
    skin
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
    pub fn paint(&self, state: &mut ReplState) {
        if state.pending.is_empty() {
            return;
        }
        let _guard = state.paint_lock.as_ref().and_then(|l| l.lock().ok());
        // Erase the live status line before printing content so they don't overlap.
        eprint!("\r\x1b[2K");
        let pending = std::mem::take(&mut state.pending);
        let view_items: Vec<ReplViewItem> = pending.into_iter().flat_map(clean_repl_item).collect();
        for v in view_items {
            self.paint_view_item(&v);
        }
        let _ = std::io::stderr().flush();
    }

    fn paint_view_item(&self, v: &ReplViewItem) {
        match v {
            ReplViewItem::BlockStart { num, text } => {
                eprint!("{}{:>3}\u{2502}{} {}\r\n", DIM_FG, num, RESET, text);
            }
            ReplViewItem::BlockCont { text } => {
                let term_w = termimad::terminal_size().0 as usize;
                for seg in word_wrap(text, term_w.saturating_sub(5)) {
                    eprint!("     {}\r\n", seg);
                }
            }
            ReplViewItem::BlockEnd { num } => {
                eprint!("{}{:>3}\u{2502}{}\r\n", DIM_FG, num, RESET);
            }
            ReplViewItem::DoneStart { text } => {
                let term_w = termimad::terminal_size().0 as usize;
                let segs = word_wrap(text, term_w.saturating_sub(5));
                let mut it = segs.into_iter();
                if let Some(first) = it.next() {
                    eprint!(
                        "{}  \u{2713}\u{2502}{} {}\r\n",
                        self.theme.success.fg(),
                        RESET,
                        first
                    );
                }
                for seg in it {
                    eprint!("     {}\r\n", seg);
                }
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
                eprint!("{}     {}{}\r\n", DIM_FG, text, RESET);
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
                    DIM_FG, elapsed, tokens, RESET
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

    fn paint_structured_result(&self, json: &str) {
        let usable = termimad::terminal_size().0 as usize;
        let usable = usable.saturating_sub(4).min(60);
        // Prefix widths: "  ┌─ result " = 12, "  └" = 3 — subtract each so both lines = usable.
        let top_bar = "\u{2500}".repeat(usable.saturating_sub(12));
        let bot_bar = "\u{2500}".repeat(usable.saturating_sub(3));
        eprint!("{}  \u{2500}\u{2500} result {}{}\r\n", DIM_FG, top_bar, RESET);
        for line in json.lines() {
            eprint!("    {}{}{}\r\n", self.theme.accent.fg(), line, RESET);
        }
        eprint!("{}  \u{2500}{}{}\r\n", DIM_FG, bot_bar, RESET);
    }
}

/// Split `text` into segments of at most `max_width` display chars, breaking on
/// whitespace. Returns a single-element vec if no wrapping is needed.
fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || text.is_empty() || text.chars().count() <= max_width {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if line_len == 0 {
            line.push_str(word);
            line_len = wlen;
        } else if line_len + 1 + wlen <= max_width {
            line.push(' ');
            line.push_str(word);
            line_len += 1 + wlen;
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
            line_len = wlen;
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
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
            content: "hello".into(),
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
