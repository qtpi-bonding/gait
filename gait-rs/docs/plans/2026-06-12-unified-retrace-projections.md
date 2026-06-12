# Unified Retrace Projections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fold every derived-state view in `retrace.rs` (transcript messages, enabled loadables, active manifest, and a new effective-allow set) onto a single `Projection` trait that all read the *same* input — the mutation-applied, full-session event stream — eliminating today's raw-vs-mutated and compaction-window inconsistencies, and making session-scoped permissions (F1) and log-derived "once" fall out as projections.

**Architecture:** The append-only event **Log** (the file on disk, renamed `transcript.jsonl` → `log.jsonl` in Phase 0) is folded into **projections** (read-models). The **Transcript** is *one* projection — the `ActiveMessages` `Vec<Message>` given to the model — NOT the file. **Mutations** (`Redact`/`Replace` + ToolCall↔ToolResult cascade) are the *substrate*: computed first, producing an "effective" event view that every projection folds in a single pass. **Every** derived view — `ActiveMessages`, `ActiveLoadables`, `ActivePermissions`, `ActiveManifest` — is a `Projection` impl; mutations are the layer underneath, not a peer.

**Naming key (authoritative — supersedes any older names below):**
- **Convention:** a bare name = a logged **event** (keeps its proto name); **`Active___`** = a derived **projection** folded from the Log.
- **Log** = the append-only file, `log.jsonl` (Phase 0 renames it from `transcript.jsonl`). The word *"transcript"* is **retired entirely** — the file is the Log; the message context is `ActiveMessages`.
- **Projections:** `ActiveMessages` (the message context the model reads), `ActiveLoadables`, `ActivePermissions` (the effective allow-set, was `EffectiveAllow`), `ActiveManifest`.
- **One event renamed for clarity:** `Resolution → Manifest` (it holds the turn's full resolved spec — model · tools · skills · permissions · prompt; "Resolution" named the *act of resolving*, not the contents). All other events keep their proto names.
- **Substrate:** `Mutation` (Redact/Replace + cascade) — not a projection; computed first.

**Tech Stack:** Rust, prost/pbjson proto (`proto/log.proto` → `crate::proto::gait::log::v1`), `anyhow`. Tests: `cargo test` (per `gait-rs/CLAUDE.md`: redirect to a file, then grep — never re-run cargo just to see output).

**Design principles (carry into every task):**
- **Everything is a `Projection`.** `ActiveMessages`, `ActiveLoadables`, `ActivePermissions`, `ActiveManifest` all implement the same trait and fold the same mutation-applied, full-session stream. No projection reads raw events or a pre-sliced window. (Mutations are the substrate underneath, not a projection.)
- **Compaction is its own boundary signal — not a projection.** A `Compaction` event means: *"restart the message context from this summary, but carry all derived state forward unchanged."* So the `ActiveMessages` projection treats a `Compaction` event as a reset point (clear its accumulated messages, emit the summary); **state** projections (`ActiveLoadables`, `ActivePermissions`, `ActiveManifest`) ignore it entirely and fold the whole session. Compaction is an *event handled differently by each projection*, never a projection itself.
- **Permission is forward-only.** A grant/revoke affects *subsequent* turns and the projected state; it never re-validates an already-recorded action. Reconstruction replays recorded facts, never re-executes.
- **Revoke = a forward `deny` event, not deletion.** The Log is an append-only ledger: `allow(X)` and a later `deny(X)` BOTH stay on disk; the `ActivePermissions` projection folds them (approve inserts, deny removes) so the *derived* set no longer contains X — while history honestly records that X was allowed in between (a tool may have run under it). `Redact`/undo is a **separate** mechanism reserved for genuine mistakes / secret-scrubbing: it appends a Redact event and the projection skips the target as if it never happened. Do NOT use redact for normal revocation — it would rewrite history dishonestly.

---

## File Structure

- `proto/log.proto` — add `SCOPE_SESSION = 4` to the `Scope` enum (sibling repo of `gait-rs`; `build.rs` reads `../proto`).
- `gait-rs/src/session/retrace.rs` — add the `Projection` trait + `effective_kinds` helper; reimplement `fold` as a single pass driving all projections; migrate `enabled_loadables` and `active_manifest` to fold the *mutation-applied, full-session* stream; add the `ActivePermissions` projection. This is the heart of the change.
- `gait-rs/src/permission.rs` — add `PermissionChoice::Session`; stop writing the per-session temp file for `Once` (it is now derived from the log); `Session`/`Once` produce log-only grants.
- `gait-rs/src/ui/permission.rs` — add `[s] session` to the prompt and its parser.
- `gait-rs/src/chat.rs` — handle `PermissionChoice::Session` → `PermissionsReplyItem{scope:"session"}`.
- `gait-rs/src/turn.rs` — replace the once-approval temp-file read with `RetraceResult.effective_allow`; delete `once_approval_path`.

Three phases. **Phase 0 (Tasks 0, 0b)** does the mechanical renames: the Log file `transcript.jsonl` → `log.jsonl`, and the `Resolution` event → `Manifest`. Independently shippable. **Phase 1 (Tasks 1–5)** delivers the `Projection` trait + consistency fixes, with `ActiveMessages` as a projection too. **Phase 2 (Tasks 6–10)** adds the `ActivePermissions` projection, session scope, and removes the temp-file hack.

Also add to the File Structure: `gait-rs/src/session/transcript.rs` — a single `log_path()` helper builds `…/log.jsonl` (hard rename from `transcript.jsonl`, no backward-compat fallback).

---

## Phase 0 — Rename the Log file: `transcript.jsonl` → `log.jsonl`

### Task 0: Rename the on-disk Log — hard rename, no backward compat

**Files:**
- Modify: `gait-rs/src/session/transcript.rs` (the path that builds `…/transcript.jsonl`)
- Sweep: every literal `"transcript.jsonl"` across `gait-rs/src/**` and `gait-rs/tests/**`
- Docs: `~/.claude/skills/gait-agent/SKILL.md` references `transcript.jsonl` (update separately; not in this repo)

The file is the **Log**, not the Transcript — renaming it removes the core confusion. **Clean rename, no fallback**: a "read `log.jsonl` else `transcript.jsonl`" shim is permanent cruft for a one-time pre-release migration — it would bite us later. Pre-existing dogfood sessions in `gait-trace` simply won't be read by name (acceptable; they can be re-run or manually renamed). One canonical path helper, one filename.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn log_path_is_log_jsonl() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_eq!(
        log_path(dir.path(), "s1"),
        dir.path().join("sessions").join("s1").join("log.jsonl"),
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai log_path_is_log_jsonl 2>&1 > /tmp/t.txt; grep -E "error|cannot find" /tmp/t.txt | head`
Expected: FAIL — `cannot find function log_path`.

- [ ] **Step 3: Add the path helper in `transcript.rs`**

```rust
/// The session's append-only Log file. (The file is the Log, not the Transcript —
/// the Transcript is the `ActiveMessages`/`Transcript` projection derived from it.)
pub fn log_path(gait_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    gait_dir.join("sessions").join(session_id).join("log.jsonl")
}
```

- [ ] **Step 4: Sweep every `transcript.jsonl` literal to use `log_path(...)`**

Run: `grep -rn '"transcript.jsonl"' gait-rs/src gait-rs/tests` → replace each path construction (`gait_dir.join("sessions").join(id).join("transcript.jsonl")`) with `crate::session::transcript::log_path(gait_dir, id)` (or `gait_ai::session::transcript::log_path` in tests). Known sites include `turn.rs` (`tx_path`), `bin/gait.rs`, `chat.rs`, `proto/emitter.rs` (`Emitter::for_session`), `session/ingestor.rs`. Confirm with the grep — do not rely on this list being exhaustive.

- [ ] **Step 5: Run the full suite + commit**

Run: `cargo test -p gait-ai 2>&1 > /tmp/t.txt; grep -E "test result|FAILED" /tmp/t.txt`
Expected: all PASS; `grep -rn '"transcript.jsonl"' gait-rs/src` shows nothing — the single helper builds the path.

```bash
git add gait-rs/src gait-rs/tests
git commit -m "refactor: rename Log file transcript.jsonl -> log.jsonl (hard rename, no fallback)"
```

---

### Task 0b: Rename the `Resolution` event → `Manifest` in the proto

**Files:**
- Modify: `proto/log.proto` (message `Resolution` at :138, the oneof field `resolution` at :2)
- Modify: `gait-rs/src/proto/events.rs` (`resolution_msg` → `manifest_msg`)
- Sweep: `pb::Resolution` / `payload::Kind::Resolution` / `resolution_msg` references across `gait-rs/src/**`

"Resolution" names the *act of resolving*, not the contents (model · tools · skills · permissions · prompt). Rename the event to `Manifest`. No backward compat (same spirit as Task 0). The projection that folds it is `ActiveManifest` (Task 3).

- [ ] **Step 1: Write the failing test** in `gait-rs/src/proto/tests.rs`

```rust
#[test]
fn manifest_event_exists_resolution_gone() {
    use crate::proto::gait::log::v1::{payload, Manifest};
    // The message type is now `Manifest`, and the oneof arm is `Manifest`.
    let _ = payload::Kind::Manifest(Manifest::default());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai manifest_event_exists 2>&1 > /tmp/t.txt; grep -E "error|cannot find|no variant" /tmp/t.txt | head`
Expected: FAIL — `no variant or associated item named Manifest`.

- [ ] **Step 3: Rename in `proto/log.proto`**

At :138, `message Resolution {` → `message Manifest {`. In the `Payload` oneof (:2), `Resolution resolution = 2;` → `Manifest manifest = 2;` (keep field number 2). Build regenerates the Rust types via `build.rs`.

- [ ] **Step 4: Rename `resolution_msg` → `manifest_msg` and sweep references**

In `events.rs`, rename `pub fn resolution_msg(...) -> pb::Resolution` → `manifest_msg(...) -> pb::Manifest`. Then: `grep -rn 'resolution_msg\|pb::Resolution\|Kind::Resolution' gait-rs/src` and update each (notably `turn.rs` drift-guard at :406, and `retrace.rs` `active_manifest`).

- [ ] **Step 5: Run the full suite + commit**

Run: `cargo test -p gait-ai 2>&1 > /tmp/t.txt; grep -E "test result|FAILED" /tmp/t.txt`
Expected: all PASS; `grep -rn 'Resolution' gait-rs/src` returns nothing.

```bash
git add proto/log.proto gait-rs/src
git commit -m "refactor(proto): rename Resolution event -> Manifest"
```

---

## Phase 1 — The `Projection` trait and consistency fixes

### Task 1: Define the `Projection` trait and the effective-event view

**Files:**
- Modify: `gait-rs/src/session/retrace.rs` (add near the top, after `MutationState` at line 48)
- Test: same file `#[cfg(test)] mod tests`

The substrate already exists: `active_mutations(events) -> HashMap<String, MutationState>` (retrace.rs:254) and `apply_cascade(&mut mutations, events)` (retrace.rs:287). `effective_kind(e, mutations)` (retrace.rs:61) already returns the mutation-applied payload kind (or `None` if redacted). We add a trait and a single-pass driver input.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn effective_kinds_drops_redacted_and_applies_replace() {
    use crate::proto::{events, wire};
    // event 1: a load tool call; event 2: redact event 1
    let e1 = wire::mint(events::tool_call("tc1".into(), "load".into(),
        &serde_json::json!({"name":"slack","enabled":true})), &[], 1);
    let redact = wire::mint(events::redact(&e1.id, Some("oops".into())), &[e1.id.clone()], 2);
    let all = vec![e1, redact];
    let muts = { let mut m = active_mutations(&all); apply_cascade(&mut m, &all); m };
    let kinds: Vec<_> = effective_kinds(&all, &muts).collect();
    // event 1 is redacted → not present; redact event itself carries no payload kind we fold
    assert!(kinds.iter().all(|(_, k)| !matches!(k, payload::Kind::ToolCall(tc) if tc.name == "load")),
        "redacted load must not appear in effective kinds");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p gait-ai effective_kinds_drops_redacted 2>&1 > /tmp/t.txt; grep -E "error|cannot find" /tmp/t.txt | head`
Expected: FAIL — `cannot find function effective_kinds`.

- [ ] **Step 3: Add the trait and the `effective_kinds` iterator**

Add after `effective_kind` (retrace.rs:70):

```rust
/// Iterate `(event, effective_kind)` over the full session, applying mutations:
/// redacted events are skipped; replaced events yield their replacement kind.
/// This is the single canonical input every `Projection` folds.
pub(crate) fn effective_kinds<'a>(
    all_events: &'a [Event],
    mutations: &'a HashMap<String, MutationState>,
) -> impl Iterator<Item = (&'a Event, &'a payload::Kind)> {
    all_events.iter().filter_map(move |e| effective_kind(e, mutations).map(|k| (e, k)))
}

/// A derived read-model folded from the Log. Every projection folds the SAME
/// input — the mutation-applied, full-session stream produced by `effective_kinds`
/// — so two projections can never disagree about what happened. Mutations are the
/// substrate (applied before folding); compaction is NOT handled here (only the
/// `ActiveMessages` projection cares about it).
pub(crate) trait Projection {
    type State;
    /// Initial state before any events (seeded from config where relevant).
    fn seed(&self) -> Self::State;
    /// Fold one event's effective payload kind into the state.
    fn apply(&self, state: &mut Self::State, event: &Event, kind: &payload::Kind);
    /// Finalize (default: identity). Used by `ActiveMessages` to drop a trailing nothing.
    fn finish(&self, state: Self::State) -> Self::State { state }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p gait-ai effective_kinds_drops_redacted 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/session/retrace.rs
git commit -m "feat(retrace): add Projection trait + effective_kinds substrate view"
```

### Task 2: Migrate `enabled_loadables` to a `ActiveLoadables` projection (fix compaction + mutation leaks)

**Files:**
- Modify: `gait-rs/src/session/retrace.rs:340-359` (the existing `enabled_loadables`)
- Test: same file

Today `enabled_loadables` reads **raw** events and is called over a **post-compaction** slice (retrace.rs:390, turn.rs:623). The projection folds the **mutation-applied, full-session** stream, fixing both.

- [ ] **Step 1: Write the failing test** (consistency: undo of a `load` disables it)

```rust
#[test]
fn loadables_projection_honors_undo_and_full_session() {
    use crate::proto::{events, wire};
    use std::collections::HashSet;
    let load = wire::mint(events::tool_call("tc1".into(), "load".into(),
        &serde_json::json!({"name":"slack","enabled":true})), &[], 1);
    // A compaction AFTER the load must NOT reset the enabled set (full-session).
    let comp = wire::mint(events::compaction("summary".into()), &[], 2);
    let all = vec![load.clone(), comp];
    let p = ActiveLoadables { seed: HashSet::new() };
    let muts = HashMap::new();
    let got = run_projection(&p, &all, &muts);
    assert!(got.contains("slack"), "load before compaction must stay enabled (full session)");

    // Now redact the load → projection must drop it (honors mutations).
    let mut muts2 = HashMap::new();
    muts2.insert(load.id.clone(), MutationState::Redacted { reason: None });
    let got2 = run_projection(&p, &all, &muts2);
    assert!(!got2.contains("slack"), "redacted/undone load must be disabled");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai loadables_projection_honors_undo 2>&1 > /tmp/t.txt; grep -E "error|cannot find" /tmp/t.txt | head`
Expected: FAIL — `cannot find type ActiveLoadables` / `run_projection`.

- [ ] **Step 3: Add `run_projection` test helper + the `ActiveLoadables` projection**

Add a single-projection driver (used by tests and `fold`) below the trait:

```rust
/// Drive one projection over the mutation-applied full-session stream.
pub(crate) fn run_projection<P: Projection>(
    p: &P, all_events: &[Event], mutations: &HashMap<String, MutationState>,
) -> P::State {
    let mut state = p.seed();
    for (ev, kind) in effective_kinds(all_events, mutations) {
        p.apply(&mut state, ev, kind);
    }
    p.finish(state)
}

/// Active loadable names. Seeds from config defaults; replays `load(name, enabled)`
/// tool calls as last-write-wins toggles. Folds the FULL session (compaction does
/// not reset it) and honors mutations (an undone load disables).
pub(crate) struct ActiveLoadables {
    pub seed: std::collections::HashSet<String>,
}
impl Projection for ActiveLoadables {
    type State = std::collections::HashSet<String>;
    fn seed(&self) -> Self::State { self.seed.clone() }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::ToolCall(tc) = kind {
            if tc.name == "load" {
                let input = struct_to_value(&tc.input);
                let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let on = input.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                if on { state.insert(name); } else { state.remove(&name); }
            }
        }
    }
}
```

Then make the old free function delegate so existing callers keep working until Task 5 rewires them:

```rust
/// Deprecated shim — prefer the `ActiveLoadables` projection via `fold`. Kept so the
/// run_turn callsite compiles until Task 5 switches it to RetraceResult.
pub fn enabled_loadables(
    events: &[Event],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    run_projection(&ActiveLoadables { seed: config_enabled.clone() }, events, &HashMap::new())
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gait-ai loadables_projection 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS (and the existing `enabled_loadables_seeds_from_config_and_replays_toggles` still passes).

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/session/retrace.rs
git commit -m "feat(retrace): ActiveLoadables projection — full-session, mutation-aware"
```

### Task 3: Migrate `active_manifest` to an `ActiveManifest` projection (LWW register)

**Files:**
- Modify: `gait-rs/src/session/retrace.rs:401-410`
- Test: same file

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn active_manifest_is_last_write_wins() {
    use crate::proto::{events, wire};
    let r1 = wire::mint(wire::body(payload::Kind::Manifest(pb::Manifest {
        model: "m1".into(), ..Default::default() })), &[], 1);
    let r2 = wire::mint(wire::body(payload::Kind::Manifest(pb::Manifest {
        model: "m2".into(), ..Default::default() })), &[], 2);
    let got = run_projection(&ActiveManifest, &vec![r1, r2], &HashMap::new());
    assert_eq!(got.unwrap().model, "m2", "latest Manifest wins");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai active_manifest_is_last 2>&1 > /tmp/t.txt; grep -E "error|cannot find" /tmp/t.txt | head`
Expected: FAIL — `cannot find value ActiveManifest` (the unit struct).

- [ ] **Step 3: Add the `ActiveManifest` projection**

```rust
/// Last-write-wins register over `Manifest` events.
pub(crate) struct ActiveManifest;
impl Projection for ActiveManifest {
    type State = Option<pb::Manifest>;
    fn seed(&self) -> Self::State { None }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::Manifest(r) = kind { *state = Some(r.clone()); }
    }
}
```

Keep the existing `active_manifest` free function as a one-line shim delegating to `run_projection(&ActiveManifest, events, &HashMap::new())` so other callers are untouched.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gait-ai active_manifest 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/session/retrace.rs
git commit -m "feat(retrace): ActiveManifest projection (LWW register)"
```

### Task 4: Rebuild `fold` as a single pass over all projections

**Files:**
- Modify: `gait-rs/src/session/retrace.rs:378-395` (the `fold` body) and `RetraceResult` (retrace.rs:362-372)
- Test: same file

`ActiveMessages` becomes a `Projection` too (`State = Vec<Message>`), so *everything* goes through one trait. Port the per-event match arms from today's `messages_applying` (retrace.rs:74-250) into `ActiveMessages::apply`, with **one change**: instead of pre-slicing `all_events[last_compaction..]`, the `Compaction` arm `state.clear()`s the accumulated messages and then pushes the summary message. This makes compaction "restart the message context, carry state forward" literal — `ActiveMessages` resets on the event; state projections never see it as special. `fold` then drives all four projections in a single pass.

```rust
/// The Transcript: the Vec<Message> the model reads. Resets on a Compaction event
/// (restart context from the summary); otherwise ports the existing per-event
/// rendering from messages_applying. Honors mutations via the effective stream.
pub(crate) struct ActiveMessages;
impl Projection for ActiveMessages {
    type State = Vec<Message>;
    fn seed(&self) -> Self::State { vec![] }
    fn apply(&self, msgs: &mut Self::State, ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::Compaction(c) => {
                msgs.clear(); // restart message context (state projections carry forward)
                msgs.push(Message::User { content: MessageContent::Text(
                    format!("[Session history summary: {}]", c.summary)) });
            }
            // ... port the remaining arms verbatim from messages_applying (UserTurn,
            // AssistantText, ToolCall, ToolResult, Clarify/Inform, AgentUpdate, …),
            // pushing into `msgs`. SessionStart/Manifest/etc. are ignored here.
            _ => { /* ported arms */ }
        }
        let _ = ev;
    }
}
```

- [ ] **Step 1: Write the failing test** (one pass yields the same results as the individual fns)

```rust
#[test]
fn fold_runs_all_projections_consistently() {
    use crate::proto::{events, wire};
    use std::collections::HashSet;
    let load = wire::mint(events::tool_call("tc1".into(), "load".into(),
        &serde_json::json!({"name":"slack","enabled":true})), &[], 1);
    let all = vec![load];
    let res = fold(&all, &HashSet::new()).unwrap();
    assert!(res.enabled_loadables.contains("slack"));
    assert!(res.effective_allow.is_empty(), "no permission grants yet");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai fold_runs_all_projections 2>&1 > /tmp/t.txt; grep -E "error|no field" /tmp/t.txt | head`
Expected: FAIL — `no field effective_allow on RetraceResult`.

- [ ] **Step 3: Add the field + rewrite `fold`**

Add to `RetraceResult` (retrace.rs:362):

```rust
    /// Effective session allow-list: config baseline + log-derived once/session
    /// grants. (Once = since last TurnComplete; Session = until undone.)
    pub effective_allow: std::collections::HashSet<String>,
```

Rewrite `fold` (the seed for `ActivePermissions` is added in Task 6; until then pass an empty set):

```rust
pub fn fold(
    all_events: &[Event],
    config_enabled: &std::collections::HashSet<String>,
) -> anyhow::Result<RetraceResult> {
    // Phase 0 — substrate: compute the converged mutation set (LWW + cascade).
    let mut mutations = active_mutations(all_events);
    apply_cascade(&mut mutations, all_events);

    // Phase 1 — state projections fold the SAME mutation-applied full session.
    let enabled_loadables = run_projection(&ActiveLoadables { seed: config_enabled.clone() }, all_events, &mutations);
    let active_manifest = run_projection(&ActiveManifest, all_events, &mutations);
    let effective_allow   = run_projection(&ActivePermissions { seed: Default::default() }, all_events, &mutations);

    // ActiveMessages is a projection too — it resets on Compaction (compaction handled
    // here only); all four fold the SAME mutation-applied stream.
    let messages = run_projection(&ActiveMessages, all_events, &mutations);

    Ok(RetraceResult { messages, enabled_loadables, active_manifest, effective_allow, mutations })
}
```

Add a temporary stub `ActivePermissions` (fully implemented in Task 6) so this compiles:

```rust
pub(crate) struct ActivePermissions { pub seed: std::collections::HashSet<String> }
impl Projection for ActivePermissions {
    type State = std::collections::HashSet<String>;
    fn seed(&self) -> Self::State { self.seed.clone() }
    fn apply(&self, _s: &mut Self::State, _e: &Event, _k: &payload::Kind) {}
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gait-ai fold_runs_all_projections 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/session/retrace.rs
git commit -m "feat(retrace): single-pass fold over all projections + effective_allow field"
```

### Task 5: Switch `run_turn` to fold loadables over the full session

**Files:**
- Modify: `gait-rs/src/turn.rs:623` (the `enabled_loadables(&post_compaction, …)` call)
- Test: existing `cargo test -p gait-ai` suite (no loadable should reset at compaction)

- [ ] **Step 1: Write the failing test** in `gait-rs/tests/integration_turn.rs` — a `load` in turn 1 stays enabled in turn 3 even across a compaction. (Use the existing `ScriptedProvider` harness; script turn 1 to call `load`, inject a `Compaction` event, assert the turn-3 system prompt's `<loadable-index>` shows `enabled=true`.) If a compaction fixture is impractical at integration level, assert via `retrace::fold` directly on a hand-built event vector as in Task 2 — but place it in the integration file to lock the run_turn contract.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai loadable_survives_compaction 2>&1 > /tmp/t.txt; grep -E "test result|assert" /tmp/t.txt | head`
Expected: FAIL — loadable resets after compaction (old `post_compaction` behavior).

- [ ] **Step 3: Replace the callsite**

At turn.rs:623, replace:

```rust
let enabled_names = retrace::enabled_loadables(&post_compaction, &config_enabled);
```

with a fold over the full event list (the same `pre_events`/`post_compaction` source already read in run_turn — use the full, not the sliced, list):

```rust
// Full-session, mutation-aware (compaction no longer resets loadable state).
let enabled_names = {
    let muts = { let mut m = retrace::active_mutations(&pre_events);
                 retrace::apply_cascade(&mut m, &pre_events); m };
    retrace::run_projection(&retrace::ActiveLoadables { seed: config_enabled.clone() }, &pre_events, &muts)
};
```

(`pre_events` is the full event vector read earlier in run_turn; confirm the variable name at the callsite and use the un-sliced list. Do NOT use `post_compaction`.)

- [ ] **Step 4: Run to verify it passes + full suite green**

Run: `cargo test -p gait-ai 2>&1 > /tmp/t.txt; grep -E "test result|FAILED" /tmp/t.txt`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/turn.rs gait-rs/tests/integration_turn.rs
git commit -m "fix(turn): loadables fold full session (compaction no longer resets them)"
```

---

## Phase 2 — Permission projection: session scope + log-derived once

### Task 6: Implement the `ActivePermissions` projection (once + session lifetimes)

**Files:**
- Modify: `gait-rs/src/session/retrace.rs` (replace the Task-4 stub `ActivePermissions`)
- Test: same file

Folds `PermissionsReply` events. Each `PermissionsReplyItem` has `decision` (`"approve"`/`"deny"`), `text` (the pattern), and `scope` (`"once"|"session"|"agent"|"global"`). Lifetime rules:
- **session**: approve → insert pattern; persists for the whole session. **deny → remove pattern** (the honest forward revoke — both the approve and the deny stay in the ledger; only the derived set changes). Undo/redact of the granting turn ALSO drops it (projection folds the mutation-applied stream), but that is for mistakes, not normal revocation.
- **once**: approve → active only if no `TurnComplete` has occurred *after* this event. Implement by clearing the once-set whenever a `TurnComplete` is folded.
- **agent/global**: ignored here — they are written to config files and already present in the seed.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn effective_allow_session_persists_once_expires_on_turn_complete() {
    use crate::proto::{events, wire};
    let sess = wire::mint(events::permissions_reply(&[tx::PermissionsReplyItem{
        id: "1".into(), decision: "approve".into(),
        text: Some("shell:cargo test".into()), scope: Some("session".into()) }]), &[], 1);
    let once = wire::mint(events::permissions_reply(&[tx::PermissionsReplyItem{
        id: "2".into(), decision: "approve".into(),
        text: Some("shell:ls".into()), scope: Some("once".into()) }]), &[], 2);
    let done = wire::mint(events::turn_complete(/* minimal TurnComplete */), &[], 3);
    let all = vec![sess, once, done];
    let st = run_projection(&ActivePermissions { seed: Default::default() }, &all, &HashMap::new());
    assert!(st.session.contains("shell:cargo test"), "session grant persists past TurnComplete");
    assert!(!st.once.contains("shell:ls"), "once grant expires at TurnComplete");
}

#[test]
fn effective_allow_session_deny_revokes_but_ledger_keeps_both() {
    use crate::proto::{events, wire};
    let approve = wire::mint(events::permissions_reply(&[tx::PermissionsReplyItem{
        id: "1".into(), decision: "approve".into(),
        text: Some("shell:rm".into()), scope: Some("session".into()) }]), &[], 1);
    let deny = wire::mint(events::permissions_reply(&[tx::PermissionsReplyItem{
        id: "2".into(), decision: "deny".into(),
        text: Some("shell:rm".into()), scope: Some("session".into()) }]), &[], 2);
    let all = vec![approve, deny];
    // Ledger: BOTH events remain in the Log; only the derived set drops the pattern.
    assert_eq!(all.len(), 2, "append-only — allow and deny both stay");
    let st = run_projection(&ActivePermissions { seed: Default::default() }, &all, &HashMap::new());
    assert!(!st.session.contains("shell:rm"), "forward deny revokes from the derived set");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai effective_allow_session_persists 2>&1 > /tmp/t.txt; grep -E "error|assert|test result" /tmp/t.txt | head`
Expected: FAIL — stub returns empty.

- [ ] **Step 3: Implement the projection**

Replace the Task-4 stub:

```rust
/// Effective session allow-list. Seeds from config (global+agent, already loaded
/// into `allow`). Folds `PermissionsReply` grants as an append-only ledger:
/// `session` approve inserts and `session` deny removes (the honest forward revoke
/// — both events remain in the Log; only the derived set changes); `once` approve
/// is active only until the next `TurnComplete`. `agent`/`global` scopes are
/// config-file backed and ignored here. Because this folds the mutation-applied
/// stream, undo/redact of a grant ALSO drops it — but redact is for mistakes, not
/// normal revocation.
pub(crate) struct ActivePermissions { pub seed: std::collections::HashSet<String> }
#[derive(Default)]
pub(crate) struct AllowState {
    session: std::collections::HashSet<String>,
    once: std::collections::HashSet<String>,
}
impl Projection for ActivePermissions {
    type State = AllowState;
    fn seed(&self) -> Self::State {
        AllowState { session: self.seed.clone(), once: Default::default() }
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::TurnComplete(_) => state.once.clear(), // once grants expire per turn
            payload::Kind::PermissionsReply(r) => {
                for item in &r.items {
                    let Some(pat) = item.text.clone() else { continue; };
                    match (item.scope.as_deref(), item.decision.as_str()) {
                        (Some("session"), "approve") => { state.session.insert(pat); }
                        (Some("session"), "deny")    => { state.session.remove(&pat); } // forward revoke
                        (Some("once"), "approve")    => { state.once.insert(pat); }
                        _ => {} // agent/global via config seed; deny-once is a no-op
                    }
                }
            }
            _ => {}
        }
    }
    fn finish(&self, state: Self::State) -> Self::State { state }
}
```

Because `ActivePermissions::State` is `AllowState` (not a flat set), adjust the `fold` callsite from Task 4 to flatten:

```rust
let allow_state = run_projection(&ActivePermissions { seed: Default::default() }, all_events, &mutations);
let effective_allow: std::collections::HashSet<String> =
    allow_state.session.union(&allow_state.once).cloned().collect();
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gait-ai effective_allow_session_persists 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/session/retrace.rs
git commit -m "feat(retrace): ActivePermissions projection — session persists, once expires per turn"
```

### Task 7: Add `SCOPE_SESSION` to the proto

**Files:**
- Modify: `proto/log.proto:248` (the `Scope` enum)
- Test: a round-trip assertion in `gait-rs/src/proto/tests.rs`

- [ ] **Step 1: Write the failing test** in `gait-rs/src/proto/tests.rs`

```rust
#[test]
fn scope_session_variant_exists() {
    use crate::proto::gait::log::v1::Scope;
    assert_eq!(Scope::Session as i32, 4);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai scope_session_variant 2>&1 > /tmp/t.txt; grep -E "error|no variant" /tmp/t.txt | head`
Expected: FAIL — `no variant named Session`.

- [ ] **Step 3: Edit the proto**

`proto/log.proto:248`, change:

```proto
enum Scope    { SCOPE_UNSPECIFIED = 0; SCOPE_ONCE = 1; SCOPE_AGENT = 2; SCOPE_GLOBAL = 3; }
```
to:
```proto
enum Scope    { SCOPE_UNSPECIFIED = 0; SCOPE_ONCE = 1; SCOPE_AGENT = 2; SCOPE_GLOBAL = 3; SCOPE_SESSION = 4; }
```

- [ ] **Step 4: Run to verify it passes** (build regenerates proto via `build.rs`)

Run: `cargo test -p gait-ai scope_session_variant 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add proto/log.proto gait-rs/src/proto/tests.rs
git commit -m "feat(proto): add SCOPE_SESSION to Scope enum"
```

### Task 8: Add `PermissionChoice::Session` and stop writing the once temp file

**Files:**
- Modify: `gait-rs/src/permission.rs` (enum at line 8; `save_choice` at lines 36-55)
- Test: same file

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn session_choice_writes_no_file_and_once_is_log_only() {
    let dir = tempfile::TempDir::new().unwrap();
    // Session: no config file, no temp file — it is recorded as a log event by chat.rs.
    save_choice(&PermissionChoice::Session("shell:ls".into()), dir.path(), "s1", None).unwrap();
    assert!(!crate::turn::once_approval_path(dir.path(), "s1").exists(),
        "Session must not write the once temp file");
    // Once: also no temp file anymore (now derived from the PermissionsReply log event).
    save_choice(&PermissionChoice::Once("shell:ls".into()), dir.path(), "s1", None).unwrap();
    assert!(!crate::turn::once_approval_path(dir.path(), "s1").exists(),
        "Once must no longer write a temp file");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai session_choice_writes_no_file 2>&1 > /tmp/t.txt; grep -E "error|no variant|assert" /tmp/t.txt | head`
Expected: FAIL — `no variant named Session` and/or the `Once` temp file still written.

- [ ] **Step 3: Add the variant; make `Once`/`Session` log-only**

In the `PermissionChoice` enum (permission.rs:8), add after `Once(String)`:

```rust
    /// Approve for the rest of this session (log-derived; no config file).
    Session(String),
```

In `save_choice` (permission.rs:36-55), change the `Once` arm to a no-op (the grant is already recorded as a `PermissionsReply{scope:"once"}` event by chat.rs) and add a `Session` no-op arm:

```rust
        PermissionChoice::Once(_pattern) => {
            // No side effect — derived from the PermissionsReply(once) log event.
        }
        PermissionChoice::Session(_pattern) => {
            // No side effect — derived from the PermissionsReply(session) log event.
        }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gait-ai session_choice_writes_no_file 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS. (`once_approval_path` still exists as a function — deleted in Task 10.)

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/permission.rs
git commit -m "feat(permission): add Session choice; Once/Session are log-only (no temp file)"
```

### Task 9: REPL prompt — add `[s] session` and wire the reply item

**Files:**
- Modify: `gait-rs/src/ui/permission.rs:21,46-49` (prompt text + parser)
- Modify: `gait-rs/src/chat.rs:310` (the `match &choice` arm)
- Test: `gait-rs/src/ui/permission.rs` parser test

- [ ] **Step 1: Write the failing test** in `gait-rs/src/ui/permission.rs`

```rust
#[test]
fn parse_permission_response_session() {
    let patterns = vec!["shell:ls".to_string()];
    assert_eq!(
        parse_permission_response("s", &patterns),
        Some(crate::permission::PermissionChoice::Session("shell:ls".to_string()))
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai parse_permission_response_session 2>&1 > /tmp/t.txt; grep -E "error|no variant|assert" /tmp/t.txt | head`
Expected: FAIL.

- [ ] **Step 3: Add the prompt line + parser arm + chat reply**

In `ui/permission.rs`, the prompt (line 21) becomes:

```rust
    eprintln!("  {}[o] once   [s] session   [r] reject{}", DIM_FG, RESET);
```

Add to the parser `match` (near line 46), mirroring the `"o" | "once"` arm:

```rust
        "s" | "session" => {
            let pat = patterns.first().cloned().unwrap_or_default();
            return Some(PermissionChoice::Session(pat));
        }
```

In `chat.rs` (the `match &choice` block at line 310), add an arm alongside `Once`:

```rust
                    crate::permission::PermissionChoice::Session(pat) => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                            scope: Some("session".into()),
                        });
                        any_approved = true;
                    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p gait-ai parse_permission_response_session 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/ui/permission.rs gait-rs/src/chat.rs
git commit -m "feat(repl): [s] session permission option → PermissionsReply{scope:session}"
```

### Task 10: Switch `run_turn` to the `effective_allow` projection; delete the temp file

**Files:**
- Modify: `gait-rs/src/turn.rs:63` (delete `once_approval_path`), `:464-473` (delete temp-file read), and the `effective_allow` construction
- Test: `gait-rs/tests/integration_turn.rs` — a once grant approves exactly one turn; a session grant approves subsequent turns

- [ ] **Step 1: Write the failing test** (integration): script a turn that triggers a permission, write a `PermissionsReply{scope:"session"}` event, then a *new* turn for the same tool runs auto-approved (no re-prompt). And a `scope:"once"` grant does NOT auto-approve a turn after a `TurnComplete`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p gait-ai session_grant_approves_next_turn 2>&1 > /tmp/t.txt; grep -E "test result|assert" /tmp/t.txt | head`
Expected: FAIL — effective_allow still sourced from the temp file (session grant ignored).

- [ ] **Step 3: Rewire run_turn**

Delete `once_approval_path` (turn.rs:63). Replace the temp-file block (turn.rs:464-473):

```rust
    let once_path = once_approval_path(&gait_dir, &session_id);
    let once_allow: Vec<String> = std::fs::read_to_string(&once_path)... ;
    let _ = std::fs::remove_file(&once_path);
    let mut effective_allow = allow.clone();
    effective_allow.extend(once_allow);
```

with a fold of the full session, seeded by config `allow`:

```rust
    // Effective allow = config baseline (global+agent) + log-derived session/once
    // grants, mutation-aware. Replaces the once-approval temp file entirely.
    let mut effective_allow: std::collections::HashSet<String> = allow.iter().cloned().collect();
    {
        let muts = { let mut m = retrace::active_mutations(&pre_events);
                     retrace::apply_cascade(&mut m, &pre_events); m };
        let st = retrace::run_projection(
            &retrace::ActivePermissions { seed: effective_allow.clone() }, &pre_events, &muts);
        effective_allow.extend(st.session);
        effective_allow.extend(st.once);
    }
    let mut effective_allow: Vec<String> = effective_allow.into_iter().collect();
```

(Keep `effective_allow` a `Vec<String>` if downstream code at turn.rs:640/799 expects a Vec; the final `let mut` rebinds to a Vec. Confirm and match the existing type at those callsites.)

- [ ] **Step 4: Run to verify it passes + full suite**

Run: `cargo test -p gait-ai 2>&1 > /tmp/t.txt; grep -E "test result|FAILED" /tmp/t.txt`
Expected: all PASS. Confirm no remaining references: `grep -rn once_approval_path gait-rs/src` returns nothing.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/turn.rs gait-rs/tests/integration_turn.rs
git commit -m "feat(turn): effective_allow from projection; delete once-approval temp file"
```

---

## Self-Review

**Spec coverage:**
- Rename Log file `transcript.jsonl` → `log.jsonl` (vocabulary fix) → Task 0. ✓
- Projection trait + one consistent input → Task 1. ✓
- ActiveLoadables consistency (full-session + mutation-aware) → Task 2 + Task 5. ✓
- ActiveManifest as projection → Task 3. ✓
- `ActiveMessages` is also a projection (resets on Compaction) + single-pass `fold` → Task 4. ✓
- `ActivePermissions` projection (renamed from EffectiveAllow), session-scoped allow (F1) → Tasks 6–10. ✓
- Wrap `once` into the log (delete temp file) → Tasks 6, 8, 10. ✓
- Compaction is its own boundary signal — handled only by `ActiveMessages` (reset), ignored by state projections → Tasks 2, 4, 5. ✓
- Forward-only permission; revoke = a forward `deny` ledger entry (append-only — allow+deny both remain), redact reserved for mistakes → Task 6 folds session approve/deny; both honored by the mutation-applied stream. ✓

**Type consistency:** `Projection::apply(&self, &mut State, &Event, &payload::Kind)` is used identically in Tasks 1–6. `ActivePermissions::State = AllowState { session, once }` defined in Task 6 and flattened in Task 4's `fold` and Task 10's run_turn (both `union`/`extend` the two sets). `enabled_loadables`/`active_manifest` shims (Tasks 2–3) keep external callers compiling until rewired. The public `messages()` / `messages_from_path()` (retrace.rs:50-57) must be updated in Task 4 to delegate to `run_projection(&ActiveMessages, …)` so `ingestor.rs::retrace` and the integration tests keep working.

**Open verification for the implementer (not placeholders — confirm against live code):**
1. The exact variable name of the full event vector in `run_turn` (the plan calls it `pre_events`; confirm at turn.rs:~478 and use the **un-sliced** list, never `post_compaction`).
2. Whether `effective_allow` downstream (turn.rs:640, :799) is `Vec<String>` or a set — match it in Task 10.
3. The exact constructor names `events::permissions_reply`, `events::turn_complete`, `events::redact`, `events::compaction`, `wire::mint`, `wire::body` (used in tests) — confirm signatures in `src/proto/events.rs` / `src/proto/wire.rs` and adjust test setup to match.
4. Porting `messages_applying` into `ActiveMessages::apply`: the current fn returns `anyhow::Result` but its body is effectively infallible (uses `.unwrap_or(...)`). Make `ActiveMessages::apply` infallible (default on any parse miss, as today), so `fold`/`messages()` no longer need `?`. Confirm no arm genuinely errors before dropping the `Result`.

---

## Execution Handoff

Two phases, ten tasks. Phase 1 (Tasks 1–5) is independently shippable (the trait + consistency fixes); Phase 2 (Tasks 6–10) adds the permission projection and removes the temp-file hack.
