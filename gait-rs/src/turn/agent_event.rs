//! Engine-side `Agent` event builders.
//!
//! `agent_msg` / `agent` build the proto `Agent` message from the live
//! `ResolvedProfile` (an engine type). They lived in `proto/events.rs` for
//! historical reasons, which made proto (heading to `gait-state`) name the
//! engine profile — a state→engine back-reference. They were moved here so the
//! proto facade stays profile-free; their only caller is `turn::setup`.

use crate::profile::ResolvedProfile;
use crate::proto::gait::log::v1 as pb;
use crate::proto::wire;

/// Build the proto `Agent` message from the live resolved profile.
/// Mapped field-by-field from the in-memory `ResolvedProfile`. The `mcp_servers`
/// list is sorted because the source `mcp_clients` is a `HashMap` whose
/// iteration order is nondeterministic; sorting makes the emitted event stable
/// so the drift check in `run_turn` doesn't thrash on otherwise-identical
/// agent configs.
pub fn agent_msg(resolved: &ResolvedProfile) -> pb::Agent {
    #[cfg(feature = "mcp")]
    let mut mcp_servers: Vec<String> = resolved.mcp_clients.keys().cloned().collect();
    #[cfg(not(feature = "mcp"))]
    let mut mcp_servers: Vec<String> = Vec::new();
    mcp_servers.sort();
    pb::Agent {
        instructions: resolved.instructions.clone(),
        tools: resolved
            .permissioned
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect(),
        mcp_servers,
        skills: resolved
            .skill_registry
            .iter()
            .filter(|s| s.model_invocable)
            .map(|s| s.name.clone())
            .collect(),
        plugins: Vec::new(),
        permissions: Some(pb::PermissionPolicy {
            allow: resolved.permissioned.allow.clone(),
            deny: resolved.permissioned.deny.clone(),
        }),
        through_seq: 0,
        inference: Some(pb::Inference {
            name: String::new(),
            model: resolved.model.clone(),
            llm_params: Some(crate::proto::events::llm_params_to_proto(
                &resolved.llm_params,
            )),
            compaction_threshold_tokens: resolved.context_policy.compaction_threshold_tokens,
        }),
    }
}

/// The `Agent` event body.
#[allow(dead_code)] // exercised by the mapping test; `agent_msg` is the prod entry
pub fn agent(resolved: &ResolvedProfile) -> pb::event::Body {
    wire::body(pb::payload::Kind::Agent(agent_msg(resolved)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(body: &pb::event::Body) -> &pb::payload::Kind {
        match body {
            pb::event::Body::Payload(p) => p.kind.as_ref().expect("kind set"),
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    // ── Dummy provider for the mapping test (we never call into it) ──────────
    use crate::types::{Message, Tool};
    use async_trait::async_trait;
    use gait_provider::provider::{Provider, ProviderError, StreamChunk};
    use std::sync::Arc;

    pub struct DummyProvider;
    #[async_trait]
    impl Provider for DummyProvider {
        fn model_id(&self) -> &str {
            "dummy"
        }
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[Arc<dyn Tool>],
            _params: &crate::config::LlmParamsConfig,
            _tail_context: Option<&str>,
        ) -> Result<
            std::pin::Pin<
                Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
            >,
            ProviderError,
        > {
            Err(ProviderError::Other("dummy".into()))
        }
    }

    #[test]
    fn agent_maps_fields_from_resolved_profile() {
        use crate::profile::ResolvedProfile;
        use crate::tools::StructuredDoneTool;

        // Build a ResolvedProfile with everything populated, via for_test
        // (which gives a stub provider + empty allow/deny/skills) and then
        // override the fields the mapping reads. We pass a real Tool (Output)
        // so the Vec<Arc<dyn Tool>> is constructable in for_test.
        let mut profile = ResolvedProfile::for_test(
            Arc::new(DummyProvider) as Arc<dyn gait_provider::provider::Provider>,
            vec![Arc::new(StructuredDoneTool::new())],
            vec!["read".into()],
            vec!["shell:rm *".into()],
        );
        profile.model = "anthropic/x".into();
        profile.instructions = "be terse".into();

        let body = agent(&profile);
        let r = match kind_of(&body) {
            pb::payload::Kind::Agent(r) => r.clone(),
            other => panic!("expected Agent, got {other:?}"),
        };
        assert_eq!(r.inference.unwrap().model, "anthropic/x");
        let perms = r.permissions.as_ref().expect("permissions");
        assert_eq!(perms.allow, vec!["read".to_string()]);
        assert_eq!(perms.deny, vec!["shell:rm *".to_string()]);
        // mcp_servers is empty in for_test (no MCP clients); trivially sorted.
        assert!(r.mcp_servers.is_empty());
        // skills is empty in for_test default.
        assert!(r.skills.is_empty());
    }
}
