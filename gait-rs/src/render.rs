use termimad::MadSkin;

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

    /// Parse one NDJSON line and render to terminal.
    pub fn handle_line(&mut self, json_line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json_line) else { return };
        match v["type"].as_str() {
            Some("text_delta") => {
                let content = v["content"].as_str().unwrap_or("");
                self.line_buf.push_str(content);
                while let Some(pos) = self.line_buf.find('\n') {
                    let line = self.line_buf[..pos].to_string();
                    self.line_buf = self.line_buf[pos + 1..].to_string();
                    self.skin.print_text(&line);
                }
            }
            Some("tool_call") => {
                let name = v["name"].as_str().unwrap_or("?");
                eprintln!("\x1b[2m  \u{27f3} {}(\u{2026})\x1b[0m", name);
            }
            Some("turn_complete") => {
                self.flush();
            }
            Some("error") => {
                self.flush();
                let msg = v["message"].as_str().unwrap_or("unknown error");
                eprintln!("\x1b[31m[error] {}\x1b[0m", msg);
            }
            _ => {}
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
}
