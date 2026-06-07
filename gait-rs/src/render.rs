use termimad::MadSkin;
use crate::session::transcript::PermissionItem;

pub enum RenderAction {
    Continue,
    PermissionNeeded(Vec<PermissionItem>),
    TurnComplete,
}

pub struct Renderer {
    pub line_buf: String,
    skin: MadSkin,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            line_buf: String::new(),
            skin: MadSkin::default(),
        }
    }

    /// Parse one NDJSON line and render to terminal. Returns a RenderAction
    /// so callers can intercept events that require interactive handling.
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
                    self.skin.print_text(&line);
                }
                RenderAction::Continue
            }
            Some("tool_call") => {
                let name = v["name"].as_str().unwrap_or("?");
                eprintln!("\x1b[2m  \u{27f3} {}(\u{2026})\x1b[0m", name);
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
            Some("turn_complete") => {
                self.flush();
                RenderAction::TurnComplete
            }
            Some("error") => {
                self.flush();
                let msg = v["message"].as_str().unwrap_or("unknown error");
                eprintln!("\x1b[31m[error] {}\x1b[0m", msg);
                RenderAction::Continue
            }
            _ => RenderAction::Continue,
        }
    }

    /// Flush any partial line remaining in the buffer.
    pub fn flush(&mut self) {
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.skin.print_text(&line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_accumulates_and_flushes_on_newline() {
        let mut r = Renderer::new();
        r.handle_line(r#"{"type":"text_delta","content":"hello ","ts":1}"#);
        assert_eq!(r.line_buf, "hello ");
        r.handle_line(r#"{"type":"text_delta","content":"world\n","ts":2}"#);
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn unknown_event_types_are_ignored() {
        let mut r = Renderer::new();
        r.handle_line(r#"{"type":"session_start","id":"s1","ts":1}"#);
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn invalid_json_is_ignored() {
        let mut r = Renderer::new();
        r.handle_line("not json at all");
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn turn_complete_flushes_remaining_buffer() {
        let mut r = Renderer::new();
        r.handle_line(r#"{"type":"text_delta","content":"partial","ts":1}"#);
        assert_eq!(r.line_buf, "partial");
        r.handle_line(r#"{"type":"turn_complete","ts":2}"#);
        assert_eq!(r.line_buf, "");
    }

    #[test]
    fn permissions_event_returns_permission_needed() {
        let mut r = Renderer::new();
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
                assert_eq!(items[0].patterns, vec!["cargo *", "cargo test"]);
            }
            _ => panic!("expected PermissionNeeded"),
        }
    }

    #[test]
    fn turn_complete_returns_turn_complete_action() {
        let mut r = Renderer::new();
        let action = r.handle_line(r#"{"type":"turn_complete","ts":2}"#);
        assert!(matches!(action, RenderAction::TurnComplete));
    }

    #[test]
    fn text_delta_returns_continue() {
        let mut r = Renderer::new();
        let action = r.handle_line(r#"{"type":"text_delta","content":"hello","ts":1}"#);
        assert!(matches!(action, RenderAction::Continue));
    }
}
