//! Spec-06 §5 Task 4.5: guard against re-introducing the deleted `WorkflowRunner`
//! subsystem (item 10) — the trait, the `RunWorkflowTool`, and the
//! `ToolCtx::with_workflow_runner` DI hook must never come back. A brain
//! reaching into the workflow layer via a tool is the inversion spec-04/item-9
//! forbids; workflows compose at the DAG level, not via a brain tool.
use std::fs;
use std::path::{Path, PathBuf};

fn scan(dir: &Path, needles: &[&str], out: &mut String) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            scan(&p, needles, out);
            continue;
        }
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        // Skip this guard's own source — its needles are built via concat! so
        // they never appear as a contiguous substring here anyway, but the
        // exclusion is cheap insurance against a future rewrite of the guard.
        if p.file_name().and_then(|n| n.to_str()) == Some("workflow_runner_removed.rs") {
            continue;
        }
        for (i, line) in fs::read_to_string(&p).unwrap().lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for n in needles {
                if code.contains(n) {
                    out.push_str(&format!("{}:{}: {}\n", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
}

#[test]
fn workflow_runner_subsystem_stays_deleted() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Built via concat! so these needles don't self-match this file (which
    // legitimately mentions them in doc comments/strings elsewhere).
    let trait_needle = concat!("trait ", "WorkflowRunner");
    let tool_needle = concat!("Run", "WorkflowTool");
    let with_runner_needle = concat!("with_", "workflow_runner");
    let module_needle = concat!("workflow_", "runner::");
    let needles = [trait_needle, tool_needle, with_runner_needle, module_needle];

    let mut leaks = String::new();
    for c in [
        "src",
        "crates/gait-cli/src",
        "crates/gait-repl/src",
        "crates/gait-workflow/src",
    ] {
        scan(&root.join(c), &needles, &mut leaks);
    }
    assert!(
        leaks.trim().is_empty(),
        "WorkflowRunner subsystem must stay deleted:\n{leaks}"
    );
}
