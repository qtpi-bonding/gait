//! Append-only pbjson event log for a single workflow run.
//!
//! File layout: one [`WorkflowEvent`] per line in
//! `{workflow_runs_dir}/{run_id}/log.jsonl` (JSON via pbjson, camelCase).
//! Single-writer, append-only. State for a run is reconstructed by replaying
//! events.
//!
//! Each line is an enveloped [`WorkflowEvent`] (`id`/`seq`/`parents`/
//! `content_hash`) minted via `workflow_envelope::mint`; hashes are recorded,
//! not verified on read (matches the session log).
//!
//! Logs whose format `version` is below [`LEGACY_BELOW_VERSION`] predate the
//! current format: they are refused with [`LogError::Legacy`] by every entry
//! point ([`WorkflowRunLog::open`], [`WorkflowRunLog::read_events`], and
//! therefore `emit`) and are never parsed, self-healed, or rewritten on disk.
//!
//! Appends are serialized by an advisory lock on a dedicated sibling lock
//! file (`log.jsonl.lock`), so separate handles — and separate processes —
//! can never mint the same `seq` or interleave torn writes.

use std::io::Write;
use std::path::{Path, PathBuf};

use gait_core::proto::gait::workflow::v1::{
    NodeComplete, NodeError, NodeErrorKind, NodeSkip, NodeStart, Workflow, WorkflowComplete,
    WorkflowError, WorkflowEvent, WorkflowGate, WorkflowStart,
};

/// Current format generation for the workflow run log. Mirrors the contract
/// used by `gait.log.v1` (bump on breaking schema changes).
pub const WORKFLOW_LOG_VERSION: u32 = 1;

/// The first log format generation that is still readable. Logs with a
/// `version` below this value predate the current format and are refused.
pub const LEGACY_BELOW_VERSION: u32 = 1;

/// Typed errors for the workflow run log. Returned to callers wrapped in
/// `anyhow::Error` — use `err.downcast_ref::<LogError>()` to inspect them.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The log was written by a format older than [`LEGACY_BELOW_VERSION`].
    /// Such a log cannot be read, resumed, or aborted; it is left untouched.
    #[error(
        "legacy workflow log (format version {version}): this log predates the \
         current format and cannot be read, resumed, or aborted"
    )]
    Legacy { version: u32 },
}

/// A handle to an open workflow-run log file. Use [`WorkflowRunLog::create`]
/// to start a new run, or [`WorkflowRunLog::open`] to attach to an existing
/// run (e.g. on resume after a gate pause).
#[derive(Debug)]
pub struct WorkflowRunLog {
    path: PathBuf,
}

impl WorkflowRunLog {
    /// Create a new workflow-run log at `{workflow_runs_dir}/{run_id}/log.jsonl`.
    /// The run directory is created if it does not exist; the log file itself
    /// is created exclusively (`O_CREAT | O_EXCL`) — if it already exists,
    /// this returns an error rather than truncating a live run.
    pub fn create(workflow_runs_dir: &Path, run_id: &str) -> anyhow::Result<Self> {
        let run_dir = workflow_runs_dir.join(run_id);
        std::fs::create_dir_all(&run_dir)?;
        let path = run_dir.join("log.jsonl");
        // O_EXCL: fail loudly rather than truncating a live run on ID collision.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "workflow run log already exists at {}: {}",
                    path.display(),
                    e
                )
            })?;
        Ok(Self { path })
    }

    /// Open an existing workflow-run log at `{workflow_runs_dir}/{run_id}/log.jsonl`
    /// for append-only writes. The file must already exist; use [`create`] to
    /// start a new run.
    ///
    /// Refuses a legacy (pre-[`LEGACY_BELOW_VERSION`]) log with
    /// [`LogError::Legacy`] and does not touch the file.
    pub fn open(workflow_runs_dir: &Path, run_id: &str) -> anyhow::Result<Self> {
        let path = workflow_runs_dir.join(run_id).join("log.jsonl");
        if !path.exists() {
            anyhow::bail!("workflow run log not found: {}", path.display());
        }
        let text = std::fs::read_to_string(&path)?;
        if let Some(version) = legacy_version(&text) {
            return Err(LogError::Legacy { version }.into());
        }
        Ok(Self { path })
    }

    /// Path to the underlying log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read all events from the log. Blank lines are skipped.
    ///
    /// Refuses a legacy (pre-[`LEGACY_BELOW_VERSION`]) log with
    /// [`LogError::Legacy`] before any parsing happens: the raw bytes are
    /// checked for a legacy format `version` first, so a legacy log is never
    /// parsed, never self-healed, and left byte-identical on disk.
    ///
    /// Tail-tolerant: because writes are append-only, a crash mid-`writeln!`
    /// can only ever tear the LAST line. If the last non-blank line fails to
    /// parse, it's treated as a torn write — logged, dropped, and the file is
    /// rewritten on disk to just the good events, so the torn tail doesn't
    /// persist and corrupt the next read/write cycle (`emit` calls
    /// `read_events` on every append to find its parent event). A parse
    /// failure on any EARLIER line is real corruption, not a crash artifact,
    /// and still hard-errors.
    pub fn read_events(&self) -> anyhow::Result<Vec<WorkflowEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path)?;
        if let Some(version) = legacy_version(&text) {
            return Err(LogError::Legacy { version }.into());
        }
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let mut events = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            match serde_json::from_str::<WorkflowEvent>(line) {
                Ok(ev) => events.push(ev),
                Err(e) if i == lines.len() - 1 => {
                    eprintln!(
                        "[gait workflow] {}: dropping unparseable trailing line (torn write from a mid-write crash): {}",
                        self.path.display(),
                        e
                    );
                    self.rewrite_clean(&events)?;
                    break;
                }
                Err(e) => {
                    anyhow::bail!(
                        "corrupt workflow run log at {} (line {} of {}): {}",
                        self.path.display(),
                        i + 1,
                        lines.len(),
                        e
                    );
                }
            }
        }
        Ok(events)
    }

    /// Path to the advisory lock file guarding append-serialization, sibling
    /// to the log: `{...}/log.jsonl.lock`. It is never read as a run (only
    /// `log.jsonl` is).
    fn lock_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".lock");
        PathBuf::from(p)
    }

    /// Rewrite the log file to contain exactly `events`, one JSON line each.
    /// Used by `read_events` to truncate a torn trailing write off disk once
    /// it's been dropped from the in-memory result.
    ///
    /// Crash-atomic: writes to a sibling temp file in the same directory,
    /// then renames it over the real path. Rename is atomic on the same
    /// filesystem, so a crash mid-rewrite leaves either the old (torn-tail)
    /// file or the fully-written new (clean) file — never a half-written
    /// one. Using `std::fs::write` directly on `self.path` would `O_TRUNC`
    /// the file first, and a crash between truncation and write completing
    /// would lose the ENTIRE log, not just the already-dropped torn tail.
    fn rewrite_clean(&self, events: &[WorkflowEvent]) -> anyhow::Result<()> {
        let mut buf = String::new();
        for ev in events {
            buf.push_str(&serde_json::to_string(ev)?);
            buf.push('\n');
        }
        let tmp_path = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp_path, buf)?;
        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    // -- writers ------------------------------------------------------------

    /// Append a workflow start event. The `run_id` should be the same id used
    /// to create the log directory.
    pub fn workflow_start(
        &self,
        run_id: &str,
        workflow: &str,
        yaml_path: &str,
        args: &[String],
        def: &Workflow,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::WorkflowStart(WorkflowStart {
            run_id: run_id.to_string(),
            workflow: workflow.to_string(),
            yaml_path: yaml_path.to_string(),
            args: args.to_vec(),
            def: Some(def.clone()),
            ..Default::default()
        }))
    }

    /// Append a workflow start event for a script (JS) workflow. The workflow
    /// has no YAML definition and no path: the script's path, sha256, and full
    /// source text are recorded instead.
    pub fn script_workflow_start(
        &self,
        run_id: &str,
        workflow: &str,
        script_path: &str,
        script_sha256: &str,
        script_source: &str,
        args: &[String],
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::WorkflowStart(WorkflowStart {
            run_id: run_id.to_string(),
            workflow: workflow.to_string(),
            yaml_path: String::new(),
            args: args.to_vec(),
            def: None,
            script_path: script_path.to_string(),
            script_sha256: script_sha256.to_string(),
            script_source: script_source.to_string(),
        }))
    }

    /// Append a node start event.
    pub fn node_start(
        &self,
        node_id: &str,
        node_type: &str,
        attempt: u32,
        session_id: &str,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::NodeStart(NodeStart {
            node_id: node_id.to_string(),
            node_type: node_type.to_string(),
            attempt,
            session_id: session_id.to_string(),
        }))
    }

    /// Append a node complete event.
    pub fn node_complete(
        &self,
        node_id: &str,
        session_id: &str,
        output: &str,
        exit_code: Option<i32>,
        json_output: &str,
        routed_to: &str,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::NodeComplete(NodeComplete {
            node_id: node_id.to_string(),
            session_id: session_id.to_string(),
            output: output.to_string(),
            exit_code,
            json_output: json_output.to_string(),
            routed_to: routed_to.to_string(),
        }))
    }

    /// Append a node error event.
    pub fn node_error(
        &self,
        node_id: &str,
        error: &str,
        attempt: u32,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::NodeError(NodeError {
            node_id: node_id.to_string(),
            error: error.to_string(),
            attempt,
            ..Default::default()
        }))
    }

    /// Append a node error event with kind CANCELLED (an in-flight step that
    /// was cancelled, e.g. by a sibling failure or an interrupted run).
    pub fn node_cancelled(
        &self,
        node_id: &str,
        reason: &str,
        attempt: u32,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::NodeError(NodeError {
            node_id: node_id.to_string(),
            error: reason.to_string(),
            attempt,
            kind: NodeErrorKind::Cancelled as i32,
        }))
    }

    /// Append a node skip event (e.g. inactive branch path).
    pub fn node_skip(&self, node_id: &str, reason: &str) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::NodeSkip(NodeSkip {
            node_id: node_id.to_string(),
            reason: reason.to_string(),
        }))
    }

    /// Append a workflow gate pause event.
    pub fn workflow_gate(&self, node_id: &str, message: &str) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::WorkflowGate(WorkflowGate {
            node_id: node_id.to_string(),
            message: message.to_string(),
        }))
    }

    /// Append a workflow complete event.
    pub fn workflow_complete(&self, run_id: &str) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::WorkflowComplete(WorkflowComplete {
            run_id: run_id.to_string(),
        }))
    }

    /// Append a workflow error event (terminal failure).
    pub fn workflow_error(
        &self,
        run_id: &str,
        failed_node: &str,
        error: &str,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        self.emit(Kind::WorkflowError(WorkflowError {
            run_id: run_id.to_string(),
            failed_node: failed_node.to_string(),
            error: error.to_string(),
        }))
    }

    /// Append a hook-run event (before/after node hook execution).
    pub fn hook_run(
        &self,
        node_id: &str,
        hook_type: &str,
        command: &str,
        exit_code: i32,
    ) -> anyhow::Result<WorkflowEvent> {
        use gait_core::proto::gait::workflow::v1::{workflow_event::Kind, WorkflowHookRun};
        self.emit(Kind::HookRun(WorkflowHookRun {
            node_id: node_id.to_string(),
            hook_type: hook_type.to_string(),
            command: command.to_string(),
            exit_code,
        }))
    }

    /// Mint an enveloped WorkflowEvent for `kind` (chained after the current
    /// tail) and append it as one JSON line. Hashes are RECORDED, not verified
    /// on read — mirrors the session log.
    ///
    /// The whole read-tail → mint → append sequence is serialized by an
    /// advisory exclusive lock on a dedicated sibling lock file
    /// (`log.jsonl.lock`, via stable `File::lock`), so separate handles and
    /// separate processes can never read the same tail and mint the same
    /// `seq`, nor interleave torn writes. The lock is held across the
    /// `read_events()` call — including if it errors (e.g. a legacy log) —
    /// and released when the lock file handle drops.
    fn emit(
        &self,
        kind: gait_core::proto::gait::workflow::v1::workflow_event::Kind,
    ) -> anyhow::Result<WorkflowEvent> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())?;
        lock.lock()?;

        let result = self.emit_locked(kind);

        drop(lock);
        result
    }

    /// The read-tail → mint → append body of [`emit`], run while the lock
    /// file is held. Separated out so the lock covers the whole sequence and
    /// is released on every return path.
    fn emit_locked(
        &self,
        kind: gait_core::proto::gait::workflow::v1::workflow_event::Kind,
    ) -> anyhow::Result<WorkflowEvent> {
        let prev = self.read_events()?.pop();
        let ev =
            gait_core::proto::workflow_envelope::mint(WORKFLOW_LOG_VERSION, kind, prev.as_ref());
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&ev)?)?;
        Ok(ev)
    }
}

/// Detect a legacy workflow log by reading the RAW JSON of the log's lines
/// (before any pbjson parse, so unknown keys in a legacy log don't matter).
///
/// For every non-blank line that parses as a JSON object, take its `version`
/// field (a missing key means 0 — pbjson omits zero values). A line whose
/// JSON does not parse at all (e.g. a torn/garbage tail) is ignored and left
/// to the normal path. Returns `Some(version)` for the first line whose
/// version is below [`LEGACY_BELOW_VERSION`], or `None` if no legacy line is
/// found. The version field is present in every line, so in practice the
/// first line decides; checking every parseable line is a safety net.
fn legacy_version(text: &str) -> Option<u32> {
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // Torn/garbage line: not a version signal; the normal path will
            // handle it (as a torn tail, or as real corruption).
            continue;
        };
        let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if version < LEGACY_BELOW_VERSION {
            return Some(version);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn script_workflow_start_writes_and_reads_back_script_fields() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-script").unwrap();
        let args = vec!["--flag".to_string(), "x".to_string()];
        log.script_workflow_start(
            "run-script",
            "my-script-wf",
            "scripts/wf.js",
            "deadbeef",
            "export default async () => {}",
            &args,
        )
        .unwrap();

        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        let events = log.read_events().unwrap();
        assert_eq!(events.len(), 1);
        let Kind::WorkflowStart(start) = &events[0].kind.as_ref().unwrap() else {
            panic!("expected a WorkflowStart event");
        };
        assert_eq!(start.run_id, "run-script");
        assert_eq!(start.workflow, "my-script-wf");
        assert_eq!(start.yaml_path, "", "a script workflow has no yaml path");
        assert!(start.def.is_none(), "a script workflow has no yaml def");
        assert_eq!(start.script_path, "scripts/wf.js");
        assert_eq!(start.script_sha256, "deadbeef");
        assert_eq!(start.script_source, "export default async () => {}");
        assert_eq!(start.args, args);
    }

    #[test]
    fn log_roundtrips_workflow_start() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "test-run-20260616-120000").unwrap();
        let def = gait_core::proto::gait::workflow::v1::Workflow::default();
        log.workflow_start(
            "test-run-20260616-120000",
            "test-workflow",
            "workflows/test.yaml",
            &[],
            &def,
        )
        .unwrap(); // return value (WorkflowEvent) is intentionally discarded in tests
        let events = log.read_events().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].kind.is_some());
    }

    #[test]
    fn workflow_event_has_envelope_fields() {
        let ev = gait_core::proto::gait::workflow::v1::WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            id: "abc".into(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: None,
        };
        assert_eq!(ev.seq, 1);
        assert_eq!(ev.id, "abc");
    }

    #[test]
    fn appended_events_carry_monotonic_envelope() {
        use gait_core::proto::hash::content_hash;
        use gait_core::proto::workflow_envelope::body_json;
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-x").unwrap();
        log.node_start("a", "shell", 1, "sa").unwrap();
        log.node_complete("a", "sa", "ok", Some(0), "{}", "")
            .unwrap();
        let evs = log.read_events().unwrap();
        assert_eq!(evs[0].seq, 1);
        assert!(evs[0].parents.is_empty());
        assert_eq!(evs[1].seq, 2);
        assert_eq!(evs[1].parents, vec![evs[0].id.clone()]);
        assert!(!evs[0].id.is_empty() && evs[0].id != evs[1].id);
        for e in &evs {
            assert_eq!(e.content_hash, content_hash(e.version, &body_json(e)));
        }
    }

    #[test]
    fn hook_run_writer_records_typed_event() {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-h").unwrap();
        log.hook_run("n", "before", "echo hi", 0).unwrap();
        let evs = log.read_events().unwrap();
        assert!(matches!(evs[0].kind, Some(Kind::HookRun(_))));
    }

    #[test]
    fn hook_run_is_a_typed_kind() {
        use gait_core::proto::gait::workflow::v1::{workflow_event::Kind, WorkflowHookRun};
        let k = Kind::HookRun(WorkflowHookRun {
            node_id: "n".into(),
            hook_type: "before".into(),
            command: "echo hi".into(),
            exit_code: 0,
        });
        assert!(matches!(k, Kind::HookRun(_)));
    }

    #[test]
    fn node_cancelled_writes_a_cancelled_kind_node_error() {
        use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-cancel").unwrap();
        log.node_start("a", "shell", 1, "").unwrap();
        log.node_cancelled("a", "workflow run interrupted", 1)
            .unwrap();
        let evs = log.read_events().unwrap();
        assert_eq!(evs.len(), 2);
        let Kind::NodeError(ne) = &evs[1].kind.as_ref().unwrap() else {
            panic!("expected a NodeError event");
        };
        assert_eq!(ne.node_id, "a");
        assert_eq!(ne.error, "workflow run interrupted");
        assert_eq!(ne.attempt, 1);
        assert_eq!(ne.kind, NodeErrorKind::Cancelled as i32);
    }

    #[test]
    fn read_events_self_heals_a_torn_trailing_write() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-torn").unwrap();
        log.node_start("a", "shell", 1, "sa").unwrap();
        log.node_complete("a", "sa", "ok", Some(0), "{}", "")
            .unwrap();

        // Simulate a crash mid-writeln!: append a truncated, unparseable line
        // directly to the file (no trailing newline, matches a torn write).
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .unwrap();
            write!(
                f,
                r#"{{"version":1,"id":"broken","seq":3,"kind":{{"nodeStart"#
            )
            .unwrap();
        }

        let events = log.read_events().unwrap();
        assert_eq!(
            events.len(),
            2,
            "the torn trailing line must be dropped, not fail the whole read"
        );

        // The self-heal must persist to disk: a second, independent read must
        // not see the torn line again (proves it was truncated, not just
        // filtered out of this one Vec).
        let log2 = WorkflowRunLog::open(dir.path(), "run-torn").unwrap();
        let events2 = log2.read_events().unwrap();
        assert_eq!(events2.len(), 2);
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            !raw.contains("broken"),
            "the torn line's bytes must be gone from disk after self-heal, got: {raw}"
        );
    }

    #[test]
    fn read_events_self_heals_when_the_torn_line_is_the_only_line() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-only-line-torn").unwrap();

        // No complete events written yet — simulate a crash during the very
        // first writeln!, before any event ever completed.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .unwrap();
            write!(
                f,
                r#"{{"version":1,"id":"broken","seq":1,"kind":{{"workflowStart"#
            )
            .unwrap();
        }

        let events = log.read_events().unwrap();
        assert_eq!(
            events.len(),
            0,
            "a torn-only-line log must self-heal to empty, not error"
        );

        let log2 = WorkflowRunLog::open(dir.path(), "run-only-line-torn").unwrap();
        assert_eq!(log2.read_events().unwrap().len(), 0);
    }

    #[test]
    fn emit_after_self_heal_chains_onto_the_last_good_event_not_the_torn_one() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-resume").unwrap();
        let first = log.node_start("a", "shell", 1, "sa").unwrap();

        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .unwrap();
            write!(
                f,
                r#"{{"version":1,"id":"broken","seq":2,"kind":{{"nodeCom"#
            )
            .unwrap();
        }

        // A normal write after the crash must succeed and chain onto `first`,
        // not the dropped torn line — this is the "resume after crash" case.
        let second = log
            .node_complete("a", "sa", "ok", Some(0), "{}", "")
            .unwrap();
        assert_eq!(second.parents, vec![first.id.clone()]);

        let events = log.read_events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, first.id);
        assert_eq!(events[1].id, second.id);
    }

    #[test]
    fn read_events_hard_errors_on_non_last_line_corruption() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "run-mid-corrupt").unwrap();
        log.node_start("a", "shell", 1, "sa").unwrap();
        log.node_complete("a", "sa", "ok", Some(0), "{}", "")
            .unwrap();

        // Garbage in the MIDDLE (a valid-looking line follows it), not the tail.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(log.path())
                .unwrap();
            writeln!(f, "not json at all").unwrap();
            writeln!(
                f,
                r#"{{"version":1,"id":"c","seq":4,"ts":0,"contentHash":"","parents":[],"kind":null}}"#
            )
            .unwrap();
        }

        let result = log.read_events();
        assert!(
            result.is_err(),
            "corruption that is NOT the last line must hard-error, not self-heal"
        );
    }
}
