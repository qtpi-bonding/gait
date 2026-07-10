//! Scoped registry: whitelist resolution feeding the register/enable/load
//! model (docs/plans/2026-07-06-scoped-registry-revamp.md). Combines an
//! agent's own frontmatter whitelist (Phase 1) with a session's additive
//! whitelist (Phase 2) into the effective *visible* set the catalog gate and
//! `Toggle`-write enforcement (Phase 3) both check against.

use crate::agent_frontmatter::{read_agent_mcp, read_agent_plugins, read_agent_skills};
use std::collections::HashSet;
use std::path::Path;

/// Per-kind agent-frontmatter whitelist (Phase 1's `skills:`/`mcp:`/`plugins:`
/// frontmatter fields). Default-deny: an agent with no field for a kind sees
/// nothing of that kind from the global registry (empty list). A literal
/// `"*"` entry means "everything" (opt back into today's unfiltered
/// behavior).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentWhitelist {
    pub skills: Vec<String>,
    pub mcp: Vec<String>,
    pub plugins: Vec<String>,
}

impl AgentWhitelist {
    /// Read all three whitelists from an agent's frontmatter. `None` (no
    /// agent file -- e.g. no `--agent` given) yields all-empty (default-deny).
    pub fn from_agent_path(agent_path: Option<&Path>) -> Self {
        match agent_path {
            Some(p) => AgentWhitelist {
                skills: read_agent_skills(p.to_path_buf()).unwrap_or_default(),
                mcp: read_agent_mcp(p.to_path_buf()).unwrap_or_default(),
                plugins: read_agent_plugins(p.to_path_buf()).unwrap_or_default(),
            },
            None => AgentWhitelist::default(),
        }
    }

    /// Every kind sees everything -- the escape hatch, used for synthetic
    /// (non-agent-file) profiles like workflow nodes and the curate filter,
    /// which aren't the untrusted, narrowly-scoped case this plan restricts.
    pub fn wildcard() -> Self {
        AgentWhitelist {
            skills: vec!["*".to_string()],
            mcp: vec!["*".to_string()],
            plugins: vec!["*".to_string()],
        }
    }
}

/// Does `whitelist` grant visibility to `name`? Empty = default-deny (nothing
/// visible); a literal `"*"` entry = everything visible.
pub fn whitelist_allows(whitelist: &[String], name: &str) -> bool {
    whitelist.iter().any(|w| w == "*" || w == name)
}

/// Resolve the effective *visible* set for one kind: `agent_whitelist` (all
/// of `all_names`, if it contains `"*"`; else only the named ones) **union**
/// `session_whitelist` (Phase 2's additive, per-session extra grants -- a
/// session can only add visibility on top of the agent's own whitelist, never
/// narrow it). Used by the catalog gate (`finish_after_resume`) and the
/// `Toggle`-write enforcement site, not the genesis seed gate (`init`) --
/// session data isn't reachable there yet (chicken-and-egg: the seed is the
/// fold's own input). See the plan's two correction passes for why the seed
/// and catalog gates are deliberately different mechanisms.
pub fn resolve_visible_set(
    all_names: &[String],
    agent_whitelist: &[String],
    session_whitelist: &HashSet<String>,
) -> HashSet<String> {
    let mut visible: HashSet<String> = if agent_whitelist.iter().any(|w| w == "*") {
        all_names.iter().cloned().collect()
    } else {
        all_names
            .iter()
            .filter(|n| agent_whitelist.iter().any(|w| w == *n))
            .cloned()
            .collect()
    };
    visible.extend(session_whitelist.iter().cloned());
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_allows_empty_denies_everything() {
        assert!(!whitelist_allows(&[], "tdd"));
    }

    #[test]
    fn whitelist_allows_wildcard_allows_everything() {
        assert!(whitelist_allows(&["*".to_string()], "anything"));
    }

    #[test]
    fn whitelist_allows_named_entry_only() {
        let wl = vec!["tdd".to_string()];
        assert!(whitelist_allows(&wl, "tdd"));
        assert!(!whitelist_allows(&wl, "other"));
    }

    #[test]
    fn resolve_visible_set_empty_agent_whitelist_is_default_deny() {
        let all = vec!["tdd".to_string(), "brainstorming".to_string()];
        let visible = resolve_visible_set(&all, &[], &HashSet::new());
        assert!(visible.is_empty());
    }

    #[test]
    fn resolve_visible_set_wildcard_agent_whitelist_sees_everything() {
        let all = vec!["tdd".to_string(), "brainstorming".to_string()];
        let visible = resolve_visible_set(&all, &["*".to_string()], &HashSet::new());
        assert_eq!(visible.len(), 2);
    }

    #[test]
    fn resolve_visible_set_named_agent_whitelist_filters_to_named_only() {
        let all = vec!["tdd".to_string(), "brainstorming".to_string()];
        let visible = resolve_visible_set(&all, &["tdd".to_string()], &HashSet::new());
        assert_eq!(visible, ["tdd".to_string()].into_iter().collect());
    }

    #[test]
    fn resolve_visible_set_session_whitelist_is_additive_union() {
        let all = vec!["tdd".to_string(), "brainstorming".to_string()];
        let mut session = HashSet::new();
        session.insert("brainstorming".to_string());
        // Agent whitelist alone would see only tdd; session extends it.
        let visible = resolve_visible_set(&all, &["tdd".to_string()], &session);
        assert!(visible.contains("tdd"));
        assert!(visible.contains("brainstorming"));
    }

    #[test]
    fn resolve_visible_set_session_whitelist_cannot_narrow_agent_grant() {
        // Session scope is additive-only; an empty session whitelist doesn't
        // remove what the agent whitelist already grants.
        let all = vec!["tdd".to_string()];
        let visible = resolve_visible_set(&all, &["*".to_string()], &HashSet::new());
        assert!(visible.contains("tdd"));
    }
}
