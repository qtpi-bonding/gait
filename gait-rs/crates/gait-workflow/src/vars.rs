use regex::Regex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

static RE_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_-]*)\.output\.([a-zA-Z_][a-zA-Z0-9_.]*)")
        .expect("valid regex literal")
});

static RE_OUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_-]*)\.output").expect("valid regex literal")
});

static RE_EXIT_NODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_-]*)\.exit").expect("valid regex literal")
});

/// Sentinel value used by `exec_prompt_node` to signal that a prompt node with
/// no prompt text has no input available and must be left open (resumed later).
pub const NODE_OPEN_SENTINEL: &str = "__node_open__";

#[derive(Debug, Clone)]
pub struct NodeOutput {
    pub text: String,
    pub exit_code: Option<i32>,
    pub json: Option<serde_json::Value>,
    pub routed_to: Option<String>,
    /// The effective turn session id for this node ("" for shell/branch/gate).
    pub session_id: String,
}

impl NodeOutput {
    /// Construct the "node is open / waiting for input" sentinel output.
    /// The runner maps `NODE_OPEN_SENTINEL` → `NodeTaskOutcome::Open`.
    pub fn open() -> Self {
        NodeOutput {
            text: String::new(),
            exit_code: None,
            json: None,
            routed_to: Some(NODE_OPEN_SENTINEL.to_string()),
            session_id: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VarContext {
    pub workflow_id: String,
    pub session_id: String,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub args: String,
    pub node_id: String,
    pub node_outputs: HashMap<String, NodeOutput>,
    pub node_exit: Option<i32>,
    pub failed_node: Option<String>,
}

pub fn substitute(template: &str, ctx: &VarContext) -> String {
    substitute_impl(template, ctx, false)
}

/// Same substitution as `substitute`, except every value drawn from a prior
/// node's output/exit code/JSON field is POSIX single-quote wrapped before
/// being spliced in, so it lands in `sh -c` as one opaque shell word — never
/// as parseable shell syntax. Well-known vars ($WORKFLOW_ID, $SESSION_ID,
/// $WORKSPACE, $CODEBASE, $ARGS, $NODE_ID, $NODE_EXIT, $FAILED_NODE) are
/// always gait-controlled and never carry untrusted content, so they are
/// substituted raw, exactly like `substitute`. Used only by `exec_shell_node`
/// — every other substitution call site (prompt text, gate messages, branch
/// conditions) keeps using `substitute` or `substitute_checked`.
pub fn substitute_shell(template: &str, ctx: &VarContext) -> String {
    substitute_impl(template, ctx, true)
}

fn substitute_impl(template: &str, ctx: &VarContext, quote: bool) -> String {
    let wrap = |s: String| if quote { shell_quote(&s) } else { s };
    let mut result = template.to_string();

    // $node_id.output.field (most specific — must be first)
    result = RE_FIELD
        .replace_all(&result, |caps: &regex::Captures| {
            let node_id = &caps[1];
            let field_path = &caps[2];
            if let Some(out) = ctx.node_outputs.get(node_id) {
                if let Some(json) = &out.json {
                    let value: Option<&serde_json::Value> = field_path.split('.').try_fold(
                        json as &serde_json::Value,
                        |acc: &serde_json::Value, k: &str| acc.get(k),
                    );
                    return wrap(
                        value
                            .map(|v| match v {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default(),
                    );
                }
            }
            caps[0].to_string()
        })
        .to_string();

    // $node_id.output
    result = RE_OUT
        .replace_all(&result, |caps: &regex::Captures| {
            wrap(
                ctx.node_outputs
                    .get(&caps[1])
                    .map(|o| o.text.clone())
                    .unwrap_or_default(),
            )
        })
        .to_string();

    // $node_id.exit
    result = RE_EXIT_NODE
        .replace_all(&result, |caps: &regex::Captures| {
            wrap(
                ctx.node_outputs
                    .get(&caps[1])
                    .and_then(|o| o.exit_code.map(|e| e.to_string()))
                    .unwrap_or_default(),
            )
        })
        .to_string();

    // Well-known vars — longest names first to avoid partial replacement.
    // Always gait-controlled, never untrusted — substituted raw regardless of `quote`.
    let replacements: &[(&str, String)] = &[
        ("$WORKFLOW_ID", ctx.workflow_id.clone()),
        ("$SESSION_ID", ctx.session_id.clone()),
        ("$NODE_ID", ctx.node_id.clone()),
        ("$WORKSPACE", ctx.gait_dir.to_string_lossy().to_string()),
        ("$CODEBASE", ctx.codebase.to_string_lossy().to_string()),
        ("$ARGS", ctx.args.clone()),
        (
            "$NODE_EXIT",
            ctx.node_exit.map(|e| e.to_string()).unwrap_or_default(),
        ),
        ("$FAILED_NODE", ctx.failed_node.clone().unwrap_or_default()),
    ];
    for (key, val) in replacements {
        result = result.replace(key, val);
    }
    result
}

/// POSIX single-quote-wrap: the standard, fully-correct way to make an
/// arbitrary byte string one opaque shell word (same algorithm as Python's
/// `shlex.quote` / git's `sq_quote`). Close the quote, insert a literal
/// escaped quote, reopen — `it's` becomes `'it'\'s'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn eval_bool_expr(expr: &str) -> bool {
    let expr = expr.trim();
    // Check operators longest-first to avoid partial matches
    for op in &["==", "!=", "<=", ">=", "<", ">"] {
        if let Some(pos) = expr.find(op) {
            // Skip if the char before is part of another op (!, <, >)
            let before_char = if pos > 0 {
                expr.as_bytes().get(pos - 1).copied()
            } else {
                None
            };
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
            "<" => l < r,
            ">" => l > r,
            "<=" => l <= r,
            ">=" => l >= r,
            _ => false,
        }
    } else {
        match op {
            "==" => lhs == rhs,
            "!=" => lhs != rhs,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ctx() -> VarContext {
        let mut outputs = HashMap::new();
        outputs.insert(
            "test".into(),
            NodeOutput {
                text: "PASS\nall good".into(),
                exit_code: Some(0),
                json: Some(serde_json::json!({ "pr_url": "https://github.com/pr/1" })),
                routed_to: None,
                session_id: "s1".into(),
            },
        );
        VarContext {
            workflow_id: "wf1".into(),
            session_id: "s1".into(),
            gait_dir: std::path::PathBuf::from("/ws"),
            codebase: std::path::PathBuf::from("/cb"),
            args: "arg1".into(),
            node_id: "n1".into(),
            node_outputs: outputs,
            node_exit: Some(0),
            failed_node: None,
        }
    }

    #[test]
    fn substitute_well_known_vars() {
        let c = ctx();
        assert_eq!(
            substitute("wf=$WORKFLOW_ID sid=$SESSION_ID", &c),
            "wf=wf1 sid=s1"
        );
        assert_eq!(substitute("ws=$WORKSPACE", &c), "ws=/ws");
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
        assert_eq!(
            substitute("url=$test.output.pr_url", &c),
            "url=https://github.com/pr/1"
        );
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

    #[test]
    fn substitute_shell_quotes_untrusted_node_output() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "mal".into(),
            NodeOutput {
                text: "$(rm -rf /)".into(),
                exit_code: Some(0),
                json: None,
                routed_to: None,
                session_id: "s1".into(),
            },
        );
        let c = VarContext {
            workflow_id: "wf1".into(),
            session_id: "s1".into(),
            gait_dir: std::path::PathBuf::from("/ws"),
            codebase: std::path::PathBuf::from("/cb"),
            args: "".into(),
            node_id: "n1".into(),
            node_outputs: outputs,
            node_exit: None,
            failed_node: None,
        };
        assert_eq!(
            substitute_shell("echo $mal.output", &c),
            "echo '$(rm -rf /)'"
        );
    }

    #[test]
    fn substitute_shell_escapes_embedded_single_quote() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "mal".into(),
            NodeOutput {
                text: "it's done".into(),
                exit_code: Some(0),
                json: None,
                routed_to: None,
                session_id: "s1".into(),
            },
        );
        let c = VarContext {
            workflow_id: "wf1".into(),
            session_id: "s1".into(),
            gait_dir: std::path::PathBuf::from("/ws"),
            codebase: std::path::PathBuf::from("/cb"),
            args: "".into(),
            node_id: "n1".into(),
            node_outputs: outputs,
            node_exit: None,
            failed_node: None,
        };
        assert_eq!(
            substitute_shell("echo $mal.output", &c),
            r"echo 'it'\''s done'"
        );
    }

    #[test]
    fn substitute_shell_leaves_well_known_vars_unquoted() {
        let c = ctx();
        assert_eq!(
            substitute_shell("wf=$WORKFLOW_ID sid=$SESSION_ID", &c),
            "wf=wf1 sid=s1"
        );
    }
}
