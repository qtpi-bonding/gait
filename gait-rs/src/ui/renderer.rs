use std::io::Write;
use termimad::MadSkin;
use termimad::crossterm::style::Color as CColor;
use crate::session::transcript::{self as tx, PermissionItem, ClarifyItem, InformItem};
use crate::proto::gait::log::v1 as pb;
use crate::theme::{Theme, RESET, DIM_FG};

pub enum RenderAction {
    Continue,
    PermissionNeeded(Vec<PermissionItem>),
    ClarifyNeeded(Vec<ClarifyItem>),
    InformNeeded(Vec<InformItem>),
    TurnComplete(Vec<String>),
}

// ── proto → transcript item converters ────────────────────────────────────────

fn perm_to_tx(p: &pb::PermissionItem) -> tx::PermissionItem {
    tx::PermissionItem {
        id: p.id.clone(),
        cmd: p.cmd.clone(),
        reason: p.reason.clone(),
        patterns: p.patterns.clone(),
        tool_name: p.tool_name.clone(),
        tool_args: p.tool_args.clone()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null),
    }
}

fn clarify_to_tx(c: &pb::ClarifyItem) -> tx::ClarifyItem {
    tx::ClarifyItem {
        id: c.id.clone(),
        content: c.content.clone(),
        why: c.why.clone(),
        kind: match pb::ClarifyKind::try_from(c.kind) {
            Ok(pb::ClarifyKind::Is) => "is".to_string(),
            Ok(pb::ClarifyKind::Ought) => "ought".to_string(),
            _ => String::new(),
        },
    }
}

fn inform_to_tx(i: &pb::InformItem) -> tx::InformItem {
    tx::InformItem {
        id: i.id.clone(),
        content: i.content.clone(),
        context: i.context.clone(),
    }
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

    /// Line-buffer streaming text, flushing each completed line through
    /// `push_line`. `pub(crate)` so the in-process REPL (Task 7b) can paint
    /// `TextDelta` stream items directly without round-tripping through JSON.
    pub(crate) fn push_text(&mut self, content: &str) {
        self.line_buf.push_str(content);
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].to_string();
            self.line_buf = self.line_buf[pos + 1..].to_string();
            self.push_line(&line);
        }
    }

    /// Render the DIM spinner line for a tool call and record it as pending.
    /// Shared by the proto path (`handle_event`) and the transitional flat
    /// path so the two render identically.
    fn render_tool_call(&mut self, name: &str) {
        eprintln!("{}  \u{27f3} {}(\u{2026}){}", DIM_FG, name, RESET);
        self.pending_tool = Some(name.to_string());
    }

    /// Render a tool result: clear the pending spinner line, print the ✓/✗
    /// status with the tool name (and timing if known), then a digest of the
    /// output. Shared by the proto and flat paths.
    fn render_tool_result(&mut self, ok: bool, output: &str, elapsed_ms: Option<u64>) {
        let had_pending = self.pending_tool.is_some();
        let name = self.pending_tool.take().unwrap_or_else(|| "?".to_string());
        if had_pending {
            eprint!("\x1b[1A\x1b[2K");
        }
        let (color, mark) = if ok {
            (self.theme.success.fg(), '\u{2713}')
        } else {
            (self.theme.danger.fg(), '\u{2717}')
        };
        let timing = elapsed_ms.map(|ms| format!(" \u{b7} {}ms", ms)).unwrap_or_default();
        eprintln!("{}  {} {}{}{}", color, mark, name, timing, RESET);
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

    /// Dispatch a decoded proto [`pb::Event`] to the appropriate rendering.
    /// `pub(crate)` so the in-process REPL (Task 7b) can feed events directly.
    pub(crate) fn handle_event(&mut self, ev: &pb::Event) -> RenderAction {
        let Some(pb::event::Body::Payload(p)) = &ev.body else {
            return RenderAction::Continue;
        };
        match &p.kind {
            // AssistantText is the FULL accumulated turn text. The live stream
            // already painted it via text deltas — re-rendering here would
            // DOUBLE the text. No-op.
            Some(pb::payload::Kind::AssistantText(_)) => RenderAction::Continue,

            Some(pb::payload::Kind::ToolCall(tc)) => {
                // proto ToolCall has no timing; timing comes from ToolResult.duration.
                self.render_tool_call(&tc.name);
                RenderAction::Continue
            }
            Some(pb::payload::Kind::ToolResult(tr)) => {
                let elapsed_ms = tr.duration.as_ref()
                    .map(|d| (d.seconds as u64) * 1000 + (d.nanos as u64) / 1_000_000);
                let ok = tr.process.as_ref().map_or(true, |pr| pr.exit_code == 0);
                self.render_tool_result(ok, &tr.output, elapsed_ms);
                RenderAction::Continue
            }
            Some(pb::payload::Kind::Permissions(pm)) => {
                self.flush();
                RenderAction::PermissionNeeded(pm.items.iter().map(perm_to_tx).collect())
            }
            Some(pb::payload::Kind::Clarify(c)) => {
                self.flush();
                RenderAction::ClarifyNeeded(c.items.iter().map(clarify_to_tx).collect())
            }
            Some(pb::payload::Kind::Inform(inf)) => {
                self.flush();
                RenderAction::InformNeeded(inf.items.iter().map(inform_to_tx).collect())
            }
            Some(pb::payload::Kind::TurnComplete(tc)) => {
                self.flush();
                let blocks = if tc.blocks.is_empty() {
                    self.turn_blocks.clone()
                } else {
                    tc.blocks.clone()
                };
                RenderAction::TurnComplete(blocks)
            }
            Some(pb::payload::Kind::Error(e)) => {
                self.flush();
                eprintln!("{}[error] {}{}", self.theme.danger.fg(), e.message, RESET);
                RenderAction::Continue
            }
            // SessionStart, Resolution, UserTurn, Raw, replies, etc. — no-op.
            _ => RenderAction::Continue,
        }
    }

    /// Render one line of the `gait run` NDJSON stdout stream: a flat `text_delta`
    /// line or a proto `Event` line. Used for rendering a serialized event stream
    /// (e.g. log replay or external consumers); the in-process REPL uses
    /// `handle_event` directly instead.
    pub fn handle_line(&mut self, json_line: &str) -> RenderAction {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json_line) else {
            return RenderAction::Continue;
        };
        // Flat legacy text_delta (still emitted on stdout by the CLI channel drain).
        if v.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
            self.push_text(v["content"].as_str().unwrap_or(""));
            return RenderAction::Continue;
        }
        // Otherwise a proto Event line (or an unrelated flat line like
        // session_id, which fails proto parse → Continue).
        match serde_json::from_str::<pb::Event>(json_line) {
            Ok(ev) => self.handle_event(&ev),
            Err(_) => RenderAction::Continue,
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
    use crate::proto::events;

    fn renderer() -> Renderer {
        Renderer::new(Theme::dark())
    }

    /// Mint a full proto `Event` from an event body for feeding handle_event.
    fn ev(body: pb::event::Body) -> pb::Event {
        crate::proto::envelope::mint(body, None)
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
        let tc = events::turn_complete(
            &tx::TurnUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 },
            "h".into(), false, None, vec![], &tx::TurnOutput { message: "done".into(), result: None },
        );
        r.handle_event(&ev(tc));
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn handle_event_permissions_returns_permission_needed() {
        let mut r = renderer();
        let body = events::permissions(&[tx::PermissionItem {
            id: "tc-1".into(),
            cmd: "cargo test".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["cargo *".into(), "cargo test".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "cargo test"}),
        }]);
        let action = r.handle_event(&ev(body));
        match action {
            RenderAction::PermissionNeeded(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].cmd, "cargo test");
                assert_eq!(items[0].tool_name, "shell");
                assert_eq!(items[0].tool_args["cmd"], "cargo test");
            }
            _ => panic!("expected PermissionNeeded"),
        }
    }

    #[test]
    fn handle_event_turn_complete_returns_blocks() {
        let mut r = renderer();
        let body = events::turn_complete(
            &tx::TurnUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 },
            "h".into(), false, None, vec!["block one".into()],
            &tx::TurnOutput { message: "done".into(), result: None },
        );
        match r.handle_event(&ev(body)) {
            RenderAction::TurnComplete(blocks) => assert_eq!(blocks, vec!["block one".to_string()]),
            _ => panic!("expected TurnComplete"),
        }
    }

    #[test]
    fn handle_event_tool_call_sets_pending() {
        let mut r = renderer();
        let body = events::tool_call("call-1".into(), "read".into(), &serde_json::json!({"file_path": "a.rs"}));
        let action = r.handle_event(&ev(body));
        assert!(matches!(action, RenderAction::Continue));
        assert_eq!(r.pending_tool.as_deref(), Some("read"));
    }

    #[test]
    fn handle_event_tool_result_renders() {
        let mut r = renderer();

        // ok case: process None → ok defaults true; with a 1.5s duration → 1500ms.
        r.pending_tool = Some("read".into());
        let duration = pbjson_types::Duration { seconds: 1, nanos: 500_000_000 };
        let ok_body = events::tool_result("call-1".into(), "line one\nline two".into(), Some(duration), None);
        let action = r.handle_event(&ev(ok_body));
        assert!(matches!(action, RenderAction::Continue));
        // pending_tool consumed by render_tool_result.
        assert_eq!(r.pending_tool, None);

        // failure case: process with non-zero exit_code → ok false. No duration.
        r.pending_tool = Some("shell".into());
        let fail_body = events::tool_result(
            "call-2".into(),
            "boom".into(),
            None,
            Some(pb::ProcessResult { exit_code: 1, timed_out: false }),
        );
        let action = r.handle_event(&ev(fail_body));
        assert!(matches!(action, RenderAction::Continue));
        assert_eq!(r.pending_tool, None);
    }

    #[test]
    fn handle_event_assistant_text_is_noop() {
        let mut r = renderer();
        r.turn_blocks.push("prior".into());
        let before_blocks = r.turn_blocks.clone();
        let action = r.handle_event(&ev(events::assistant_text("full accumulated text".into())));
        assert!(matches!(action, RenderAction::Continue));
        // No double-render: line_buf untouched, turn_blocks unchanged.
        assert_eq!(r.line_buf, "");
        assert_eq!(r.turn_blocks, before_blocks);
    }

    #[test]
    fn clarify_kind_maps_is_and_ought() {
        let mut r = renderer();
        let body = events::clarify(&[
            tx::ClarifyItem { id: "1".into(), content: "c1".into(), why: "w1".into(), kind: "is".into() },
            tx::ClarifyItem { id: "2".into(), content: "c2".into(), why: "w2".into(), kind: "ought".into() },
        ]);
        match r.handle_event(&ev(body)) {
            RenderAction::ClarifyNeeded(items) => {
                assert_eq!(items[0].kind, "is");
                assert_eq!(items[1].kind, "ought");
            }
            _ => panic!("expected ClarifyNeeded"),
        }
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
        // No trailing newline — flush() (via handle_event TurnComplete) captures it.
        let tc = events::turn_complete(
            &tx::TurnUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 },
            "h".into(), false, None, vec![], &tx::TurnOutput { message: "done".into(), result: None },
        );
        r.handle_event(&ev(tc));
        assert_eq!(r.turn_blocks.len(), 1);
        assert_eq!(r.turn_blocks[0], "trailing");
    }
}
