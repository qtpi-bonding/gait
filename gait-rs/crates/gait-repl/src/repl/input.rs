pub fn is_bang_command(s: &str) -> bool {
    s.starts_with('!')
}

pub fn bang_command_str(s: &str) -> &str {
    s.strip_prefix("! ")
        .or_else(|| s.strip_prefix('!'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bang_command_detected() {
        assert!(is_bang_command("! ls"));
        assert!(is_bang_command("!ls"));
        assert!(!is_bang_command("ls"));
        assert!(!is_bang_command("/new"));
    }

    #[test]
    fn bang_command_extracts_cmd() {
        assert_eq!(bang_command_str("! ls -la"), "ls -la");
        assert_eq!(bang_command_str("!pwd"), "pwd");
    }
}
