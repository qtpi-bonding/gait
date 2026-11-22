//! Global process registry: `<gait_dir>/processes.jsonl`.
//!
//! The registry is a flat append-only file shared by **concurrent** `gait`
//! processes.  Each line is one JSON-encoded [`Line`] (either a `Start` or a
//! `Stop` marker).  Writes are protected by an exclusive `flock` held only
//! for the duration of the append.
//!
//! `active()` folds the full file: Start inserts by name, Stop removes.
//! `is_alive()` double-checks liveness *and* identity (start-time) to avoid
//! sending signals to a pid-recycled stranger.

use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use fs2::FileExt;

use crate::proto::gait::log::v1 as pb;

// ---------------------------------------------------------------------------
// Internal wire type (what we actually write to the file).
// The variant bodies are pbjson-serialized proto messages (they get serde
// impls from pbjson); the outer enum is internally tagged as `kind` so the
// line on disk looks like `{"kind":"start",...}` / `{"kind":"stop",...}`.
// ---------------------------------------------------------------------------
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Start(pb::ProcessStart),
    Stop(pb::ProcessStop),
}

// ---------------------------------------------------------------------------
// One-time migration of the legacy registry file name.
// ---------------------------------------------------------------------------

/// The registry's file name before the rename to `processes.jsonl`.
pub const LEGACY_REGISTRY_FILE: &str = "daemons.jsonl";
/// The registry's file name.
pub const REGISTRY_FILE: &str = "processes.jsonl";

/// Why the legacy registry could not be migrated.
#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.state.processes")]
pub enum ProcessesError {
    #[error(
        "both {legacy} and {current} exist; refusing to merge them. Keep the one you want as \
         {current} and move the other aside"
    )]
    #[coded(code = "registry_conflict", class = Logical)]
    RegistryConflict { legacy: String, current: String },
    #[error("cannot migrate {legacy} to {current}: {why}")]
    #[coded(code = "registry_migration_failed", class = Infrastructure)]
    MigrationFailed {
        legacy: String,
        current: String,
        why: String,
    },
}

/// First free backup name next to `legacy`: `daemons.jsonl.bak`, then
/// `.bak.1`, `.bak.2`, ... An existing backup is never overwritten.
fn backup_path(legacy: &Path) -> std::path::PathBuf {
    let name = legacy.file_name().unwrap_or_default().to_string_lossy();
    let dir = legacy.parent().unwrap_or_else(|| Path::new("."));
    let first = dir.join(format!("{name}.bak"));
    if !first.exists() {
        return first;
    }
    (1u32..)
        .map(|n| dir.join(format!("{name}.bak.{n}")))
        .find(|p| !p.exists())
        .expect("a free backup name")
}

/// When `<gait_dir>/daemons.jsonl` exists and `processes.jsonl` does not, copy
/// the old file to a `.bak` and rename it to `processes.jsonl`, once. Both
/// present is a [`ProcessesError::RegistryConflict`] (never merged). After the
/// migration nothing reads `daemons.jsonl`. Every line is kept as it was.
pub fn migrate_legacy_registry(gait_dir: &Path) -> Result<(), ProcessesError> {
    let legacy = gait_dir.join(LEGACY_REGISTRY_FILE);
    let current = gait_dir.join(REGISTRY_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    if current.exists() {
        return Err(ProcessesError::RegistryConflict {
            legacy: legacy.display().to_string(),
            current: current.display().to_string(),
        });
    }
    let failed = |why: io::Error| ProcessesError::MigrationFailed {
        legacy: legacy.display().to_string(),
        current: current.display().to_string(),
        why: why.to_string(),
    };
    std::fs::copy(&legacy, backup_path(&legacy)).map_err(failed)?;
    match std::fs::rename(&legacy, &current) {
        Ok(()) => Ok(()),
        // A concurrent process migrated between our check and the rename.
        Err(e) if e.kind() == io::ErrorKind::NotFound && current.exists() => Ok(()),
        Err(e) => Err(failed(e)),
    }
}

fn ensure_migrated(gait_dir: &Path) -> io::Result<()> {
    migrate_legacy_registry(gait_dir).map_err(|e| io::Error::other(e.to_string()))
}

// ---------------------------------------------------------------------------
// Registry API
// ---------------------------------------------------------------------------

/// Append a `ProcessStart` entry to `<gait_dir>/processes.jsonl`.
///
/// Returns `Err(AlreadyExists)` if a live process with the same name is already
/// registered in the gait-dir registry.  Note: this check is advisory (TOCTOU
/// window between the read and the write); for a CLI tool that is acceptable.
pub fn append_start(gait_dir: &Path, rec: &pb::ProcessStart) -> io::Result<()> {
    ensure_migrated(gait_dir)?;
    // Check for duplicate name among currently-active records.
    let existing = active(gait_dir)?;
    if existing.iter().any(|r| r.name == rec.name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "process '{}' is already registered (use a different name or stop it first)",
                rec.name
            ),
        ));
    }
    let path = gait_dir.join(REGISTRY_FILE);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.lock_exclusive()?;
    let line = serde_json::to_string(&Line::Start(rec.clone()))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{}", line)?;
    f.unlock()?;
    Ok(())
}

/// Append a `ProcessStop` entry to `<gait_dir>/processes.jsonl`.
///
/// `exit_code` is the exit status of the process command: 0 for normal exit,
/// 124 for timeout-kill, negative for signal-terminated processes.
pub fn append_stop(gait_dir: &Path, name: &str, exit_code: i32) -> io::Result<()> {
    ensure_migrated(gait_dir)?;
    let path = gait_dir.join(REGISTRY_FILE);
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.lock_exclusive()?;
    let line = serde_json::to_string(&Line::Stop(pb::ProcessStop {
        name: name.to_owned(),
        exit_code,
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{}", line)?;
    f.unlock()?;
    Ok(())
}

/// Read `<gait_dir>/processes.jsonl` and return the last-known record for every
/// process that has a `Start` with no subsequent `Stop`.
pub fn active(gait_dir: &Path) -> io::Result<Vec<pb::ProcessStart>> {
    ensure_migrated(gait_dir)?;
    let path = gait_dir.join(REGISTRY_FILE);
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = OpenOptions::new().read(true).open(&path)?;
    f.lock_shared()?;
    let reader = BufReader::new(&f);
    let mut records: Vec<pb::ProcessStart> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(trimmed) {
            Ok(Line::Start(rec)) => {
                if let Some(existing) = records.iter_mut().find(|r| r.name == rec.name) {
                    *existing = rec;
                } else {
                    records.push(rec);
                }
            }
            Ok(Line::Stop(s)) => {
                records.retain(|r| r.name != s.name);
            }
            Err(_) => {
                // Tolerate malformed lines — skip silently.
            }
        }
    }
    f.unlock()?;
    Ok(records)
}

/// Read `<gait_dir>/processes.jsonl` and return the most recent `Start` for
/// every process name that has ever appeared, paired with the exit code of
/// the `Stop` that followed it (`None` if it hasn't been stopped — either
/// still running, or dead without ever having a `Stop` recorded).
///
/// Unlike `active()`, this never drops a name once it's been stopped — it's
/// the source for a full/history view (`gait processes list --all`) rather
/// than "what's live right now".
pub fn history(gait_dir: &Path) -> io::Result<Vec<(pb::ProcessStart, Option<i32>)>> {
    ensure_migrated(gait_dir)?;
    let path = gait_dir.join(REGISTRY_FILE);
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = OpenOptions::new().read(true).open(&path)?;
    f.lock_shared()?;
    let reader = BufReader::new(&f);
    let mut records: Vec<(pb::ProcessStart, Option<i32>)> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(trimmed) {
            Ok(Line::Start(rec)) => {
                if let Some(existing) = records.iter_mut().find(|r| r.0.name == rec.name) {
                    *existing = (rec, None);
                } else {
                    records.push((rec, None));
                }
            }
            Ok(Line::Stop(s)) => {
                if let Some(existing) = records.iter_mut().find(|r| r.0.name == s.name) {
                    existing.1 = Some(s.exit_code);
                }
            }
            Err(_) => {
                // Tolerate malformed lines — skip silently.
            }
        }
    }
    f.unlock()?;
    Ok(records)
}

/// Classify a process's exit code into a human status.
///
/// A clean exit (code `0`, including the sentinel `0` recorded by a manual
/// `gait processes stop`) is `"done"`. A timeout-kill (`124`, from `run` timeout
/// handling) or a signal-terminated / unclean exit (negative — including the
/// `-9` sentinel appended when `list`/the completion poll merely *observes* a
/// process dead with no `Stop` ever recorded, e.g. SIGKILL, OOM, or the owning
/// session being torn down mid-flight) is `"killed"`.
///
/// Centralized so every caller — the live-turn completion poll
/// (`turn::completion_poll`) and `gait processes list --all` — agrees on one
/// meaning instead of each re-deriving the same 124/negative rule.
pub fn classify_exit(exit_code: i32) -> &'static str {
    if exit_code == 124 || exit_code < 0 {
        "killed"
    } else {
        "done"
    }
}

// ---------------------------------------------------------------------------
// Liveness + identity check
// ---------------------------------------------------------------------------

/// Names + exit codes of processes owned by `session` that have a `Stop`
/// record. `active()` drops stopped entries, so this surfaces them for
/// completion polling. Reads the registry directly with a shared lock; an
/// absent or unreadable file is not an error.
pub fn stopped_for_session(gait_dir: &Path, session: &str) -> Vec<(String, i32)> {
    use std::collections::HashMap;
    if ensure_migrated(gait_dir).is_err() {
        return vec![];
    }
    let path = gait_dir.join(REGISTRY_FILE);
    if !path.exists() {
        return vec![];
    }
    let Ok(f) = OpenOptions::new().read(true).open(&path) else {
        return vec![];
    };
    let _ = f.lock_shared();
    let reader = BufReader::new(&f);
    // name -> owning session, name -> recorded exit_code
    let mut starts: HashMap<String, String> = HashMap::new();
    let mut stops: HashMap<String, i32> = HashMap::new();
    for line in reader.lines().map_while(Result::ok) {
        match serde_json::from_str::<Line>(line.trim()) {
            Ok(Line::Start(s)) => {
                starts.insert(s.name.clone(), s.session.clone());
            }
            Ok(Line::Stop(s)) => {
                stops.insert(s.name.clone(), s.exit_code);
            }
            _ => {}
        }
    }
    let _ = f.unlock();
    stops
        .into_iter()
        .filter(|(name, _)| starts.get(name).map(|s| s == session).unwrap_or(false))
        .collect()
}

/// Returns `true` iff the process described by `rec` is still running **and**
/// has the same start-time (guards against pid reuse).
///
/// Fail-closed: any read failure → `false`.
pub fn is_alive(rec: &pb::ProcessStart) -> bool {
    // 1. Is the pid present in the kernel's process table?
    if unsafe { libc::kill(rec.pid, 0) } != 0 {
        return false;
    }
    // 2. Identity check: start-time must match what we recorded.
    match crate::process::proc_start_time(rec.pid) {
        Some(t) => t == rec.started_at,
        None => false, // fail-closed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_rec(name: &str, pid: i32) -> pb::ProcessStart {
        pb::ProcessStart {
            name: name.to_owned(),
            pid,
            pgid: pid,
            started_at: 12345,
            cmd: "echo hello".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/process.log".to_owned(),
            session: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn round_trip_append_and_active() {
        let dir = TempDir::new().unwrap();
        let rec = make_rec("svc-a", 9999);
        append_start(dir.path(), &rec).unwrap();
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "svc-a");
        assert_eq!(active[0].pid, 9999);
    }

    #[test]
    fn stop_removes_entry() {
        let dir = TempDir::new().unwrap();
        append_start(dir.path(), &make_rec("svc-b", 1111)).unwrap();
        append_start(dir.path(), &make_rec("svc-c", 2222)).unwrap();
        append_stop(dir.path(), "svc-b", 0).unwrap();
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "svc-c");
    }

    /// `append_stop` must persist the exit code on disk so consumers can read
    /// it back from the raw jsonl. (active() drops stopped entries, so we
    /// inspect the file directly.)
    #[test]
    fn stop_records_exit_code() {
        let dir = TempDir::new().unwrap();
        append_start(dir.path(), &make_rec("svc-x", 4242)).unwrap();
        append_stop(dir.path(), "svc-x", 124).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("processes.jsonl")).unwrap();
        assert!(raw.contains("\"kind\":\"stop\""), "got: {raw}");
        // pbjson serializes proto fields as camelCase by default; tolerate either.
        assert!(
            raw.contains("\"exit_code\":124") || raw.contains("\"exitCode\":124"),
            "got: {raw}"
        );
    }

    #[test]
    fn is_alive_true_for_self() {
        let my_pid = std::process::id() as i32;
        let start = crate::process::proc_start_time(my_pid)
            .expect("proc_start_time should work for own pid");
        let rec = pb::ProcessStart {
            name: "self".to_owned(),
            pid: my_pid,
            pgid: my_pid,
            started_at: start,
            cmd: "test".to_owned(),
            cwd: "".to_owned(),
            log_path: "".to_owned(),
            session: String::new(),
            ..Default::default()
        };
        assert!(is_alive(&rec), "current process should be alive");
    }

    #[test]
    fn is_alive_false_for_dead_pid() {
        // Use pid 1 with a deliberately wrong start-time so identity check fails.
        let rec = pb::ProcessStart {
            name: "dead".to_owned(),
            pid: 1,
            pgid: 1,
            started_at: u64::MAX, // will never match
            cmd: "init".to_owned(),
            cwd: "".to_owned(),
            log_path: "".to_owned(),
            session: String::new(),
            ..Default::default()
        };
        assert!(
            !is_alive(&rec),
            "wrong start-time should fail identity check"
        );
    }

    // -----------------------------------------------------------------------
    // Integration tests: live process + per-turn sweep + stop
    // -----------------------------------------------------------------------

    /// Spawn a real `sleep 999` in its own session (setsid), register it, then
    /// verify that `active()` + `is_alive()` reports it alive.
    #[test]
    fn active_sweep_reports_live_process_as_alive() {
        use std::process::Command;

        let dir = TempDir::new().unwrap();

        // Spawn sleep 999 as a child; it gets a new session via pre_exec setsid.
        let mut child = unsafe {
            Command::new("sleep")
                .arg("999")
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn sleep 999")
        };

        let pid = child.id() as i32;
        let pgid = unsafe { libc::getpgid(pid) };
        assert!(pgid > 0, "getpgid should succeed");

        let started_at = crate::process::proc_start_time(pid).expect("proc_start_time for child");

        let rec = pb::ProcessStart {
            name: "test-sleep-alive".to_owned(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/test-sleep.log".to_owned(),
            session: String::new(),
            ..Default::default()
        };

        append_start(dir.path(), &rec).unwrap();

        let recs = active(dir.path()).unwrap();
        assert_eq!(recs.len(), 1, "one active process expected");
        let found = &recs[0];
        assert_eq!(found.name, "test-sleep-alive");
        assert!(is_alive(found), "live sleep process should be alive");

        // Cleanup: kill the child before the test exits.
        let _ = child.kill();
        let _ = child.wait();
    }

    /// After stopping a live process (killpg + append_stop), `active()` must no
    /// longer list it and `is_alive()` must return false.
    #[test]
    fn stop_removes_live_process_and_is_alive_flips_false() {
        use std::process::Command;

        let dir = TempDir::new().unwrap();

        let mut child = unsafe {
            Command::new("sleep")
                .arg("999")
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn sleep 999")
        };

        let pid = child.id() as i32;
        let pgid = unsafe { libc::getpgid(pid) };
        assert!(pgid > 0);

        let started_at = crate::process::proc_start_time(pid).expect("proc_start_time for child");

        let rec = pb::ProcessStart {
            name: "test-sleep-stop".to_owned(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/test-sleep-stop.log".to_owned(),
            session: String::new(),
            ..Default::default()
        };

        append_start(dir.path(), &rec).unwrap();

        // Replicate cmd_processes_stop logic: identity-checked kill + append_stop.
        assert!(is_alive(&rec), "must be alive before stop");
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        // Give the kernel a moment to reap.
        let _ = child.wait();

        append_stop(dir.path(), "test-sleep-stop", 0).unwrap();

        // 1. active() must not list it any more.
        let recs = active(dir.path()).unwrap();
        assert!(
            recs.iter().all(|r| r.name != "test-sleep-stop"),
            "stopped process must not appear in active()"
        );

        // 2. is_alive must return false.
        assert!(!is_alive(&rec), "is_alive must be false after stop");
    }

    const LEGACY: &str = concat!(
        r#"{"kind":"start","name":"live","pid":1,"pgid":1,"startedAt":"1","cmd":"x","cwd":"/","logPath":"/l","session":""}"#,
        "\n",
        r#"{"kind":"start","name":"gone","pid":2,"pgid":2,"startedAt":"2","cmd":"y","cwd":"/","logPath":"/g","session":""}"#,
        "\n",
        r#"{"kind":"stop","name":"gone","exitCode":0}"#,
        "\n",
    );

    #[test]
    fn legacy_registry_is_backed_up_and_renamed_once() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("daemons.jsonl"), LEGACY).unwrap();
        migrate_legacy_registry(dir.path()).unwrap();
        assert!(!dir.path().join("daemons.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("processes.jsonl")).unwrap(),
            LEGACY
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemons.jsonl.bak")).unwrap(),
            LEGACY
        );
        // Both entries survive the move: one live, one finished.
        let hist = history(dir.path()).unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(
            hist.iter().find(|(r, _)| r.name == "gone").unwrap().1,
            Some(0)
        );
        assert_eq!(active(dir.path()).unwrap().len(), 1);
        // A second open changes nothing.
        migrate_legacy_registry(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("processes.jsonl")).unwrap(),
            LEGACY
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemons.jsonl.bak")).unwrap(),
            LEGACY
        );
        assert!(!dir.path().join("daemons.jsonl.bak.1").exists());
    }

    #[test]
    fn migration_never_overwrites_an_existing_backup() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("daemons.jsonl.bak"), "older").unwrap();
        std::fs::write(dir.path().join("daemons.jsonl"), LEGACY).unwrap();
        migrate_legacy_registry(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemons.jsonl.bak")).unwrap(),
            "older"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemons.jsonl.bak.1")).unwrap(),
            LEGACY
        );
    }

    #[test]
    fn both_registry_files_present_is_a_coded_conflict() {
        use coded_error::CodedError;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("daemons.jsonl"), LEGACY).unwrap();
        std::fs::write(dir.path().join("processes.jsonl"), "").unwrap();
        let err = migrate_legacy_registry(dir.path()).unwrap_err();
        assert_eq!(err.code(), "gait.state.processes.registry_conflict");
        let text = err.to_string();
        assert!(
            text.contains("daemons.jsonl") && text.contains("processes.jsonl"),
            "{text}"
        );
        assert!(
            active(dir.path()).is_err(),
            "registry reads fail rather than merge"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("daemons.jsonl")).unwrap(),
            LEGACY
        );
    }

    #[test]
    fn concurrent_appends_do_not_corrupt() {
        let dir = Arc::new(TempDir::new().unwrap());
        let n = 20usize;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let d = Arc::clone(&dir);
                std::thread::spawn(move || {
                    let rec = make_rec(&format!("svc-{}", i), (1000 + i) as i32);
                    append_start(d.path(), &rec).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), n, "all {} records should be present", n);
    }
}
