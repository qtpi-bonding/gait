---
status: partial
---

# Retrace + Log/Transcript Naming — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Consolidate the two `Log → working-state` projections (message reconstruction and loadable-toggle replay) into a single named `retrace` module, establishing the `Log` / `Transcript` / `Retrace` vocabulary from the agent-architecture spec — with zero behavior change.

**Architecture:** Today the reconstruction lives in `session/context.rs` (`reconstruct_messages*`) and the loadable replay lives loose in `turn.rs` (`derive_enabled_loadables`). The spec names the reconstruction the **`Retrace`** primitive and observes it already computes *two* projections from the immutable event `Log`: the **`Transcript`** (the `Vec<Message>` the model sees) and the **enabled-loadable set** (last-write-wins toggle replay). This plan renames `context` → `retrace`, renames the message functions, and moves `derive_enabled_loadables` into `retrace` so both projections sit together behind one name. Pure refactor; existing tests are the safety net.

**Tech Stack:** Rust, `cargo test`. No new dependencies.

**Scope:** Plan 1 of the agent-architecture refactor sequence. It is intentionally a safe, self-contained leaf (no dependency on the later `Profile`/`Session`/`Scheduler` plans) and ships independently. It introduces **no** new types (`Transcript` stays `Vec<Message>`; a newtype is deferred) and changes **no** behavior or wire format.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `gait-rs/src/session/retrace.rs` | **(renamed from `context.rs`)** The `Retrace`: rebuilds working state from the `Log`. Owns `messages` (the `Transcript` projection), `messages_from_path`, and `enabled_loadables` (the toggle-replay projection). |
| `gait-rs/src/session/mod.rs` | Module wiring + re-exports. `pub mod retrace;` replaces `pub mod context;`. |
| `gait-rs/src/turn.rs` | Calls `retrace::messages` / `retrace::enabled_loadables`. No longer defines `derive_enabled_loadables`. |

Target public API of `session::retrace`:

```rust
pub fn messages(all_events: &[AnyEvent]) -> anyhow::Result<Vec<Message>>;       // was reconstruct_messages_from_events
pub fn messages_from_path(path: &Path) -> anyhow::Result<Vec<Message>>;         // was reconstruct_messages
pub fn enabled_loadables(
    events: &[AnyEvent],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String>;                                          // was turn::derive_enabled_loadables
```

(The message function is named `messages`, **not** `transcript`, to avoid colliding with the existing `session::transcript` module. The module doc records that its output *is* the Transcript.)

---

### Task 1: Rename `context` module → `retrace`; rename the message functions

**Files:**
- Rename: `gait-rs/src/session/context.rs` → `gait-rs/src/session/retrace.rs`
- Modify: `gait-rs/src/session/mod.rs`
- Modify: `gait-rs/src/turn.rs` (import at line 14; call sites at lines 555, 580, and the test at ~1996)

- [ ] **Step 1: Rename the file with git (preserves history)**

Run (from repo root):
```bash
git mv gait-rs/src/session/context.rs gait-rs/src/session/retrace.rs
```

- [ ] **Step 2: Rename the two message functions inside `retrace.rs`**

In `gait-rs/src/session/retrace.rs`, change the two function signatures and the internal caller:

```rust
// was: pub fn reconstruct_messages_from_events(all_events: &[AnyEvent]) -> anyhow::Result<Vec<Message>> {
pub fn messages(all_events: &[AnyEvent]) -> anyhow::Result<Vec<Message>> {
    // ... body unchanged ...
}

// was: pub fn reconstruct_messages(path: &Path) -> anyhow::Result<Vec<Message>> {
pub fn messages_from_path(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all_events = read_events(path)?;
    messages(&all_events)          // was: reconstruct_messages_from_events(&all_events)
}
```

In the same file's `#[cfg(test)]` module, update the three test call sites that call the renamed functions:
- `reconstruct_messages(&path)` → `messages_from_path(&path)` (in `empty_transcript_returns_empty`, `user_and_assistant_text_reconstructed`, `tool_call_and_result_paired`, `parallel_tool_calls_grouped`, `compaction_breakpoint_discards_earlier_events`)
- `reconstruct_messages_from_events(&events)` → `messages(&events)` (in `permissions_excluded_from_context_reconstruction`)

- [ ] **Step 3: Update `session/mod.rs`**

In `gait-rs/src/session/mod.rs`, replace:
```rust
pub mod context;
...
pub use context::reconstruct_messages;
```
with:
```rust
pub mod retrace;
...
pub use retrace::messages_from_path;
```
Leave the other lines (`pub mod transcript;`, `pub use transcript::*;`, `pub mod session_json;`, the `session_json` re-export) unchanged.

- [ ] **Step 4: Update `turn.rs` import and message call sites**

In `gait-rs/src/turn.rs`:
- Line 14: replace `use crate::session::context::reconstruct_messages_from_events;` with `use crate::session::retrace;`
- Line 555: `messages = reconstruct_messages_from_events(&events_after)?;` → `messages = retrace::messages(&events_after)?;`
- Line 580: `messages = reconstruct_messages_from_events(&events)?;` → `messages = retrace::messages(&events)?;`
- In the test module (~line 1996): `let messages = reconstruct_messages_from_events(&events_after).unwrap();` → `let messages = retrace::messages(&events_after).unwrap();`

(Leave `derive_enabled_loadables` and its call sites alone — Task 2 handles them.)

- [ ] **Step 5: Verify nothing else references the old names**

Run (from repo root):
```bash
grep -rn "reconstruct_messages\|session::context" gait-rs/src --include="*.rs"
```
Expected: **no output** (all references renamed). If any remain, update them the same way.

- [ ] **Step 6: Run the full library test suite — must stay green**

Run (from `gait-rs/`):
```bash
cargo test --lib
```
Expected: PASS. The renamed reconstruction tests (`session::retrace::tests::*`) and all `turn.rs` tests pass. This is a behavior-preserving rename — any failure means a missed call site.

- [ ] **Step 7: Commit**

```bash
git add gait-rs/src/session/retrace.rs gait-rs/src/session/mod.rs gait-rs/src/turn.rs
git commit -m "refactor: rename session::context → retrace; messages()/messages_from_path()"
```

---

### Task 2: Move `derive_enabled_loadables` into `retrace` as `enabled_loadables`

**Files:**
- Modify: `gait-rs/src/session/retrace.rs` (add `enabled_loadables` + moved test)
- Modify: `gait-rs/src/turn.rs` (remove the function + its test; update call sites at lines 594 and ~1499)

- [ ] **Step 1: Add a failing test for the new `retrace::enabled_loadables`**

Append this test to the `#[cfg(test)] mod tests` block in `gait-rs/src/session/retrace.rs`:

```rust
    #[test]
    fn enabled_loadables_seeds_from_config_and_replays_toggles() {
        use std::collections::HashSet;
        // config seeds `tdd`; transcript enables `slack_send`, disables `tdd`
        let config_enabled: HashSet<String> = ["tdd".into()].into();
        let events = vec![
            AnyEvent::ToolCall { id: "1".into(), name: "load".into(),
                input: serde_json::json!({"name": "slack_send", "enabled": true}), ts: 1 },
            AnyEvent::ToolCall { id: "2".into(), name: "load".into(),
                input: serde_json::json!({"name": "tdd", "enabled": false}), ts: 2 },
        ];
        let enabled = enabled_loadables(&events, &config_enabled);
        assert!(enabled.contains("slack_send"), "slack should be enabled after load");
        assert!(!enabled.contains("tdd"), "tdd should be disabled after unload (LWW per entity)");
    }
```

- [ ] **Step 2: Run the test to verify it fails (function does not exist yet)**

Run (from `gait-rs/`):
```bash
cargo test --lib retrace::tests::enabled_loadables_seeds_from_config_and_replays_toggles
```
Expected: FAIL — compile error `cannot find function `enabled_loadables` in this scope`.

- [ ] **Step 3: Add the `enabled_loadables` function to `retrace.rs`**

Add this function to `gait-rs/src/session/retrace.rs` (above the `#[cfg(test)]` block). It is the verbatim body moved from `turn.rs:156-174`:

```rust
/// Derive which loadable names are currently enabled, from the Log.
/// Seeds from config defaults (`config_enabled`), then replays the transcript's
/// `load(name, enabled=…)` tool calls as last-write-wins toggles per entity.
pub fn enabled_loadables(
    events: &[AnyEvent],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut enabled = config_enabled.clone();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "load" {
                let item_name = input.get("name")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                let on = input.get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if on { enabled.insert(item_name); } else { enabled.remove(&item_name); }
            }
        }
    }
    enabled
}
```

- [ ] **Step 4: Run the new test to verify it passes**

Run (from `gait-rs/`):
```bash
cargo test --lib retrace::tests::enabled_loadables_seeds_from_config_and_replays_toggles
```
Expected: PASS.

- [ ] **Step 5: Remove the old function and its test from `turn.rs`; update call sites**

In `gait-rs/src/turn.rs`:
- Delete the `derive_enabled_loadables` function (the doc comment + body at lines 152–174).
- Delete the `derive_enabled_loadables_seeds_from_config_and_replays_transcript` test (lines ~1484–1502) — it now lives in `retrace.rs`.
- Update the production call site at line 594: `let enabled_names = derive_enabled_loadables(&post_compaction, &opts.config_enabled);` → `let enabled_names = retrace::enabled_loadables(&post_compaction, &opts.config_enabled);`
- Fix the now-stale section comment at line 1456 (`// ── unified <loadable-index> + derive_enabled_loadables ──…`) → `// ── unified <loadable-index> + retrace::enabled_loadables ──…` so it doesn't reference the moved function.

- [ ] **Step 6: Verify no stale references remain**

Run (from repo root):
```bash
grep -rn "derive_enabled_loadables" gait-rs/src --include="*.rs"
```
Expected: **no output**.

- [ ] **Step 7: Run the full library test suite**

Run (from `gait-rs/`):
```bash
cargo test --lib
```
Expected: PASS (the moved test runs under `session::retrace::tests`; `turn.rs` tests still pass).

- [ ] **Step 8: Commit**

```bash
git add gait-rs/src/session/retrace.rs gait-rs/src/turn.rs
git commit -m "refactor: move derive_enabled_loadables into retrace::enabled_loadables"
```

---

### Task 3: Document the `Retrace` vocabulary; pin the two-projection invariant

**Files:**
- Modify: `gait-rs/src/session/retrace.rs` (module doc comment + one test)

- [ ] **Step 1: Add a module-level doc comment establishing the vocabulary**

Add to the very top of `gait-rs/src/session/retrace.rs` (above the `use` lines):

```rust
//! # Retrace — the `Log → working-state` fold
//!
//! Rebuilds the model's working state from the immutable event `Log`. It produces
//! two projections:
//!
//! - [`messages`] — the **Transcript**: the `Vec<Message>` the model reasons over.
//!   Read-only today; its only mutation is prefix-`Compaction` (truncate-before-summary).
//! - [`enabled_loadables`] — the active loadable set, via last-write-wins toggle
//!   replay of `load(name, enabled=…)` events.
//!
//! General message-list mutation (`redact`/`replace` by event id) is a future spec;
//! see `docs/superpowers/specs/2026-06-10-agent-architecture-design.md`. The `Log`
//! JSONL wire format is frozen.
```

- [ ] **Step 2: Add a test pinning that both projections read the same `Log` slice**

Append to the `#[cfg(test)] mod tests` block in `retrace.rs`:

```rust
    #[test]
    fn both_projections_derive_from_one_event_log() {
        use std::collections::HashSet;
        // One Log: a user turn + a load toggle.
        let events = vec![
            AnyEvent::UserTurn { content: "hi".into(), from: None, context: None, ts: 0 },
            AnyEvent::ToolCall { id: "1".into(), name: "load".into(),
                input: serde_json::json!({"name": "slack_send", "enabled": true}), ts: 1 },
        ];
        // Transcript projection sees the user turn (load ToolCall renders as an assistant msg).
        let msgs = messages(&events).unwrap();
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hi"));
        // Loadable projection sees the toggle.
        let enabled = enabled_loadables(&events, &HashSet::new());
        assert!(enabled.contains("slack_send"));
    }
```

- [ ] **Step 3: Run the retrace tests**

Run (from `gait-rs/`):
```bash
cargo test --lib retrace
```
Expected: PASS (all `session::retrace::tests::*`).

- [ ] **Step 4: Run the full suite once more**

Run (from `gait-rs/`):
```bash
cargo test --lib
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/session/retrace.rs
git commit -m "docs: document retrace as the Log→working-state fold; pin two-projection test"
```

---

## Self-Review

**Spec coverage** (against `2026-06-10-agent-architecture-design.md`):
- *"`Retrace` — name it (today it's unnamed)"* → Tasks 1–2 create the `retrace` module owning both projections. ✓
- *"It already computes two projections… messages + enabled-loadable set via LWW toggle replay"* → both live in `retrace`; Task 3's test pins it. ✓
- *"`Log` / `Transcript` vocabulary; Transcript = read-only message projection; mutation deferred"* → Task 3 module doc. ✓
- *"`Log` JSONL wire format is frozen"* → no event/schema changes in this plan. ✓
- Out of scope here (correctly deferred to later plans): `Transcript` newtype, `Profile`/`ResolvedProfile`/`Session`, `Harness`/`Scheduler`, `Sandbox`. Not in this plan. ✓

**Placeholder scan:** No TBD/TODO; every code step shows complete code; every command has an expected result. ✓

**Type/name consistency:** `messages`, `messages_from_path`, `enabled_loadables` used identically in the File Structure table, the task bodies, and the tests. The `messages` name deliberately avoids the `session::transcript` module collision (noted in File Structure). The moved `enabled_loadables` body is verbatim from `turn.rs:156-174`; its test is the verbatim logic from `turn.rs:1485-1502`. ✓

**Risk:** Behavior-preserving rename guarded by the pre-existing reconstruction + loadables tests. The only failure mode is a missed call site, caught by the Step-5 `grep` checks and `cargo test --lib` in each task.
