# Phase 6 — `/skillname` REPL Trigger

**Status:** READY TO IMPLEMENT  
**Depends on:** Phase 3b done (`Toggle{kind,name,enabled}` proto message in
`payload::Kind::Toggle`, `events::toggle(kind, name, enabled)` constructor,
`ActiveAgent` folds OR-Set per kind); Phase 5 done (`skills/<name>/SKILL.md`
discoverable via `crate::skills::scan_skills(gait_dir)`).

Authoritative spec: `docs/plans/2026-06-12-agent-checkpoint-model.md` §5
(`/skillname` bullet).

---

## Goal

When the user types `/foo` (exactly a slash followed by a skill name) at the
REPL prompt, gait emits a `Toggle{kind:SKILL, name:"foo", enabled:true}` event
to the session log — **without** sending anything to the model. Because
`ActiveAgent` folds it as an OR-Set element, the skill stays enabled across all
subsequent turns in the session. Log truncation (undo) removes the event and
reverts the state. `/foo off` emits `enabled:false`. `/nope` where no
`skills/nope/SKILL.md` exists prints a friendly error and emits nothing.

---

## Architecture

### Why a dedicated command check in `chat.rs`, not a branch in `expand_input`

`expand_input` (repl_input.rs:188) is an **inline sigil expander** — it receives
a user message that will be sent to the model, and it replaces sub-regions of the
string (e.g. `$(cmd)`, `@path`, `>(N)`). It returns an augmented version of the
input string. Skill-toggle is a **whole-line command** that:

1. must NOT be forwarded to the model at all,
2. has a side-effect (appending an event to the session log), and
3. consumes the REPL turn entirely.

Adding it to `expand_input` would require a return-type change (a discriminated
union of "string to send" vs "consumed command"), coupling it with the scanner and
the `gait_dir` / `session_id` state that the expander doesn't hold today, and
making every non-REPL caller (the CLI path) aware of it. That is the wrong layer.

The correct placement is the **same early-return command block in `chat.rs`** that
handles `/n`, `/q`, and `/u`. The check runs _after_ `is_bang_command` but
_before_ `expand_input` (line 157), so the raw slash-prefixed string is still
available unmodified. This matches the existing pattern: every REPL built-in is
a `continue` or `break` that never reaches `expand_input`.

### Integration point in `chat.rs` (lines 104–156)

The existing command dispatch looks like:

```rust
// line 88-104: bang commands (!cmd) → shell, continue
// line 106-116: match { "/n" | "/new" → new session, "/q" → break, _ → {} }
// line 120-155: /u /uc /ut undo dispatch
// line 157: rl.add_history_entry(&line)
// line 160: let line = expand_input(&line, ...)
```

The skill-toggle handler is inserted **after the undo block and before
`rl.add_history_entry`** (i.e. between line 155 and 157). It short-circuits with
`continue` when it recognises a skill command, so `add_history_entry` and
`expand_input` are never reached for those lines.

### Parse rules

A line is a skill-toggle command iff:
1. It starts with `/` (the sigil).
2. The character immediately after `/` is not one of the reserved prefixes
   already consumed: `n`, `q`, `u` (handled by earlier arms). In practice we
   check `starts_with('/')` and the remaining token is neither a known built-in
   nor a prefix of one.

Concretely: strip the leading `/`. Split on the first ASCII space. The first
token is the **skill name**; if a second token exists and equals `"off"` (case-
insensitive), `enabled = false`; otherwise `enabled = true`.

Valid skill name: non-empty, all chars satisfy `|c: char| c.is_alphanumeric() || c == '-' || c == '_'`. (This rejects slash-only `/`, built-in aliases, and paths.)

The check fires **only if** the line matches — the existing `/n`/`/q`/`/u` arms
are already consumed before this code runs, so there is no ambiguity.

### Skill resolution

Call `crate::skills::scan_skills(&gait_dir)` (added in Phase 5) to get the
installed `Vec<SkillDef>`. Check whether any entry has `.name == skill_name`. If
none: `eprintln!` a friendly error, `continue`. No event is emitted.

Because `scan_skills` does one `read_dir` call per REPL command, it re-checks
the disk on every invocation — new skills dropped on disk are immediately
discoverable without restarting gait. This is the correct and cheap behaviour
(the scan is O(n) over a directory, not recursive).

### Event emission

```rust
let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
let _ = wire::append(&tx_path, events::toggle(ToggleKind::Skill, &skill_name, enabled));
```

`wire::append` (wire.rs:73) mints the event envelope (seq, parents, content_hash)
and appends one JSON line to `log.jsonl`. It is the same call used for
`clarify_reply`, `inform_reply`, and `permissions_reply` in the existing REPL
loop (chat.rs:274, 280, 371).

`events::toggle` is the constructor added in Phase 3b:
```rust
pub fn toggle(kind: pb::ToggleKind, name: &str, enabled: bool) -> pb::event::Body {
    wire::body(pb::payload::Kind::Toggle(pb::Toggle {
        kind: kind as i32,
        name: name.to_string(),
        enabled,
    }))
}
```

### User feedback

On success:
```
skill foo enabled
```
or
```
skill foo disabled
```
(printed via `eprintln!` to match the style of `/n`'s "new session …" message).

On unknown skill:
```
[gait] no such skill "nope" — is skills/nope/SKILL.md present?
```

---

## Tech Stack

- **Rust stable** — no new deps. `crate::skills::scan_skills` (Phase 5),
  `crate::proto::{events, wire}`, `crate::proto::gait::log::v1::ToggleKind` (Phase 3b).
- **Test commands** (from `gait-rs/CLAUDE.md`):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -30
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```

---

## Files

| File | Role |
|---|---|
| `src/chat.rs` | Add skill-toggle command block; call `scan_skills`, `wire::append`, `events::toggle` |
| `src/repl_input.rs` | Add `parse_skill_toggle` helper (pure fn, testable in isolation) |
| `src/proto/events.rs` | Assumed done in Phase 3b: `events::toggle(kind, name, enabled)` |
| `proto/log.proto` | Assumed done in Phase 3b: `Toggle`, `ToggleKind` messages + `payload::Kind::Toggle` |

No changes to `turn.rs`, `retrace.rs`, or `ActiveAgent` — they already fold
`Toggle` events (Phase 3b).

---

## Tasks

---

### Task 1 — Add `parse_skill_toggle` to `repl_input.rs`

**Goal:** a pure, side-effect-free parser that extracts `(skill_name, enabled)`
from a raw REPL line, or returns `None` if the line is not a skill-toggle
command. Putting it in `repl_input.rs` keeps all REPL input parsing in one file
and makes it independently testable without mocking the log.

**Files:** `src/repl_input.rs`

#### Steps

- [ ] **Test first.** Add a `#[cfg(test)]` module `skill_toggle_tests` at the
  bottom of `src/repl_input.rs` (after the existing `quote_reply_tests` block,
  `repl_input.rs:387`):

  ```rust
  #[cfg(test)]
  mod skill_toggle_tests {
      use super::parse_skill_toggle;

      #[test]
      fn enable_bare_name() {
          assert_eq!(parse_skill_toggle("/tdd"), Some(("tdd".to_string(), true)));
      }

      #[test]
      fn enable_with_trailing_whitespace() {
          assert_eq!(parse_skill_toggle("/tdd  "), Some(("tdd".to_string(), true)));
      }

      #[test]
      fn disable_with_off() {
          assert_eq!(parse_skill_toggle("/tdd off"), Some(("tdd".to_string(), false)));
      }

      #[test]
      fn disable_with_off_uppercase() {
          assert_eq!(parse_skill_toggle("/tdd OFF"), Some(("tdd".to_string(), false)));
      }

      #[test]
      fn disable_with_extra_whitespace() {
          assert_eq!(parse_skill_toggle("/tdd   off"), Some(("tdd".to_string(), false)));
      }

      #[test]
      fn plain_message_returns_none() {
          assert_eq!(parse_skill_toggle("hello world"), None);
      }

      #[test]
      fn known_builtins_return_none() {
          // Reserved built-ins must not match even though they start with /
          assert_eq!(parse_skill_toggle("/n"), None);
          assert_eq!(parse_skill_toggle("/new"), None);
          assert_eq!(parse_skill_toggle("/q"), None);
          assert_eq!(parse_skill_toggle("/exit"), None);
          assert_eq!(parse_skill_toggle("/u"), None);
          assert_eq!(parse_skill_toggle("/uc"), None);
          assert_eq!(parse_skill_toggle("/ut"), None);
      }

      #[test]
      fn bare_slash_returns_none() {
          assert_eq!(parse_skill_toggle("/"), None);
      }

      #[test]
      fn invalid_chars_in_name_return_none() {
          // Paths and sigil-like names are not skill names
          assert_eq!(parse_skill_toggle("/foo/bar"), None);
          assert_eq!(parse_skill_toggle("/foo bar baz"), None); // third token → invalid
      }

      #[test]
      fn hyphen_underscore_allowed() {
          assert_eq!(parse_skill_toggle("/my-skill"), Some(("my-skill".to_string(), true)));
          assert_eq!(parse_skill_toggle("/my_skill"), Some(("my_skill".to_string(), true)));
      }
  }
  ```

- [ ] **Run** (expect: does-not-compile — `parse_skill_toggle` missing):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error\[" /tmp/gait-test-output.txt | head -10
  ```
  Expect `error[E0425]: cannot find function \`parse_skill_toggle\``.

- [ ] **Implement.** Add the following `pub(crate)` function to `src/repl_input.rs`
  (before the `#[cfg(test)]` block at line 257, after `expand_file_marker`):

  ```rust
  /// Reserved REPL built-in prefixes that must NOT be treated as skill names.
  const BUILTIN_PREFIXES: &[&str] = &["n", "new", "q", "exit", "quit", "u", "uc", "ut"];

  /// Parse a REPL line as a skill-toggle command.
  ///
  /// Returns `Some((skill_name, enabled))` when the line is exactly:
  ///   `/skillname`        → enable  (enabled = true)
  ///   `/skillname off`    → disable (enabled = false)
  ///
  /// Returns `None` for any other input, including:
  ///   - lines not starting with `/`
  ///   - reserved built-in commands (`/n`, `/q`, `/u`, `/uc`, `/ut`, `/exit`)
  ///   - bare `/` with no name
  ///   - names containing `/` (path-like) or characters outside `[a-zA-Z0-9_-]`
  ///   - three or more whitespace-separated tokens
  pub(crate) fn parse_skill_toggle(line: &str) -> Option<(String, bool)> {
      let rest = line.strip_prefix('/')?;
      // Bare slash
      if rest.is_empty() { return None; }

      // Split on whitespace: expect 1 or 2 tokens
      let mut tokens = rest.split_whitespace();
      let name = tokens.next()?;
      let modifier = tokens.next(); // "off" or absent
      // Reject any further tokens
      if tokens.next().is_some() { return None; }

      // Reserved built-in names
      if BUILTIN_PREFIXES.contains(&name) { return None; }

      // Validate name: alphanumeric, hyphen, underscore only
      if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
          return None;
      }

      let enabled = match modifier {
          None => true,
          Some(m) if m.eq_ignore_ascii_case("off") => false,
          _ => return None, // unrecognised modifier
      };

      Some((name.to_string(), enabled))
  }
  ```

- [ ] **Run** (expect: all `skill_toggle_tests` pass):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test skill_toggle 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "test result" /tmp/gait-test-output.txt
  ```
  Expected: `test result: ok. 10 passed; 0 failed`.

- [ ] **Commit:**
  ```
  feat(repl_input): add parse_skill_toggle — pure parser for /skillname REPL command

  Recognises /name (enable) and /name off (disable); guards against
  reserved built-in prefixes, bare slash, path-like names, and extra
  tokens.  10 unit tests.  No side effects — caller owns emit + resolve.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 2 — Wire skill-toggle into `chat.rs`

**Goal:** in `run_chat`, after the undo block and before `rl.add_history_entry`,
check `parse_skill_toggle` and, on a match, resolve the skill against disk,
emit the `Toggle` event, print feedback, and `continue`.

**Files:** `src/chat.rs`

#### Steps

- [ ] **Read** `src/chat.rs` lines 1–20 (imports) and 104–160 (the command
  dispatch block). Note the exact position of `let _ = rl.add_history_entry(&line);`
  (line 157) — the skill-toggle block goes immediately before it.

- [ ] **Add the import** for `parse_skill_toggle` to the existing `use
  crate::repl_input::{...}` line (currently at line 15):

  ```rust
  use crate::repl_input::{
      is_bang_command, bang_command_str, expand_input, ExpandCtx,
      run_and_capture_pty_raw, CaptureResult,
      parse_skill_toggle,           // ← add this
  };
  ```

  Also add the `ToggleKind` import to the `use crate::proto::{events, wire};` area.
  After Phase 3b, `ToggleKind` is in the generated proto module:

  ```rust
  use crate::proto::gait::log::v1::ToggleKind;
  ```

- [ ] **Insert** the skill-toggle handler block between the closing `};` of the
  undo block (line 155) and `let _ = rl.add_history_entry(&line);` (line 157).
  The insertion goes at line 156 (after the `else { line }` arm closes):

  ```rust
  // /skillname [off] — emit Toggle{kind:SKILL} event; do NOT forward to model.
  if let Some((skill_name, enabled)) = parse_skill_toggle(&line) {
      let available = crate::skills::scan_skills(&gait_dir);
      if !available.iter().any(|s| s.name == skill_name) {
          eprintln!(
              "[gait] no such skill {:?} — is skills/{}/SKILL.md present?",
              skill_name, skill_name,
          );
      } else {
          let tx_path = gait_dir
              .join("sessions")
              .join(&session_id)
              .join("log.jsonl");
          let _ = wire::append(
              &tx_path,
              events::toggle(ToggleKind::Skill, &skill_name, enabled),
          );
          if enabled {
              eprintln!("skill {} enabled", skill_name);
          } else {
              eprintln!("skill {} disabled", skill_name);
          }
      }
      continue;
  }
  ```

  The `continue` statement means control returns to the top of the `loop`,
  bypassing `rl.add_history_entry`, `expand_input`, session loading, and the
  turn spawn entirely. A skill-toggle line is never added to readline history
  (matches the behaviour of `/n` and `/q`).

- [ ] **Build** (not test yet — test is in Task 3):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```
  Expected: `exit: 0`, zero errors.

- [ ] **Commit:**
  ```
  feat(chat): wire /skillname toggle — emit Toggle{SKILL} event from REPL

  After the undo block, check parse_skill_toggle(). On match: resolve
  skill against scan_skills(), emit wire::append(events::toggle(Skill,
  name, enabled)), print "skill <name> enabled/disabled".  Unknown skill
  prints a friendly error; no event emitted.  Command never forwarded to
  the model (continue before expand_input).

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 3 — Unit tests for the `chat.rs` skill-toggle path

The REPL loop is async and hard to unit-test end-to-end. Instead, extract and
test the three logical sub-functions that together implement the behaviour:

1. `parse_skill_toggle` — already tested in Task 1.
2. Skill resolution (does the name exist on disk?) — tested via `scan_skills`
   (already covered in Phase 5 tests). No new test needed here.
3. The `wire::append` + `events::toggle` round-trip — test that a `Toggle` event
   written to a temp log file deserialises correctly.

**Files:** `src/proto/events.rs` (add to the existing `#[cfg(test)] mod tests`
block, `events.rs:418`)

#### Steps

- [ ] **Add the following tests** to `src/proto/events.rs` inside the existing
  `#[cfg(test)] mod tests` block (after the last test, near line 527):

  ```rust
  #[test]
  fn toggle_skill_enabled_round_trips() {
      use crate::proto::gait::log::v1::{ToggleKind, payload};
      use crate::proto::wire;
      use tempfile::TempDir;

      let dir = TempDir::new().unwrap();
      let path = dir.path().join("log.jsonl");

      wire::append(&path, events::toggle(ToggleKind::Skill, "tdd", true))
          .expect("append toggle");

      let evs = wire::read_events(&path).expect("read events");
      assert_eq!(evs.len(), 1);
      match evs[0].body.as_ref().and_then(|b| match b {
          crate::proto::gait::log::v1::event::Body::Payload(p) => p.kind.as_ref(),
          _ => None,
      }) {
          Some(payload::Kind::Toggle(t)) => {
              assert_eq!(t.kind, ToggleKind::Skill as i32);
              assert_eq!(t.name, "tdd");
              assert!(t.enabled);
          }
          other => panic!("expected Toggle payload, got {other:?}"),
      }
  }

  #[test]
  fn toggle_skill_disabled_round_trips() {
      use crate::proto::gait::log::v1::{ToggleKind, payload};
      use crate::proto::wire;
      use tempfile::TempDir;

      let dir = TempDir::new().unwrap();
      let path = dir.path().join("log.jsonl");

      wire::append(&path, events::toggle(ToggleKind::Skill, "tdd", false))
          .expect("append toggle");

      let evs = wire::read_events(&path).expect("read events");
      match evs[0].body.as_ref().and_then(|b| match b {
          crate::proto::gait::log::v1::event::Body::Payload(p) => p.kind.as_ref(),
          _ => None,
      }) {
          Some(payload::Kind::Toggle(t)) => {
              assert!(!t.enabled, "enabled must be false");
          }
          other => panic!("expected Toggle payload, got {other:?}"),
      }
  }

  #[test]
  fn toggle_chains_seq_after_other_event() {
      use crate::proto::gait::log::v1::ToggleKind;
      use crate::proto::wire;
      use tempfile::TempDir;

      let dir = TempDir::new().unwrap();
      let path = dir.path().join("log.jsonl");

      wire::append(&path, events::assistant_text("hello".into())).unwrap();
      wire::append(&path, events::toggle(ToggleKind::Skill, "tdd", true)).unwrap();

      let evs = wire::read_events(&path).unwrap();
      assert_eq!(evs.len(), 2);
      assert_eq!(evs[0].seq, 1);
      assert_eq!(evs[1].seq, 2);
      assert_eq!(evs[1].parents, vec![evs[0].id.clone()],
          "Toggle must chain off the previous event");
  }
  ```

  These three tests verify:
  - A `Toggle{SKILL, "tdd", true}` event written by `wire::append` round-trips
    cleanly through `wire::read_events` with the correct proto fields.
  - `enabled:false` round-trips.
  - The event envelope chains correctly (seq/parents), matching the contract
    verified in `wire.rs` tests.

- [ ] **Run** the new tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test toggle_skill 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "test result" /tmp/gait-test-output.txt
  ```
  Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Run full suite:**
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `test result: ok. N passed; 0 failed`.

- [ ] **Commit:**
  ```
  test(events): Toggle round-trip — skill enable/disable + envelope chaining

  Three tests: enabled=true round-trip, enabled=false round-trip, seq/parents
  chain after a prior event.  Covers the wire path used by the /skillname
  REPL trigger.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 4 — Update hints string in `chat.rs`

**Goal:** surface `/skillname` in the startup hint so users discover it.

**Files:** `src/chat.rs`

#### Steps

- [ ] **Read** `src/chat.rs` lines 18–25 (the `mod hints` block).

  Current value of `hints::STARTUP` (line 21):
  ```rust
  pub const STARTUP: &str =
      "ctrl+d or /q to exit  \u{2022}  /n new session  \u{2022}  /u [N] undo  \u{2022}  !cmd shell";
  ```

- [ ] **Replace** with:
  ```rust
  pub const STARTUP: &str =
      "ctrl+d or /q to exit  \u{2022}  /n new session  \u{2022}  /u [N] undo  \u{2022}  /skill enable  \u{2022}  !cmd shell";
  ```

- [ ] **Build:**
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -10
  ```
  Expected: `exit: 0`.

- [ ] **Commit:**
  ```
  chore(chat): add /skill to REPL startup hint

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 5 — Final build + test pass, dogfood smoke test

**Steps:**

- [ ] **Full clean build:**
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```
  Expected: `exit: 0`, zero errors.

- [ ] **Full test suite:**
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `test result: ok. N passed; 0 failed`.

- [ ] **Install:**
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && make install
  ```

- [ ] **Smoke test — enable an existing skill:**
  ```bash
  mkdir -p /Users/aicoder/gait-test-ws/skills/hello
  cat > /Users/aicoder/gait-test-ws/skills/hello/SKILL.md <<'EOF'
  ---
  name: hello
  description: Hello world skill for smoke testing Phase 6
  ---
  # Hello
  Say hello to the user.
  EOF
  gait --gait-dir /Users/aicoder/gait-test-ws --model minimax/MiniMax-M3
  ```
  In the REPL, type:
  ```
  /hello
  ```
  Expected output: `skill hello enabled`

- [ ] **Smoke test — check event was persisted.** In a second terminal (while
  the REPL session ID is visible from the startup line):
  ```bash
  SESSION=<session-id>
  tail -1 /Users/aicoder/gait-test-ws/sessions/$SESSION/log.jsonl | python3 -m json.tool | grep -A3 '"toggle"'
  ```
  Expected: a JSON object with `"kind": 1` (SKILL), `"name": "hello"`, `"enabled": true`.

- [ ] **Smoke test — disable:**
  ```
  /hello off
  ```
  Expected: `skill hello disabled`

  Verify last line of log:
  ```bash
  tail -1 /Users/aicoder/gait-test-ws/sessions/$SESSION/log.jsonl | python3 -m json.tool | grep '"enabled"'
  ```
  Expected: `"enabled": false`

- [ ] **Smoke test — unknown skill:**
  ```
  /nope
  ```
  Expected output:
  ```
  [gait] no such skill "nope" — is skills/nope/SKILL.md present?
  ```
  No new event appended to the log.

- [ ] **Smoke test — built-in shadows not broken:**
  Type `/n` → new session (not treated as skill toggle).
  Type `/u` with no arg → undo list (not treated as skill toggle).

- [ ] **Commit** if any fixes were needed during smoke test:
  ```
  fix(chat): smoke-test fixes for /skillname trigger

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

## Summary of Changes

| What | File | Line(s) | Before | After |
|---|---|---|---|---|
| `parse_skill_toggle` parser | `src/repl_input.rs` | after line 255 | — | `pub(crate) fn parse_skill_toggle(line: &str) -> Option<(String, bool)>` |
| `BUILTIN_PREFIXES` constant | `src/repl_input.rs` | before fn | — | `const BUILTIN_PREFIXES: &[&str]` |
| 10 parser unit tests | `src/repl_input.rs` | after line 470 | — | `mod skill_toggle_tests` |
| Import `parse_skill_toggle` | `src/chat.rs` | line 15 | — | added to existing `use crate::repl_input::{...}` |
| Import `ToggleKind` | `src/chat.rs` | ~line 13 | — | `use crate::proto::gait::log::v1::ToggleKind;` |
| Skill-toggle handler block | `src/chat.rs` | between lines 155–157 | — | ~12 lines resolving, emitting, printing |
| Updated startup hint | `src/chat.rs` | line 21 | `… /u [N] undo …` | `… /u [N] undo  •  /skill enable …` |
| 3 Toggle round-trip tests | `src/proto/events.rs` | after line 527 | — | `toggle_skill_enabled_round_trips`, `…disabled…`, `…chains_seq…` |

No changes to `turn.rs`, `retrace.rs`, `ActiveAgent`, or any proto file — Phase
3b owns those. No changes to `repl_input::expand_input` — skill-toggle is not an
inline expander.

---

## Design Decisions

**Why `scan_skills` on every toggle, not a cached registry?**
The REPL session builds a fresh `Profile` per turn anyway (`chat.rs:185`). A one-
shot `read_dir` on toggle is far cheaper than a turn and avoids adding mutable
session state to `run_chat`. Skills can be added to disk and immediately toggled
without restarting.

**Why `continue` before `rl.add_history_entry`?**
Slash commands (`/n`, `/q`, `/u`) are never added to readline history — they are
navigation, not tasks. Skill-toggle follows the same convention. The toggle is
already durable via the session log; history would be noise.

**Why reject three-token lines like `/foo bar baz`?**
A future command extension (e.g. `/foo bar` with `bar` meaning something other
than `off`) should not silently treat the third token as garbage. Returning `None`
means an unrecognised form falls through to the model as a plain message, which is
the safe default.

**`off` case-insensitive?**
Yes — `/tdd OFF` and `/tdd off` both disable. This matches how most CLI flags
behave and is the least surprising for users.
