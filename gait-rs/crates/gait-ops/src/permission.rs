//! Permission verbs: `allow`, `deny`, `unlock`.
//!
//! Sources: `crates/gait-cli/src/cli/session.rs` (`cmd_allow`, `cmd_deny`,
//! `cmd_unlock`). The core decision logic already lives in gait-core; these
//! ops wrap it and own the log/config writes. `unlock` has a distinct
//! per-scope branch (once/session write only the reply; global/agent also
//! persist a `SandboxGrant` via `persist_grant_items(parent)`; agent first
//! resolves the target via
//! `agent_frontmatter::resolve_markdown_context_path(MarkdownKind::Agent, name)`)
//! — do not flatten it.
use crate::OpError;
use coded_error::ResultExt;
use std::path::{Path, PathBuf};

// -----------------------------------------------------------------------------
// allow
// -----------------------------------------------------------------------------

/// The scope of a grant — either a permission allow or a sandbox unlock.
/// `Once` is the default, matching the CLI's default `--scope once`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrantScope {
    /// Save pattern to `gait.jsonc` (`allow[]`/`sandbox.*`).
    Global,
    /// Save pattern to the agent's frontmatter (`allow[]`).
    Agent,
    /// Save pattern to the currently active mode's MODE.md (`allow[]`).
    Mode,
    /// Log-only, lifetime = session. No file write.
    Session,
    /// Log-only, lifetime = one turn. No file write.
    #[default]
    Once,
}

impl From<GrantScope> for gait_state::proto::gait::log::v1::Scope {
    fn from(s: GrantScope) -> Self {
        match s {
            GrantScope::Global => Self::Global,
            GrantScope::Agent => Self::Agent,
            GrantScope::Mode => Self::Mode,
            GrantScope::Session => Self::Session,
            GrantScope::Once => Self::Once,
        }
    }
}

/// Parameters for [`allow`]. Mirrors the `AllowArgs` clap glue fields the CLI
/// uses; the op itself is clap-free. `scope` defaults to `GrantScope::Once`
/// to match the CLI's `--scope once` default.
#[derive(Debug, Clone, Default)]
pub struct AllowParams {
    pub scope: GrantScope,
    pub pattern: Option<String>,
}

/// Resolve the name of the session's currently active mode from its event log.
/// Returns `None` when the log cannot be read or no mode is active.
pub fn resolve_active_mode_name(gait_dir: &Path, session_id: &str) -> Option<String> {
    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let evs = gait_state::session::log::read_events(&tx_path).ok()?;
    let mut muts = gait_state::session::retrace::active_mutations(&evs);
    gait_state::session::retrace::apply_cascade(&mut muts, &evs);
    gait_state::session::retrace::active_mode(&evs, &muts).map(|m| m.name)
}

/// Outcome returned by [`allow`]. Carries the reply items the op wrote (so the
/// adapter can render them under `--json`) and the scope it stamped on each.
#[derive(Debug)]
pub struct AllowOutcome {
    pub reply_items: Vec<gait_state::proto::gait::log::v1::PermissionsReplyItem>,
    pub scope: GrantScope,
}

/// Approve all pending permission requests for `session_id`. Wraps
/// `gait_state::permissions::compute_allow_decisions` + `write_permissions_reply`.
pub fn allow(
    gait_dir: &Path,
    session_id: &str,
    params: AllowParams,
) -> anyhow::Result<AllowOutcome> {
    use gait_state::permissions::{
        compute_allow_decisions, AllowParams as CoreAllowParams, AllowScope as CoreAllowScope,
    };
    use gait_state::session::log::{session_state, write_permissions_reply, SessionState};

    let core_scope = match params.scope {
        GrantScope::Global => CoreAllowScope::Global,
        GrantScope::Mode => CoreAllowScope::Mode,
        GrantScope::Session => CoreAllowScope::Session,
        GrantScope::Once => CoreAllowScope::Once,
        GrantScope::Agent => {
            return Err(anyhow::anyhow!(
                "scope 'agent' is not supported for allow; use --scope mode"
            ));
        }
    };

    let mode_name = resolve_active_mode_name(gait_dir, session_id);

    let core_params = CoreAllowParams {
        scope: core_scope,
        pattern: params.pattern.clone(),
    };

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let state = session_state(&tx_path).coded()?;
    let items = match state {
        SessionState::AwaitingPermission { items } => items,
        _ => {
            return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                "no pending permissions for session {session_id}"
            ))));
        }
    };

    let reply_items = compute_allow_decisions(
        &items,
        &core_params,
        gait_dir,
        session_id,
        mode_name.as_deref(),
    )?;

    write_permissions_reply(&tx_path, "user", reply_items.clone()).coded()?;

    Ok(AllowOutcome {
        reply_items,
        scope: params.scope,
    })
}

// -----------------------------------------------------------------------------
// deny
// -----------------------------------------------------------------------------

/// Outcome returned by [`deny`].
#[derive(Debug)]
pub struct DenyOutcome {
    pub reply_items: Vec<gait_state::proto::gait::log::v1::PermissionsReplyItem>,
}

/// Deny all pending permission requests for `session_id`. Builds deny reply
/// items and writes the `PermissionsReply` event to the session log.
pub fn deny(gait_dir: &Path, session_id: &str) -> anyhow::Result<DenyOutcome> {
    use gait_state::proto::gait::log::v1 as pb;
    use gait_state::session::log::{session_state, write_permissions_reply, SessionState};

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let state = session_state(&tx_path).coded()?;
    let items = match state {
        SessionState::AwaitingPermission { items } => items,
        _ => {
            return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                "no pending permissions for session {session_id}"
            ))));
        }
    };

    let reply: Vec<pb::PermissionsReplyItem> = items
        .iter()
        .map(|it| pb::PermissionsReplyItem {
            id: it.id.clone(),
            decision: pb::Decision::Deny as i32,
            text: None,
            scope: pb::Scope::Unspecified as i32,
        })
        .collect();

    if reply.is_empty() {
        return Ok(DenyOutcome { reply_items: reply });
    }

    write_permissions_reply(&tx_path, "user", reply.clone()).coded()?;
    Ok(DenyOutcome { reply_items: reply })
}

// -----------------------------------------------------------------------------
// reply — explicit per-item answers (one decision per pending item)
// -----------------------------------------------------------------------------

use gait_state::proto::gait::log::v1 as pb;

/// One consumer's decision about a single pending permission item. Unlike
/// `allow`/`deny` (which apply one pattern-match to the whole batch), each
/// `ItemChoice` targets one pending item by `id`, so a caller can allow some and
/// deny others — and carry the exact sub-command pattern it chose.
#[derive(Debug, Clone)]
pub struct ItemChoice {
    /// Must match a `PermissionItem.id` in the pending `AwaitingPermission` batch.
    pub id: String,
    pub decision: pb::Decision,
    /// Scope for an Allow. Ignored for Deny.
    pub scope: GrantScope,
    /// The chosen pattern (a rung of the item's ladder). Required for Allow.
    pub pattern: Option<String>,
}

/// Outcome of [`reply`]: the reply items written.
#[derive(Debug)]
pub struct ReplyOutcome {
    pub reply_items: Vec<pb::PermissionsReplyItem>,
}

/// Write one `PermissionsReply` covering explicit per-item `choices`. Persists any
/// Global/Mode grant via `save_choice`; Once/Session are log-derived.
///
/// Errors if there is no pending `AwaitingPermission` state, if a choice's `id`
/// matches no pending item, if an Allow choice has no `pattern`, or if the
/// choices do not cover **every** unique pending id — an omitted id would be
/// silently denied at resume and can never be answered (`session_state` returns
/// `Idle` after the first reply, session/log/mod.rs:112). Multiple choices with
/// the same id (compound-command segments) are permitted; the engine collapses
/// them per-id at resume.
pub fn reply(
    gait_dir: &Path,
    session_id: &str,
    choices: Vec<ItemChoice>,
) -> anyhow::Result<ReplyOutcome> {
    use gait_state::session::log::{session_state, SessionState};

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let state = session_state(&tx_path).coded()?;
    let items = match state {
        SessionState::AwaitingPermission { items } => items,
        _ => {
            return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                "no pending permissions for session {session_id}"
            ))));
        }
    };
    reply_for_items(gait_dir, session_id, items, choices)
}

/// Write a permission reply for a request which is known by the caller to be
/// pending. This is the adapter seam for runners that observe a permission
/// event in a live brain stream before terminal bookkeeping is appended to the
/// log. `reply` deliberately keeps its stricter `session_state` check for the
/// ordinary CLI/API path; ACP supplies the request it just presented to its
/// client and therefore must not lose it merely because a `RunComplete` event
/// followed the pause event.
pub fn reply_for_items(
    gait_dir: &Path,
    session_id: &str,
    items: Vec<pb::PermissionItem>,
    choices: Vec<ItemChoice>,
) -> anyhow::Result<ReplyOutcome> {
    use gait_state::permissions::{save_choice, PermissionChoice};
    use gait_state::session::log::write_permissions_reply;

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let pending: std::collections::HashSet<&str> = items.iter().map(|it| it.id.as_str()).collect();
    let answered: std::collections::HashSet<&str> = choices.iter().map(|c| c.id.as_str()).collect();

    // Reject unknown ids first: a choice naming no pending item is always a
    // caller error, and must be diagnosed *before* the completeness check —
    // otherwise a single mistargeted choice would be misreported as "incomplete"
    // (it leaves the real pending item unanswered too).
    for c in &choices {
        if !pending.contains(c.id.as_str()) {
            return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                "no pending permission item '{}' for session {session_id}",
                c.id
            ))));
        }
    }

    // Completeness: every unique pending id must be answered. An omitted id is
    // permanently unanswerable (session_state flips to Idle after this reply) and
    // silently denied at resume — reject rather than half-answer.
    for it in &items {
        if !answered.contains(it.id.as_str()) {
            return Err(anyhow::anyhow!(
                "incomplete reply: pending item '{}' for session {session_id} was not answered",
                it.id
            ));
        }
    }

    let mut reply_items: Vec<pb::PermissionsReplyItem> = Vec::with_capacity(choices.len());
    for c in &choices {
        let item = if c.decision == pb::Decision::Allow {
            let Some(pattern) = c.pattern.clone() else {
                return Err(anyhow::anyhow!(
                    "allow choice for item '{}' is missing a pattern",
                    c.id
                ));
            };
            let mode_name = resolve_active_mode_name(gait_dir, session_id);
            let (choice, scope_tag) = match c.scope {
                GrantScope::Global => {
                    (PermissionChoice::Global(pattern.clone()), pb::Scope::Global)
                }
                GrantScope::Mode => (PermissionChoice::Mode(pattern.clone()), pb::Scope::Mode),
                GrantScope::Session => (
                    PermissionChoice::Session(pattern.clone()),
                    pb::Scope::Session,
                ),
                GrantScope::Once => (PermissionChoice::Once(pattern.clone()), pb::Scope::Once),
                GrantScope::Agent => {
                    return Err(anyhow::anyhow!(
                        "scope 'agent' is not supported for reply/allow; use 'mode'"
                    ));
                }
            };
            save_choice(&choice, gait_dir, session_id, mode_name.as_deref())?;
            pb::PermissionsReplyItem {
                id: c.id.clone(),
                decision: pb::Decision::Allow as i32,
                text: Some(pattern),
                scope: scope_tag as i32,
            }
        } else if c.scope == GrantScope::Session {
            // Mirrors the Allow/Session branch above: requires a pattern,
            // persists via save_choice (log-only, see DenySession's doc
            // comment), and carries the pattern in `text` so the
            // ActivePermissions fold picks it up into `deny_session`.
            let Some(pattern) = c.pattern.clone() else {
                return Err(anyhow::anyhow!(
                    "session-scoped deny choice for item '{}' is missing a pattern",
                    c.id
                ));
            };
            save_choice(
                &PermissionChoice::DenySession(pattern.clone()),
                gait_dir,
                session_id,
                None,
            )?;
            pb::PermissionsReplyItem {
                id: c.id.clone(),
                decision: pb::Decision::Deny as i32,
                text: Some(pattern),
                scope: pb::Scope::Session as i32,
            }
        } else {
            // Plain "reject once" — no persistence, matches today's behavior.
            pb::PermissionsReplyItem {
                id: c.id.clone(),
                decision: pb::Decision::Deny as i32,
                text: None,
                scope: pb::Scope::Unspecified as i32,
            }
        };
        reply_items.push(item);
    }

    if reply_items.is_empty() {
        return Ok(ReplyOutcome { reply_items });
    }
    write_permissions_reply(&tx_path, "user", reply_items.clone()).coded()?;
    Ok(ReplyOutcome { reply_items })
}
// -----------------------------------------------------------------------------
// unlock
// -----------------------------------------------------------------------------

/// Parameters for [`unlock`].
#[derive(Debug, Clone, Default)]
pub struct UnlockParams {
    pub gait_dir: PathBuf,
    pub session_id: String,
    pub scope: GrantScope,
    /// Required when `scope == GrantScope::Agent`. Resolves against
    /// `<gait_dir>/agents/<name>/AGENT.md`.
    pub agent: Option<String>,
}

/// Outcome returned by [`unlock`]. Carries the reply items, the count of
/// grants persisted to config/frontmatter (0 for once/session), and any
/// non-fatal warnings the persistence step emitted.
#[derive(Debug)]
pub struct UnlockOutcome {
    pub reply_items: Vec<gait_state::proto::gait::log::v1::SandboxUnlockReplyItem>,
    pub persisted_grants: usize,
    pub warnings: Vec<String>,
}

/// Approve all pending unlock requests for `session_id`. The branch fan-out
/// from the original `cmd_unlock` is preserved:
///
/// * `Once` / `Session` — write only the `SandboxUnlockReply` to the log.
/// * `Global` — persist every item as a `SandboxGrant` to `gait.jsonc`,
///   then write the reply.
/// * `Agent` — first resolve `<gait_dir>/agents/<name>/AGENT.md` via
///   `resolve_markdown_context_path`, then persist every item as a
///   `SandboxGrant` against the agent file's parent dir, then write the
///   reply. Without `--agent`, the grant is silently log-only (matching
///   the original CLI warning behavior).
pub fn unlock(params: UnlockParams) -> anyhow::Result<UnlockOutcome> {
    use gait_state::proto::gait::log::v1 as pb;
    use gait_state::session::log::{session_state, write_unlock_reply, SessionState};

    let scope_pb: pb::Scope = params.scope.into();

    let tx_path = gait_state::session::live::log_path(&params.gait_dir, &params.session_id);
    let state = session_state(&tx_path).coded()?;
    let items = match state {
        SessionState::AwaitingSandboxUnlock { items } => items,
        _ => {
            return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                "no pending unlock for session {}",
                params.session_id
            ))));
        }
    };

    if items.is_empty() {
        return Ok(UnlockOutcome {
            reply_items: Vec::new(),
            persisted_grants: 0,
            warnings: Vec::new(),
        });
    }

    let mut warnings: Vec<String> = Vec::new();
    let mut persisted_grants: usize = 0;

    // Per-scope persistence side-effects (file writes happen here, never as a
    // print — Wall 1). The op captures warnings instead of printing.
    match params.scope {
        GrantScope::Once | GrantScope::Session => {
            // log-only; nothing to persist.
        }
        GrantScope::Mode => {
            // Mode is a tool-permission scope; unlock has no mode grant
            // persistence target and therefore remains log-only.
        }
        GrantScope::Global => {
            persisted_grants = persist_grant_items(&items, &params.gait_dir, &mut warnings);
        }
        GrantScope::Agent => {
            if let Some(agent_name) = params.agent.as_deref() {
                use gait_state::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};
                let agent_path = resolve_markdown_context_path(
                    &params.gait_dir,
                    MarkdownKind::Agent,
                    agent_name,
                );
                if let Some(parent) = agent_path.parent() {
                    persisted_grants = persist_grant_items(&items, parent, &mut warnings);
                }
            } else {
                warnings.push(
                    "--agent required for agent-scope grants; writing to log only".to_string(),
                );
            }
        }
    }

    // Always write the SandboxUnlockReply to the log.
    let reply_items: Vec<pb::SandboxUnlockReplyItem> = items
        .iter()
        .map(|it| pb::SandboxUnlockReplyItem {
            id: it.id.clone(),
            decision: pb::Decision::Allow as i32,
            scope: scope_pb as i32,
        })
        .collect();

    write_unlock_reply(&tx_path, "user", reply_items.clone()).coded()?;

    Ok(UnlockOutcome {
        reply_items,
        persisted_grants,
        warnings,
    })
}

/// Route a list of `SandboxUnlockItem`s through `SandboxGrant`. Each item's
/// `dimension` is mapped to a `SandboxGrant` arm and persisted via
/// `SandboxGrant::apply(target_dir, resource)`. Failures and unknown
/// dimensions are captured as warnings on the supplied buffer instead of
/// printed (Wall 1). Returns the number of grants successfully persisted.
fn persist_grant_items(
    items: &[gait_state::proto::gait::log::v1::SandboxUnlockItem],
    target_dir: &Path,
    warnings: &mut Vec<String>,
) -> usize {
    use gait_state::sandbox_grant::SandboxGrant;

    let mut ok = 0;
    for item in items {
        match SandboxGrant::from_dimension(&item.dimension) {
            Some(g) => {
                if let Err(e) = g.apply(target_dir, &item.resource) {
                    warnings.push(format!("could not persist grant to config: {e}"));
                } else {
                    ok += 1;
                }
            }
            None => warnings.push(format!(
                "unknown dimension {:?}, skipping config write",
                item.dimension
            )),
        }
    }
    ok
}

// -----------------------------------------------------------------------------
// tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gait_state::proto::gait::log::v1::{event, payload};
    use gait_state::proto::wire;
    use gait_state::session::log::read_events;
    use tempfile::TempDir;

    #[test]
    fn grant_scope_default_is_once() {
        assert_eq!(GrantScope::default(), GrantScope::Once);
    }

    #[test]
    fn grant_scope_maps_to_proto_scope() {
        use gait_state::proto::gait::log::v1 as pb;
        assert_eq!(pb::Scope::from(GrantScope::Global), pb::Scope::Global);
        assert_eq!(pb::Scope::from(GrantScope::Agent), pb::Scope::Agent);
        assert_eq!(pb::Scope::from(GrantScope::Mode), pb::Scope::Mode);
        assert_eq!(pb::Scope::from(GrantScope::Session), pb::Scope::Session);
        assert_eq!(pb::Scope::from(GrantScope::Once), pb::Scope::Once);
    }

    /// Build a minimal gait-dir with `gait.jsonc` so `SandboxGrant::apply`
    /// has something to read/write, plus a session log path ready to be
    /// seeded with an Unlock event.
    fn ws() -> TempDir {
        let ws = TempDir::new().unwrap();
        // Minimal config so config::append_* has a starting block.
        std::fs::write(
            ws.path().join("gait.jsonc"),
            r#"{
  "allow": ["read"],
  "sandbox": { "on": true, "readable": [], "writable": [], "network": false }
}
"#,
        )
        .unwrap();
        ws
    }

    /// Seed `tx_path` with a single Permissions event carrying one item.
    fn seed_permission(tx_path: &Path, item: gait_state::proto::gait::log::v1::PermissionItem) {
        let body = gait_state::proto::events::permissions(&[item]);
        wire::append(tx_path, body).unwrap();
    }

    /// Seed `tx_path` with a single Unlock event carrying one item.
    fn seed_unlock(tx_path: &Path, item: gait_state::proto::gait::log::v1::SandboxUnlockItem) {
        let body = gait_state::proto::events::sandbox_unlock(&[item]);
        wire::append(tx_path, body).unwrap();
    }

    fn make_session(ws: &Path, id: &str) -> std::path::PathBuf {
        let tx_path = gait_state::session::live::log_path(ws, id);
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
        tx_path
    }

    fn read_kind(tx_path: &Path, idx: usize) -> payload::Kind {
        let events = read_events(tx_path).unwrap();
        let e = events.get(idx).expect("event present");
        match &e.body {
            Some(event::Body::Payload(p)) => p.kind.clone().expect("kind set"),
            other => panic!("expected payload body, got {other:?}"),
        }
    }

    fn reload_config(ws: &Path) -> gait_state::config::GaitConfig {
        gait_state::config::load_config(ws)
    }

    // -- allow --

    #[test]
    fn allow_writes_permissions_reply_for_pending_items() {
        let ws = ws();
        let tx = make_session(ws.path(), "sallow01");
        seed_permission(
            &tx,
            gait_state::proto::gait::log::v1::PermissionItem {
                id: "p1".into(),
                tool_name: "shell".into(),
                cmd: "cargo test".into(),
                patterns: vec!["cargo *".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        );

        let outcome = allow(
            ws.path(),
            "sallow01",
            AllowParams {
                scope: GrantScope::Once,
                pattern: None,
            },
        )
        .unwrap();
        assert_eq!(outcome.reply_items.len(), 1);
        assert_eq!(
            outcome.reply_items[0].decision,
            gait_state::proto::gait::log::v1::Decision::Allow as i32,
        );
        assert_eq!(outcome.scope, GrantScope::Once);

        // A PermissionsReply event now trails the Permissions request.
        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::PermissionsReply(_)));
    }

    #[test]
    fn allow_session_scope_grants_real_session_with_pattern() {
        let ws = ws();
        let tx = make_session(ws.path(), "sallow02");
        seed_permission(
            &tx,
            gait_state::proto::gait::log::v1::PermissionItem {
                id: "p1".into(),
                tool_name: "shell".into(),
                cmd: "ls".into(),
                patterns: vec!["ls".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        );

        let outcome = allow(
            ws.path(),
            "sallow02",
            AllowParams {
                scope: GrantScope::Session,
                pattern: None,
            },
        )
        .unwrap();
        assert_eq!(
            outcome.reply_items[0].scope,
            gait_state::proto::gait::log::v1::Scope::Session as i32,
        );
        // The hack produced Scope::Session but text:None (dropped by the fold).
        // A genuine session grant carries the pattern.
        assert_eq!(outcome.reply_items[0].text.as_deref(), Some("ls"));
    }

    #[test]
    fn allow_returns_not_found_when_no_pending_permissions() {
        let ws = ws();
        let _tx = make_session(ws.path(), "sallow03");
        let err = allow(
            ws.path(),
            "sallow03",
            AllowParams {
                scope: GrantScope::Once,
                pattern: None,
            },
        )
        .unwrap_err();
        assert!(matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::NotFound(_))));
    }

    // -- deny --

    #[test]
    fn deny_writes_deny_reply_for_pending_items() {
        let ws = ws();
        let tx = make_session(ws.path(), "sdeny01");
        seed_permission(
            &tx,
            gait_state::proto::gait::log::v1::PermissionItem {
                id: "p1".into(),
                tool_name: "shell".into(),
                cmd: "rm -rf /".into(),
                patterns: vec!["rm -rf /".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        );

        let outcome = deny(ws.path(), "sdeny01").unwrap();
        assert_eq!(outcome.reply_items.len(), 1);
        assert_eq!(
            outcome.reply_items[0].decision,
            gait_state::proto::gait::log::v1::Decision::Deny as i32,
        );

        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::PermissionsReply(_)));
    }

    #[test]
    fn deny_returns_not_found_when_no_pending_permissions() {
        let ws = ws();
        let _tx = make_session(ws.path(), "sdeny02");
        let err = deny(ws.path(), "sdeny02").unwrap_err();
        assert!(matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::NotFound(_))));
    }

    // -- unlock (3 plan tests + a few extras) --

    fn make_unlock_item(
        id: &str,
        dimension: &str,
        resource: &str,
    ) -> gait_state::proto::gait::log::v1::SandboxUnlockItem {
        gait_state::proto::gait::log::v1::SandboxUnlockItem {
            id: id.into(),
            tool_name: "write".into(),
            resource: resource.into(),
            dimension: dimension.into(),
            preview: None,
        }
    }

    /// Test (a): once scope appends only the SandboxUnlockReply — no config change.
    #[test]
    fn unlock_once_appends_only_unlock_reply_no_config_change() {
        let ws = ws();
        let tx = make_session(ws.path(), "sunc01");
        seed_unlock(&tx, make_unlock_item("u1", "write", "/tmp/a.txt"));
        let config_before = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc01".to_string(),
            scope: GrantScope::Once,
            agent: None,
        })
        .unwrap();
        assert_eq!(out.reply_items.len(), 1);
        assert_eq!(out.persisted_grants, 0);
        assert!(out.warnings.is_empty());

        // Log gained one SandboxUnlockReply event.
        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::SandboxUnlockReply(_)));
        // gait.jsonc untouched.
        let config_after = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();
        assert_eq!(config_before, config_after);
    }

    /// Test (b): global scope also writes the sandbox grant to gait.jsonc.
    #[test]
    fn unlock_global_writes_config_grant() {
        let ws = ws();
        let tx = make_session(ws.path(), "sunc02");
        seed_unlock(&tx, make_unlock_item("u1", "write", "/tmp/a.txt"));

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc02".to_string(),
            scope: GrantScope::Global,
            agent: None,
        })
        .unwrap();
        assert_eq!(out.reply_items.len(), 1);
        assert_eq!(out.persisted_grants, 1, "one write grant must persist");
        assert!(
            out.warnings.is_empty(),
            "no warnings expected: {:?}",
            out.warnings
        );

        // gait.jsonc now has /tmp/a.txt in sandbox.writable.
        let sb = reload_config(ws.path()).sandbox;
        assert!(
            sb.writable.iter().any(|p| p == "/tmp/a.txt"),
            "global write grant must land in sandbox.writable, got {:?}",
            sb.writable
        );

        // Log gained SandboxUnlockReply too.
        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::SandboxUnlockReply(_)));
    }

    /// Test (c): agent scope with a name resolves the agent frontmatter path
    /// and persists the grant into a `gait.jsonc` alongside it (parent dir =
    /// agents/<name>/) — same `SandboxGrant::apply` sink as the global case,
    /// just scoped to the agent's own directory rather than the gait-dir root.
    #[test]
    fn unlock_agent_with_name_resolves_frontmatter_and_writes_grant() {
        let ws = ws();
        let tx = make_session(ws.path(), "sunc03");
        let agent_path = gait_state::agent_frontmatter::resolve_markdown_context_path(
            ws.path(),
            gait_state::agent_frontmatter::MarkdownKind::Agent,
            "coder",
        );
        std::fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
        std::fs::write(&agent_path, "---\nallow:\n  - read\n---\n# prompt body\n").unwrap();

        seed_unlock(&tx, make_unlock_item("u1", "read", "/etc/hosts"));

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc03".to_string(),
            scope: GrantScope::Agent,
            agent: Some("coder".to_string()),
        })
        .unwrap();
        assert_eq!(out.persisted_grants, 1, "one read grant must persist");
        assert!(
            out.warnings.is_empty(),
            "no warnings expected: {:?}",
            out.warnings
        );

        // The grant lands in a gait.jsonc next to the agent file (the same
        // SandboxGrant::apply sink as global, just rooted at the agent's dir).
        let sb = reload_config(agent_path.parent().unwrap()).sandbox;
        assert!(
            sb.readable.iter().any(|p| p == "/etc/hosts"),
            "agent-scope grant must land in the agent dir's sandbox.readable; got {:?}",
            sb.readable
        );

        // Log gained SandboxUnlockReply.
        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::SandboxUnlockReply(_)));
    }

    /// Agent scope without a name captures a warning and writes the reply
    /// log-only — matches the original CLI behavior.
    #[test]
    fn unlock_agent_without_name_emits_warning_no_persist() {
        let ws = ws();
        let tx = make_session(ws.path(), "sunc04");
        seed_unlock(&tx, make_unlock_item("u1", "read", "/etc/hosts"));
        let config_before = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc04".to_string(),
            scope: GrantScope::Agent,
            agent: None,
        })
        .unwrap();
        assert_eq!(out.persisted_grants, 0);
        assert!(
            out.warnings.iter().any(|w| w.contains("--agent required")),
            "expected --agent warning, got {:?}",
            out.warnings
        );
        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::SandboxUnlockReply(_)));
        let config_after = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();
        assert_eq!(config_before, config_after);
    }

    #[test]
    fn unlock_unknown_dimension_emits_warning_but_still_writes_reply() {
        let ws = ws();
        let tx = make_session(ws.path(), "sunc05");
        seed_unlock(&tx, make_unlock_item("u1", "mystery", "/some/path"));

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc05".to_string(),
            scope: GrantScope::Global,
            agent: None,
        })
        .unwrap();
        assert_eq!(out.persisted_grants, 0);
        assert!(
            out.warnings.iter().any(|w| w.contains("unknown dimension")),
            "expected unknown-dimension warning, got {:?}",
            out.warnings
        );
        let kind = read_kind(&tx, 1);
        assert!(matches!(kind, payload::Kind::SandboxUnlockReply(_)));
    }

    #[test]
    fn unlock_session_scope_writes_reply_no_config_change() {
        let ws = ws();
        let tx = make_session(ws.path(), "sunc06");
        seed_unlock(&tx, make_unlock_item("u1", "write", "/tmp/x.txt"));
        let config_before = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc06".to_string(),
            scope: GrantScope::Session,
            agent: None,
        })
        .unwrap();
        assert_eq!(out.reply_items.len(), 1);
        assert_eq!(out.persisted_grants, 0);
        assert_eq!(
            out.reply_items[0].scope,
            gait_state::proto::gait::log::v1::Scope::Session as i32,
        );
        let config_after = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();
        assert_eq!(config_before, config_after);
    }

    #[test]
    fn unlock_returns_not_found_when_no_pending_unlock() {
        let ws = ws();
        let _tx = make_session(ws.path(), "sunc07");
        let err = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc07".to_string(),
            scope: GrantScope::Once,
            agent: None,
        })
        .unwrap_err();
        assert!(matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::NotFound(_))));
    }

    #[test]
    fn unlock_empty_pending_items_writes_no_reply_no_grants() {
        // An empty unlock event list (no items) returns Ok with empty
        // reply_items — matches the original CLI's "no pending unlock
        // items" branch.
        let ws = ws();
        let tx = make_session(ws.path(), "sunc08");
        let body = gait_state::proto::events::sandbox_unlock(&[]);
        wire::append(&tx, body).unwrap();

        let out = unlock(UnlockParams {
            gait_dir: ws.path().to_path_buf(),
            session_id: "sunc08".to_string(),
            scope: GrantScope::Global,
            agent: None,
        })
        .unwrap();
        assert!(out.reply_items.is_empty());
        assert_eq!(out.persisted_grants, 0);
        // Still no events beyond the seeded empty Unlock.
        let events = read_events(&tx).unwrap();
        assert_eq!(events.len(), 1);
    }

    // -- reply --

    #[test]
    fn reply_writes_per_item_distinct_tool_calls() {
        use gait_state::proto::gait::log::v1 as pb;
        let ws = ws();
        let tx = make_session(ws.path(), "srep01");
        let body = gait_state::proto::events::permissions(&[
            pb::PermissionItem {
                id: "p1".into(),
                tool_name: "shell".into(),
                cmd: "cargo build".into(),
                patterns: vec!["shell:cargo *".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
            pb::PermissionItem {
                id: "p2".into(),
                tool_name: "shell".into(),
                cmd: "git push".into(),
                patterns: vec!["shell:git push".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        ]);
        gait_state::proto::wire::append(&tx, body).unwrap();

        let outcome = reply(
            ws.path(),
            "srep01",
            vec![
                ItemChoice {
                    id: "p1".into(),
                    decision: pb::Decision::Allow,
                    scope: GrantScope::Session,
                    pattern: Some("shell:cargo *".into()),
                },
                ItemChoice {
                    id: "p2".into(),
                    decision: pb::Decision::Deny,
                    scope: GrantScope::Once,
                    pattern: None,
                },
            ],
        )
        .unwrap();

        assert_eq!(outcome.reply_items.len(), 2);
        // p1: allow / session / pattern carried in text
        assert_eq!(outcome.reply_items[0].id, "p1");
        assert_eq!(outcome.reply_items[0].decision, pb::Decision::Allow as i32);
        assert_eq!(outcome.reply_items[0].scope, pb::Scope::Session as i32);
        assert_eq!(
            outcome.reply_items[0].text.as_deref(),
            Some("shell:cargo *")
        );
        // p2: deny / no pattern
        assert_eq!(outcome.reply_items[1].decision, pb::Decision::Deny as i32);
        assert_eq!(outcome.reply_items[1].text, None);
        assert_eq!(outcome.reply_items[1].scope, pb::Scope::Unspecified as i32);
        // one PermissionsReply event trails the Permissions request
        assert!(matches!(
            read_kind(&tx, 1),
            payload::Kind::PermissionsReply(_)
        ));
    }

    #[test]
    fn reply_errors_on_unknown_item_id() {
        use gait_state::proto::gait::log::v1 as pb;
        let ws = ws();
        let tx = make_session(ws.path(), "srep02");
        seed_permission(
            &tx,
            pb::PermissionItem {
                id: "p1".into(),
                tool_name: "shell".into(),
                cmd: "ls".into(),
                patterns: vec!["ls".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        );
        let err = reply(
            ws.path(),
            "srep02",
            vec![ItemChoice {
                id: "nope".into(),
                decision: pb::Decision::Deny,
                scope: GrantScope::Once,
                pattern: None,
            }],
        )
        .unwrap_err();
        assert!(matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::NotFound(_))), "got {err:?}");
    }

    #[test]
    fn reply_errors_when_no_pending_permissions() {
        use gait_state::proto::gait::log::v1 as pb;
        let ws = ws();
        make_session(ws.path(), "srep03"); // session dir exists, no Permissions event
        let err = reply(
            ws.path(),
            "srep03",
            vec![ItemChoice {
                id: "p1".into(),
                decision: pb::Decision::Deny,
                scope: GrantScope::Once,
                pattern: None,
            }],
        )
        .unwrap_err();
        assert!(matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::NotFound(_))), "got {err:?}");
    }

    #[test]
    fn reply_errors_on_incomplete_coverage() {
        // Two pending items; answer only one. The omitted item would be silently
        // denied at resume and can never be answered, so reply must reject.
        use gait_state::proto::gait::log::v1 as pb;
        let ws = ws();
        let tx = make_session(ws.path(), "srep04");
        let body = gait_state::proto::events::permissions(&[
            pb::PermissionItem {
                id: "p1".into(),
                tool_name: "read".into(),
                cmd: "read a".into(),
                patterns: vec!["read:a".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
            pb::PermissionItem {
                id: "p2".into(),
                tool_name: "shell".into(),
                cmd: "ls".into(),
                patterns: vec!["shell:ls".into()],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        ]);
        gait_state::proto::wire::append(&tx, body).unwrap();

        let err = reply(
            ws.path(),
            "srep04",
            vec![ItemChoice {
                id: "p1".into(),
                decision: pb::Decision::Allow,
                scope: GrantScope::Once,
                pattern: Some("read:a".into()),
            }],
        )
        .unwrap_err();
        assert!(
            coded_error::downcast_ref::<OpError>(&err).is_none(),
            "an untyped failure must not carry an OpError variant, got {err:?}"
        );
        // No PermissionsReply was written — the session is still AwaitingPermission.
        assert_eq!(read_events(&tx).map(|e| e.len()).unwrap_or(0), 1);
    }
}
