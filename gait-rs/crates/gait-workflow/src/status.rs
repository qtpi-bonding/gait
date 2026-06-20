//! Status block summarizing active and recent workflow runs.
//!
//! Used by the agent harness to inject a compact `<workflow_status>` block into
//! the system prompt so a model turn can see what other workflows are doing.
//! The block is derived by replaying the per-run event logs in
//! `{workflow_runs_dir}/{run_id}/log.jsonl`.

use std::path::Path;

use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
use gait_core::proto::gait::workflow::v1::WorkflowEvent;

use crate::run_log::WorkflowRunLog;

/// Per-run status, derived by replaying the event log.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunStatus {
    Running {
        current_node: String,
        completed: usize,
        total_seen: usize,
    },
    Completed,
    Failed {
        node: String,
    },
    GatePaused {
        node: String,
    },
}

/// What to show in the status block: a still-active run, or a finished one.
#[derive(Debug, Clone)]
enum RunEntry {
    Active {
        run_id: String,
        last_ts: u64,
        status: RunStatus,
    },
    Recent {
        run_id: String,
        last_ts: u64,
        status: RunStatus,
    },
}

/// Build a `<workflow_status>...</workflow_status>` block summarizing runs in
/// `workflow_runs_dir`. Returns an empty string if the directory does not
/// exist, is empty, or contains no readable run logs.
///
/// Format:
/// ```text
/// <workflow_status>
/// active:
///   - {run_id}: gate @ {node}
///   - {run_id}: running @ {current_node} ({completed}/{total_seen})
/// recent (newest first):
///   - {run_id}: completed
///   - {run_id}: failed @ {node}
/// </workflow_status>
/// ```
pub fn workflow_status_block(workflow_runs_dir: &Path) -> String {
    let entries = collect_runs(workflow_runs_dir);
    if entries.is_empty() {
        return String::new();
    }

    let (mut actives, mut recents): (Vec<RunEntry>, Vec<RunEntry>) = entries
        .into_iter()
        .partition(|e| matches!(e, RunEntry::Active { .. }));

    // Within "active": gates first (highest priority for human attention),
    // then running, with newest activity first as a tiebreaker.
    actives.sort_by(|a, b| {
        let pa = active_priority(a);
        let pb = active_priority(b);
        pa.cmp(&pb).then(entry_ts(b).cmp(&entry_ts(a)))
    });

    // Within "recent": newest first, capped at 5.
    recents.sort_by_key(|b| std::cmp::Reverse(entry_ts(b)));
    recents.truncate(5);

    let mut out = String::new();
    out.push_str("<workflow_status>\n");

    if !actives.is_empty() {
        out.push_str("active:\n");
        for e in &actives {
            let (run_id, status) = match e {
                RunEntry::Active { run_id, status, .. } => (run_id, status),
                // partition guarantees this arm is unreachable; keep the
                // pattern exhaustiveness-checked anyway.
                RunEntry::Recent { run_id, status, .. } => (run_id, status),
            };
            write_status_line(&mut out, run_id, status);
        }
    }

    if !recents.is_empty() {
        out.push_str("recent (newest first):\n");
        for e in &recents {
            let (run_id, status) = match e {
                RunEntry::Active { run_id, status, .. } => (run_id, status),
                RunEntry::Recent { run_id, status, .. } => (run_id, status),
            };
            write_status_line(&mut out, run_id, status);
        }
    }

    out.push_str("</workflow_status>");
    out
}

fn write_status_line(out: &mut String, run_id: &str, status: &RunStatus) {
    match status {
        RunStatus::GatePaused { node } => {
            out.push_str(&format!("  - {}: gate @ {}\n", run_id, node));
        }
        RunStatus::Running {
            current_node,
            completed,
            total_seen,
        } => {
            out.push_str(&format!(
                "  - {}: running @ {} ({}/{})\n",
                run_id, current_node, completed, total_seen
            ));
        }
        RunStatus::Completed => {
            out.push_str(&format!("  - {}: completed\n", run_id));
        }
        RunStatus::Failed { node } => {
            out.push_str(&format!("  - {}: failed @ {}\n", run_id, node));
        }
    }
}

fn entry_ts(e: &RunEntry) -> u64 {
    match e {
        RunEntry::Active { last_ts, .. } | RunEntry::Recent { last_ts, .. } => *last_ts,
    }
}

/// Lower = appears earlier in the "active" list. Gates first because they
/// need human input, running second, terminal states shouldn't show up here
/// (they're classified as Recent) but we still order them sanely.
fn active_priority(e: &RunEntry) -> u8 {
    match e {
        RunEntry::Active { status, .. } => match status {
            RunStatus::GatePaused { .. } => 0,
            RunStatus::Running { .. } => 1,
            RunStatus::Completed => 2,
            RunStatus::Failed { .. } => 3,
        },
        RunEntry::Recent { .. } => 99,
    }
}

/// Scan `workflow_runs_dir` and build a `RunEntry` for each readable run log.
fn collect_runs(workflow_runs_dir: &Path) -> Vec<RunEntry> {
    let entries = match std::fs::read_dir(workflow_runs_dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(run_id) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        // Skip if there's no log file — caller would never see this run.
        if !path.join("log.jsonl").exists() {
            continue;
        }
        let log = match WorkflowRunLog::open(workflow_runs_dir, &run_id) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let events = match log.read_events() {
            Ok(ev) => ev,
            Err(_) => continue,
        };
        if events.is_empty() {
            continue;
        }
        let (status, last_ts) = replay(&events);
        let entry = match &status {
            RunStatus::Completed | RunStatus::Failed { .. } => RunEntry::Recent {
                run_id,
                last_ts,
                status,
            },
            RunStatus::Running { .. } | RunStatus::GatePaused { .. } => RunEntry::Active {
                run_id,
                last_ts,
                status,
            },
        };
        out.push(entry);
    }
    out
}

/// Replay a run's event log and derive its current status and last timestamp.
fn replay(events: &[WorkflowEvent]) -> (RunStatus, u64) {
    let mut current_node = String::new();
    let mut completed = 0usize;
    let mut total_seen = 0usize;
    let mut last_ts = 0u64;
    let mut status = RunStatus::Running {
        current_node: String::new(),
        completed: 0,
        total_seen: 0,
    };

    for ev in events {
        if ev.ts > last_ts {
            last_ts = ev.ts;
        }
        match ev.kind.as_ref() {
            Some(Kind::NodeStart(ns)) => {
                total_seen += 1;
                current_node = ns.node_id.clone();
                status = RunStatus::Running {
                    current_node: current_node.clone(),
                    completed,
                    total_seen,
                };
            }
            Some(Kind::NodeComplete(_)) => {
                completed += 1;
                // current_node stays at the just-finished node until the next
                // NodeStart/terminal event moves it.
                status = RunStatus::Running {
                    current_node: current_node.clone(),
                    completed,
                    total_seen,
                };
            }
            Some(Kind::NodeSkip(_)) => {
                total_seen += 1;
            }
            Some(Kind::NodeError(_)) => {
                // Mid-run node error; status only changes on terminal workflow
                // event. Keep running tally consistent.
            }
            Some(Kind::WorkflowStart(_)) => {}
            Some(Kind::WorkflowComplete(_)) => {
                status = RunStatus::Completed;
            }
            Some(Kind::WorkflowError(we)) => {
                status = RunStatus::Failed {
                    node: we.failed_node.clone(),
                };
            }
            Some(Kind::WorkflowGate(wg)) => {
                status = RunStatus::GatePaused {
                    node: wg.node_id.clone(),
                };
            }
            None => {}
        }
    }

    (status, last_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_runs_dir_returns_empty_string() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(workflow_status_block(dir.path()), "");
        // Also: nonexistent path.
        let missing = dir.path().join("does-not-exist");
        assert_eq!(workflow_status_block(&missing), "");
    }

    #[test]
    fn completed_run_shows_in_recent() {
        let dir = tempfile::TempDir::new().unwrap();
        let run_id = "test-run-20260616-120000";
        let log = WorkflowRunLog::create(dir.path(), run_id).unwrap();
        log.workflow_start(run_id, "test-workflow", "workflows/test.yaml", &[])
            .unwrap();
        log.node_start("a", "shell", 1, "session-a").unwrap();
        log.node_complete("a", "session-a", "ok", Some(0), "{}", "")
            .unwrap();
        log.workflow_complete(run_id).unwrap();

        let block = workflow_status_block(dir.path());
        assert!(!block.is_empty(), "expected non-empty status block");
        assert!(
            block.contains(run_id),
            "block should contain run_id: {block}"
        );
        assert!(
            block.contains("completed"),
            "block should mention 'completed': {block}"
        );
    }
}
