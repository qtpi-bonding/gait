//! # Permission grants — projection + session∪once union
//!
//! This is the single owner of the *effective-allow* materialization for the
//! permission gate. The gate matches tool calls against a flat `Vec<String>`
//! of allowed patterns; this module produces that vec from three sources:
//!
//! - `seed` — the config baseline (global + agent grants, passed in by caller),
//! - `session` grants — `PermissionsReply` events with `Scope::Session`,
//! - `once` grants — `PermissionsReply` events with `Scope::Once` (cleared at
//!   `RunComplete` by the `ActivePermissions` projection).
//!
//! The underlying fold is the [`ActivePermissions`] projection in
//! [`crate::session::retrace`]; this module re-exports the projection + state
//! types and owns the `session ∪ once` union. The mutation cascade (the
//! non-permission-specific `Redact`/`Replace` fold) stays with the caller —
//! this fn receives the already-cascaded event+mutation slices and only runs
//! the permission projection on top.
//!
//! Behavior is identical to the previous inline computation in
//! `turn/mod.rs`; this is a pure relocation.

pub use crate::session::retrace::ActivePermissions;
#[allow(unused_imports)]
pub use crate::session::retrace::AllowState;

use crate::proto::gait::log::v1::Event;
use crate::session::retrace::{run_projection, MutationState};
use std::collections::HashSet;

/// Resolve the config seed + replayed session/once grants into the flat
/// allow-list the gate matches against: `session ∪ once`, seeded by `seed`
/// (config global+agent). Callers still build `evs`/`muts` via
/// `active_mutations` + `apply_cascade` (the mutation cascade is not
/// permission-specific and stays with the caller) — this fn only runs the
/// [`ActivePermissions`] projection and unions `session ∪ once`.
pub fn effective_allow(
    seed: HashSet<String>,
    evs: &[Event],
    muts: &std::collections::HashMap<String, MutationState>,
) -> Vec<String> {
    let st = run_projection(&ActivePermissions { seed }, evs, muts);
    st.session.union(&st.once).cloned().collect()
}

/// Deny-side mirror of [`effective_allow`]: session-scoped Reject patterns
/// (the `deny_session` bucket of [`crate::session::retrace::AllowState`]),
/// to be unioned into the gate's `deny` list. There is no once-scoped
/// equivalent — once-scoped Deny is a deliberate no-op, matching plain
/// "Reject" today. `seed` is empty here; the config/agent deny baseline is
/// unioned in separately by the caller via `merge_deny`.
pub fn effective_deny_session(
    evs: &[Event],
    muts: &std::collections::HashMap<String, MutationState>,
) -> Vec<String> {
    let st = run_projection(
        &ActivePermissions {
            seed: HashSet::new(),
        },
        evs,
        muts,
    );
    st.deny_session.into_iter().collect()
}

/// Overlay an active mode's allow/deny policy onto the resolved turn policy.
/// `None` for either is the identity (default / no mode, or a mode with an
/// empty list). Deny is monotonic (unioned via `merge_deny`, dedup, config
/// order first — see that fn's doc). Allow is a plain union: mode allow
/// entries are appended after `effective_allow`'s existing entries,
/// deduped. Pure; no I/O.
pub fn overlay_mode_permissions(
    effective_allow: Vec<String>,
    deny: Vec<String>,
    mode_allow: Option<&Vec<String>>,
    mode_deny: Option<&Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    let effective_allow = match mode_allow {
        Some(mode_allow) => {
            let mut ea = effective_allow;
            for p in mode_allow {
                if !ea.contains(p) {
                    ea.push(p.clone());
                }
            }
            ea
        }
        None => effective_allow,
    };
    let deny = match mode_deny {
        Some(mode_deny) => crate::permissions::merge_deny(&deny, mode_deny),
        None => deny,
    };
    (effective_allow, deny)
}

#[cfg(test)]
mod effective_deny_session_tests {
    use super::*;
    use crate::proto::events;
    use crate::proto::gait::log::v1 as pb;

    fn mint_events(bodies: Vec<pb::event::Body>) -> Vec<Event> {
        bodies
            .into_iter()
            .map(|body| {
                crate::proto::envelope::mint(
                    body,
                    None,
                    crate::proto::envelope::EventScope::Step {
                        run: 0,
                        turn: 0,
                        step: 0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn session_scoped_reject_is_unioned_in() {
        let evs = mint_events(vec![events::permissions_reply(&[
            pb::PermissionsReplyItem {
                id: "1".into(),
                decision: pb::Decision::Deny as i32,
                text: Some("shell:curl *".into()),
                scope: pb::Scope::Session as i32,
            },
        ])]);
        let deny = effective_deny_session(&evs, &std::collections::HashMap::new());
        assert_eq!(deny, vec!["shell:curl *".to_string()]);
    }

    #[test]
    fn plain_reject_once_contributes_nothing() {
        let evs = mint_events(vec![events::permissions_reply(&[
            pb::PermissionsReplyItem {
                id: "1".into(),
                decision: pb::Decision::Deny as i32,
                text: None,
                scope: pb::Scope::Unspecified as i32,
            },
        ])]);
        let deny = effective_deny_session(&evs, &std::collections::HashMap::new());
        assert!(deny.is_empty(), "once-scoped/plain reject must not persist");
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn overlay_only_monotonically_denies() {
        let ea = vec!["read:*".to_string()];
        let deny = vec![];
        let mode = vec!["write".into()];
        let (ea2, deny2) = overlay_mode_permissions(ea, deny, None, Some(&mode));
        assert_eq!(ea2, vec!["read:*".to_string()]);
        assert_eq!(deny2, vec!["write".to_string()]); // mode deny lands in the monotonic deny set
                                                      // None overlay is identity:
        let (ea3, deny3) = overlay_mode_permissions(vec!["x".into()], vec!["y".into()], None, None);
        assert_eq!((ea3, deny3), (vec!["x".to_string()], vec!["y".to_string()]));
    }

    #[test]
    fn overlay_unions_mode_allow_into_effective_allow() {
        let ea = vec!["read:*".to_string()];
        let deny = vec![];
        let mode_allow = vec!["shell:cargo *".to_string()];
        let mode_deny = vec!["write".to_string()];
        let (ea2, deny2) =
            overlay_mode_permissions(ea, deny, Some(&mode_allow), Some(&mode_deny));
        assert_eq!(ea2, vec!["read:*".to_string(), "shell:cargo *".to_string()]);
        assert_eq!(deny2, vec!["write".to_string()]);
    }

    #[test]
    fn merge_deny_unions_and_dedups() {
        let d = super::super::merge_deny(
            &["shell:rm *".into(), "shell:curl *".into()],
            &["shell:curl *".into(), "edit:secrets/*".into()],
        );
        // config denies survive; agent denies are added; no duplicates
        assert_eq!(d, vec!["shell:rm *", "shell:curl *", "edit:secrets/*"]);
    }
}
