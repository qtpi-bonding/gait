# Architecture Cleanup: Usage Projection + Drop Compat Shim

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix two boundary violations from `docs/architecture.md`: (1) `session.json.token_totals` is dead, duplicated state — replace it with a log-derived `total_usage` projection consumed by a new `gait cost` command; (2) `compat_rewrite_keys` is a silent backward-compat shim that contradicts the project's no-backward-compat policy — delete it.

**Architecture:** Token usage already lives in every `TurnComplete.usage` event. The fix is to sum it on demand (a projection) and delete the mutable sidecar field. The compat shim rewrites legacy event keys (`manifest`→`agent`, `compaction`→`summary`) on every read; we delete it so old-format logs fail loudly instead of being silently rewritten.

**Tech Stack:** Rust, prost/pbjson proto events, gait CLI.

---

## File Map

- **Refactor A — drop compat shim**
  - Modify: `src/proto/wire.rs` — delete `compat_rewrite_keys`, simplify `read_events`
  - Modify: `src/tools/list_agents.rs` — drop the `compat_rewrite_keys` call (line ~99)
- **Refactor B — usage projection + cost command + remove token_totals**
  - Modify: `src/session/retrace.rs` — add `pub fn total_usage(events) -> TurnUsage`
  - Modify: `src/bin/gait.rs` — add `cmd_cost` + dispatch arm + usage string
  - Modify: `src/session/session_json.rs` — delete `TokenTotals` struct + `token_totals` field
  - Modify: `src/tools/spawn_agent.rs` — drop `token_totals` from the `SessionJson` literal (line ~60)

Do Refactor A first (isolated, no dependents); then Refactor B.

---

## Refactor A: Delete the compat shim

### Task A1: Remove `compat_rewrite_keys` and its two call sites

**Files:**
- Modify: `src/proto/wire.rs`
- Modify: `src/tools/list_agents.rs`

- [ ] **Step 1: Write a test that a modern log line round-trips through `read_events`**

In `src/proto/wire.rs`, find the `#[cfg(test)] mod tests` block (or add one) and add:

```rust
#[test]
fn read_events_parses_modern_log_without_rewrite() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    // One canonical event written by append(), then read back.
    let body = crate::proto::events::cwd("/tmp/work");
    append(&path, body).unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1);
    match crate::proto::wire::kind(&events[0]) {
        Some(crate::proto::gait::log::v1::payload::Kind::Cwd(c)) => {
            assert_eq!(c.path, "/tmp/work");
        }
        other => panic!("expected Cwd, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run it (should pass already — establishes the baseline)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test read_events_parses_modern_log 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: PASS (the shim is a no-op on modern logs, so removing it must not change this).

- [ ] **Step 3: Delete `compat_rewrite_keys` from `src/proto/wire.rs`**

Remove the entire function (lines ~40–59):

```rust
// DELETE this whole function:
pub(crate) fn compat_rewrite_keys(line: &str) -> std::borrow::Cow<'_, str> {
    ...
}
```

- [ ] **Step 4: Simplify `read_events` to parse directly**

In `src/proto/wire.rs`, change the read loop:

```rust
// before
events.push(serde_json::from_str::<Event>(&compat_rewrite_keys(line))?);

// after
events.push(serde_json::from_str::<Event>(line)?);
```

- [ ] **Step 5: Fix the `list_agents.rs` call site**

In `src/tools/list_agents.rs` (around line 99), drop the `compat_rewrite_keys` wrap:

```rust
// before
.filter_map(|l| serde_json::from_str::<Event>(&crate::proto::wire::compat_rewrite_keys(l)).ok())

// after
.filter_map(|l| serde_json::from_str::<Event>(l).ok())
```

- [ ] **Step 6: Build and test**

```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: compiles (no remaining references to `compat_rewrite_keys`), all tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/proto/wire.rs src/tools/list_agents.rs
git commit -m "refactor: drop compat_rewrite_keys — no backward compat for legacy log keys"
```

---

## Refactor B: `total_usage` projection + `gait cost` + remove `token_totals`

### Task B1: Add the `total_usage` projection to `retrace.rs`

**Files:**
- Modify: `src/session/retrace.rs`

- [ ] **Step 1: Write the failing test**

In the `#[cfg(test)] mod tests` block of `src/session/retrace.rs`, add:

```rust
#[test]
fn total_usage_sums_turn_complete_events() {
    use crate::session::transcript::{TurnUsage, TurnOutput};
    let mk = |inp: u64, out: u64| {
        events::turn_complete(
            &TurnUsage { input: inp, output: out, cache_read: 0, cache_write: 0 },
            "deadbeef".into(), false, None, vec![],
            &TurnOutput { message: String::new(), result: None },
        )
    };
    let evs = vec![mk(100, 20), mk(50, 10)]
        .into_iter().map(wire::event).collect::<Vec<_>>();
    let total = total_usage(&evs);
    assert_eq!(total.input, 150);
    assert_eq!(total.output, 30);
}
```

(`wire::event` mints an `Event` from a `Body` — same helper the other retrace tests use. If the helper name differs in this file, mirror the existing `.map(wire::event)` pattern already used by neighboring tests.)

- [ ] **Step 2: Run it to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test total_usage_sums_turn_complete 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|^error" /tmp/gait-test-output.txt | head -10
```

Expected: compile error — `total_usage` does not exist.

- [ ] **Step 3: Implement `total_usage`**

Add to `src/session/retrace.rs` (near `active_agent_checkpoint`, which is the template for a standalone projection). Place it after that function:

```rust
/// Sum of every `TurnComplete.usage` over the log — the session's cumulative
/// token usage. Replaces the dead `session.json.token_totals` sidecar; usage is
/// derived from the log, never stored twice. Folds the whole session (ignores
/// compaction — total cost includes compacted turns).
pub fn total_usage(events: &[Event]) -> crate::session::transcript::TurnUsage {
    use crate::session::transcript::TurnUsage;
    struct Usage;
    impl Projection for Usage {
        type State = TurnUsage;
        fn seed(&self) -> Self::State {
            TurnUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 }
        }
        fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
            if let payload::Kind::TurnComplete(tc) = kind {
                if let Some(u) = &tc.usage {
                    state.input += u.input;
                    state.output += u.output;
                    state.cache_read += u.cache_read;
                    state.cache_write += u.cache_write;
                }
            }
        }
    }
    run_projection(&Usage, events, &HashMap::new())
}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test total_usage_sums_turn_complete 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/session/retrace.rs
git commit -m "feat: add total_usage projection — sums TurnComplete.usage from the log"
```

---

### Task B2: Add the `gait cost` command

**Files:**
- Modify: `src/bin/gait.rs`

- [ ] **Step 1: Add the dispatch arm**

In `src/bin/gait.rs`, find the command match (near the `"status" => cmd_status(&args[2..]),` arm) and add below it:

```rust
"cost"     => cmd_cost(&args[2..]),
```

- [ ] **Step 2: Implement `cmd_cost`**

Add this function next to `cmd_status` in `src/bin/gait.rs`:

```rust
fn cmd_cost(argv: &[String]) {
    use gait_ai::proto::wire::read_events;
    use gait_ai::session::retrace::total_usage;

    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });

    let mut session_id: Option<String> = None;
    let mut json = false;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--gait-dir" => { i += 1; }
            "--json"     => { json = true; }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }

    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait cost <session-id> [--gait-dir <path>] [--json]");
            std::process::exit(1);
        }
    };

    let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
    let events = read_events(&tx_path).unwrap_or_else(|e| {
        eprintln!("[gait] cannot read session {session_id}: {e}");
        std::process::exit(1);
    });
    let u = total_usage(&events);

    if json {
        println!(
            "{{\"input\":{},\"output\":{},\"cache_read\":{},\"cache_write\":{}}}",
            u.input, u.output, u.cache_read, u.cache_write
        );
    } else {
        println!("session {session_id}");
        println!("  input        {}", u.input);
        println!("  output       {}", u.output);
        println!("  cache read   {}", u.cache_read);
        println!("  cache write  {}", u.cache_write);
        println!("  total tokens {}", u.input + u.output + u.cache_read + u.cache_write);
    }
}
```

- [ ] **Step 3: Update the `usage()` help text**

Find the `usage()` function (or the help string listing commands) and add a line near `status`:

```
cost       <session-id> [--json]          cumulative token usage for a session
```

- [ ] **Step 4: Build**

```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
```

Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add src/bin/gait.rs
git commit -m "feat: add 'gait cost <session-id>' — reads total_usage projection"
```

---

### Task B3: Remove the dead `token_totals` sidecar

**Files:**
- Modify: `src/session/session_json.rs`
- Modify: `src/tools/spawn_agent.rs`

Note: `SessionJson` deserialization does not use `deny_unknown_fields`, so existing
`session.json` files that still contain a `token_totals` key will keep parsing — the key is
simply ignored after removal.

- [ ] **Step 1: Delete the `TokenTotals` struct and the field**

In `src/session/session_json.rs`, remove the struct (lines ~4–10):

```rust
// DELETE:
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}
```

And remove the field from `SessionJson` (line ~18–19):

```rust
// DELETE these two lines:
    #[serde(default)]
    pub token_totals: TokenTotals,
```

- [ ] **Step 2: Remove `token_totals` from the two literal constructors in this file**

In `read_session_json`, both `SessionJson { ... }` literals (the missing-file branch and the
parse-error branch) currently set `token_totals: TokenTotals::default(),`. Delete that line from
both.

- [ ] **Step 3: Fix the in-file tests**

In the `#[cfg(test)]` block, the three `SessionJson { ... }` literals set `token_totals: ...`.
Delete those lines. In `session_json_roundtrips_through_disk`, also delete the assertion:

```rust
// DELETE:
        assert_eq!(loaded.token_totals.input, 10);
```

- [ ] **Step 4: Fix the `spawn_agent.rs` literal**

In `src/tools/spawn_agent.rs` (around line 60), the `SessionJson { ... }` literal sets
`token_totals: TokenTotals::default(),`. Delete that line. If there's a `use ...TokenTotals`
import in that file, remove it too.

- [ ] **Step 5: Build and test**

```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: compiles (no remaining `TokenTotals` / `token_totals` references), all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/session/session_json.rs src/tools/spawn_agent.rs
git commit -m "refactor: remove dead session.json.token_totals — usage is a log projection now"
```

---

## Final step: mark the violations resolved in the architecture doc

- [ ] In `docs/architecture.md`, under "Known Issues / Boundary Violations", strike or remove
  items **#1** (token_totals) and **#2** (compat shim), noting they're resolved by this plan.
  Renumber the remaining issues.

```bash
git add docs/architecture.md docs/plans/2026-06-13-architecture-cleanup.md
git commit -m "docs: mark token_totals + compat-shim violations resolved"
```

---

## Self-Review Notes

- **Spec coverage:** Refactor A (delete shim) = Task A1. Refactor B (projection + command + remove field) = Tasks B1–B3. Doc update = final step. ✓
- **YAGNI check:** `gait cost` prints tokens only — no per-model dollar pricing (would need a price table; out of scope). ✓
- **Type consistency:** `total_usage` returns `crate::session::transcript::TurnUsage` (the existing type used by `turn_complete`), not a new type. The deleted `TokenTotals` was a structural duplicate of `TurnUsage`. ✓
- **No silent compat:** after Task A1, an old-format log line (`"manifest"`/`"compaction"` keys) makes `read_events` return an `Err` — loud failure, matching the no-backward-compat policy. ✓
