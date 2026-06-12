use std::io::Write;
use termimad::MadSkin;
use termimad::crossterm::style::Color as CColor;
use crate::theme::{Theme, RESET, DIM_FG};
use crate::ui::state::{ReplState, PendingItem};

/// Split markdown text into numbered blocks using paragraph/fence logic.
/// An empty line terminates a block; a fenced code section is treated as one block.
pub fn split_into_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let is_fence = line.trim_start().starts_with("```");
        if in_fence {
            if !current.is_empty() { current.push('\n'); }
            current.push_str(line);
            if is_fence {
                in_fence = false;
                let block = std::mem::take(&mut current);
                if !block.is_empty() { blocks.push(block); }
            }
        } else if line.is_empty() {
            let block = std::mem::take(&mut current);
            if !block.is_empty() { blocks.push(block); }
        } else {
            if !current.is_empty() { current.push('\n'); }
            current.push_str(line);
            if is_fence { in_fence = true; }
        }
    }
    if !current.is_empty() { blocks.push(current); }
    blocks
}

/// Extract the most meaningful argument from a tool call to show beside the tool name.
pub(crate) fn tool_target(name: &str, args: &serde_json::Value) -> Option<String> {
    let s = |key: &str| -> Option<&str> { args.get(key).and_then(|v| v.as_str()) };
    let basename = |p: &str| -> String {
        std::path::Path::new(p).file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p)
            .to_string()
    };
    let trunc = |t: &str, max: usize| -> String {
        if t.len() > max { format!("{}…", &t[..max]) } else { t.to_string() }
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

// ── ToolCallWidget ────────────────────────────────────────────────────────────

/// Stateless widget for the tool-call lifecycle animation.
///
/// Call `show_spinner` when the tool starts; call `show_result` when it finishes.
/// `show_result` auto-erases the spinner line when `had_spinner` is true — the
/// caller never needs to know the ANSI escape sequence.
pub(crate) struct ToolCallWidget;

impl ToolCallWidget {
    pub(crate) fn show_spinner(display: &str) {
        eprintln!("{}  \u{27f3} {}(\u{2026}){}", DIM_FG, display, RESET);
    }

    pub(crate) fn show_result(
        theme: &Theme,
        ok: bool,
        name: &str,
        elapsed_ms: Option<u64>,
        output: &str,
        had_spinner: bool,
    ) {
        if had_spinner { eprint!("\x1b[1A\x1b[2K"); }
        let (color, mark) = if ok {
            (theme.success.fg(), '\u{2713}')
        } else {
            (theme.danger.fg(), '\u{2717}')
        };
        let timing = elapsed_ms.map(|ms| format!(" \u{b7} {}ms", ms)).unwrap_or_default();
        eprintln!("{}  {} {}{}{}", color, mark, name, timing, RESET);
        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        match lines.len() {
            0 => {}
            1 => eprintln!("{}     {}{}", DIM_FG, lines[0], RESET),
            2 => {
                eprintln!("{}     {}{}", DIM_FG, lines[0], RESET);
                eprintln!("{}     {}{}", DIM_FG, lines[1], RESET);
            }
            n => {
                eprintln!("{}     {}{}", DIM_FG, lines[0], RESET);
                eprintln!("{}     \u{2026} ({} lines){}", DIM_FG, n - 2, RESET);
                eprintln!("{}     {}{}", DIM_FG, lines[n - 1], RESET);
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────

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

    pub fn theme(&self) -> &Theme { &self.theme }

    /// Drain all pending items from state and paint them to the terminal.
    pub fn paint(&self, state: &mut ReplState) {
        for item in state.pending.drain(..) {
            match item {
                PendingItem::Block { content, block_num } => {
                    self.paint_block(&content, block_num);
                }
                PendingItem::Spacing => { println!(); }
                PendingItem::ToolCall { display } => {
                    ToolCallWidget::show_spinner(&display);
                }
                PendingItem::ToolResult { ok, name, elapsed_ms, output, had_spinner } => {
                    ToolCallWidget::show_result(&self.theme, ok, &name, elapsed_ms, &output, had_spinner);
                }
                PendingItem::Error { message } => {
                    eprintln!("{}[error] {}{}", self.theme.danger.fg(), message, RESET);
                }
            }
        }
    }

    fn paint_block(&self, block: &str, block_num: usize) {
        let term_w = termimad::terminal_size().0 as usize;
        let content_w = term_w.saturating_sub(6);
        let fmt = self.skin.text(block, Some(content_w));
        let rendered = format!("{}", fmt);
        let mut first = true;
        for line in rendered.lines() {
            if first {
                print!("{}{:>3}\u{2502}{} {}", DIM_FG, block_num, RESET, line);
                first = false;
            } else {
                print!("     {}", line);
            }
            println!();
        }
        if first {
            println!("{}{:>3}\u{2502}{}", DIM_FG, block_num, RESET);
        }
        let _ = std::io::stdout().flush();
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::{ReplState, PendingItem};
    use crate::theme::Theme;

    #[test]
    fn paint_drains_pending() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        state.pending.push(PendingItem::Block { content: "hello".into(), block_num: 1 });
        state.pending.push(PendingItem::Spacing);
        r.paint(&mut state);
        assert!(state.pending.is_empty(), "paint must drain all pending items");
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
        state.pending.push(PendingItem::Block { content: "a".into(), block_num: 1 });
        r.paint(&mut state);
        state.pending.push(PendingItem::Block { content: "b".into(), block_num: 2 });
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
        assert_eq!(tool_target("grep", &args).as_deref(), Some("fn foo in lib.rs"));

        let args = serde_json::json!({"pattern": "**/*.rs"});
        assert_eq!(tool_target("glob", &args).as_deref(), Some("**/*.rs"));

        let args = serde_json::json!({"cmd": "cargo test"});
        assert_eq!(tool_target("shell", &args).as_deref(), Some("cargo test"));

        let args = serde_json::json!({"query": "impl Trait"});
        assert_eq!(tool_target("codesearch", &args).as_deref(), Some("impl Trait"));

        let args = serde_json::json!({"url": "https://example.com/page"});
        assert_eq!(tool_target("webfetch", &args).as_deref(), Some("https://example.com/page"));

        assert_eq!(tool_target("output", &serde_json::json!({"message": "hi"})), None);
    }

    #[test]
    fn split_into_blocks_paragraphs() {
        let text = "para one\n\npara two\n\npara three";
        let blocks = split_into_blocks(text);
        assert_eq!(blocks, vec!["para one", "para two", "para three"]);
    }

    #[test]
    fn split_into_blocks_fenced_code() {
        let text = "intro\n\n```rust\nlet x = 1;\n```\n\noutro";
        let blocks = split_into_blocks(text);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], "intro");
        assert!(blocks[1].contains("let x = 1;"));
        assert_eq!(blocks[2], "outro");
    }
}
