use std::io::{BufRead, Write};
use crate::session::transcript::PermissionItem;
use crate::theme::{Theme, RESET, DIM_FG};

pub fn prompt_permission(
    item: &PermissionItem,
    label: Option<&str>,
    theme: &Theme,
) -> crate::permission::PermissionChoice {
    eprintln!();
    if let Some(l) = label {
        eprintln!("{}⟳ {}  {}({}){}", theme.accent.fg(), item.cmd, DIM_FG, l, RESET);
    } else {
        eprintln!("{}⟳ {}{}", theme.accent.fg(), item.cmd, RESET);
    }
    eprintln!();
    for (i, pat) in item.patterns.iter().enumerate() {
        eprintln!("  {}[{}] {}{}", DIM_FG, i + 1, pat, RESET);
    }
    eprintln!();
    eprintln!("  {}[o] once   [r] reject{}", DIM_FG, RESET);
    loop {
        eprint!("  {}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().lock().read_line(&mut input) {
            Ok(0) | Err(_) => {
                eprint!("{}", RESET);
                return crate::permission::PermissionChoice::Reject;
            }
            Ok(_) => { eprint!("{}", RESET); }
        }
        match parse_permission_response(input.trim(), &item.patterns) {
            Some(c) => return c,
            None => eprintln!("  unrecognized — try 1..{}, o, or r", item.patterns.len()),
        }
    }
}

fn parse_permission_response(
    input: &str,
    patterns: &[String],
) -> Option<crate::permission::PermissionChoice> {
    use crate::permission::PermissionChoice;
    match input.trim() {
        "r" | "reject" => return Some(PermissionChoice::Reject),
        "o" | "once" => {
            let pat = patterns.last()?;
            return Some(PermissionChoice::Once(pat.clone()));
        }
        _ => {}
    }
    if let Ok(n) = input.trim().parse::<usize>() {
        if n >= 1 && n <= patterns.len() {
            return Some(PermissionChoice::Agent(patterns[n - 1].clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permission_response_number() {
        let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test *".to_string()];
        assert_eq!(
            parse_permission_response("1", &patterns),
            Some(crate::permission::PermissionChoice::Agent("shell:cargo *".to_string()))
        );
        assert_eq!(
            parse_permission_response("2", &patterns),
            Some(crate::permission::PermissionChoice::Agent("shell:cargo test *".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_once() {
        let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test".to_string()];
        assert_eq!(
            parse_permission_response("o", &patterns),
            Some(crate::permission::PermissionChoice::Once("shell:cargo test".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_reject() {
        assert_eq!(
            parse_permission_response("r", &[]),
            Some(crate::permission::PermissionChoice::Reject)
        );
    }

    #[test]
    fn parse_permission_response_invalid() {
        let p = vec!["shell:cargo *".to_string()];
        assert_eq!(parse_permission_response("99", &p), None);
        assert_eq!(parse_permission_response("xyz", &p), None);
        assert_eq!(parse_permission_response("", &p), None);
    }
}
