/// Lines to render for a permission preview: a leading blank entry (to separate
/// the diff from the subject line) followed by one entry per preview line.
/// `None` or an empty preview → empty vec (nothing rendered). The caller applies
/// its own per-line ANSI styling and supplies the trailing spacing before the patterns.
pub fn preview_block(preview: Option<&str>) -> Vec<String> {
    match preview {
        Some(p) if !p.is_empty() => {
            let mut out = vec![String::new()];
            out.extend(p.lines().map(|l| l.to_string()));
            out
        }
        _ => Vec::new(),
    }
}

/// Two-axis grammar: an optional scope letter (`o`/`s`/`a`) and an optional
/// pattern number, in either order. The number picks the pattern (default: the
/// broadest, `patterns.last()`); the letter picks the scope (default, i.e. a
/// bare number, is Once). `r` rejects; `rs`[N] rejects for the rest of this
/// session (the deny-side mirror of `s`[N] — no letter-order flexibility here,
/// `rs` only, since `sr`/`r`+digit would collide with the scope tokenizer). No
/// `g`/global — global stays config-only, for both allow and reject.
///   `2` → once-pattern-2 · `s2`/`2s` → session · `a1`/`1a` → agent · `s` → session-broadest
///   `rs` → reject-session-broadest · `rs2` → reject-session-pattern-2
pub(crate) fn parse_permission_response(
    input: &str,
    patterns: &[String],
) -> Option<gait_core::permissions::PermissionChoice> {
    use gait_core::permissions::PermissionChoice;
    let s = input.trim().to_lowercase();
    if s == "r" || s == "deny" || s == "reject" {
        return Some(PermissionChoice::Deny);
    }
    // "rs"[N] rejects for the rest of this session — the deny-side mirror of
    // "s"[N] below. Checked before the scope-letter tokenizer since 's' alone
    // is reserved there for Allow-session.
    if let Some(rest) = s.strip_prefix("rs") {
        let pattern = if rest.is_empty() {
            patterns.last()?.clone()
        } else {
            let n: usize = rest.parse().ok()?;
            if n < 1 || n > patterns.len() {
                return None;
            }
            patterns[patterns.len() - n].clone()
        };
        return Some(PermissionChoice::DenySession(pattern));
    }
    if s.is_empty() {
        return None;
    }

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

    // Number picks the pattern. 1 = most specific (exact), len = broadest.
    // Absent → most specific (patterns.last()). Out-of-range → None.
    let pattern = if digits.is_empty() {
        patterns.last()?.clone()
    } else {
        let n: usize = digits.parse().ok()?;
        if n < 1 || n > patterns.len() {
            return None;
        }
        patterns[patterns.len() - n].clone()
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

    use gait_core::permissions::PermissionChoice::{Agent, Deny, Once, Session};

    // Matches command_patterns order: broad first, specific last.
    // [1] = most specific (patterns.last()), [2] = broadest (patterns.first()).
    fn pats() -> Vec<String> {
        vec![
            "shell:cargo *".to_string(),      // broadest (index 0)
            "shell:cargo test *".to_string(), // most specific (index 1, last)
        ]
    }

    #[test]
    fn bare_number_is_once() {
        // 1 = most specific (patterns.last()), 2 = broadest (patterns.first())
        assert_eq!(
            parse_permission_response("1", &pats()),
            Some(Once("shell:cargo test *".into()))
        );
        assert_eq!(
            parse_permission_response("2", &pats()),
            Some(Once("shell:cargo *".into()))
        );
    }

    #[test]
    fn session_pattern_either_order() {
        assert_eq!(
            parse_permission_response("s2", &pats()),
            Some(Session("shell:cargo *".into()))
        );
        assert_eq!(
            parse_permission_response("2s", &pats()),
            Some(Session("shell:cargo *".into()))
        );
    }

    #[test]
    fn agent_pattern_either_order() {
        assert_eq!(
            parse_permission_response("a1", &pats()),
            Some(Agent("shell:cargo test *".into()))
        );
        assert_eq!(
            parse_permission_response("1a", &pats()),
            Some(Agent("shell:cargo test *".into()))
        );
    }

    #[test]
    fn bare_letter_uses_most_specific() {
        // No number → most specific = patterns.last()
        assert_eq!(
            parse_permission_response("s", &pats()),
            Some(Session("shell:cargo test *".into()))
        );
        assert_eq!(
            parse_permission_response("a", &pats()),
            Some(Agent("shell:cargo test *".into()))
        );
        assert_eq!(
            parse_permission_response("o", &pats()),
            Some(Once("shell:cargo test *".into()))
        );
    }

    #[test]
    fn reject_short_circuits() {
        assert_eq!(parse_permission_response("r", &[]), Some(Deny));
        assert_eq!(parse_permission_response("deny", &pats()), Some(Deny));
        assert_eq!(parse_permission_response("reject", &pats()), Some(Deny)); // backward-compat alias
    }

    #[test]
    fn out_of_range_and_junk_are_none() {
        let p = pats();
        assert_eq!(parse_permission_response("9", &p), None); // out of range
        assert_eq!(parse_permission_response("0", &p), None); // patterns are 1-based
        assert_eq!(parse_permission_response("sa", &p), None); // two letters
        assert_eq!(parse_permission_response("sx", &p), None); // junk letter
        assert_eq!(parse_permission_response("", &p), None); // empty
        assert_eq!(parse_permission_response("s", &[]), None); // no patterns → no broadest
    }

    #[test]
    fn preview_block_frames_lines_with_blanks() {
        assert_eq!(
            preview_block(Some("-a\n+b")),
            vec!["".to_string(), "-a".to_string(), "+b".to_string()]
        );
    }

    #[test]
    fn preview_block_empty_for_none_or_empty() {
        assert_eq!(preview_block(None), Vec::<String>::new());
        assert_eq!(preview_block(Some("")), Vec::<String>::new());
    }
}
