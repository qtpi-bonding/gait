//! # Permission grants — projection + session∪once union
//!
//! This is the single owner of the *effective-allow* materialization for the
//! permission gate. The gate matches tool calls against a flat `Vec<String>`
//! of allowed patterns; this module produces that vec from three sources:
//!
//! - `seed` — the config baseline (global + agent grants, passed in by caller),
//! - `session` grants — `PermissionsReply` events with `Scope::Session`,
//! - `once` grants — `PermissionsReply` events with `Scope::Once` (cleared at
//!   `TurnComplete` by the `ActivePermissions` projection).
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

pub(crate) use crate::session::retrace::ActivePermissions;
#[allow(unused_imports)]
pub(crate) use crate::session::retrace::AllowState;

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

use crate::proto::gait::log::v1 as pb;
/// Overlay an active mode's permission policy onto the resolved turn policy:
/// union its `allow` into the effective allow-list; fold its `deny` into the deny
/// list via `merge_deny` (monotonic — mode-deny clamps any session-granted allow).
/// `None` is the identity (default / no mode). Pure; no I/O.
pub fn overlay_mode_permissions(
    effective_allow: Vec<String>,
    deny: Vec<String>,
    mode: Option<&pb::PermissionPolicy>,
) -> (Vec<String>, Vec<String>) {
    match mode {
        Some(p) => {
            let mut ea = effective_allow;
            ea.extend(p.allow.iter().cloned());
            let deny = crate::permissions::merge_deny(&deny, &p.deny);
            (ea, deny)
        }
        None => (effective_allow, deny),
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    #[test]
    fn overlay_unions_allow_and_monotonically_denies() {
        let ea = vec!["read:*".to_string()];
        let deny = vec![];
        let mode = pb::PermissionPolicy {
            allow: vec!["shell:ls".into()],
            deny: vec!["write".into()],
        };
        let (ea2, deny2) = overlay_mode_permissions(ea, deny, Some(&mode));
        assert!(ea2.contains(&"read:*".to_string()) && ea2.contains(&"shell:ls".to_string()));
        assert_eq!(deny2, vec!["write".to_string()]); // mode deny lands in the monotonic deny set
                                                      // None overlay is identity:
        let (ea3, deny3) = overlay_mode_permissions(vec!["x".into()], vec!["y".into()], None);
        assert_eq!((ea3, deny3), (vec!["x".to_string()], vec!["y".to_string()]));
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
