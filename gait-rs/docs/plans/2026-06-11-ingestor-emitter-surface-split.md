# Ingestor / Emitter / StreamItem Surface Split

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **STATUS: ✅ COMPLETE — merged to `main` 2026-06-11.** All 8 tasks (1–7b) implemented via subagent-driven development with spec + code-quality review on each, merged in `a6a2f92`. A follow-up `Emitter::for_session` constructor (deduping transcript-path construction at 7 sites) merged in `3880662`. 475 tests green. **One item intentionally deferred:** streaming the separate `gait workflow` surface — Task 6 was re-scoped to leave `wire::emit` in place for it (see the re-scope note under Task 6). That work is to be redone separately.

**Goal:** Replace the implicit stdout coupling in `wire::emit()` with two distinct types — `Ingestor` (read-only transcript/retrace) and `Emitter` (write-only: transcript + an in-process channel of `StreamItem`). Rewire `run_turn()` and the REPL to use them, finish the half-done proto migration in the renderer, and eliminate the subprocess self-spawn in `chat.rs`.

**Architecture:**
```
JSONL transcript (filesystem, proto Event per line, append-only)
  ↑ Ingestor reads → retrace::messages() → Vec<Message>   [read only]
  ↓ Emitter writes durable events                          [write only]

gait-core: run_turn(session, &Emitter, caps, input) → TurnResult
  Emitter emits onto an in-process channel of StreamItem:
    emit(body)   → write Event to transcript + send StreamItem{event}   (durable)
    delta(text)  → send StreamItem{delta} only                          (ephemeral)

StreamItem (proto oneof):
    Event     event   — durable log event, full envelope (version/id/seq/hash/parents)
    TextDelta delta   — ephemeral live-typing fragment, no envelope, never persisted

CLI surface (gait run):  recv StreamItem → unwrap → NDJSON stdout
    event → serialize inner Event (current proto format)
    delta → flat {"type":"text_delta",...} line (current legacy format)
    ⇒ external stdout API byte-identical to today

REPL surface (chat.rs): recv StreamItem in-process →
    event → Renderer::handle_event(&Event)
    delta → Renderer text append
    ⇒ no subprocess, no JSONL re-parse
```

**Why `StreamItem` is proto, not a Rust enum:** the live stream must also reach cross-language consumers (FFI/gRPC) — a Rust-only wrapper can't cross that boundary. Modeling the wrapper as a proto oneof keeps the entire contract proto end-to-end. prost generates a Rust enum (`Option<stream_item::Item>`) so in-process ergonomics are identical to a hand-rolled enum, but it also encodes to bytes.

**Why one ordered channel (not two):** in gait, text deltas and structured events are points on ONE timeline (stream text → call tool → stream text → ask permission). They must be totally ordered, which a single channel guarantees and two `select!`'d channels cannot. (Archegraph uses two `StreamSink`s because its text and event streams are orthogonal concerns; gait's share a timeline — different situation, different answer.)

**Pre-existing bug this fixes:** the proto migration (commits "phase 2.x/3") converted the emit/transcript side fully to proto, but the renderer was never migrated — `handle_line` still reads the legacy flat `{"type":...}` format and silently drops every proto event (`tool_call`, `permissions`, `clarify`, `inform`, `turn_complete`) into its `_ => Continue` arm. Only the surviving flat `text_delta` `println!`s still render. So the REPL currently shows streaming text but no tool indicators / permission prompts (masked in daily use by allow-lists). Task 7a fixes this as its own verifiable commit before the transport swap.

**Tech stack:** Rust, `tokio::sync::mpsc::unbounded_channel` (sync non-blocking `send()`, async/blocking `recv()`), prost + pbjson proto types, existing `retrace` + `wire` modules.

**Reference implementation:** Proven in `/Users/aicoder/Documents/archegraph`. `crates/archegraph-watch` = daemonless core emitting proto on tokio mpsc streams. `crates/archescope-native` = FFI bridge to Dart/Flutter via `flutter_rust_bridge` `StreamSink<Vec<u8>>`. `crates/archegraph-stream` = Tonic gRPC server wrapping the *same* daemonless core. Our Emitter ≈ archegraph-watch; a future gait FFI crate ≈ archescope-native; a future Serverpod/gRPC wrapper ≈ archegraph-stream. **The core never grows a server — wrapper crates do.**

---

### Task 1: Define `Ingestor` (read-only transcript handle)

**Files:**
- Create: `src/session/ingestor.rs`
- Modify: `src/session/mod.rs` — add `pub mod ingestor; pub use ingestor::Ingestor;`
- Modify: `src/lib.rs` — add `pub use session::Ingestor;`

The `Ingestor` is a read-only handle to a transcript file: it returns events and retraced messages, nothing else. It has NO emit capability — the type enforces the read/write boundary.

- [x] **Step 1: Write `src/session/ingestor.rs`**

```rust
use std::path::{Path, PathBuf};
use crate::proto::gait::log::v1::Event;
use crate::session::{retrace, transcript};
use crate::types::Message;

pub struct Ingestor {
    pub path: PathBuf,
}

impl Ingestor {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_session(gait_dir: &Path, session_id: &str) -> Self {
        Self::new(
            gait_dir.join("sessions").join(session_id).join("transcript.jsonl"),
        )
    }

    /// Read all events from the transcript file.
    pub fn events(&self) -> anyhow::Result<Vec<Event>> {
        transcript::read_events(&self.path)
    }

    /// Reconstruct the LLM message history from the transcript.
    pub fn retrace(&self) -> anyhow::Result<Vec<Message>> {
        retrace::messages_from_path(&self.path)
    }
}
```
(Verified: `retrace::messages_from_path` exists at retrace.rs:54; `transcript::read_events` at transcript.rs.)

- [x] **Step 2: Export from `src/session/mod.rs`** — add after `pub mod retrace;`:
```rust
pub mod ingestor;
pub use ingestor::Ingestor;
```

- [x] **Step 3: Re-export from `src/lib.rs`** — add `pub use session::Ingestor;`

- [x] **Step 4: Verify build**
```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
```
Expected: exit 0.

- [x] **Step 5: Tests in `src/session/ingestor.rs`**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn from_session_builds_correct_path() {
        let dir = tempdir().unwrap();
        let ingestor = Ingestor::from_session(dir.path(), "s1234567");
        assert_eq!(ingestor.path, dir.path().join("sessions/s1234567/transcript.jsonl"));
    }

    #[test]
    fn events_empty_for_missing_file() {
        let dir = tempdir().unwrap();
        let ingestor = Ingestor::from_session(dir.path(), "s0000000");
        assert!(ingestor.events().unwrap_or_default().is_empty());
    }
}
```

- [x] **Step 6: Run tests**
```bash
cargo test session::ingestor 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

- [x] **Step 7: Commit**
```bash
git add src/session/ingestor.rs src/session/mod.rs src/lib.rs
git commit -m "feat: add Ingestor — read-only transcript handle for retrace"
```

---

### Task 2: Add `StreamItem` + `TextDelta` to the proto schema

**Files:**
- Modify: `/Users/aicoder/Documents/ideation/gait/proto/log.proto` — the proto source is a SIBLING of gait-rs, not inside it (`build.rs:30` uses `PathBuf::from("../proto")`). All paths below use the absolute path.
- Modify: `src/proto/mod.rs` only if explicit re-exports are needed (check how existing messages are surfaced)

`StreamItem` is the live-stream envelope: a oneof of a durable `Event` or an ephemeral `TextDelta`. **No `version` field** (ephemeral, never persisted; proto oneof evolution gives forward-compat). `Event` keeps its existing `version`.

- [x] **Step 1: Read the current proto to match style/package**
```bash
sed -n '1,40p' /Users/aicoder/Documents/ideation/gait/proto/log.proto
grep -n "message Event\|message Payload\|oneof" /Users/aicoder/Documents/ideation/gait/proto/log.proto | head
```

- [x] **Step 2: Add the two messages to `/Users/aicoder/Documents/ideation/gait/proto/log.proto`** (in the same package as `Event`, after `Event`):
```proto
// Ephemeral live-typing fragment. NOT a log event — no envelope, never persisted.
message TextDelta {
  string content = 1;
}

// Live-stream envelope. Carries either a durable log Event or an ephemeral
// TextDelta. Never persisted; produced+consumed within a single session.
message StreamItem {
  oneof item {
    Event     event = 1;
    TextDelta delta = 2;
  }
}
```

- [x] **Step 3: Build to regenerate prost types**
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
```
Expected: exit 0. The generated types are `gait::log::v1::{StreamItem, TextDelta, stream_item::Item}`.

- [x] **Step 4: Confirm the generated path with a throwaway reference**

Temporarily add to any test or `main`:
```rust
let _ = crate::proto::gait::log::v1::StreamItem {
    item: Some(crate::proto::gait::log::v1::stream_item::Item::Delta(
        crate::proto::gait::log::v1::TextDelta { content: "x".into() },
    )),
};
```
Build, confirm it compiles, then remove it. This pins the exact generated names for later tasks. (If the package path differs from `gait::log::v1`, use whatever the existing `Event` import uses — grep `gait::log::v1::Event` in the codebase.)

- [x] **Step 5: Commit**
```bash
git add ../proto/log.proto
git commit -m "feat(proto): add StreamItem (Event|TextDelta) live-stream envelope"
```
(The proto lives outside gait-rs; `git add` it via its repo-relative path.)

---

### Task 3: Define `Emitter` (write-only: transcript + StreamItem channel)

**Files:**
- Create: `src/proto/emitter.rs`
- Modify: `src/proto/wire.rs` — extract `write_to_transcript`; keep `append`; leave `emit` (with its println) intact for now so existing callers compile
- Modify: `src/proto/mod.rs` — `pub mod emitter; pub use emitter::Emitter;`
- Modify: `src/lib.rs` — `pub use proto::Emitter;`

`Emitter::emit()` always writes the transcript AND sends `StreamItem{event}`. `Emitter::delta()` sends `StreamItem{delta}` only (no transcript write — deltas are ephemeral). The type has NO retrace method.

- [x] **Step 1: Extract `write_to_transcript` in `src/proto/wire.rs`**

The current `emit` (wire.rs:77) and `append` (wire.rs:61) duplicate the mint+write logic. Extract it:
```rust
pub(crate) fn write_to_transcript(path: &Path, body: event::Body) -> anyhow::Result<Event> {
    let prev = last_event(path)?;
    let ev = mint(body, prev.as_ref());
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let json = serde_json::to_string(&ev)?;
    writeln!(file, "{}", json)?;
    Ok(ev)
}
```
Rewrite `append` to call it. Leave `emit` as `write_to_transcript(...)` + `println!("{}", serde_json::to_string(&ev)?)` so current callers keep working until Task 4/5/6 remove them.

- [x] **Step 2: Write `src/proto/emitter.rs`**
```rust
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;
use crate::proto::{wire, gait::log::v1::{Event, event, StreamItem, TextDelta, stream_item}};

pub struct Emitter {
    pub tx_path: PathBuf,
    sender: UnboundedSender<StreamItem>,
}

impl Emitter {
    pub fn new(tx_path: PathBuf, sender: UnboundedSender<StreamItem>) -> Self {
        Self { tx_path, sender }
    }

    /// Durable: write Event to transcript AND send StreamItem{event}. Always both.
    /// `UnboundedSender::send` is sync + non-blocking — safe from anywhere.
    pub fn emit(&self, body: event::Body) -> anyhow::Result<Event> {
        let event = wire::write_to_transcript(&self.tx_path, body)?;
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Event(event.clone())),
        });
        Ok(event)
    }

    /// Ephemeral: send StreamItem{delta} only. No transcript write.
    pub fn delta(&self, content: String) {
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Delta(TextDelta { content })),
        });
    }
}
```

- [x] **Step 3: Export** — `src/proto/mod.rs`: `pub mod emitter; pub use emitter::Emitter;` ; `src/lib.rs`: `pub use proto::Emitter;`

- [x] **Step 4: Build** (existing `wire::emit` callers still compile)
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
```

- [x] **Step 5: Tests in `src/proto/emitter.rs`**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tempfile::tempdir;
    use crate::proto::gait::log::v1::stream_item::Item;

    fn make(dir: &std::path::Path) -> (Emitter, mpsc::UnboundedReceiver<StreamItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Emitter::new(dir.join("transcript.jsonl"), tx), rx)
    }

    #[test]
    fn emit_writes_file_and_sends_event() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        em.emit(crate::proto::events::session_start("s1234567".to_string(), None)).unwrap();
        assert!(em.tx_path.exists());
        match rx.try_recv().unwrap().item {
            Some(Item::Event(_)) => {}
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn delta_sends_only_channel_no_file() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        em.delta("hi".to_string());
        // delta does NOT write the transcript
        assert!(!em.tx_path.exists());
        match rx.try_recv().unwrap().item {
            Some(Item::Delta(d)) => assert_eq!(d.content, "hi"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }
}
```
(Verify `events::session_start` signature: `(session_id: String, assigned_by: Option<String>)` at events.rs:15 — adjust args if it differs.)

- [x] **Step 6: Run tests**
```bash
cargo test proto::emitter 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

- [x] **Step 7: Commit**
```bash
git add src/proto/emitter.rs src/proto/wire.rs src/proto/mod.rs src/lib.rs
git commit -m "feat: add Emitter — transcript+StreamItem channel; extract wire::write_to_transcript"
```

---

### Task 4: Rewire `run_turn()` to use `Emitter`; wire the CLI drain

**Files:**
- Modify: `src/turn.rs`
- Modify: `src/lib.rs` (signature of `run_turn` re-export unchanged in name, but callers change)
- Modify: `src/bin/gait.rs` (real CLI drain so `gait run` never breaks)
- Modify: `src/workflow/exec_prompt.rs` (placeholder)

`run_turn` gains `emitter: &Emitter`, builds an `Ingestor` internally, replaces every `wire::emit(&tx_path, …)` with `emitter.emit(…)`, and replaces the three flat `text_delta` `println!`s with `emitter.delta(…)`.

- [x] **Step 1: Update `run_turn` signature (turn.rs:393)**
```rust
pub async fn run_turn(
    session: &crate::session::Session,
    emitter: &crate::proto::Emitter,
    caps: crate::profile::ResolvedProfile,
    input: TurnInput,
) -> anyhow::Result<TurnResult>
```
Add imports: `use crate::proto::Emitter;` and `use crate::session::Ingestor;`

- [x] **Step 2: Build the Ingestor inside `run_turn`**

After `let session_id = …; let gait_dir = …;`:
```rust
let ingestor = Ingestor::from_session(&gait_dir, &session_id);
```

- [x] **Step 3: Replace every production `wire::emit(&tx_path, …)` with `emitter.emit(…)`**

Confirmed call sites in turn.rs (line numbers approximate — grep to confirm):
```bash
grep -n "wire::emit" src/turn.rs
```
Known: 542, 552, 557, 588, 600, 725, 745, 859, 862, 955, 958, 1039, 1042, 1063, 1076, 1083, 1135 (plus 188, 240 inside `execute_tool_call` — handled in Task 5). Replace each `wire::emit(&tx_path, X)?` → `emitter.emit(X)?`. **Keep the `tx_path` local binding** — it's still needed by (a) the `execute_tool_call` calls until Task 5, and (b) `ingestor` reads. `tx_path` and `emitter.tx_path` point at the same file, so this is consistent.

**KEEP `execute_tool_call` ON `tx_path` IN THIS TASK (compile-safety).** `execute_tool_call` still takes `tx_path: &Path` until Task 5. At its call sites (turn.rs:540, 911, 1010, 1089), pass `&emitter.tx_path` (the field is `pub`): `execute_tool_call(&tc, &tools, &emitter.tx_path, prefix)`. Its internal `wire::emit` (188/240) still does transcript+println during this task — the REPL subprocess reads those from stdout, so behavior is preserved. Task 5 switches it to `&Emitter`. This keeps Task 4's commit compiling and behaving.

- [x] **Step 4: Replace the three `text_delta` `println!`s with `emitter.delta`**

turn.rs:702, 1048, 1129 currently:
```rust
println!("{}", serde_json::json!({"type": "text_delta", "content": t, "ts": now_ms()}));
```
Replace each with:
```rust
emitter.delta(t.to_string());
```
(`t` is the streamed token text. Drop the `ts` — ephemeral deltas don't need it; the CLI restamps for stdout compat in Step 7.)

- [x] **Step 5: Replace `read_events(&tx_path)` history reads with `ingestor.events()`**

turn.rs uses `read_events(&tx_path)` at ~474, 567, 585, 602 and `retrace::*` on the result. Replace `read_events(&tx_path)?` → `ingestor.events()?`. Leave `retrace::messages(&events)?` / `retrace::active_resolution(&events)` as-is (they take `&[Event]`).

- [x] **Step 6: Build — expect `run_turn` call-site errors only**
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -30
```
`run_turn` now takes `&Emitter`, so ALL FOUR callers break and must be fixed in this task:
  - `src/bin/gait.rs:~260` — real CLI drain (Step 7)
  - `src/workflow/exec_prompt.rs:~59` — dropped-receiver placeholder (Step 8)
  - `src/turn.rs:370` (`apply_curate_filter`, **production** sub-turn) — Step 8b
  - `src/turn.rs:1195` (test bridge `run_turn_test_call`) — Step 9

`execute_tool_call` does NOT error (kept on `tx_path` per Step 3).

- [x] **Step 7: Wire the REAL CLI drain in `src/bin/gait.rs` (the `run` handler, ~line 260)**

This keeps `gait run` stdout byte-identical to today (proto Event lines + flat text_delta lines), so the still-subprocess REPL keeps working through Tasks 4–7a.
```rust
use gait_ai::proto::gait::log::v1::stream_item::Item;
use gait_ai::session::transcript::now_ms;

let tx_path = gait_dir.join("sessions").join(&session_id).join("transcript.jsonl");
let (tx, mut rx) =
    tokio::sync::mpsc::unbounded_channel::<gait_ai::proto::gait::log::v1::StreamItem>();
let emitter = gait_ai::Emitter::new(tx_path, tx);

// run_turn borrows &session/&emitter; move owned copies into the task for 'static.
let session_owned = session;            // move (Session not needed after the turn here)
let turn_handle = tokio::spawn(async move {
    run_turn(&session_owned, &emitter, resolved, input).await
    // emitter drops here → channel closes → rx.recv() yields None
});

while let Some(item) = rx.recv().await {
    match item.item {
        Some(Item::Event(ev)) => {
            if let Ok(json) = serde_json::to_string(&ev) { println!("{}", json); }
        }
        Some(Item::Delta(d)) => {
            println!("{}", serde_json::json!(
                {"type": "text_delta", "content": d.content, "ts": now_ms()}));
        }
        None => {}
    }
}
let turn_result = turn_handle.await??;
```
(If `session` is used after the turn in gait.rs, wrap it in `Arc` and clone into the task. Check the existing post-turn code at gait.rs:272+ which uses `session`/`gait_dir`/`session_id` — those owned values are fine; only `session` moved into the task needs care.)

- [x] **Step 8: Placeholder for `src/workflow/exec_prompt.rs` (~line 59)**

Workflow event streaming is out of scope. Give it a dropped receiver:
```rust
let tx_path = /* session transcript path, as currently derived */;
let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
let emitter = crate::proto::Emitter::new(tx_path, tx);
let result = run_turn(&session, &emitter, caps, input).await?;
```
Note in a comment that workflow turns currently discard the live stream (transcript still written via `emitter.emit`); proper workflow streaming is future work.

- [x] **Step 8b: Fix `apply_curate_filter` (turn.rs:370)**

This internal sub-turn calls `run_turn(&session, caps, …)`. Give it a dropped-receiver emitter (its events go to the transcript only — curate is a side turn, not part of the main live stream):
```rust
let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
let curate_emitter = crate::proto::Emitter::new(
    session.gait_dir.join("sessions").join(&session.id).join("transcript.jsonl"), tx);
let _ = run_turn(&session, &curate_emitter, caps,
    TurnInput { task: Some(task), auto: true, max_iter: Some(1) }).await;
```
(Confirm `Session` field names `gait_dir`/`id` — grep session_obj.rs.)

- [x] **Step 9: Fix the test bridge (turn.rs:1195) + build + full test suite**

The test bridge `run_turn_test_call` (turn.rs:~1175–1195) calls `run_turn(&session, caps, input)`. Add a dropped-receiver emitter there too (same pattern as Step 8b). Then:
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -30
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```
Expected: build exit 0, all tests pass. Any other turn.rs test that calls `run_turn` directly needs the same dropped-receiver emitter arg.

- [x] **Step 10: Smoke test `gait run` (stdout must be unchanged)**
```bash
make install
/Users/aicoder/develop/gait run --gait-dir /Users/aicoder/Documents/gait-trace \
  --codebase /Users/aicoder/Documents/ideation/gait/gait-rs \
  --task "Call output() with the word hello." --auto | head -20
```
Expected: same proto Event lines + flat `text_delta` lines as before.

- [x] **Step 11: Commit**
```bash
git add src/turn.rs src/bin/gait.rs src/workflow/exec_prompt.rs src/lib.rs
git commit -m "feat: run_turn takes &Emitter; Ingestor internal; CLI drains StreamItem→stdout"
```

---

### Task 5: Thread `&Emitter` into `execute_tool_call`

**Files:**
- Modify: `src/turn.rs`

`execute_tool_call` (turn.rs:181) is a private async free fn taking `tx_path: &Path` and calling `wire::emit` at 188 and 240. `ToolCtx` is NOT involved in emission and is NOT modified.

- [x] **Step 1: Update signature**
```rust
async fn execute_tool_call(
    tc: &ToolCall,
    tools: &[Box<dyn Tool>],
    emitter: &crate::proto::Emitter,
    approval_prefix: Option<String>,
) -> anyhow::Result<(AssistantPart, ToolResult)>
```

- [x] **Step 2: Replace the two internal `wire::emit` calls**
```rust
// 188:
emitter.emit(events::tool_call(id.clone(), tc.tool_name.clone(), &tc.args))?;
// 240:
emitter.emit(events::tool_result(id.clone(), output.clone(), duration, process))?;
```

- [x] **Step 3: Update ALL `execute_tool_call` call sites**

Confirmed at turn.rs:540, 911, 1010, 1089 (grep to confirm):
```bash
grep -n "execute_tool_call(" src/turn.rs
```
Change each `execute_tool_call(&tc, &tools, &tx_path, prefix)` → `execute_tool_call(&tc, &tools, emitter, prefix)`.

- [x] **Step 4: Build + test**
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

- [x] **Step 5: Commit**
```bash
git add src/turn.rs
git commit -m "feat: execute_tool_call takes &Emitter instead of tx_path"
```

---

### Task 6: Remove `wire::emit`'s stdout side

> **⚠️ RE-SCOPED DURING EXECUTION — what actually shipped differs from the steps below.**
> The premise ("after Tasks 4–5 no production code calls `wire::emit`") proved **false**: `wire::emit` has ~17 call sites repo-wide across two distinct surfaces. The reachable-from-`run_turn` writers (`init_session`, `config_hook` hook_run, `write_permissions_reply`) still `println!`'d — which would leak raw JSON into the *in-process* REPL (Task 7b). The remaining ~14 sites are the separate `gait workflow` command, which has its own NDJSON stdout and no Emitter/channel (out of scope; to be redone later).
>
> **What shipped (commit `fe52956`):** switched only the three `run_turn`-reachable writers to `wire::append` (transcript-only, no stdout). **`wire::emit` was NOT removed** — it stays for the workflow surface. Net effect: `gait run` stdout no longer carries the proto `SessionStart` line (consumers use the flat `session_id` line, unchanged); the in-process REPL no longer leaks raw JSON. The steps below describe the original (un-shipped) "remove `wire::emit` entirely" plan and are kept for history.

**Files:**
- Modify: `src/proto/wire.rs`
- Modify: any remaining `wire::emit` caller

After Tasks 4–5, no production code calls `wire::emit`. Remove its `println!` (the stdout write now lives in the CLI surface).

- [x] **Step 1: Find remaining callers**
```bash
grep -rn "wire::emit\b" src/ | grep -v "test"
```

- [x] **Step 2: Remove `wire::emit`** entirely; update any stragglers to `Emitter::emit` (production) or `wire::append`/`write_to_transcript` (tests). If a test genuinely needs the old behavior, it should use `wire::append` (transcript only) — nothing should print to stdout from `wire` anymore.

- [x] **Step 3: Build + test**
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

- [x] **Step 4: Commit**
```bash
git add src/proto/wire.rs src/
git commit -m "refactor: remove wire::emit stdout side; CLI surface owns stdout"
```

---

### Task 7a: Renderer speaks proto (fixes the dropped-events bug)

**Files:**
- Modify: `src/ui/renderer.rs`

This is the standalone fix for the half-done migration. Add `handle_event(&Event)` dispatching on proto payload kinds, and make `handle_line` transitional: the subprocess stdout currently carries BOTH proto `Event` lines and flat `text_delta` lines, so `handle_line` must handle both. `chat.rs` is unchanged here (still subprocess, still calls `handle_line`). After this task the REPL renders tool calls / permissions / clarify / inform again — **verify it**.

- [x] **Step 1: Extract delta-text handling and add `handle_event` + `handle_payload`**

The existing `handle_line` (renderer.rs:166) text-delta arm does line-buffering via `push_line`. Extract its inner logic into a helper:
```rust
fn push_text(&mut self, content: &str) {
    self.line_buf.push_str(content);
    while let Some(pos) = self.line_buf.find('\n') {
        let line = self.line_buf[..pos].to_string();
        self.line_buf = self.line_buf[pos + 1..].to_string();
        self.push_line(&line);
    }
}
```

Add the proto dispatch:
```rust
use crate::proto::gait::log::v1::{Event, event, payload};

impl Renderer {
    /// Handle a durable proto Event. (Text streaming comes via deltas, NOT here.)
    pub fn handle_event(&mut self, ev: &Event) -> RenderAction {
        let Some(event::Body::Payload(p)) = &ev.body else { return RenderAction::Continue };
        match &p.kind {
            // IMPORTANT: AssistantText is the FULL accumulated text, already painted
            // live by the deltas. Do NOT re-render it or text doubles.
            Some(payload::Kind::AssistantText(_)) => RenderAction::Continue,

            Some(payload::Kind::ToolCall(tc)) => {
                // proto field is `name` (NOT tool_name) — log.proto:142
                eprintln!("{}  \u{27f3} {}(\u{2026}){}", DIM_FG, tc.name, RESET);
                self.pending_tool = Some(tc.name.clone());
                // proto ToolCall has no ts; timing falls back to None
                RenderAction::Continue
            }
            Some(payload::Kind::ToolResult(tr)) => {
                // proto ToolResult (log.proto:145) has NO `ok` bool — only `output`,
                // `duration`, and optional `process: ProcessResult`. Derive ok from
                // process.exit_code (== 0, or None ⇒ ok). Mirror the existing render.
                self.render_tool_result(tr);   // factor a helper; see note below
                RenderAction::Continue
            }
            Some(payload::Kind::Permissions(p)) => {
                self.flush();
                RenderAction::PermissionNeeded(p.items.iter().map(perm_to_tx).collect())
            }
            Some(payload::Kind::Clarify(c)) => {
                self.flush();
                RenderAction::ClarifyNeeded(c.items.iter().map(clarify_to_tx).collect())
            }
            Some(payload::Kind::Inform(i)) => {
                self.flush();
                RenderAction::InformNeeded(i.items.iter().map(inform_to_tx).collect())
            }
            Some(payload::Kind::TurnComplete(tc)) => {
                self.flush();
                let blocks = if tc.blocks.is_empty() { self.turn_blocks.clone() }
                             else { tc.blocks.clone() };
                RenderAction::TurnComplete(blocks)
            }
            Some(payload::Kind::Error(e)) => {
                self.flush();
                eprintln!("{}[error] {}{}", self.theme.danger.fg(), e.message, RESET);
                RenderAction::Continue
            }
            _ => RenderAction::Continue,   // SessionStart, Resolution, UserTurn, Raw, etc.
        }
    }
}
```

**ITEM-TYPE CONVERSION (required — verified against real code).** `RenderAction::{PermissionNeeded,ClarifyNeeded,InformNeeded}` carry `crate::session::transcript::{PermissionItem,ClarifyItem,InformItem}` (renderer.rs:4,9–11), and `prompt_permission`/`prompt_clarify`/`prompt_inform` all take those **transcript** types (permission.rs:6, clarify.rs:5, inform.rs:5). The proto payloads carry `pb::{PermissionItem,ClarifyItem,InformItem}`, which are **structurally different** (e.g. `ClarifyItem.kind` is `String` in transcript vs `i32` enum in proto; `PermissionItem.tool_args` is `serde_json::Value` vs `pbjson_types::Struct`). They are NOT interchangeable.

Decision: **keep `RenderAction` and the prompts on the transcript types** (smaller blast radius — prompts/chat.rs unchanged) and convert proto→transcript inside the renderer. Add these helpers to `renderer.rs` (they invert the existing transcript→proto converters in events.rs:331,363):
```rust
use crate::session::transcript as tx;
use crate::proto::gait::log::v1 as pb;

fn perm_to_tx(p: &pb::PermissionItem) -> tx::PermissionItem {
    tx::PermissionItem {
        id: p.id.clone(),
        cmd: p.cmd.clone(),
        reason: p.reason.clone(),
        patterns: p.patterns.clone(),
        tool_name: p.tool_name.clone(),
        // proto Struct → serde_json::Value; None ⇒ Null
        tool_args: p.tool_args.clone()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null),
    }
}

fn clarify_to_tx(c: &pb::ClarifyItem) -> tx::ClarifyItem {
    tx::ClarifyItem {
        id: c.id.clone(),
        content: c.content.clone(),
        why: c.why.clone(),
        kind: match pb::ClarifyKind::try_from(c.kind) {
            Ok(pb::ClarifyKind::Is) => "is".to_string(),
            Ok(pb::ClarifyKind::Ought) => "ought".to_string(),
            _ => String::new(),
        },
    }
}

fn inform_to_tx(i: &pb::InformItem) -> tx::InformItem {
    tx::InformItem {
        id: i.id.clone(),
        content: i.content.clone(),
        context: i.context.clone(),
    }
}
```
(Verify `pbjson_types::Struct` serializes to a JSON object via `serde_json::to_value` — pbjson Structs implement Serialize. If the exact round-trip differs, mirror events.rs `to_struct` inverse. Confirm `ClarifyKind::try_from(i32)` exists — prost enums impl `TryFrom<i32>`.)

**Note on `render_tool_result`:** factor the existing tool_result rendering block (renderer.rs ~197–224) into a helper that takes the fields it needs, then call it from both the (transitional) flat path and `handle_event`. Inspect the generated proto `ToolResult` for the `ok`/`output` field names (it may nest a `ProcessResult`).

- [x] **Step 2: Make `handle_line` transitional (handles BOTH formats)**
```rust
pub fn handle_line(&mut self, line: &str) -> RenderAction {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return RenderAction::Continue;
    };
    // Flat legacy text_delta (still emitted by the CLI surface for stdout compat)
    if v.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
        self.push_text(v["content"].as_str().unwrap_or(""));
        return RenderAction::Continue;
    }
    // Otherwise it's a proto Event line
    match serde_json::from_str::<Event>(line) {
        Ok(ev) => self.handle_event(&ev),
        Err(_) => RenderAction::Continue,
    }
}
```

- [x] **Step 3: Keep existing flat-format tests green; add proto tests**

The existing flat-format tests (text_delta, permissions-as-flat-json, turn_complete) should still pass because `handle_line` still accepts flat text_delta and the flat-`type` test inputs. BUT the flat `permissions`/`clarify`/`turn_complete` test inputs no longer match a real stream — rewrite those tests to construct a proto `Event` and call `handle_event`. Add:
  - `handle_event_tool_call_sets_pending`
  - `handle_event_permissions_returns_permission_needed`
  - `handle_event_turn_complete_returns_blocks`
  - `handle_event_assistant_text_is_noop` (asserts no double-render: `line_buf`/`turn_blocks` unchanged)

- [x] **Step 4: Build + test**
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
cargo test ui::renderer 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

- [x] **Step 5: VERIFY the bug is fixed (live REPL, still subprocess)**
```bash
make install
/Users/aicoder/develop/gait chat --gait-dir /Users/aicoder/Documents/gait-trace \
  --codebase /Users/aicoder/Documents/ideation/gait/gait-rs
```
Give it a task that triggers a tool needing approval. Confirm: tool-call indicator (`⟳ name(…)`) appears, a permission prompt appears, and approving continues the turn. (Before this task, the prompt never appeared.)

- [x] **Step 6: Commit**
```bash
git add src/ui/renderer.rs
git commit -m "fix: renderer consumes proto events (handle_event); REPL renders tools/permissions again"
```
(Only `renderer.rs` changes — `chat.rs` still calls `handle_line`, and the prompts keep their transcript item types thanks to the converters.)

---

### Task 7b: REPL calls `run_turn()` in-process; delete the subprocess

**Files:**
- Modify: `src/chat.rs`
- Modify: `src/bin/gait.rs` (await the now-async `run_chat`)

The renderer already speaks proto (7a). Now swap the transport: `run_chat` becomes async, spawns `run_turn` with an in-process `StreamItem` channel, and drains it — `Event` → `handle_event`, `Delta` → `push_text`. No subprocess, no stdout re-parse.

- [x] **Step 1: Make `run_chat` async + update call site**

`src/chat.rs`: `pub fn run_chat(...)` → `pub async fn run_chat(...)`.
`src/bin/gait.rs:811`: `gait_ai::chat::run_chat(gait_dir, codebase, agent_name)` → `… .await`. (`#[tokio::main]` is at gait.rs:110, so `.await` is valid.)

- [x] **Step 2: Build session + profile once per task line**

Verified signatures (match EXACTLY — earlier drafts got these wrong):
- `Session::load` (session_obj.rs) = `(gait_dir: PathBuf, id: String, default_codebase: PathBuf, repos: Repos) -> Self` — NOT fallible, owned args. `Repos` via `crate::git::Repos::from_workspace(&gait_dir, &codebase)` (match gait.rs:~235).
- `Profile::from_config_and_agent` (profile.rs:53) = `(config, agent_path: Option<&Path>, model_override: Option<String>) -> Self` — NOT fallible.
- `profile.resolve` (profile.rs:95) = async `(self, codebase_path: PathBuf, session_id: String, gait_dir: PathBuf, repos: Repos, api_key: Option<String>) -> Result<ResolvedProfile>` — **takes `self` by value and ALL args OWNED (not refs)**.
- **`Profile` is NOT `Clone`** (profile.rs:7, no derive). So you cannot `profile.clone().resolve(...)`. Two options: (a) `#[derive(Clone)]` on `Profile` (all its fields are Clone — cheap, one line), or (b) **rebuild the Profile inside the loop** each turn via `from_config_and_agent` (it's cheap and not fallible). This plan uses (b) to avoid touching `Profile`.
- Resolve `api_key` as gait.rs does (~line 233; grep `resolve_api_key`).

```bash
grep -n "fn load\b" src/session/session_obj.rs
grep -n "fn from_config_and_agent\|fn resolve\b\|resolve_api_key\|Repos::from_workspace" src/profile.rs src/bin/gait.rs
```

- [x] **Step 3: Rewrite the `'spawn` loop**
```rust
use crate::proto::gait::log::v1::stream_item::Item;

// once per task line (outside 'spawn):
let repos = crate::git::Repos::from_workspace(&gait_dir, &codebase);
let session = std::sync::Arc::new(crate::session::Session::load(
    gait_dir.clone(), session_id.clone(), codebase.clone(), repos.clone()));
let config = crate::config::load_config(&gait_dir)?;
let api_key = /* resolve as gait.rs does — grep resolve_api_key */;

'spawn: loop {
    let tx_path = gait_dir.join("sessions").join(&session_id).join("transcript.jsonl");
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = crate::proto::Emitter::new(tx_path, tx);

    // Rebuild Profile per turn (Profile is not Clone) and resolve with OWNED args.
    let profile = crate::profile::Profile::from_config_and_agent(
        &config, Some(agent_path.as_path()), None);
    let caps = profile.resolve(
        codebase.clone(), session_id.clone(), gait_dir.clone(),
        repos.clone(), api_key.clone()).await?;
    let input = TurnInput { task: Some(line.clone()), auto: false, max_iter: config.max_iter };

    let session_for_turn = session.clone();
    let turn_handle = tokio::spawn(async move {
        crate::turn::run_turn(&*session_for_turn, &emitter, caps, input).await
        // emitter drops here → channel closes
    });

    let mut renderer = Renderer::new(theme.clone());
    let mut timer = Timer::start();
    let mut pending_permissions = vec![];
    let mut pending_clarify = vec![];
    let mut pending_inform = vec![];
    let mut turn_complete_blocks = None;

    while let Some(item) = rx.recv().await {
        timer.stop();
        let action = match item.item {
            Some(Item::Event(ev)) => renderer.handle_event(&ev),
            Some(Item::Delta(d)) => { renderer.push_text(&d.content); RenderAction::Continue }
            None => RenderAction::Continue,
        };
        match action {
            RenderAction::PermissionNeeded(items) => pending_permissions = items,
            RenderAction::ClarifyNeeded(items)   => pending_clarify = items,
            RenderAction::InformNeeded(items)     => pending_inform = items,
            RenderAction::TurnComplete(blocks)    => turn_complete_blocks = Some(blocks),
            _ => {}
        }
    }
    renderer.flush();
    timer.stop();
    let _turn_result = turn_handle.await??;

    current_turn_blocks.extend(
        turn_complete_blocks.unwrap_or_else(|| renderer.turn_blocks.drain(..).collect()));

    // ... existing clarify/inform/permission handling + 'spawn re-loop, unchanged ...
}
```
`push_text` must be `pub(crate)` (or `pub`) so `chat.rs` can call it. Preserve the existing `last_turn_blocks`/`current_turn_blocks` quote-reply logic (chat.rs:158, 205–207, 311).

- [x] **Step 4: Remove subprocess machinery from `chat.rs`**

Remove `use std::process::{Command, Stdio};`, `use std::io::{BufRead, BufReader};`, and the `let gait_bin = std::env::current_exe()…` line — UNLESS `Command` is still used for the `/u` undo shell-out (chat.rs:~130–143). Keep only what undo needs.

- [x] **Step 5: Build + full test suite**
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -30
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

- [x] **Step 6: Smoke test the REPL (no subprocess now)**
```bash
make install
/Users/aicoder/develop/gait chat --gait-dir /Users/aicoder/Documents/gait-trace \
  --codebase /Users/aicoder/Documents/ideation/gait/gait-rs
```
Verify: streaming text, timer, tool indicators, permission/clarify/inform prompts, `/u` undo, `!cmd` bang commands, `>N` quote-reply all work. Confirm no `gait run` child process is spawned (`ps` during a turn shows no child gait).

- [x] **Step 7: Commit**
```bash
git add src/chat.rs src/bin/gait.rs
git commit -m "feat: REPL calls run_turn() in-process via StreamItem channel; delete subprocess self-spawn"
```
