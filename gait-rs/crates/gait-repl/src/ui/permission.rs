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

/// Two-axis grammar: an optional scope letter (`o`/`s`/`m`) and an optional
/// pattern number, in either order. The number picks the pattern (default: the
/// broadest, `patterns.last()`); the letter picks the scope (default, i.e. a
/// bare number, is Once). `r` rejects; `rs`[N] rejects for the rest of this
/// session (the deny-side mirror of `s`[N] — no letter-order flexibility here,
/// `rs` only, since `sr`/`r`+digit would collide with the scope tokenizer). No
/// `g`/global — global stays config-only, for both allow and reject.
///   `2` → once-pattern-2 · `s2`/`2s` → session · `m1`/`1m` → mode · `s` → session-broadest
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
            'o' | 's' | 'm' if letter.is_none() => letter = Some(c),
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
        Some('m') => PermissionChoice::Mode(pattern),
        Some('o') => PermissionChoice::Once(pattern),
        None => {
            // a bare letter was handled above; here digits are guaranteed present
            PermissionChoice::Once(pattern)
        }
        Some(_) => return None, // unreachable: tokenizer only sets o/s/a
    })
}

/// After the REPL appends an interaction reply to the session log, emit
/// `PermissionDecided` (via gait-ops, the single emitter) when that reply is a
/// `PermissionsReply`. Other reply kinds are ignored.
pub(crate) fn emit_decision(appended: &gait_core::proto::gait::log::v1::Event, session_id: &str) {
    use gait_core::proto::gait::log::v1 as pb;
    if let Some(pb::event::Body::Payload(p)) = &appended.body {
        if let Some(pb::payload::Kind::PermissionsReply(r)) = &p.kind {
            gait_ops::permission::emit_reply_decided(&appended.id, session_id, &r.items);
        }
    }
}

/// Translate a REPL-side [`gait_core::permissions::PermissionChoice`] into the
/// ops-door [`gait_ops::permission::ItemChoice`] shape (same `pb::Decision`
/// enum). Deny carries no pattern and is scoped `Once` (scope is ignored for
/// Deny); DenySession mirrors Session on the deny side.
fn item_choice(
    id: &str,
    choice: &gait_core::permissions::PermissionChoice,
) -> gait_ops::permission::ItemChoice {
    use gait_core::permissions::PermissionChoice;
    use gait_core::proto::gait::log::v1::Decision;
    use gait_ops::permission::GrantScope;
    match choice {
        PermissionChoice::Once(p) => gait_ops::permission::ItemChoice {
            id: id.to_string(),
            decision: Decision::Allow,
            scope: GrantScope::Once,
            pattern: Some(p.clone()),
        },
        PermissionChoice::Session(p) => gait_ops::permission::ItemChoice {
            id: id.to_string(),
            decision: Decision::Allow,
            scope: GrantScope::Session,
            pattern: Some(p.clone()),
        },
        PermissionChoice::Mode(p) => gait_ops::permission::ItemChoice {
            id: id.to_string(),
            decision: Decision::Allow,
            scope: GrantScope::Mode,
            pattern: Some(p.clone()),
        },
        PermissionChoice::Global(p) => gait_ops::permission::ItemChoice {
            id: id.to_string(),
            decision: Decision::Allow,
            scope: GrantScope::Global,
            pattern: Some(p.clone()),
        },
        PermissionChoice::Deny => gait_ops::permission::ItemChoice {
            id: id.to_string(),
            decision: Decision::Deny,
            scope: GrantScope::Once,
            pattern: None,
        },
        PermissionChoice::DenySession(p) => gait_ops::permission::ItemChoice {
            id: id.to_string(),
            decision: Decision::Deny,
            scope: GrantScope::Session,
            pattern: Some(p.clone()),
        },
    }
}

/// Answer a pending permission batch through the shared ops door
/// ([`gait_ops::permission::reply_for_items`]). `items` and `choices`
/// correspond by index (one choice per item); a length mismatch is a caller
/// bug and errors. No other logic lives here: the door validates the choices,
/// saves each grant, writes the `PermissionsReply` to the session log and
/// emits `PermissionDecided` — because the door saves a grant *before* it
/// writes the log, a grant is only ever logged after its save succeeded.
pub(crate) fn reply_through_door(
    gait_dir: &std::path::Path,
    session_id: &str,
    items: &[gait_core::proto::gait::log::v1::PermissionItem],
    choices: &[gait_core::permissions::PermissionChoice],
) -> anyhow::Result<gait_ops::permission::ReplyOutcome> {
    if items.len() != choices.len() {
        return Err(anyhow::anyhow!(
            "permission reply mismatch: {} items but {} choices",
            items.len(),
            choices.len()
        ));
    }
    let item_choices = items
        .iter()
        .zip(choices)
        .map(|(item, choice)| item_choice(&item.id, choice))
        .collect();
    gait_ops::permission::reply_for_items(gait_dir, session_id, items.to_vec(), item_choices)
}

#[cfg(test)]
mod tests {
    use super::*;

    use gait_core::permissions::PermissionChoice::{Deny, Mode, Once, Session};

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
    fn mode_pattern_either_order() {
        assert_eq!(
            parse_permission_response("m1", &pats()),
            Some(Mode("shell:cargo test *".into()))
        );
        assert_eq!(
            parse_permission_response("1m", &pats()),
            Some(Mode("shell:cargo test *".into()))
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
            parse_permission_response("m", &pats()),
            Some(Mode("shell:cargo test *".into()))
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

    // -- reply_through_door --

    use gait_core::permissions::PermissionChoice;
    use gait_core::proto::gait::log::v1 as pb;
    use gait_ops::permission::{GrantScope, ItemChoice as DoorChoice, ReplyOutcome};
    use tempfile::TempDir;

    /// Fresh workspace: minimal `gait.jsonc` (Global grants append to it) plus
    /// the parent dir of the session log for `session_id`.
    fn fresh_ws(session_id: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("gait.jsonc"),
            r#"{ "allow": [], "sandbox": { "on": true, "readable": [], "writable": [], "network": false } }"#,
        )
        .unwrap();
        let log = gait_core::session::live::log_path(dir.path(), session_id);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        (dir, log)
    }

    fn item() -> pb::PermissionItem {
        pb::PermissionItem {
            id: "p1".into(),
            cmd: "ls".into(),
            patterns: vec!["ls".into()],
            tool_name: "shell".into(),
            reason: String::new(),
            preview: None,
            tool_args: None,
        }
    }

    fn has_permissions_reply(log: &std::path::Path) -> bool {
        let events = gait_core::session::log::read_events(log).unwrap_or_default();
        events.iter().any(|e| {
            matches!(
                &e.body,
                Some(pb::event::Body::Payload(p))
                    if matches!(p.kind, Some(pb::payload::Kind::PermissionsReply(_)))
            )
        })
    }

    #[test]
    fn reply_through_door_equals_reply_for_items_for_every_kind() {
        use gait_core::permissions::PermissionChoice::DenySession;
        let kinds: Vec<PermissionChoice> = vec![
            Once("ls".into()),
            Session("ls".into()),
            Mode("ls".into()),
            PermissionChoice::Global("ls".into()),
            Deny,
            DenySession("ls".into()),
        ];
        for (i, choice) in kinds.iter().enumerate() {
            let sid = format!("rdoor001-{i}");
            let (dir_a, _log_a) = fresh_ws(&sid);
            let (dir_b, _log_b) = fresh_ws(&format!("{sid}-twin"));

            let a = reply_through_door(dir_a.path(), &sid, &[item()], std::slice::from_ref(choice))
                .unwrap();
            let b = gait_ops::permission::reply_for_items(
                dir_b.path(),
                &format!("{sid}-twin"),
                vec![item()],
                vec![DoorChoice {
                    // Hand-written for each kind, exactly what the kind implies.
                    id: "p1".into(),
                    decision: if matches!(choice, Deny | DenySession(_)) {
                        pb::Decision::Deny
                    } else {
                        pb::Decision::Allow
                    },
                    scope: match choice {
                        Once(_) => GrantScope::Once,
                        Session(_) => GrantScope::Session,
                        Mode(_) => GrantScope::Mode,
                        PermissionChoice::Global(_) => GrantScope::Global,
                        Deny => GrantScope::Once,
                        DenySession(_) => GrantScope::Session,
                    },
                    pattern: match choice {
                        Deny => None,
                        _ => Some("ls".into()),
                    },
                }],
            )
            .unwrap();
            assert_eq!(a.reply_items, b.reply_items, "kind {choice:?}");

            // Literal expected decision/scope/text per kind.
            let (d, s, t) = match choice {
                Once(_) | Session(_) | Mode(_) | PermissionChoice::Global(_) => (
                    pb::Decision::Allow as i32,
                    match choice {
                        Once(_) => pb::Scope::Once as i32,
                        Session(_) => pb::Scope::Session as i32,
                        Mode(_) => pb::Scope::Mode as i32,
                        _ => pb::Scope::Global as i32,
                    },
                    Some("ls".to_string()),
                ),
                Deny => (
                    pb::Decision::Deny as i32,
                    pb::Scope::Unspecified as i32,
                    None,
                ),
                DenySession(_) => (
                    pb::Decision::Deny as i32,
                    pb::Scope::Session as i32,
                    Some("ls".to_string()),
                ),
            };
            let ReplyOutcome { reply_items } = a;
            assert_eq!(reply_items.len(), 1);
            assert_eq!(reply_items[0].id, "p1");
            assert_eq!(reply_items[0].decision, d, "kind {choice:?}");
            assert_eq!(reply_items[0].scope, s, "kind {choice:?}");
            assert_eq!(reply_items[0].text, t, "kind {choice:?}");
        }
    }

    #[test]
    fn a_failed_save_is_an_error_and_logs_no_reply() {
        use gait_core::permissions::PermissionChoice::Global;
        // Make the Global grant's save impossible: gait.jsonc is a directory.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("gait.jsonc")).unwrap();
        let log = gait_core::session::live::log_path(dir.path(), "rdoor001-fail");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();

        let err = reply_through_door(
            dir.path(),
            "rdoor001-fail",
            &[item()],
            &[Global("ls".into())],
        );
        assert!(err.is_err(), "a failed grant save must be an error");
        // No PermissionsReply was logged — the reply is written only after the
        // save succeeded.
        assert!(!has_permissions_reply(&log));
    }

    #[test]
    fn mismatched_item_and_choice_counts_are_an_error() {
        let (dir, _log) = fresh_ws("rdoor001-mismatch");
        let err = reply_through_door(
            dir.path(),
            "rdoor001-mismatch",
            &[item()],
            &[Once("ls".into()), Session("ls".into())],
        );
        assert!(
            err.is_err(),
            "2 items/1 choices (or 1/2) must be a caller error"
        );
    }
}
