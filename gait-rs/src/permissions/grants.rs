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
