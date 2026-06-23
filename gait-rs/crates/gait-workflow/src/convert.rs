//! Canonical workflow IR. yaml -> serde WorkflowDef (parser) -> proto Workflow
//! (canonical). The proto is the source of truth; serde is only the parser.
use gait_core::proto::gait::workflow::v1::{
    node, BranchNode, GateNode, Hooks, Node, OnError, PromptNode, ShellNode, Workflow,
};

use crate::schema::{HooksConfig, NodeDef, OnErrorConfig, WorkflowDef};

pub fn to_proto(def: &WorkflowDef) -> Workflow {
    Workflow {
        version: 1,
        name: def.name.clone(),
        version_label: def.version.clone(),
        model: def.model.clone(),
        allow: def.allow.clone(),
        auto: def.auto,
        nodes: def.nodes.iter().map(node_to_proto).collect(),
    }
}

fn hooks(h: &Option<HooksConfig>) -> Option<Hooks> {
    h.as_ref().map(|h| Hooks {
        before: h.before.clone(),
        after: h.after.clone(),
        error: h.error.clone(),
    })
}

fn on_error(o: &Option<OnErrorConfig>) -> Option<OnError> {
    o.as_ref().map(|o| OnError {
        retry_times: o.retry_times,
        delay: o.delay,
        backoff: o.backoff.clone(),
        context: o.context.clone(),
        goto: o.goto.clone(),
    })
}

fn node_to_proto(n: &NodeDef) -> Node {
    let body = if let Some(p) = &n.prompt {
        Some(node::Body::Prompt(PromptNode {
            prompt: p.clone(),
            continue_session: n.continue_session.clone().unwrap_or_default(),
            codebase: n.codebase.clone(),
        }))
    } else if let Some(s) = &n.shell {
        Some(node::Body::Shell(ShellNode {
            command: s.run().to_string(),
            timeout_ms: Some(s.timeout_ms()),
        }))
    } else if let Some(b) = &n.branch {
        Some(node::Body::Branch(BranchNode {
            on: b.on.clone(),
            then: b.then.clone(),
            r#else: b.else_.clone(),
            cases: b.cases.clone(),
        }))
    } else {
        n.gate.as_ref().map(|g| {
            node::Body::Gate(GateNode {
                message: g.message().to_string(),
            })
        })
    };
    Node {
        id: n.id.clone(),
        depends_on: n.depends_on.clone(),
        model: n.model.clone(),
        auto: n.auto,
        allow: n.allow.clone().unwrap_or_default(),
        agent: n.agent.clone(),
        system_prompt: n.system_prompt.clone(),
        files: n.files.clone().unwrap_or_default(),
        output_schema_json: n
            .output_schema
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
        on_error: on_error(&n.on_error),
        goto: n.goto.clone(),
        hooks: hooks(&n.hooks),
        body,
    }
}

/// `{workflow}-{unixsecs}-{subsec_nanos:09}-{counter}` — human-readable AND
/// collision-free. Full subsec nanos makes cross-process same-instant collision
/// astronomically unlikely; a process-local atomic counter guarantees uniqueness
/// for repeated in-process calls.
pub fn mint_run_id(workflow_name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{workflow_name}-{}-{:09}-{n}",
        now.as_secs(),
        now.subsec_nanos()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::node;

    #[test]
    fn converts_behavioral_fields_not_just_obvious_ones() {
        let def: crate::schema::WorkflowDef = serde_yaml::from_str(
            r#"
name: t
version: "1"
model: anthropic/x
allow: [Bash]
auto: true
nodes:
  - id: scan
    prompt: "do {{ args.dir }}"
    on_error: { retry_times: 2, goto: fix }
    hooks: { after: "echo done" }
  - id: br
    depends_on: [scan]
    branch: { on: "{{ scan.output }}", then: fix, else: stop }
  - id: sh
    shell: "ls -1"
"#,
        )
        .unwrap();
        let p = to_proto(&def);
        assert_eq!(p.version, 1);
        assert_eq!(p.version_label, "1");
        let scan = &p.nodes[0];
        assert_eq!(scan.on_error.as_ref().unwrap().retry_times, 2);
        assert_eq!(scan.on_error.as_ref().unwrap().goto.as_deref(), Some("fix"));
        assert_eq!(
            scan.hooks.as_ref().unwrap().after.as_deref(),
            Some("echo done")
        );
        match scan.body.as_ref().unwrap() {
            node::Body::Prompt(pn) => assert!(pn.prompt.contains("do")),
            _ => panic!(),
        }
        match p.nodes[1].body.as_ref().unwrap() {
            node::Body::Branch(b) => {
                assert!(b.on.contains("scan.output"));
                assert_eq!(b.then.as_deref(), Some("fix"));
                assert_eq!(b.r#else.as_deref(), Some("stop"));
            }
            _ => panic!(),
        }
        match p.nodes[2].body.as_ref().unwrap() {
            node::Body::Shell(s) => assert_eq!(s.command, "ls -1"),
            _ => panic!(),
        }
    }

    #[test]
    fn mint_run_id_prefixed_and_monotonic() {
        let a = mint_run_id("t");
        let b = mint_run_id("t");
        assert!(a.starts_with("t-"));
        assert_ne!(a, b, "consecutive mints must differ (monotonic suffix)");
    }
}
