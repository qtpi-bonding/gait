use gait_core::proto::gait::workflow::v1::{node, Workflow};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValidationError {
    DuplicateNodeId(String),
    DanglingReference {
        from_node: String,
        reference_kind: &'static str,
        target: String,
    },
    DependencyCycle(Vec<String>),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateNodeId(id) => write!(f, "duplicate node id: '{id}'"),
            Self::DanglingReference {
                from_node,
                reference_kind,
                target,
            } => write!(
                f,
                "node '{from_node}' has a {reference_kind} reference to nonexistent node '{target}'"
            ),
            Self::DependencyCycle(path) => write!(f, "depends_on cycle: {}", path.join(" -> ")),
        }
    }
}

/// Validate a workflow graph's shape: duplicate ids, dangling references, and
/// depends_on cycles. Does NOT validate branch-condition semantics or reject
/// goto/branch cycles (those are intentional — see
/// docs/superpowers/specs/2026-07-18-workflow-graph-validation-design.md
/// Non-goals). Pure function, no I/O. Returns every problem found, not just
/// the first.
pub fn validate_graph(wf: &Workflow) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let mut seen_ids: HashSet<&str> = HashSet::new();
    let mut duplicate_ids: HashSet<&str> = HashSet::new();
    for n in &wf.nodes {
        if !seen_ids.insert(n.id.as_str()) {
            duplicate_ids.insert(n.id.as_str());
        }
    }
    for id in &duplicate_ids {
        errors.push(ValidationError::DuplicateNodeId(id.to_string()));
    }

    let known_ids: HashSet<&str> = wf.nodes.iter().map(|n| n.id.as_str()).collect();

    for n in &wf.nodes {
        for dep in &n.depends_on {
            if !known_ids.contains(dep.as_str()) {
                errors.push(ValidationError::DanglingReference {
                    from_node: n.id.clone(),
                    reference_kind: "depends_on",
                    target: dep.clone(),
                });
            }
        }
        if let Some(goto) = &n.goto {
            if !known_ids.contains(goto.as_str()) {
                errors.push(ValidationError::DanglingReference {
                    from_node: n.id.clone(),
                    reference_kind: "goto",
                    target: goto.clone(),
                });
            }
        }
        if let Some(oe) = &n.on_error {
            if let Some(goto) = &oe.goto {
                if goto != "abort" && goto != "continue" && !known_ids.contains(goto.as_str()) {
                    errors.push(ValidationError::DanglingReference {
                        from_node: n.id.clone(),
                        reference_kind: "on_error.goto",
                        target: goto.clone(),
                    });
                }
            }
        }
        if let Some(node::Body::Branch(b)) = &n.body {
            if let Some(then) = &b.then {
                if !known_ids.contains(then.as_str()) {
                    errors.push(ValidationError::DanglingReference {
                        from_node: n.id.clone(),
                        reference_kind: "branch.then",
                        target: then.clone(),
                    });
                }
            }
            if let Some(else_) = &b.r#else {
                if !known_ids.contains(else_.as_str()) {
                    errors.push(ValidationError::DanglingReference {
                        from_node: n.id.clone(),
                        reference_kind: "branch.else",
                        target: else_.clone(),
                    });
                }
            }
            for target in b.cases.values() {
                if !known_ids.contains(target.as_str()) {
                    errors.push(ValidationError::DanglingReference {
                        from_node: n.id.clone(),
                        reference_kind: "branch.cases",
                        target: target.clone(),
                    });
                }
            }
        }
    }

    // Cycle detection on depends_on only — NOT goto/branch (see Non-goals).
    // Skip entirely if duplicate ids exist: the adjacency map below is keyed
    // by id, so a duplicate would silently merge two distinct nodes' edges
    // and could misreport a cycle that doesn't structurally exist (or hide a
    // real one). The duplicate-id error above already tells the user what to
    // fix first.
    if duplicate_ids.is_empty() {
        if let Some(cycle) = find_depends_on_cycle(wf) {
            errors.push(ValidationError::DependencyCycle(cycle));
        }
    }

    errors
}

/// Standard three-color DFS cycle detection over the depends_on graph.
/// Returns the first cycle found as a path, e.g. ["a", "b", "c", "a"], or
/// None if the graph is acyclic. Dangling depends_on entries (checked above)
/// are simply skipped here — they can't participate in a cycle back to a
/// real node.
fn find_depends_on_cycle(wf: &Workflow) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let adjacency: HashMap<&str, &[String]> = wf
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.depends_on.as_slice()))
        .collect();
    let mut color: HashMap<&str, Color> = wf
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), Color::White))
        .collect();

    fn visit<'a>(
        id: &'a str,
        adjacency: &HashMap<&'a str, &'a [String]>,
        color: &mut HashMap<&'a str, Color>,
        path: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        color.insert(id, Color::Gray);
        path.push(id.to_string());
        if let Some(deps) = adjacency.get(id) {
            for dep in deps.iter() {
                let dep = dep.as_str();
                match color.get(dep).copied() {
                    Some(Color::Gray) => {
                        let start = path.iter().position(|n| n == dep).unwrap_or(0);
                        let mut cycle = path[start..].to_vec();
                        cycle.push(dep.to_string());
                        return Some(cycle);
                    }
                    Some(Color::Black) | None => {}
                    Some(Color::White) => {
                        if let Some(cycle) = visit(dep, adjacency, color, path) {
                            return Some(cycle);
                        }
                    }
                }
            }
        }
        path.pop();
        color.insert(id, Color::Black);
        None
    }

    for n in &wf.nodes {
        if color.get(n.id.as_str()) == Some(&Color::White) {
            let mut path = Vec::new();
            if let Some(cycle) = visit(n.id.as_str(), &adjacency, &mut color, &mut path) {
                return Some(cycle);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::{BranchNode, Node, OnError};

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            ..Default::default()
        }
    }

    fn node_with_depends_on(id: &str, deps: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn wf(nodes: Vec<Node>) -> Workflow {
        Workflow {
            nodes,
            ..Default::default()
        }
    }

    #[test]
    fn valid_single_node_graph_has_no_errors() {
        let w = wf(vec![node("a")]);
        assert!(validate_graph(&w).is_empty());
    }

    #[test]
    fn valid_linear_chain_has_no_errors() {
        let w = wf(vec![
            node("a"),
            node_with_depends_on("b", &["a"]),
            node_with_depends_on("c", &["b"]),
        ]);
        assert!(validate_graph(&w).is_empty());
    }

    #[test]
    fn detects_duplicate_node_id() {
        let w = wf(vec![node("a"), node("a")]);
        let errors = validate_graph(&w);
        assert!(errors.contains(&ValidationError::DuplicateNodeId("a".to_string())));
    }

    #[test]
    fn detects_dangling_depends_on() {
        let w = wf(vec![node_with_depends_on("a", &["nonexistent"])]);
        let errors = validate_graph(&w);
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "a".to_string(),
            reference_kind: "depends_on",
            target: "nonexistent".to_string(),
        }));
    }

    #[test]
    fn detects_dangling_goto() {
        let w = wf(vec![Node {
            id: "a".to_string(),
            goto: Some("nonexistent".to_string()),
            ..Default::default()
        }]);
        let errors = validate_graph(&w);
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "a".to_string(),
            reference_kind: "goto",
            target: "nonexistent".to_string(),
        }));
    }

    #[test]
    fn detects_dangling_on_error_goto() {
        let w = wf(vec![Node {
            id: "a".to_string(),
            on_error: Some(OnError {
                goto: Some("nonexistent".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        let errors = validate_graph(&w);
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "a".to_string(),
            reference_kind: "on_error.goto",
            target: "nonexistent".to_string(),
        }));
    }

    #[test]
    fn on_error_goto_abort_and_continue_are_not_dangling() {
        for keyword in ["abort", "continue"] {
            let w = wf(vec![Node {
                id: "a".to_string(),
                on_error: Some(OnError {
                    goto: Some(keyword.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }]);
            let errors = validate_graph(&w);
            assert!(
                errors.is_empty(),
                "'{keyword}' must not be flagged as dangling, got: {errors:?}"
            );
        }
    }

    #[test]
    fn detects_dangling_branch_then_else_and_cases() {
        let mut cases = HashMap::new();
        cases.insert("x".to_string(), "nonexistent-case-target".to_string());
        let w = wf(vec![Node {
            id: "a".to_string(),
            body: Some(node::Body::Branch(BranchNode {
                on: "$foo".to_string(),
                then: Some("nonexistent-then".to_string()),
                r#else: Some("nonexistent-else".to_string()),
                cases,
            })),
            ..Default::default()
        }]);
        let errors = validate_graph(&w);
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "a".to_string(),
            reference_kind: "branch.then",
            target: "nonexistent-then".to_string(),
        }));
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "a".to_string(),
            reference_kind: "branch.else",
            target: "nonexistent-else".to_string(),
        }));
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "a".to_string(),
            reference_kind: "branch.cases",
            target: "nonexistent-case-target".to_string(),
        }));
    }

    #[test]
    fn valid_branch_targets_produce_no_errors() {
        let mut cases = HashMap::new();
        cases.insert("x".to_string(), "c".to_string());
        let w = wf(vec![
            Node {
                id: "a".to_string(),
                body: Some(node::Body::Branch(BranchNode {
                    on: "$foo".to_string(),
                    then: Some("b".to_string()),
                    r#else: Some("c".to_string()),
                    cases,
                })),
                ..Default::default()
            },
            node("b"),
            node("c"),
        ]);
        assert!(validate_graph(&w).is_empty());
    }

    #[test]
    fn detects_simple_three_node_cycle() {
        let w = wf(vec![
            node_with_depends_on("a", &["c"]),
            node_with_depends_on("b", &["a"]),
            node_with_depends_on("c", &["b"]),
        ]);
        let errors = validate_graph(&w);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ValidationError::DependencyCycle(_))),
            "expected a DependencyCycle error, got: {errors:?}"
        );
    }

    #[test]
    fn goto_only_cycle_does_not_trigger_dependency_cycle() {
        // a and b goto each other — no depends_on at all. This is an
        // intentional, supported control-flow loop (see design spec
        // Non-goals) and must NOT be reported as a DependencyCycle.
        let w = wf(vec![
            Node {
                id: "a".to_string(),
                goto: Some("b".to_string()),
                ..Default::default()
            },
            Node {
                id: "b".to_string(),
                goto: Some("a".to_string()),
                ..Default::default()
            },
        ]);
        let errors = validate_graph(&w);
        assert!(
            !errors
                .iter()
                .any(|e| matches!(e, ValidationError::DependencyCycle(_))),
            "goto-only cycle must not be reported as a depends_on cycle, got: {errors:?}"
        );
        assert!(
            errors.is_empty(),
            "goto targets exist and form a valid loop, got: {errors:?}"
        );
    }

    #[test]
    fn multiple_independent_errors_are_all_returned() {
        let w = wf(vec![
            node("a"),
            node("a"),                                   // duplicate
            node_with_depends_on("b", &["nonexistent"]), // dangling
        ]);
        let errors = validate_graph(&w);
        // Order-insensitive: duplicate-id reporting iterates a HashSet internally.
        let errors: HashSet<&ValidationError> = errors.iter().collect();
        assert!(errors.contains(&ValidationError::DuplicateNodeId("a".to_string())));
        assert!(errors.contains(&ValidationError::DanglingReference {
            from_node: "b".to_string(),
            reference_kind: "depends_on",
            target: "nonexistent".to_string(),
        }));
    }

    #[test]
    fn duplicate_ids_do_not_spuriously_trigger_cycle_check() {
        // Two nodes named "a", one of which depends_on "a" — this is
        // ambiguous (which "a"?) and must be reported as a duplicate-id
        // error, not additionally (and confusingly) as a cycle from the
        // merged adjacency the duplicate would otherwise produce.
        let w = wf(vec![node("a"), node_with_depends_on("a", &["a"])]);
        let errors = validate_graph(&w);
        assert!(errors
            .iter()
            .any(|e| matches!(e, ValidationError::DuplicateNodeId(_))));
        assert!(
            !errors
                .iter()
                .any(|e| matches!(e, ValidationError::DependencyCycle(_))),
            "cycle check must be skipped when duplicate ids are present, got: {errors:?}"
        );
    }
}
