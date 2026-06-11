use std::io::{BufRead, Write};
use crate::session::transcript::{ClarifyItem, ClarifyReplyItem};
use crate::theme::{Theme, RESET, DIM_FG};

pub fn prompt_clarify(items: &[ClarifyItem], theme: &Theme) -> Vec<ClarifyReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}? [{}] {}{}", theme.accent.fg(), item.kind, item.content, RESET);
        eprintln!("  {}why: {}{}", DIM_FG, item.why, RESET);
        eprint!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        eprint!("{}", RESET);
        replies.push(ClarifyReplyItem {
            id: item.id.clone(),
            content: input.trim().to_string(),
            reasoning: None,
        });
    }
    replies
}
