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
    eprintln!("  {}number = once · s = session · a = agent · r = reject   (e.g. s2, 2s){}", DIM_FG, RESET);
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
            None => eprintln!("  unrecognized — e.g. 2, s, s2, 2a, r (patterns 1..{})", item.patterns.len()),
        }
    }
}

/// Two-axis grammar: an optional scope letter (`o`/`s`/`a`) and an optional
/// pattern number, in either order. The number picks the pattern (default: the
/// broadest, `patterns.last()`); the letter picks the scope (default, i.e. a
/// bare number, is Once). `r` rejects. No `g`/global — global stays config-only.
///   `2` → once-pattern-2 · `s2`/`2s` → session · `a1`/`1a` → agent · `s` → session-broadest
pub(crate) fn parse_permission_response(
    input: &str,
    patterns: &[String],
) -> Option<crate::permission::PermissionChoice> {
    use crate::permission::PermissionChoice;
    let s = input.trim().to_lowercase();
    if s == "r" || s == "reject" { return Some(PermissionChoice::Reject); }
    if s.is_empty() { return None; }

    // Tokenize into at most one scope letter + an optional digit run, any order.
    let mut letter: Option<char> = None;
    let mut digits = String::new();
    for c in s.chars() {
        match c {
            'o' | 's' | 'a' if letter.is_none() => letter = Some(c),
            d if d.is_ascii_digit() => digits.push(d),
            _ => return None, // a second letter or any junk → unrecognized
        }
    }

    // Number picks the pattern; absent → broadest. Out-of-range → None.
    let pattern = if digits.is_empty() {
        patterns.last()?.clone()
    } else {
        let n: usize = digits.parse().ok()?;
        if n < 1 || n > patterns.len() { return None; }
        patterns[n - 1].clone()
    };

    Some(match letter {
        Some('s') => PermissionChoice::Session(pattern),
        Some('a') => PermissionChoice::Agent(pattern),
        Some('o') => PermissionChoice::Once(pattern),
        None => {
            // a bare letter was handled above; here digits are guaranteed present
            PermissionChoice::Once(pattern)
        }
        Some(_) => return None, // unreachable: tokenizer only sets o/s/a
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::permission::PermissionChoice::{Once, Session, Agent, Reject};

    fn pats() -> Vec<String> {
        vec!["shell:cargo test *".to_string(), "shell:cargo *".to_string()]
    }

    #[test]
    fn bare_number_is_once() {
        assert_eq!(parse_permission_response("1", &pats()), Some(Once("shell:cargo test *".into())));
        assert_eq!(parse_permission_response("2", &pats()), Some(Once("shell:cargo *".into())));
    }

    #[test]
    fn session_pattern_either_order() {
        assert_eq!(parse_permission_response("s2", &pats()), Some(Session("shell:cargo *".into())));
        assert_eq!(parse_permission_response("2s", &pats()), Some(Session("shell:cargo *".into())));
    }

    #[test]
    fn agent_pattern_either_order() {
        assert_eq!(parse_permission_response("a1", &pats()), Some(Agent("shell:cargo test *".into())));
        assert_eq!(parse_permission_response("1a", &pats()), Some(Agent("shell:cargo test *".into())));
    }

    #[test]
    fn bare_letter_uses_broadest() {
        // broadest = patterns.last()
        assert_eq!(parse_permission_response("s", &pats()), Some(Session("shell:cargo *".into())));
        assert_eq!(parse_permission_response("a", &pats()), Some(Agent("shell:cargo *".into())));
        assert_eq!(parse_permission_response("o", &pats()), Some(Once("shell:cargo *".into())));
    }

    #[test]
    fn reject_short_circuits() {
        assert_eq!(parse_permission_response("r", &[]), Some(Reject));
        assert_eq!(parse_permission_response("reject", &pats()), Some(Reject));
    }

    #[test]
    fn out_of_range_and_junk_are_none() {
        let p = pats();
        assert_eq!(parse_permission_response("9", &p), None);   // out of range
        assert_eq!(parse_permission_response("0", &p), None);   // patterns are 1-based
        assert_eq!(parse_permission_response("sa", &p), None);  // two letters
        assert_eq!(parse_permission_response("sx", &p), None);  // junk letter
        assert_eq!(parse_permission_response("", &p), None);    // empty
        assert_eq!(parse_permission_response("s", &[]), None);  // no patterns → no broadest
    }
}
