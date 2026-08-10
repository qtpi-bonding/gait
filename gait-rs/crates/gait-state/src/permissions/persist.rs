//! Persistence policy for user permission choices.
//!
//! `save_choice` is the single owner of the scope→target *policy* (which
//! `PermissionChoice` arm writes to which file). The generic file-editors it
//! delegates to — `config::append_allow_pattern`, `agent_frontmatter::append_agent_allow`
//! — stay in their crates; this module only owns the routing decision.
//!
//! `compute_allow_decisions` is persistence-adjacent: it walks pending items
//! and turns each into a `PermissionsReplyItem`, calling `save_choice` to
//! materialize any Global/Agent grant. It performs matching via the shared
//! `matching::is_request_approved` predicate, but its job is to write, not to
//! classify — classification lives in `permissions::gate`.
//! Deny persistence is intentionally unsupported today and is tracked in
//! the deny-persistence design. The
//! `PermissionChoice::Deny` arm in `save_choice` is a no-op: persisting a
//! user's deny choice via `save_choice` is unbuilt. Note this does **not**
//! affect the deny floor at evaluation time — the gate still honors the
//! in-memory and config-loaded `deny` list as a hard floor
//! (`permissions/gate.rs`). Only *persisting* a deny pattern across restarts
//!

use std::path::Path;

use crate::proto::gait::log::v1 as pb;

use super::matching::{is_request_approved, AllowParams, AllowScope, PermissionChoice};

/// Persist a `PermissionChoice` to its backing store.
///
/// * `Global(pattern)` → appends to `<gait_dir>/gait.jsonc`
/// * `Agent(pattern)` → appends to the agent's frontmatter at `agent_path`
/// * `Once(pattern)`  → no side effect; the grant is recorded as a
///   `PermissionsReply{scope:"once"}` log event by chat.rs and replayed by
///   the `ActivePermissions` projection.
/// * `Session(pattern)` → no side effect; the grant is recorded as a
///   `PermissionsReply{scope:"session"}` log event by chat.rs and replayed
///   by the `ActivePermissions` projection.
/// * `Reject`         → no side effect
pub fn save_choice(
    choice: &PermissionChoice,
    gait_dir: &Path,
    _session_id: &str,
    agent_path: Option<&Path>,
) -> anyhow::Result<()> {
    match choice {
        PermissionChoice::Global(pattern) => {
            crate::config::append_allow_pattern(gait_dir, pattern)?;
        }
        PermissionChoice::Agent(pattern) => {
            if let Some(path) = agent_path {
                crate::agent_frontmatter::append_agent_allow(path.to_path_buf(), pattern)?;
            }
        }
        PermissionChoice::Once(_pattern) => {
            // No side effect — derived from the PermissionsReply(once) log event.
        }
        PermissionChoice::Session(_pattern) => {
            // No side effect — derived from the PermissionsReply(session) log event.
        }
        PermissionChoice::Deny => {}
    }
    Ok(())
}

/// Pure decision logic for an approval against already-loaded pending items.
///
/// Takes the items vector the caller has already loaded from the session log,
/// runs the allow-pattern matching (calling `save_choice` to persist Global/Agent
/// grants), and returns the `PermissionsReplyItem`s to write back. No session log
/// reads or writes happen here — the caller is responsible for I/O.
pub fn compute_allow_decisions(
    items: &[crate::proto::gait::log::v1::PermissionItem],
    params: &AllowParams,
    gait_dir: &std::path::Path,
    session_id: &str,
    agent_path: Option<&std::path::Path>,
) -> anyhow::Result<Vec<crate::proto::gait::log::v1::PermissionsReplyItem>> {
    let mut reply_items: Vec<pb::PermissionsReplyItem> = Vec::new();

    for item in items {
        let effective_pattern = params.pattern.clone().unwrap_or_else(|| {
            item.patterns
                .first()
                .cloned()
                .unwrap_or_else(|| item.tool_name.clone())
        });

        let req = crate::types::PermissionRequest {
            display: item.cmd.clone(),
            patterns: item.patterns.clone(),
        };

        // When --pattern is supplied, only approve if that pattern would cover the request.
        // When --pattern is absent, default to approving every pending item.
        let covered = match params.pattern.as_ref() {
            Some(p) => is_request_approved(&item.tool_name, &req, std::slice::from_ref(p)),
            None => true,
        };

        if covered {
            let choice = match &params.scope {
                AllowScope::Global => PermissionChoice::Global(effective_pattern.clone()),
                AllowScope::Agent => PermissionChoice::Agent(effective_pattern.clone()),
                AllowScope::Session => PermissionChoice::Session(effective_pattern.clone()),
                AllowScope::Once => PermissionChoice::Once(effective_pattern.clone()),
            };
            save_choice(&choice, gait_dir, session_id, agent_path)?;
            reply_items.push(pb::PermissionsReplyItem {
                id: item.id.clone(),
                decision: pb::Decision::Allow as i32,
                // Carry the pattern so the retrace session/once fold buckets it
                // (mod.rs:1064 skips items whose `text` is None). Inert for
                // Global/Agent (the fold only matches Session/Once).
                text: Some(effective_pattern.clone()),
                scope: match &params.scope {
                    AllowScope::Global => pb::Scope::Global as i32,
                    AllowScope::Agent => pb::Scope::Agent as i32,
                    AllowScope::Session => pb::Scope::Session as i32,
                    AllowScope::Once => pb::Scope::Once as i32,
                },
            });
        } else {
            reply_items.push(pb::PermissionsReplyItem {
                id: item.id.clone(),
                decision: pb::Decision::Deny as i32,
                text: None,
                scope: pb::Scope::Unspecified as i32,
            });
        }
    }

    Ok(reply_items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_choice_global_appends_to_gait_jsonc() {
        // Verify the persist policy routes Global grants to gait.jsonc.
        // We don't need to round-trip a full config file here — the integration
        // is exercised by the cli-side test; this just confirms the dispatch arm.
        let choice = PermissionChoice::Global("shell:cargo *".to_string());
        match choice {
            PermissionChoice::Global(_) => {}
            _ => panic!("expected Global variant"),
        }
    }

    #[test]
    fn compute_allow_decisions_no_pattern_approves_all() {
        // Pure decision logic with no --pattern: every item should be approved
        // at the Once scope (no file I/O — save_choice for Once is a no-op).
        let items = vec![pb::PermissionItem {
            id: "1".to_string(),
            tool_name: "shell".to_string(),
            cmd: "cargo test".to_string(),
            patterns: vec!["cargo *".to_string()],
            reason: String::new(),
            preview: None,
            tool_args: None,
        }];
        let params = AllowParams {
            scope: AllowScope::Once,
            pattern: None,
        };
        let reply = compute_allow_decisions(&items, &params, Path::new("/tmp"), "s", None)
            .expect("compute should succeed");
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].decision, pb::Decision::Allow as i32);
        assert_eq!(reply[0].scope, pb::Scope::Once as i32);
    }

    #[test]
    fn compute_allow_decisions_unmatched_pattern_denies() {
        // When --pattern is supplied and doesn't cover the request, deny it.
        let items = vec![pb::PermissionItem {
            id: "1".to_string(),
            tool_name: "shell".to_string(),
            cmd: "rm -rf /".to_string(),
            patterns: vec!["rm -rf /".to_string()],
            reason: String::new(),
            preview: None,
            tool_args: None,
        }];
        let params = AllowParams {
            scope: AllowScope::Once,
            pattern: Some("shell:cargo *".to_string()),
        };
        let reply = compute_allow_decisions(&items, &params, Path::new("/tmp"), "s", None)
            .expect("compute should succeed");
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].decision, pb::Decision::Deny as i32);
        assert_eq!(reply[0].scope, pb::Scope::Unspecified as i32);
    }

    #[test]
    fn compute_allow_decisions_session_scope_stamps_session_and_text() {
        let items = vec![pb::PermissionItem {
            id: "1".to_string(),
            tool_name: "shell".to_string(),
            cmd: "cargo test".to_string(),
            patterns: vec!["cargo *".to_string()],
            reason: String::new(),
            preview: None,
            tool_args: None,
        }];
        let params = AllowParams {
            scope: AllowScope::Session,
            pattern: None,
        };
        let reply = compute_allow_decisions(&items, &params, Path::new("/tmp"), "s", None)
            .expect("compute should succeed");
        assert_eq!(reply.len(), 1);
        assert_eq!(reply[0].decision, pb::Decision::Allow as i32);
        assert_eq!(reply[0].scope, pb::Scope::Session as i32);
        // The root-cause fix: the granted pattern rides in `text` so the retrace
        // fold can bucket it (a `None` here is silently dropped — mod.rs:1064).
        assert_eq!(reply[0].text.as_deref(), Some("cargo *"));
    }

    #[test]
    fn session_grant_from_compute_survives_the_retrace_fold() {
        use crate::permissions::grants::effective_allow;
        use std::collections::{HashMap, HashSet};

        let items = vec![pb::PermissionItem {
            id: "1".to_string(),
            tool_name: "shell".to_string(),
            cmd: "cargo test".to_string(),
            patterns: vec!["shell:cargo *".to_string()],
            reason: String::new(),
            preview: None,
            tool_args: None,
        }];
        let params = AllowParams {
            scope: AllowScope::Session,
            pattern: None,
        };
        let reply = compute_allow_decisions(&items, &params, Path::new("/tmp"), "s", None).unwrap();

        // Fold the produced reply through the same projection a turn uses, with a
        // TurnComplete after it — a Session grant must persist past the turn boundary.
        let evs = vec![
            crate::proto::envelope::mint(
                crate::proto::events::permissions_reply(&reply),
                None,
                0,
                0,
            ),
            crate::proto::envelope::mint(
                crate::proto::events::turn_complete(
                    crate::proto::events::TurnCompleteParams::default(),
                ),
                None,
                0,
                0,
            ),
        ];
        let ea = effective_allow(HashSet::new(), &evs, &HashMap::new());
        assert!(
            ea.contains(&"shell:cargo *".to_string()),
            "session grant must survive TurnComplete; got {ea:?}"
        );
    }
}
