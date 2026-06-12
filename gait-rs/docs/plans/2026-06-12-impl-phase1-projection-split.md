# Phase 1 — Projection trait split (checkpoint generalization)

> Implements **Phase 1** of the agent-checkpoint refactor
> (`docs/plans/2026-06-12-agent-checkpoint-model.md`, §3 + §8.1). **Pure refactor,
> ZERO behavior change.** The reset stays **positional** (C1); `ActiveMessages`'s
> checkpoint reproduces today's synthesized-summary reset exactly (C2).

## Goal

Generalize the projection fold so a checkpoint (today: `Compaction`) is a framework
concern handled by `run_projection`, not an `ActiveMessages` special-case in its `apply`.

## Architecture

`Projection` gains a `checkpoint(&self, kind) -> Option<Self::State>` hook (default
`None`). `run_projection`'s inner loop checks `checkpoint(kind)` first: a `Some(base)`
resets the fold's running state to that baseline (positionally, where the event sits in
the stream); a `None` falls through to the existing `apply`. `ActiveMessages`'s current
inline `Compaction` arm — which clears `msgs` and pushes a synthesized
`[Session history summary: …]` user message — moves verbatim into an
`ActiveMessages::checkpoint` impl that *returns* that same one-message baseline state.
No projection other than `ActiveMessages` overrides `checkpoint`, so every other fold is
byte-identical.

## Tech Stack

Rust, single crate `gait-rs`. No proto changes, no new events, no new dependencies. Edits
confined to one file (`src/session/retrace.rs`) plus one new test in the same file's
`#[cfg(test)]` module.

## Files

- **Modify** `src/session/retrace.rs`
  - `Projection` trait (`retrace.rs:87`): add the `checkpoint` default method.
  - `run_projection` (`retrace.rs:97`): change the inner loop to branch on `checkpoint`.
  - `ActiveMessages` (`retrace.rs:189`): remove the `Compaction` arm from `apply`; add a
    `checkpoint` impl returning the summary baseline. Refactor the summary-message
    construction into a shared `MessagesState::with_summary` constructor so `apply` (now
    deleted) and `checkpoint` can't drift.
  - test module (`retrace.rs:584`): add ONE new regression test
    `active_messages_two_compactions_second_wins`.

---

## Task 1 — Add the `checkpoint` hook to the `Projection` trait

**Files**
- Modify `src/session/retrace.rs:87-95` (trait `Projection`).

**Steps**
- [ ] Edit the trait. BEFORE (`retrace.rs:87-95`):
  ```rust
  pub(crate) trait Projection {
      type State;
      /// Initial state before any events (seeded from config where relevant).
      fn seed(&self) -> Self::State;
      /// Fold one event's effective payload kind into the state.
      fn apply(&self, state: &mut Self::State, event: &Event, kind: &payload::Kind);
      /// Finalize (default: identity).
      fn finish(&self, state: Self::State) -> Self::State { state }
  }
  ```
  AFTER:
  ```rust
  pub(crate) trait Projection {
      type State;
      /// Initial state before any events (seeded from config where relevant).
      fn seed(&self) -> Self::State;
      /// If `kind` is a checkpoint for THIS projection's dimension, return the
      /// baseline state it establishes — the fold resets to it (positionally,
      /// where the event sits in the stream; `through_seq` is NOT consulted).
      /// Default: not a checkpoint for any projection.
      fn checkpoint(&self, _kind: &payload::Kind) -> Option<Self::State> { None }
      /// Fold one event's effective payload kind into the state.
      fn apply(&self, state: &mut Self::State, event: &Event, kind: &payload::Kind);
      /// Finalize (default: identity).
      fn finish(&self, state: Self::State) -> Self::State { state }
  }
  ```
- [ ] Build (the trait now compiles; no override yet, default keeps every impl valid):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```
  Expected: `exit: 0`, no `error` lines.
- [ ] Commit:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/session/retrace.rs && git commit -m "$(cat <<'EOF'
refactor(retrace): add Projection::checkpoint hook (default None)

Phase 1 step 1 of the agent-checkpoint split: a projection can now declare
a checkpoint kind for its dimension. Default returns None, so every existing
projection is unchanged. No call site uses it yet.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
  ```

---

## Task 2 — Branch `run_projection` on `checkpoint`

**Files**
- Modify `src/session/retrace.rs:97-107` (`run_projection`).

This is wired before any projection overrides `checkpoint`, so it is still a no-op behavior
change at this point (every `checkpoint` returns `None` → always falls through to `apply`).
All existing tests must still pass.

**Steps**
- [ ] Edit the loop. BEFORE (`retrace.rs:97-107`):
  ```rust
  pub(crate) fn run_projection<P: Projection>(
      p: &P,
      all_events: &[Event],
      mutations: &HashMap<String, MutationState>,
  ) -> P::State {
      let mut state = p.seed();
      for (ev, kind) in effective_kinds(all_events, mutations) {
          p.apply(&mut state, ev, kind);
      }
      p.finish(state)
  }
  ```
  AFTER:
  ```rust
  pub(crate) fn run_projection<P: Projection>(
      p: &P,
      all_events: &[Event],
      mutations: &HashMap<String, MutationState>,
  ) -> P::State {
      let mut state = p.seed();
      for (ev, kind) in effective_kinds(all_events, mutations) {
          match p.checkpoint(kind) {
              // A checkpoint resets the fold baseline positionally (C1: where the
              // event sits in the stream; `through_seq` is never consulted).
              Some(base) => state = base,
              None => p.apply(&mut state, ev, kind),
          }
      }
      p.finish(state)
  }
  ```
- [ ] Run the full retrace test module (everything still green; `Compaction` is still
  handled by `ActiveMessages::apply` at this point):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test session::retrace 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -20
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `exit: 0`, a `test result: ok.` line, no `FAILED`. In particular
  `compaction_breakpoint_discards_earlier_events` and `active_messages_resets_on_compaction`
  still pass (handled by `apply`, untouched).
- [ ] Commit:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/session/retrace.rs && git commit -m "$(cat <<'EOF'
refactor(retrace): run_projection consults checkpoint() before apply

The fold now resets to a projection's checkpoint baseline when one is
returned, else falls through to apply. No projection overrides checkpoint
yet, so behavior is unchanged (every checkpoint() == None). Reset is
positional — through_seq is never read (C1).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
  ```

---

## Task 3 — Add the failing two-compaction regression test (C2)

Write the new test FIRST. It must already pass against today's `apply`-based handling
(the current code already wipes prior summaries — the second `Compaction`'s `clear()`
drops the first summary). Running it now proves it captures today's behavior; it then
guards the Task-4 move. Today only `active_messages_resets_on_compaction` (single
compaction) is tested; this adds the missing two-compaction case the spec demands.

**Files**
- Modify `src/session/retrace.rs` test module — insert directly after
  `active_messages_resets_on_compaction` (ends `retrace.rs:870`).

**Steps**
- [ ] Insert the test after line 870 (after the closing `}` of
  `active_messages_resets_on_compaction`):
  ```rust
  #[test]
  fn active_messages_two_compactions_second_wins() {
      use std::collections::HashMap;
      // Two Compaction events in one log. Each resets the message context to its
      // own summary; the SECOND summary replaces the first (positional reset, C1).
      let cu = CompactionUsage { input: 1, output: 1 };
      let evs = mint_events(vec![
          events::user_turn("before-first".into(), None, None),
          events::compaction("first summary".into(), 1, &cu),
          events::user_turn("between".into(), None, None),
          events::compaction("second summary".into(), 1, &cu),
          events::user_turn("after-second".into(), None, None),
      ]);
      let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
      // Only the second summary + the turn after it survive; the first summary and
      // everything before the second compaction are gone.
      assert_eq!(msgs.len(), 2, "second summary + post-second turn, got: {msgs:?}");
      assert!(matches!(&msgs[0],
          Message::User { content: MessageContent::Text(t) }
          if t.contains("second summary") && !t.contains("first summary")),
          "second compaction's summary replaces the first");
      assert!(matches!(&msgs[1],
          Message::User { content: MessageContent::Text(t) } if t == "after-second"));
  }
  ```
- [ ] Run just this test — it must PASS now (it documents today's behavior):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test active_messages_two_compactions_second_wins 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `exit: 0`, `test result: ok. 1 passed`.
- [ ] Commit:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/session/retrace.rs && git commit -m "$(cat <<'EOF'
test(retrace): add two-compaction regression (second summary wins)

C2: pins the behavior that a second Compaction's summary replaces the
first's, before moving the reset into checkpoint(). Passes against today's
apply-based handling.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
  ```

---

## Task 4 — Move `ActiveMessages` compaction handling into `checkpoint` (C2)

Move the summary-baseline construction out of `apply` and into a `checkpoint` impl. Extract
a `MessagesState::with_summary` constructor so the exact synthesized message
(`[Session history summary: {summary}]`) lives in one place. After this, the `Compaction`
arm is gone from `apply`; the framework loop (Task 2) drives the reset.

**Files**
- Modify `src/session/retrace.rs:177-181` (`MessagesState` struct) — add the constructor.
- Modify `src/session/retrace.rs:189-360` (`impl Projection for ActiveMessages`) — add
  `checkpoint`, delete the `Compaction` arm from `apply`.

**Steps**
- [ ] Add the constructor on `MessagesState`. BEFORE (`retrace.rs:177-181`):
  ```rust
  /// Accumulator for the `ActiveMessages` projection.
  pub(crate) struct MessagesState {
      msgs: Vec<Message>,
      phase: MsgPhase,
  }
  ```
  AFTER:
  ```rust
  /// Accumulator for the `ActiveMessages` projection.
  pub(crate) struct MessagesState {
      msgs: Vec<Message>,
      phase: MsgPhase,
  }
  impl MessagesState {
      /// The post-compaction baseline: a context restarted from the synthesized
      /// summary user message. Reproduces the historical clear()-then-push reset.
      fn with_summary(summary: &str) -> Self {
          MessagesState {
              msgs: vec![Message::User {
                  content: MessageContent::Text(
                      format!("[Session history summary: {}]", summary)),
              }],
              phase: MsgPhase::Other,
          }
      }
  }
  ```
- [ ] Add the `checkpoint` impl and delete the `Compaction` arm. In
  `impl Projection for ActiveMessages` (`retrace.rs:190-195`), BEFORE:
  ```rust
  impl Projection for ActiveMessages {
      type State = MessagesState;
      fn seed(&self) -> Self::State {
          MessagesState { msgs: vec![], phase: MsgPhase::Other }
      }
      fn apply(&self, st: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
          match kind {
              payload::Kind::Compaction(c) => {
  ```
  AFTER:
  ```rust
  impl Projection for ActiveMessages {
      type State = MessagesState;
      fn seed(&self) -> Self::State {
          MessagesState { msgs: vec![], phase: MsgPhase::Other }
      }
      /// `Compaction` is the message dimension's checkpoint: reset the context to a
      /// baseline restarted from the synthesized summary (C2 — a fold-from-payload,
      /// not empty). Reset is positional (C1): `through_seq` is never read.
      fn checkpoint(&self, kind: &payload::Kind) -> Option<Self::State> {
          if let payload::Kind::Compaction(c) = kind {
              Some(MessagesState::with_summary(&c.summary))
          } else {
              None
          }
      }
      fn apply(&self, st: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
          match kind {
  ```
  Then DELETE the old `Compaction` arm body (`retrace.rs:197-206`), which is:
  ```rust
              payload::Kind::Compaction(c) => {
                  // Restart the message context from the summary (state projections
                  // carry forward; only this projection resets).
                  st.msgs.clear();
                  st.msgs.push(Message::User {
                      content: MessageContent::Text(
                          format!("[Session history summary: {}]", c.summary)),
                  });
                  st.phase = MsgPhase::Other;
              }

  ```
  The next arm (`payload::Kind::ToolCall(tc) =>`) becomes the first arm of `apply`'s match.
  > Note: with the `Compaction` arm gone, `apply` no longer reads any `Compaction`. The
  > `_ => { st.phase = MsgPhase::Other; }` catch-all at `retrace.rs:357` still covers every
  > other non-rendering kind unchanged — and `Compaction` can never reach `apply` now,
  > because `run_projection` intercepts it via `checkpoint` first.
- [ ] Build, then run the WHOLE retrace module — all existing compaction tests plus the new
  two-compaction test must pass unchanged:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error\|^warning: unused" /tmp/gait-build-output.txt | head -20
  cargo test session::retrace 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: build `exit: 0` with no `error` and no `unused` warning (the extracted
  constructor and `checkpoint` are both used); test `exit: 0`, `test result: ok.`, no
  `FAILED`. Specifically these stay green:
  `compaction_breakpoint_discards_earlier_events`, `active_messages_resets_on_compaction`,
  `active_messages_two_compactions_second_wins`, `loadables_projection_full_session_and_mutation_aware`
  (proves the OTHER projections do NOT reset on `Compaction` — their `checkpoint` still
  returns `None`).
- [ ] Run the full suite once to confirm nothing outside retrace regressed:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -20
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `exit: 0`, every `test result` line `ok.`, no `FAILED`.
- [ ] Commit:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/session/retrace.rs && git commit -m "$(cat <<'EOF'
refactor(retrace): ActiveMessages compaction reset via checkpoint()

Move the Compaction handling out of ActiveMessages::apply into a
checkpoint() impl returning the summary baseline (C2 — fold-from-payload
containing the synthesized [Session history summary: …] message, not
empty). The framework loop now drives the reset; the reset stays positional
(C1, through_seq untouched). Behavior identical — verified by the existing
compaction tests plus the new two-compaction regression. State projections
keep checkpoint() == None, so they still ignore Compaction.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
  ```

---

## Done-when (Phase 1 exit criteria)

- [ ] `Projection::checkpoint` exists with a `None` default; `run_projection` branches on it.
- [ ] `ActiveMessages` handles `Compaction` ONLY via `checkpoint`; its `apply` has no
  `Compaction` arm.
- [ ] No `through_seq` comparison anywhere (C1 — grep confirms):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -n "through_seq" src/session/retrace.rs
  ```
  Expected: no matches in `retrace.rs` (it is neither read nor compared by the fold).
- [ ] No proto changes, no new event kinds — only the one new test
  (`active_messages_two_compactions_second_wins`).
- [ ] `cargo test` fully green.
