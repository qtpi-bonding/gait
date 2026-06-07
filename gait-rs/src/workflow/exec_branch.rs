use std::collections::HashMap;
use crate::workflow::schema::BranchConfig;
use crate::workflow::vars::{NodeOutput, eval_bool_expr};

pub fn resolve_branch(cfg: &BranchConfig, _outputs: &HashMap<String, NodeOutput>) -> Option<String> {
    let expr = cfg.on.trim();
    if !cfg.cases.is_empty() {
        if let Some(target) = cfg.cases.get(expr) {
            return Some(target.clone());
        }
        return cfg.else_.clone();
    }
    if eval_bool_expr(expr) {
        cfg.then.clone()
    } else {
        cfg.else_.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_way_branch_then_path() {
        let cfg = crate::workflow::schema::BranchConfig {
            on: "0 == 0".into(), then: Some("ship".into()),
            else_: Some("fix".into()), cases: Default::default(),
        };
        let outputs = std::collections::HashMap::new();
        let result = resolve_branch(&cfg, &outputs);
        assert_eq!(result, Some("ship".to_string()));
    }

    #[test]
    fn two_way_branch_else_path() {
        let cfg = crate::workflow::schema::BranchConfig {
            on: "1 == 0".into(), then: Some("ship".into()),
            else_: Some("fix".into()), cases: Default::default(),
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
        let cfg = crate::workflow::schema::BranchConfig {
            on: "0".into(), then: None, else_: Some("fallback".into()), cases,
        };
        let outputs = std::collections::HashMap::new();
        let result = resolve_branch(&cfg, &outputs);
        assert_eq!(result, Some("ship".to_string()));
    }
}
