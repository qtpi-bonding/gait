use std::io::Write;
use termimad::MadSkin;
use termimad::crossterm::style::Color as CColor;
use crate::session::transcript::{PermissionItem, ClarifyItem, InformItem};
use crate::theme::{Theme, RESET, DIM, DIM_FG};

pub enum RenderAction {
    Continue,
    PermissionNeeded(Vec<PermissionItem>),
    ClarifyNeeded(Vec<ClarifyItem>),
    InformNeeded(Vec<InformItem>),
    TurnComplete(Vec<String>),
}

/// Split markdown text into numbered blocks using the same paragraph/fence logic as Renderer.
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

pub struct Renderer {
    pub line_buf: String,
    block_buf: String,
    in_fence: bool,
    skin: MadSkin,
    pub theme: Theme,
    pending_tool: Option<String>,
    pending_ts: Option<u64>,
    /// Raw content of each numbered block in the current agent turn, for `>N` quote-reply.
    pub turn_blocks: Vec<String>,
    block_num: usize,
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

    // Code blocks get the opposite mode's accent: muted teal in dark, bright cyan in light.
    let code_block_color = if theme.is_dark {
        to_ccolor(0x00, 0x62, 0x80)
    } else {
        to_ccolor(0x00, 0xC3, 0xFF)
    };

    let mut skin = MadSkin::default();

    // Body, headings, bold, italic → terminal default (no fg set)

    // Inline code: full accent (no background, so full cyan reads fine)
    skin.inline_code.set_fgbg(accent, CColor::Reset);

    // Code blocks: opposite-mode accent via set_fgbg (LineStyle, not CompoundStyle)
    skin.code_block.set_fgbg(code_block_color, CColor::Reset);

    // Structural chrome: DarkGrey (dim, recedes, no hue)
    skin.quote_mark = termimad::StyledChar::from_fg_char(CColor::DarkGrey, '▐');
    skin.horizontal_rule = termimad::StyledChar::from_fg_char(CColor::DarkGrey, '─');

    skin
}

impl Renderer {
    pub fn new(theme: Theme) -> Self {
        let skin = themed_skin(&theme);
        Self {
            line_buf: String::new(),
            block_buf: String::new(),
            in_fence: false,
            skin,
            theme,
            pending_tool: None,
            pending_ts: None,
            turn_blocks: Vec::new(),
            block_num: 0,
        }
    }

    fn push_line(&mut self, line: &str) {
        let is_fence_marker = line.trim_start().starts_with("```");

        if self.in_fence {
            self.append_to_block(line);
            if is_fence_marker {
                self.in_fence = false;
                self.render_block();
            }
        } else if line.is_empty() {
            self.render_block();
            println!();
        } else {
            self.append_to_block(line);
            if is_fence_marker {
                self.in_fence = true;
            }
        }
    }

    fn append_to_block(&mut self, line: &str) {
        if !self.block_buf.is_empty() {
            self.block_buf.push('\n');
        }
        self.block_buf.push_str(line);
    }

    fn render_block(&mut self) {
        if !self.block_buf.is_empty() {
            let block = std::mem::take(&mut self.block_buf);
            self.block_num += 1;
            self.turn_blocks.push(block.clone());
            // Capture termimad's rendered output so we can prefix every line:
            // first line gets "  N│", continuation lines get "    " (same width).
            // No space after │ — content starts immediately.
            // Subtract gutter width (5: "  N│ ") so termimad wraps at word boundaries
            // before the gutter is added, preventing mid-word terminal wraps.
            let term_w = termimad::terminal_size().0 as usize;
            let content_w = term_w.saturating_sub(6); // 5-char gutter + 1 space
            let fmt = self.skin.text(&block, Some(content_w));
            let rendered = format!("{}", fmt);
            let mut first = true;
            for line in rendered.lines() {
                if first {
                    print!("{}{:>3}\u{2502}{} {}", DIM_FG, self.block_num, RESET, line);
                    first = false;
                } else {
                    print!("     {}", line);
                }
                println!();
            }
            if first {
                println!("{}{:>3}\u{2502}{}", DIM_FG, self.block_num, RESET);
            }
            let _ = std::io::stdout().flush();
        }
    }

    pub fn handle_line(&mut self, json_line: &str) -> RenderAction {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json_line) else {
            return RenderAction::Continue;
        };
        match v["type"].as_str() {
            Some("text_delta") => {
                let content = v["content"].as_str().unwrap_or("");
                self.line_buf.push_str(content);
                while let Some(pos) = self.line_buf.find('\n') {
                    let line = self.line_buf[..pos].to_string();
                    self.line_buf = self.line_buf[pos + 1..].to_string();
                    self.push_line(&line);
                }
                RenderAction::Continue
            }
            Some("tool_call") => {
                let name = v["name"].as_str().unwrap_or("?").to_string();
                eprintln!("{}  \u{27f3} {}(\u{2026}){}", DIM_FG, name, RESET);
                self.pending_tool = Some(name);
                self.pending_ts = v["ts"].as_u64();
                RenderAction::Continue
            }
            Some("tool_result") => {
                let had_pending = self.pending_tool.is_some();
                let name = self.pending_tool.take()
                    .or_else(|| v["name"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "?".to_string());
                let ok = v["ok"].as_bool().unwrap_or(true);
                let elapsed = self.pending_ts.take()
                    .zip(v["ts"].as_u64())
                    .map(|(start, end)| end.saturating_sub(start));
                if had_pending {
                    eprint!("\x1b[1A\x1b[2K");
                }
                let (color, mark) = if ok {
                    (self.theme.success.fg(), '\u{2713}')
                } else {
                    (self.theme.danger.fg(), '\u{2717}')
                };
                let timing = elapsed.map(|ms| format!(" \u{b7} {}ms", ms)).unwrap_or_default();
                eprintln!("{}  {} {}{}{}", color, mark, name, timing, RESET);
                if let Some(output) = v["output"].as_str() {
                    let lines: Vec<&str> = output.lines()
                        .filter(|l| !l.trim().is_empty())
                        .collect();
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
                RenderAction::Continue
            }
            Some("permissions") => {
                self.flush();
                let items: Vec<PermissionItem> = v["items"]
                    .as_array()
                    .map(|arr| arr.iter()
                        .filter_map(|i| serde_json::from_value(i.clone()).ok())
                        .collect())
                    .unwrap_or_default();
                RenderAction::PermissionNeeded(items)
            }
            Some("clarify") => {
                self.flush();
                let items: Vec<ClarifyItem> = v["items"]
                    .as_array()
                    .map(|arr| arr.iter()
                        .filter_map(|i| serde_json::from_value(i.clone()).ok())
                        .collect())
                    .unwrap_or_default();
                RenderAction::ClarifyNeeded(items)
            }
            Some("inform") => {
                self.flush();
                let items: Vec<InformItem> = v["items"]
                    .as_array()
                    .map(|arr| arr.iter()
                        .filter_map(|i| serde_json::from_value(i.clone()).ok())
                        .collect())
                    .unwrap_or_default();
                RenderAction::InformNeeded(items)
            }
            Some("turn_complete") => {
                self.flush();
                let blocks: Vec<String> = v["blocks"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|b| b.as_str().map(str::to_string)).collect())
                    .unwrap_or_else(|| self.turn_blocks.clone());
                RenderAction::TurnComplete(blocks)
            }
            Some("error") => {
                self.flush();
                let msg = v["message"].as_str().unwrap_or("unknown error");
                eprintln!("{}[error] {}{}", self.theme.danger.fg(), msg, RESET);
                RenderAction::Continue
            }
            _ => RenderAction::Continue,
        }
    }

    pub fn flush(&mut self) {
        if !self.line_buf.is_empty() {
            let partial = std::mem::take(&mut self.line_buf);
            self.append_to_block(&partial);
        }
        self.render_block();
        self.in_fence = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer() -> Renderer {
        Renderer::new(Theme::dark())
    }

    #[test]
    fn text_delta_accumulates_and_flushes_on_newline() {
        let mut r = renderer();
        r.handle_line(r#"{"type":"text_delta","content":"hello ","ts":1}"#);
        assert_eq!(r.line_buf, "hello ");
        r.handle_line(r#"{"type":"text_delta","content":"world\n","ts":2}"#);
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let mut r = renderer();
        r.handle_line(r#"{"type":"session_start","id":"s1","ts":1}"#);
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn invalid_json_is_ignored() {
        let mut r = renderer();
        r.handle_line("not json at all");
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn turn_complete_flushes_remaining_buffer() {
        let mut r = renderer();
        r.handle_line(r#"{"type":"text_delta","content":"partial","ts":1}"#);
        assert_eq!(r.line_buf, "partial");
        r.handle_line(r#"{"type":"turn_complete","ts":2}"#);
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn permissions_event_returns_permission_needed() {
        let mut r = renderer();
        let json = serde_json::json!({
            "type": "permissions",
            "items": [
                {
                    "id": "tc-1",
                    "cmd": "cargo test",
                    "reason": "shell requires permission",
                    "patterns": ["cargo *", "cargo test"]
                }
            ],
            "ts": 1
        });
        let action = r.handle_line(&json.to_string());
        match action {
            RenderAction::PermissionNeeded(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].cmd, "cargo test");
            }
            _ => panic!("expected PermissionNeeded"),
        }
    }

    #[test]
    fn turn_complete_returns_turn_complete_action() {
        let mut r = renderer();
        let action = r.handle_line(r#"{"type":"turn_complete","ts":2}"#);
        assert!(matches!(action, RenderAction::TurnComplete(_)));
    }

    #[test]
    fn text_delta_returns_continue() {
        let mut r = renderer();
        let action = r.handle_line(r#"{"type":"text_delta","content":"hello","ts":1}"#);
        assert!(matches!(action, RenderAction::Continue));
    }

    #[test]
    fn turn_blocks_populated_on_paragraph() {
        let mut r = renderer();
        // Paragraph: text line then empty line flushes the block
        r.handle_line(r#"{"type":"text_delta","content":"hello world\n","ts":1}"#);
        r.handle_line(r#"{"type":"text_delta","content":"\n","ts":2}"#);
        assert_eq!(r.turn_blocks.len(), 1);
        assert_eq!(r.turn_blocks[0], "hello world");
    }

    #[test]
    fn turn_blocks_numbered_sequentially() {
        let mut r = renderer();
        r.handle_line(r#"{"type":"text_delta","content":"para one\n\n","ts":1}"#);
        r.handle_line(r#"{"type":"text_delta","content":"para two\n\n","ts":2}"#);
        assert_eq!(r.turn_blocks.len(), 2);
        assert_eq!(r.block_num, 2);
    }

    #[test]
    fn turn_complete_flushes_to_turn_blocks() {
        let mut r = renderer();
        r.handle_line(r#"{"type":"text_delta","content":"trailing","ts":1}"#);
        // No trailing newline — flush() captures it
        r.handle_line(r#"{"type":"turn_complete","ts":2}"#);
        assert_eq!(r.turn_blocks.len(), 1);
        assert_eq!(r.turn_blocks[0], "trailing");
    }
}
