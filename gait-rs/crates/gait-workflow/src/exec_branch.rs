use crate::vars::{eval_bool_expr, NodeOutput};
use gait_core::proto::gait::workflow::v1::BranchNode;
use std::collections::HashMap;

pub fn resolve_branch(cfg: &BranchNode, _outputs: &HashMap<String, NodeOutput>) -> Option<String> {
    let expr = cfg.on.trim();
    if !cfg.cases.is_empty() {
        if let Some(target) = cfg.cases.get(expr) {
            return Some(target.clone());
        }
        return cfg.r#else.clone();
    }
    if eval_bool_expr(expr) {
        cfg.then.clone()
    } else {
        cfg.r#else.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_way_branch_then_path() {
        let cfg = BranchNode {
            on: "0 == 0".into(),
            then: Some("ship".into()),
            r#else: Some("fix".into()),
            cases: Default::default(),
        };
        let outputs = std::collections::HashMap::new();
        let result = resolve_branch(&cfg, &outputs);
        assert_eq!(result, Some("ship".to_string()));
    }

    #[test]
    fn two_way_branch_else_path() {
        let cfg = BranchNode {
            on: "1 == 0".into(),
            then: Some("ship".into()),
            r#else: Some("fix".into()),
            cases: Default::default(),
        };
        let outputs = std::collections::HashMap::new();
        let result = resolve_branch(&cfg, &outputs);
        assert_eq!(result, Some("fix".to_string()));
    }

    #[test]
    fn multiway_branch_cases() {
        let mut cases = std::collections::HashMap::new();
        cases.insert("0".into(), "ship".into());
        cases.insert("1".into(), "fix".into());
        let cfg = BranchNode {
            on: "0".into(),
            then: None,
            r#else: Some("fallback".into()),
            cases,
        };
        let outputs = std::collections::HashMap::new();
        let result = resolve_branch(&cfg, &outputs);
        assert_eq!(result, Some("ship".to_string()));
    }
}
