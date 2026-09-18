//! Permissions facade — the single owner of the tool-permission pipeline.
//!
//! This module is the **only** call site for permission decisions. The submodule
//! layout is:
//!
//! - [`matching`] — the pure pattern matcher (the one matcher in the system).
//! - [`gate`] — the decision tree (`classify` returning [`gate::GateDecision`]).
//! - [`grants`] — grant resolution (re-exports [`grants::ActivePermissions`]
//!   /[`grants::AllowState`] from `retrace`, and owns
//!   [`grants::effective_allow`]).
//! - [`persist`] — the scope→target persistence policy
//!   ([`persist::save_choice`] / [`persist::compute_allow_decisions`]).
//!
//! # 1. Decision order (per gate, in [`gate::classify`])
//!
//! For every tool call, [`gate::classify`] runs, in order:
//!
//! 1. **Tool [`ToolPermission`](crate::types::ToolPermission) default** — the
//!    tool's self-described permission baseline.
//! 2. **Deny hard floor** — applied in **all** modes, including auto. Any
//!    deny-list entry matching the request short-circuits to
//!    [`gate::GateDecision::Deny`]. Deny is checked **before** allow.
//! 3. **Allow** — if any allow-list entry matches, the request is approved
//!    (`auto` mode short-circuits straight to
//!    [`gate::GateDecision::Allow`]; non-auto paths emit
//!    [`gate::GateDecision::Allow`] with an `approval_prefix`).
//! 4. **Ask (gate)** — otherwise, the request is queued for user approval via
//!    [`gate::GateDecision::Gate`].
//!
//! # 2. Grant scopes and lifecycle
//!
//! | Scope    | Persisted where                                         | Enters via                                                    | Lifetime                                                       |
//! |----------|---------------------------------------------------------|---------------------------------------------------------------|----------------------------------------------------------------|
//! | `Global` | config (`gait.jsonc` `allow[]`)                         | the `seed` passed to [`grants::effective_allow`]              | until removed from config                                      |
//! | `Mode`   | `modes/<name>/MODE.yaml` `allow[]`                      | [`grants::overlay_mode_permissions`], applied after `effective_allow` is resolved | while the mode is active / until changed                       |
//! | `Session`| log (`PermissionsReply` events)                         | `retrace::ActivePermissions` fold (session bucket)            | the current session                                            |
//! | `Once`   | log (`PermissionsReply` events)                         | `retrace::ActivePermissions` fold (once bucket)               | one turn — cleared at `RunComplete`                           |
//!
//! Session-scoped `Deny` forward-revokes a prior session-granted allow via the
//! `retrace::ActivePermissions` fold, AND adds the pattern to the fold's
//! `deny_session` bucket — [`grants::effective_deny_session`] unions this into
//! the gate's `deny` list, so an identical request is never asked about again
//! this session (the Deny-side mirror of session-scoped `Allow`). Once-scoped
//! `Deny` is a true no-op and does not persist. The persist-side policy lives
//! in [`persist::save_choice`]; the mechanism (file editing) stays in
//! `config` / `modes` — neither has a Deny arm yet, since no
//! caller constructs a file-backed (Global/Mode) deny choice today.
//!
//! # 3. The single-matcher invariant
//!
//! All grant sources — config seed, mode overlay, replayed
//! `session ∪ once` grants — collapse into **one**
//! `effective_allow: Vec<String>` built by [`grants::effective_allow`] and
//! matched by exactly one predicate, [`matching::matches_pattern_entry`]
//! (reached through [`matching::is_request_approved`],
//! [`matching::is_request_denied`], [`matching::find_matched_allow_pattern`],
//! and [`matching::find_matched_deny_pattern`]).
//!
//! **No grant source may introduce its own comparison.** If you find yourself
//! adding a second matcher, you are reintroducing the divergence this facade
//! was created to prevent — write a regression test against
//! `matches_pattern_entry` instead.

pub mod gate;
pub mod grants;
pub mod matching;
pub mod persist;

// Facade surface — every item an external consumer may rely on by name.
// `matches_pattern_entry` itself stays private to `matching`; it is the
// shared predicate behind the `is_request_*` / `find_matched_*` helpers
// below.
pub use matching::{
    command_patterns, find_matched_allow_pattern, find_matched_deny_pattern, is_request_approved,
    is_request_denied, mcp_permission, path_patterns, split_commands, wildcard_match, AllowParams,
    AllowScope, PermissionChoice,
};
pub use persist::{compute_allow_decisions, save_choice};

/// Deny is monotonic: every layer's deny is final. Union config-deny with the
/// agent's deny (dedup, config order first). Only `allow` is innermost-wins.
///
/// Owning this in `permissions/` keeps the deny-vs-allow direction state-side;
/// the engine's `profile.rs` reaches down to it.
pub fn merge_deny(config_deny: &[String], agent_deny: &[String]) -> Vec<String> {
    let mut out = config_deny.to_vec();
    for d in agent_deny {
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}
