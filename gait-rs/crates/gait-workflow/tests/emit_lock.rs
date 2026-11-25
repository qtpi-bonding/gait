//! Concurrent-append safety of `WorkflowRunLog`.
//!
//! `emit` reads the tail, mints the next `seq`, and appends. Without a lock,
//! two handles racing can read the same tail and mint the same `seq`. `emit`
//! therefore holds an advisory file lock around the read-and-append; this
//! test races eight separate handles and asserts strictly monotonic,
//! gap-free `seq` values.

use std::sync::{Barrier, Mutex};

use gait_core::proto::gait::workflow::v1::WorkflowEvent;
use gait_workflow::run_log::WorkflowRunLog;
use tempfile::TempDir;

const THREADS: usize = 8;
const APPENDS_PER_THREAD: usize = 25;
const TOTAL: usize = THREADS * APPENDS_PER_THREAD;

#[test]
fn eight_handles_append_concurrently_with_monotonic_seq() {
    let dir = TempDir::new().unwrap();
    let runs = dir.path().to_path_buf();
    WorkflowRunLog::create(&runs, "r1").unwrap();

    let barrier = std::sync::Arc::new(Barrier::new(THREADS));
    let errors: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let barrier = barrier.clone();
        let errors = errors.clone();
        let runs = runs.clone();
        let handle = std::thread::spawn(move || {
            // Each thread gets ITS OWN handle to the same log file.
            let log = WorkflowRunLog::open(&runs, "r1").expect("open own handle");
            barrier.wait();
            for i in 0..APPENDS_PER_THREAD {
                let node_id = format!("t{t}-n{i}");
                if let Err(e) = log.node_start(&node_id, "shell", 1, "session") {
                    errors.lock().unwrap().push(format!("{node_id}: {e}"));
                }
            }
        });
        handles.push(handle);
    }

    // Propagate panics from the worker threads.
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }

    let errors = errors.lock().unwrap();
    assert!(
        errors.is_empty(),
        "every append must return Ok, got errors: {errors:?}"
    );
    drop(errors);

    // Read the file directly, bypassing the log API.
    let path = runs.join("r1").join("log.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        TOTAL,
        "expected exactly {TOTAL} non-empty lines in the log file"
    );

    let mut seqs = Vec::with_capacity(TOTAL);
    for (i, line) in lines.iter().enumerate() {
        let ev: WorkflowEvent =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} unparseable: {e}"));
        seqs.push(ev.seq);
    }

    // seq must be exactly 1..=200 in file order: strictly monotonic, no gaps,
    // no duplicates (two handles must never mint the same seq).
    let expected: Vec<u64> = (1..=TOTAL as u64).collect();
    assert_eq!(
        seqs, expected,
        "seq values must be exactly 1..={TOTAL} in file order"
    );

    // The last line must end with a newline (complete, not torn).
    assert!(
        text.ends_with('\n'),
        "the last line must end with a newline"
    );

    // A fresh independent handle must read all 200 events.
    let fresh = WorkflowRunLog::open(&runs, "r1").unwrap();
    assert_eq!(
        fresh.read_events().unwrap().len(),
        TOTAL,
        "a fresh handle must see all {TOTAL} events"
    );
}
