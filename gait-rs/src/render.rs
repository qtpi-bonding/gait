use termimad::MadSkin;
use termimad::crossterm::style::Color as CColor;
use crate::session::transcript::{PermissionItem, ClarifyItem, InformItem};
use crate::theme::{Theme, RESET, DIM, DIM_FG};

pub enum RenderAction {
    Continue,
    PermissionNeeded(Vec<PermissionItem>),
    ClarifyNeeded(Vec<ClarifyItem>),
    InformNeeded(Vec<InformItem>),
    TurnComplete,
}

pub struct Renderer {
    pub line_buf: String,
    block_buf: String,
    in_fence: bool,
    skin: MadSkin,
    pub theme: Theme,
    pending_tool: Option<String>,
    pending_ts: Option<u64>,
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
            self.skin.print_text(&block);
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
                        _ => {
                            eprintln!("{}     {}{}", DIM_FG, lines[0], RESET);
                            eprintln!("{}     \u{2026}{}", DIM_FG, RESET);
                            eprintln!("{}     {}{}", DIM_FG, lines[lines.len() - 1], RESET);
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
                RenderAction::TurnComplete
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
        assert!(matches!(action, RenderAction::TurnComplete));
    }

    #[test]
    fn text_delta_returns_continue() {
        let mut r = renderer();
        let action = r.handle_line(r#"{"type":"text_delta","content":"hello","ts":1}"#);
        assert!(matches!(action, RenderAction::Continue));
    }
}
