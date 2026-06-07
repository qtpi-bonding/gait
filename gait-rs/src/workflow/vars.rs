use std::collections::HashMap;
use std::path::PathBuf;
use regex::Regex;

#[derive(Debug, Clone)]
pub struct NodeOutput {
    pub text: String,
    pub exit_code: Option<i32>,
    pub json: Option<serde_json::Value>,
    pub routed_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VarContext {
    pub workflow_id: String,
    pub session_id: String,
    pub workspace: PathBuf,
    pub codebase: PathBuf,
    pub args: String,
    pub node_id: String,
    pub node_outputs: HashMap<String, NodeOutput>,
    pub loop_iter: Option<u32>,
    pub loop_max: Option<u32>,
    pub loop_output: Option<String>,
    pub loop_signal: Option<String>,
    pub node_exit: Option<i32>,
    pub failed_node: Option<String>,
}

pub fn substitute(template: &str, ctx: &VarContext) -> String {
    let mut result = template.to_string();

    // $node_id.output.field (most specific — must be first)
    let re_field = Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_-]*)\.output\.([a-zA-Z_][a-zA-Z0-9_.]*)").unwrap();
    result = re_field.replace_all(&result, |caps: &regex::Captures| {
        let node_id = &caps[1];
        let field_path = &caps[2];
        if let Some(out) = ctx.node_outputs.get(node_id) {
            if let Some(json) = &out.json {
                let value = field_path.split('.').fold(Some(json), |acc, k| acc.and_then(|v| v.get(k)));
                return value.map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                }).unwrap_or_default();
            }
        }
        caps[0].to_string()
    }).to_string();

    // $node_id.output
    let re_out = Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_-]*)\.output").unwrap();
    result = re_out.replace_all(&result, |caps: &regex::Captures| {
        ctx.node_outputs.get(&caps[1]).map(|o| o.text.clone()).unwrap_or_default()
    }).to_string();

    // $node_id.exit
    let re_exit_node = Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_-]*)\.exit").unwrap();
    result = re_exit_node.replace_all(&result, |caps: &regex::Captures| {
        ctx.node_outputs.get(&caps[1])
            .and_then(|o| o.exit_code.map(|e| e.to_string()))
            .unwrap_or_default()
    }).to_string();

    // Well-known vars — longest names first to avoid partial replacement
    let replacements: &[(&str, String)] = &[
        ("$WORKFLOW_ID",  ctx.workflow_id.clone()),
        ("$SESSION_ID",   ctx.session_id.clone()),
        ("$NODE_ID",      ctx.node_id.clone()),
        ("$WORKSPACE",    ctx.workspace.to_string_lossy().to_string()),
        ("$CODEBASE",     ctx.codebase.to_string_lossy().to_string()),
        ("$ARGS",         ctx.args.clone()),
        ("$NODE_EXIT",    ctx.node_exit.map(|e| e.to_string()).unwrap_or_default()),
        ("$LOOP_OUTPUT",  ctx.loop_output.clone().unwrap_or_default()),
        ("$LOOP_SIGNAL",  ctx.loop_signal.clone().unwrap_or_default()),
        ("$LOOP_ITER",    ctx.loop_iter.map(|i| i.to_string()).unwrap_or_default()),
        ("$LOOP_MAX",     ctx.loop_max.map(|m| m.to_string()).unwrap_or_default()),
        ("$FAILED_NODE",  ctx.failed_node.clone().unwrap_or_default()),
    ];
    for (key, val) in replacements {
        result = result.replace(key, val);
    }
    result
}

pub fn eval_bool_expr(expr: &str) -> bool {
    let expr = expr.trim();
    // Check operators longest-first to avoid partial matches
    for op in &["==", "!=", "<=", ">=", "<", ">"] {
        if let Some(pos) = expr.find(op) {
            // Skip if the char before is part of another op (!, <, >)
            let before_char = if pos > 0 { expr.as_bytes().get(pos - 1).copied() } else { None };
            if matches!(before_char, Some(b'!' | b'<' | b'>')) {
                continue;
            }
            let lhs = expr[..pos].trim();
            let rhs = expr[pos + op.len()..].trim();
            if !lhs.is_empty() && !rhs.is_empty() {
                return compare_values(lhs, rhs, op);
            }
        }
    }
    !matches!(expr, "" | "0" | "false")
}

fn compare_values(lhs: &str, rhs: &str, op: &str) -> bool {
    if let (Ok(l), Ok(r)) = (lhs.parse::<f64>(), rhs.parse::<f64>()) {
        match op {
            "==" => l == r,
            "!=" => l != r,
            "<"  => l < r,
            ">"  => l > r,
            "<=" => l <= r,
            ">=" => l >= r,
            _    => false,
        }
    } else {
        match op {
            "==" => lhs == rhs,
            "!=" => lhs != rhs,
            _    => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ctx() -> VarContext {
        let mut outputs = HashMap::new();
        outputs.insert("test".into(), NodeOutput {
            text: "PASS\nall good".into(),
            exit_code: Some(0),
            json: Some(serde_json::json!({ "pr_url": "https://github.com/pr/1" })),
            routed_to: None,
        });
        VarContext {
            workflow_id: "wf1".into(),
            session_id: "s1".into(),
            workspace: std::path::PathBuf::from("/ws"),
            codebase: std::path::PathBuf::from("/cb"),
            args: "arg1".into(),
            node_id: "n1".into(),
            node_outputs: outputs,
            loop_iter: Some(3),
            loop_max: Some(10),
            loop_output: Some("iter done".into()),
            loop_signal: Some("DONE".into()),
            node_exit: Some(0),
            failed_node: None,
        }
    }

    #[test]
    fn substitute_well_known_vars() {
        let c = ctx();
        assert_eq!(substitute("wf=$WORKFLOW_ID sid=$SESSION_ID", &c), "wf=wf1 sid=s1");
        assert_eq!(substitute("ws=$WORKSPACE", &c), "ws=/ws");
        assert_eq!(substitute("iter=$LOOP_ITER max=$LOOP_MAX", &c), "iter=3 max=10");
    }

    #[test]
    fn substitute_node_output() {
        let c = ctx();
        assert_eq!(substitute("out=$test.output", &c), "out=PASS\nall good");
        assert_eq!(substitute("exit=$test.exit", &c), "exit=0");
    }

    #[test]
    fn substitute_node_output_json_field() {
        let c = ctx();
        assert_eq!(substitute("url=$test.output.pr_url", &c), "url=https://github.com/pr/1");
    }

    #[test]
    fn eval_bool_expr_numeric() {
        assert!(eval_bool_expr("0 == 0"));
        assert!(!eval_bool_expr("1 == 0"));
        assert!(eval_bool_expr("1 != 0"));
        assert!(eval_bool_expr("5 > 3"));
        assert!(eval_bool_expr("3 <= 3"));
    }

    #[test]
    fn eval_bool_expr_string() {
        assert!(eval_bool_expr("hello == hello"));
        assert!(!eval_bool_expr("a == b"));
    }
}
