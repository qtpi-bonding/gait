use std::io::{BufRead, Write};
use crate::session::transcript::{InformItem, InformReplyItem};
use crate::theme::{Theme, RESET, DIM_FG};

pub fn prompt_inform(items: &[InformItem], theme: &Theme) -> Vec<InformReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}→ {}{}", theme.accent.fg(), item.content, RESET);
        if let Some(ctx) = &item.context {
            eprintln!("  {}context: {}{}", DIM_FG, ctx, RESET);
        }
        eprintln!("  {}Press Enter to confirm, or type a correction:{}", DIM_FG, RESET);
        eprint!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        eprint!("{}", RESET);
        let response = input.trim().to_string();
        replies.push(InformReplyItem {
            id: item.id.clone(),
            content: if response.is_empty() { "understood".into() } else { response },
        });
    }
    replies
}
