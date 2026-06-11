---
status: done
---

# Harness / Scheduler Split — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the *policy* ("what runs next when a turn ends") out of `NativeHarnessBackend::after_turn` into a `Scheduler` trait that returns action descriptions, leaving the `HarnessBackend` as pure *mechanism* (lock / append / commit / spawn) that executes those actions.

**Architecture:** Today `NativeHarnessBackend::after_turn` (harness.rs) conflates a one-line policy decision (`if assigned_by → resume the assigner`, plus a `TurnResult → (status, summary)` mapping) with all the mechanism (acquire the assigner's lock, append an `AgentUpdate`, commit the trace repo, spawn `gait run --auto`). This plan introduces `Scheduler::next(&TurnEnded) -> Vec<Action>` (the policy) and `HarnessBackend::execute(&Action)` (the mechanism). `NativeScheduler` reproduces today's behavior; the call site loops `for action in scheduler.next(..) { harness.execute(action) }`. This is the seam external orchestrators (grorg, archebuild) replace.

**Tech Stack:** Rust, `async_trait`, `cargo test`. No new dependencies.

**Scope:** Plan 2 of the agent-architecture refactor sequence. Self-contained: touches `harness.rs`, a new `scheduler.rs`, one call site in `bin/gait.rs`, and `lib.rs` exports. Behavior is identical to today (`NativeScheduler` + `NativeHarnessBackend` = the current `after_turn`).

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `gait-rs/src/scheduler.rs` | **(new)** `Action` enum, `TurnEnded` context, `Scheduler` trait, `NativeScheduler` (the policy). Pure functions, no I/O — unit-testable. |
| `gait-rs/src/harness.rs` | `HarnessBackend` becomes mechanism: `execute(&self, action: &Action)`. `NativeHarnessBackend` performs lock/append/commit/spawn. No `TurnResult` inspection. |
| `gait-rs/src/bin/gait.rs` | After `run_turn`, build a `NativeScheduler` + `NativeHarnessBackend`, then `for action in scheduler.next(&ended) { harness.execute(&action).await }`. |
| `gait-rs/src/lib.rs` | Add `pub mod scheduler;` + re-exports. |

Target new types (`scheduler.rs`):

```rust
pub struct TurnEnded<'a> {
    pub session_id: &'a str,
    pub assigned_by: Option<&'a str>,
    pub result: &'a crate::turn::TurnResult,
}

pub enum Action {
    /// Notify a target session of this session's result, then resume it.
    NotifyAndResume { target_session: String, from_session: String, status: String, summary: String },
}

pub trait Scheduler: Send + Sync {
    fn next(&self, ended: &TurnEnded) -> Vec<Action>;
}

pub struct NativeScheduler;
```

---

### Task 1: Create the `scheduler` module (policy)

**Files:**
- Create: `gait-rs/src/scheduler.rs`
- Modify: `gait-rs/src/lib.rs` (add `pub mod scheduler;`)

- [ ] **Step 1: Write the failing test**

Create `gait-rs/src/scheduler.rs` with this content:

```rust
//! Scheduler — the policy half of coordination. Given a turn that just ended,
//! it decides *what should happen next* and returns `Action` descriptions. The
//! `HarnessBackend` (mechanism) executes them. External orchestrators (grorg,
//! archebuild) supply their own `Scheduler`; `NativeScheduler` is the standalone
//! default (resume the assigner, if any).

use crate::turn::TurnResult;

pub struct TurnEnded<'a> {
    pub session_id: &'a str,
    pub assigned_by: Option<&'a str>,
    pub result: &'a TurnResult,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Notify `target_session` of `from_session`'s result, then resume it.
    NotifyAndResume {
        target_session: String,
        from_session: String,
        status: String,
        summary: String,
    },
}

pub trait Scheduler: Send + Sync {
    fn next(&self, ended: &TurnEnded) -> Vec<Action>;
}

/// Map a TurnResult to the (status, summary) reported up to an assigner.
fn summarize(result: &TurnResult) -> (String, String) {
    match result {
        TurnResult::Done { message, .. } =>
            ("done".to_string(), message.chars().take(500).collect()),
        TurnResult::Permissions =>
            ("permissions".to_string(), "needs permissions approval".to_string()),
        TurnResult::Clarify { .. } =>
            ("clarify".to_string(), "paused for clarification".to_string()),
        TurnResult::Inform { .. } =>
            ("inform".to_string(), "paused to inform user".to_string()),
    }
}

pub struct NativeScheduler;

impl Scheduler for NativeScheduler {
    fn next(&self, ended: &TurnEnded) -> Vec<Action> {
        let Some(assigner) = ended.assigned_by else { return vec![]; };
        let (status, summary) = summarize(ended.result);
        vec![Action::NotifyAndResume {
            target_session: assigner.to_string(),
            from_session: ended.session_id.to_string(),
            status,
            summary,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_assigner_yields_no_actions() {
        let result = TurnResult::Done { message: "ok".into(), result: None };
        let ended = TurnEnded { session_id: "s1", assigned_by: None, result: &result };
        assert!(NativeScheduler.next(&ended).is_empty());
    }

    #[test]
    fn assigner_yields_notify_and_resume_with_done_status() {
        let result = TurnResult::Done { message: "built the parser".into(), result: None };
        let ended = TurnEnded { session_id: "child", assigned_by: Some("parent"), result: &result };
        let actions = NativeScheduler.next(&ended);
        assert_eq!(actions, vec![Action::NotifyAndResume {
            target_session: "parent".into(),
            from_session: "child".into(),
            status: "done".into(),
            summary: "built the parser".into(),
        }]);
    }

    #[test]
    fn pause_results_map_to_their_status() {
        let result = TurnResult::Permissions;
        let ended = TurnEnded { session_id: "child", assigned_by: Some("parent"), result: &result };
        let actions = NativeScheduler.next(&ended);
        match &actions[0] {
            Action::NotifyAndResume { status, .. } => assert_eq!(status, "permissions"),
        }
    }
}
```

Add to `gait-rs/src/lib.rs` (alongside the other `pub mod` lines):
```rust
pub mod scheduler;
```

- [ ] **Step 2: Run the tests to verify they pass**

Run (from `gait-rs/`):
```bash
cargo test --lib scheduler
```
Expected: PASS (3 tests). (`summarize` is reused from the current `after_turn` match verbatim, so behavior matches.)

- [ ] **Step 3: Commit**

```bash
git add gait-rs/src/scheduler.rs gait-rs/src/lib.rs
git commit -m "feat: add scheduler module (NativeScheduler policy: TurnResult+assigned_by -> Action)"
```

---

### Task 2: Reduce `HarnessBackend` to mechanism (`execute(&Action)`)

**Files:**
- Modify: `gait-rs/src/harness.rs` (replace `after_turn` with `execute`; move `summarize` logic out — it now lives in the scheduler)
- Modify: `gait-rs/src/lib.rs` (export `Action`, `Scheduler`, `NativeScheduler`)

- [ ] **Step 1: Replace the trait method and impls in `harness.rs`**

Rewrite the trait + impls so the trait takes an `Action` and performs only mechanism. Replace the `HarnessBackend` trait, `NoopHarnessBackend` impl, and `NativeHarnessBackend::after_turn` with:

```rust
use crate::scheduler::Action;

#[async_trait::async_trait]
pub trait HarnessBackend: Send + Sync {
    /// Execute a scheduler-produced action (mechanism only).
    async fn execute(&self, action: &Action) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl HarnessBackend for NoopHarnessBackend {
    async fn execute(&self, _action: &Action) -> anyhow::Result<()> { Ok(()) }
}

#[async_trait::async_trait]
impl HarnessBackend for NativeHarnessBackend {
    async fn execute(&self, action: &Action) -> anyhow::Result<()> {
        match action {
            Action::NotifyAndResume { target_session, from_session, status, summary } => {
                let event = AnyEvent::AgentUpdate {
                    session_id: from_session.clone(),
                    status: status.clone(),
                    summary: summary.clone(),
                    items: None,
                    ts: now_ms(),
                };

                let target_dir = self.workspace.join("sessions").join(target_session);
                let tx_path = target_dir.join("transcript.jsonl");

                // Only retry on "already running" — fail fast on I/O errors.
                let mut lock: Option<SessionLock> = None;
                for _ in 0..20 {
                    match SessionLock::acquire(&target_dir) {
                        Ok(l) => { lock = Some(l); break; }
                        Err(e) => {
                            if !e.to_string().contains("already running") {
                                return Err(e.context(format!(
                                    "unexpected error acquiring lock on session {}", target_session)));
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
                let _lock = lock.ok_or_else(|| anyhow::anyhow!(
                    "failed to acquire lock on session {} after 20 attempts", target_session))?;

                append_event(&tx_path, &event)?;
                let repos = crate::git::Repos::production();
                repos.traces.commit_session(
                    &self.workspace,
                    target_session,
                    &format!("session({}): agent_update from {}", target_session, from_session),
                )?;
                drop(_lock);

                let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
                std::process::Command::new(&exe)
                    .args(["run", "--session", target_session.as_str(),
                           "--gait-dir", self.workspace.to_str().unwrap_or("."),
                           "--auto"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                Ok(())
            }
        }
    }
}
```

Keep the `NativeHarnessBackend { workspace }` struct and its `new()` unchanged. Fix the now-stale top-of-file import at `harness.rs:2`: change `use crate::turn::{TurnResult, SessionLock};` to `use crate::turn::SessionLock;` (keep `SessionLock` — `execute` still uses it; drop `TurnResult` — the result mapping moved to the scheduler).

- [ ] **Step 2: Update the harness unit tests in `harness.rs`**

Replace the two existing tests (`noop_harness_always_succeeds`, `native_harness_with_no_assigned_by_is_noop`) with mechanism-level tests. **Also delete the now-unused `use crate::turn::TurnResult;` at the top of the `#[cfg(test)] mod tests` block (harness.rs:119)** — the new test below only needs `Action` (already in scope via `use super::*;`):

```rust
    #[tokio::test]
    async fn noop_harness_execute_succeeds() {
        let action = Action::NotifyAndResume {
            target_session: "parent".into(), from_session: "child".into(),
            status: "done".into(), summary: "x".into(),
        };
        assert!(NoopHarnessBackend.execute(&action).await.is_ok());
    }
```

(The "no assigned_by is a no-op" case is now covered by `NativeScheduler` returning zero actions — tested in Task 1, so there is nothing for the harness to execute. The native execute path that spawns a subprocess is exercised by the integration test in `tests/integration_coordination.rs`, unchanged.)

- [ ] **Step 3: Update `lib.rs` exports**

In `gait-rs/src/lib.rs`, alongside `pub use harness::{HarnessBackend, NoopHarnessBackend, NativeHarnessBackend};`, add:
```rust
pub use scheduler::{Action, Scheduler, NativeScheduler, TurnEnded};
```

- [ ] **Step 4: Run the harness + scheduler tests**

Run (from `gait-rs/`):
```bash
cargo test --lib harness
cargo test --lib scheduler
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/harness.rs gait-rs/src/lib.rs
git commit -m "refactor: HarnessBackend is mechanism-only (execute(Action)); policy moved to Scheduler"
```

---

### Task 3: Wire the call site in `bin/gait.rs`

**Files:**
- Modify: `gait-rs/src/bin/gait.rs` (the harness block after `run_turn`, ~lines 384-388)

- [ ] **Step 1: Replace the after-turn call**

In `gait-rs/src/bin/gait.rs`, replace the current block:
```rust
    let sj = gait_ai::session::session_json::read_session_json(&workspace, &session_id);
    let harness = gait_ai::harness::NativeHarnessBackend::new(workspace.clone());
    if let Err(e) = harness.after_turn(&session_id, sj.assigned_by.as_deref(), &turn_result).await {
        eprintln!("[gait] warning: harness after_turn failed: {}", e);
    }
```
with:
```rust
    let sj = gait_ai::session::session_json::read_session_json(&workspace, &session_id);
    let scheduler = gait_ai::scheduler::NativeScheduler;
    let harness = gait_ai::harness::NativeHarnessBackend::new(workspace.clone());
    let ended = gait_ai::scheduler::TurnEnded {
        session_id: &session_id,
        assigned_by: sj.assigned_by.as_deref(),
        result: &turn_result,
    };
    for action in scheduler.next(&ended) {
        if let Err(e) = harness.execute(&action).await {
            eprintln!("[gait] warning: harness execute failed: {}", e);
        }
    }
```

Ensure the trait `gait_ai::harness::HarnessBackend` is in scope (the existing `use` at bin/gait.rs:10 already imports `HarnessBackend`; keep it — `execute` is a trait method).

- [ ] **Step 2: Build and run the coordination integration test**

Run (from `gait-rs/`):
```bash
cargo test --test integration_coordination
```
Expected: PASS — `spawn_agent_via_run_turn_creates_child_session` and `run_turn_permissions_result_integration` still pass (the spawn path is unchanged; only the harness invocation shape changed).

- [ ] **Step 3: Run the full suite**

Run (from `gait-rs/`):
```bash
cargo test
```
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/bin/gait.rs
git commit -m "refactor: drive after-turn coordination via Scheduler.next + Harness.execute"
```

---

## Self-Review

**Spec coverage** (against the agent-architecture spec's `Harness`/`Scheduler` sections):
- *"Scheduler returns action descriptions, never side effects — the Harness executes them"* → `Scheduler::next -> Vec<Action>`; `HarnessBackend::execute(&Action)`. ✓
- *"NativeScheduler = today's assigned_by → ResumeAssigner peer-spawn"* → `NativeScheduler::next` reproduces the `assigned_by` guard + `TurnResult` mapping verbatim. ✓
- *"the suspend/escalation seam lands on Scheduler"* → the `TurnResult::{Permissions,Clarify,Inform}` → status mapping now lives in the scheduler's `summarize`; future `EscalateAndPark` is a new `Action` variant + `NativeScheduler` branch (out of scope here, but the seam exists). ✓
- *"the seam external orchestrators drive"* → a grorg/archebuild `Scheduler` impl now substitutes cleanly; `Harness::execute` is the mechanism it reuses. ✓

**Placeholder scan:** No TBD/TODO; all code complete; commands have expected outputs. ✓

**Type/name consistency:** `Action::NotifyAndResume { target_session, from_session, status, summary }` used identically in `scheduler.rs`, the harness `execute` match, and the tests. `TurnEnded` fields (`session_id`, `assigned_by`, `result`) consistent. `summarize` outputs the same `(status, summary)` strings the old `after_turn` produced (`done`/`permissions`/`clarify`/`inform`; 500-char message truncation preserved). ✓

**Risk:** Behavior-preserving. The one observable difference is internal structure; the integration test guards the end-to-end spawn path. The `from_session`/`target_session` naming makes the commit message identical to today's (`agent_update from {from_session}`).
