# Phase 2 — `Manifest → Agent`, `Compaction → Summary` rename (impl plan)

> Implements **Phase 2** of `docs/plans/2026-06-12-agent-checkpoint-model.md` (§0 I1/M4,
> §2 proto changes, §8 phase 2). Pure rename + envelope trim + old-log back-compat.
> **No new projection, no new delta events** — those are Phases 3a/3b.

## Goal

Rename the two checkpoint proto messages to their dimension names and trim the agent
envelope, **without breaking decode of pre-rename `sessions/*/log.jsonl`**:

1. Proto: `Manifest → Agent`, `Compaction → Summary`. Oneof arms keep their field
   numbers (`agent = 2`, `summary = 17`) — only the *names* change. Add
   `uint64 through_seq = 7;` to `Agent` (audit-only). **Remove `output_schema_retries`**
   from the envelope (M4) — it already lives on `Profile`/`ResolvedProfile` from
   `GaitConfig`, so it just stops being written to the event.
2. Rust: update every read/write site to `pb::Agent`, `payload::Kind::Agent`,
   `pb::Summary`, `payload::Kind::Summary`.
3. **Back-compat (I1):** pbjson generates a hand-rolled `visit_str` that matches the
   proto field *name* (snake+camel) with **no serde-alias support** — so renamed
   types stop decoding `"manifest"`/`"compaction"` keys. Fix with a **key-rewrite at
   the JSON decode chokepoint** plus a **pre-rename fixture test**.

## Architecture / key findings

- **Wire is camelCase JSON keyed by proto field name.** Build pipeline:
  `build.rs` → prost (structs) + pbjson (`Serialize`/`Deserialize`). pbjson's oneof
  deserializer is a hand-written `visit_str` matching e.g. `"compaction" => Compaction`
  (verified in the generated `gait.log.v1.serde.rs`, Payload `GeneratedField::visit_str`).
- **pbjson-build has NO alias mechanism** (only `ignore_unknown_fields`,
  `preserve_proto_field_names`, `emit_fields`, `btree_map`, …). prost's
  `field_attribute`/`type_attribute` only decorate prost's *own* structs, not pbjson's
  separately-generated `Deserialize`. **So `#[serde(alias="manifest")]` cannot be
  injected into the generated matcher.** ⇒ the I1 fix is a **pre-decode key rewrite**,
  not a serde attribute.
- **Decode chokepoint:** all old-log decode flows through `wire::read_events`
  (`src/proto/wire.rs:38`) — `Ingestor::events` → `transcript::read_events` →
  `wire::read_events`. Two *other* `from_str::<Event>` sites read persisted logs and
  also need the rewrite: `src/tools/list_agents.rs:81` (peer sessions) and
  `src/ui/renderer.rs:325` (replay render). `build.rs` does **not** set
  `ignore_unknown_fields`, so an un-rewritten `"manifest"` key is a hard decode error
  (silent-loss is actually a hard error here — good, the fixture will catch it).
- **M4 is write-side only.** `output_schema_retries` already lives on
  `ResolvedProfile.output_schema_retries` (sourced from `config.output_schema_retries`,
  `src/profile.rs:89`). Phase 2 just stops writing it into the `Agent` event and drops
  the field. No resolve-time change — the runtime value is unchanged.

## Tech stack

Rust 2021, prost 0.13 / pbjson 0.7, serde_json. Tests via `cargo test`. **No checked-in
codegen** — proto regenerates from `proto/log.proto` on build.

## Files

- `proto/log.proto` — rename messages + oneof arms; add `through_seq`; drop retries field.
- `src/proto/wire.rs` — add `compat_rewrite_keys` + apply in `read_events`; back-compat test.
- `src/proto/events.rs` — `manifest_msg`/`manifest`→`agent_msg`/`agent`; `compaction`→`summary`; drop retries write; update tests.
- `src/turn.rs` — `manifest`→`agent` local + emit + drift comment.
- `src/session/retrace.rs` — `ActiveManifest`→`ActiveAgent` (LWW shim, behavior-identical); `active_manifest`→`active_agent`; `Kind::Compaction`→`Kind::Summary`; `RetraceResult.active_manifest`; tests.
- `src/session/transcript.rs` — `last_compaction_index` `Kind::Compaction`→`Kind::Summary`.
- `src/tools/list_agents.rs` — route decode through compat rewrite.
- `src/ui/renderer.rs` — route decode through compat rewrite; comment.
- `src/proto/tests.rs` — `Manifest`→`Agent` smoke test.

> **NOTE on naming:** Phase 2 renames the *LWW shim* `ActiveManifest → ActiveAgent` and
> `active_manifest → active_agent` to match the new type, but it stays a **plain LWW
> register over the `Agent` event** (today's behavior). The real fold-rewrite
> (`checkpoint()`, seed-from-config, deleting `ActiveLoadables`) is **Phase 3a** — do
> NOT pull it forward here.

> **Build/test rule (from `gait-rs/CLAUDE.md`):** always redirect to a temp file then
> grep; never re-run to re-view. Prefix every cargo invocation with
> `cd /Users/aicoder/Documents/ideation/gait/gait-rs && `.

---

## Task 1 — Write the back-compat fixture test FIRST (red)

Lock the I1 contract before touching proto: a hand-written pre-rename log line with
`"manifest"`/`"compaction"` keys must decode. Written against the *future* type names so
it fails to compile until the rename lands — TDD red.

**Files:** `src/proto/wire.rs`

- [ ] In the `#[cfg(test)] mod tests` block of `src/proto/wire.rs` (after the existing
      tests near `src/proto/wire.rs:105`), add the fixture test. The JSON uses the
      **old** payload keys; the assertions reference the **new** `Kind::Agent` /
      `Kind::Summary` variants (which don't exist yet):
  ```rust
  #[test]
  fn pre_rename_log_decodes_via_compat_rewrite() {
      use crate::proto::gait::log::v1::{event, payload};
      let dir = tempfile::tempdir().unwrap();
      let path = dir.path().join("log.jsonl");
      // Two hand-written PRE-RENAME lines, exactly as old sessions persisted them:
      // a "manifest" payload (old Manifest) and a "compaction" payload (old Compaction).
      let old = concat!(
          r#"{"version":1,"id":"11111111-1111-4111-8111-111111111111","seq":1,"ts":1,"contentHash":"x","parents":[],"payload":{"manifest":{"model":"anthropic/x","instructions":"be terse","outputSchemaRetries":7}}}"#,
          "\n",
          r#"{"version":1,"id":"22222222-2222-4222-8222-222222222222","seq":2,"ts":2,"contentHash":"y","parents":["11111111-1111-4111-8111-111111111111"],"payload":{"compaction":{"summary":"did stuff","turnsCovered":3}}}"#,
          "\n",
      );
      std::fs::write(&path, old).unwrap();

      let events = read_events(&path).expect("pre-rename log must still decode");
      assert_eq!(events.len(), 2, "both legacy events decode");

      let k0 = match &events[0].body { Some(event::Body::Payload(p)) => p.kind.clone(), _ => None };
      assert!(matches!(k0, Some(payload::Kind::Agent(_))), "old \"manifest\" → Kind::Agent");
      let k1 = match &events[1].body { Some(event::Body::Payload(p)) => p.kind.clone(), _ => None };
      assert!(matches!(k1, Some(payload::Kind::Summary(_))), "old \"compaction\" → Kind::Summary");

      // outputSchemaRetries (dropped from the envelope, M4) is silently ignored, not an error.
      if let Some(payload::Kind::Agent(a)) = k0 { assert_eq!(a.model, "anthropic/x"); }
  }
  ```
  Cite: insert after `src/proto/wire.rs:105` (inside `mod tests`). `tempfile` is already
  a dev-dep (used elsewhere in this file's tests).
- [ ] Confirm it fails to compile (red — `Kind::Agent`/`Kind::Summary` undefined):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test --lib pre_rename_log_decodes 2>&1 > /tmp/p2-t1.txt; echo "exit: $?"
  grep -E "^error\[|cannot find|no variant" /tmp/p2-t1.txt | head
  ```
  Expected: compile errors naming `Agent`/`Summary` not found in `payload::Kind`.
- [ ] **Do not commit yet** — this test is the acceptance gate for Tasks 2–4.

---

## Task 2 — Proto rename + envelope trim

Rename the messages and oneof arms (keep field numbers), add `through_seq` to `Agent`,
remove `output_schema_retries` from the envelope.

**Files:** `proto/log.proto`

- [ ] `proto/log.proto:74` — in `Payload.oneof kind`, rename the arm (keep number `2`):
  ```proto
  Agent              agent               = 2;
  ```
  (was `Manifest manifest = 2;`).
- [ ] `proto/log.proto:87` — rename the arm (keep number `17`):
  ```proto
  Summary            summary             = 17;
  ```
  (was `Compaction compaction = 17;`).
- [ ] `proto/log.proto:143-151` — replace the `Manifest` message with `Agent`: rename,
      add `through_seq = 7`, **drop** `output_schema_retries`:
  ```proto
  // The agent-dimension checkpoint: a full snapshot of the composed agent state.
  // First instance = opening. Re-emitted whole on drift (Phase 2: LWW, like today).
  message Agent {
    string model                 = 1;   // LWW-projected (LWW-Register). "provider/model-id".
    string instructions          = 2;   // composed system prompt — stable layers (Phase 2: full, as today).
    repeated string tools        = 3;   // resolved tool names — AUDIT-ONLY
    repeated string mcp_servers  = 4;
    repeated string skills       = 5;
    PermissionPolicy permissions = 6;
    uint64 through_seq           = 7;   // audit metadata only — reset is positional (C1)
    // output_schema_retries removed (M4) — read from gait.jsonc/GaitConfig at resolve time
  }
  ```
  **Field-number note:** old `Manifest` had `instructions = 6`; new `Agent` puts
  `instructions = 2`. The wire is JSON keyed by *name*, so the JSON key changes from
  `"instructions"` (unchanged name) — the number is schema-identity only and instructions
  was already field 6 carrying camelCase `"instructions"`; reordering to field 2 keeps the
  key `"instructions"`. tools/mcp/skills/permissions keys are likewise name-stable. The
  only renamed/removed keys at the *payload-kind* level are `manifest`→`agent` (handled by
  Task 4 rewrite) and the dropped `outputSchemaRetries` (handled: silently ignored on read
  per Task 4; the fixture asserts no error).
- [ ] `proto/log.proto:182-187` — rename `Compaction` → `Summary` (fields unchanged):
  ```proto
  message Summary {
    string summary        = 1;
    uint32 turns_covered  = 2;
    CompactionUsage usage = 3;
    optional uint64 through_seq = 4;      // seq boundary it summarizes through; unset = none
  }
  ```
  (Keep `CompactionUsage` as the usage type name — renaming it is out of scope; only the
  two checkpoint messages rename. Confirm `CompactionUsage` at `proto/log.proto:132` is
  untouched.)
- [ ] Update the doc comment at `proto/log.proto:9` block / nearby if it references
      `Manifest`/`Compaction` by name (grep the proto): none expected besides the messages
      themselves — skip if no hits.
- [ ] Regenerate + check the codegen compiles the *types* (call sites will still be red):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && touch build.rs && cargo build 2>&1 > /tmp/p2-t2.txt; echo "exit: $?"
  grep -E "^error" /tmp/p2-t2.txt | grep -iv "Manifest|Compaction" | head
  grep -c "cannot find.*Manifest\|cannot find.*Compaction\|no variant.*Manifest\|no variant.*Compaction" /tmp/p2-t2.txt
  ```
  Expected: errors ONLY about old `Manifest`/`Compaction`/`Kind::Manifest`/`Kind::Compaction`
  references in `src/` (Tasks 3–5 fix them). No errors about the proto/pbjson build itself.
- [ ] No commit yet (tree doesn't compile).

---

## Task 3 — Update Rust write sites + drift-guard rename

`src/proto/events.rs`, `src/turn.rs`. Rename the builder fns and the live emit.

**Files:** `src/proto/events.rs`, `src/turn.rs`

- [ ] `src/proto/events.rs:251-266` — rename `manifest_msg` → `agent_msg`, return
      `pb::Agent`, drop the retries write, set `through_seq: 0`:
  ```rust
  pub fn agent_msg(resolved: &crate::profile::ResolvedProfile) -> pb::Agent {
      let mut mcp_servers: Vec<String> = resolved.mcp_clients.keys().cloned().collect();
      mcp_servers.sort();
      pb::Agent {
          model: resolved.model.clone(),
          instructions: resolved.instructions.clone(),
          tools: resolved.tools.iter().map(|t| t.name().to_string()).collect(),
          mcp_servers,
          skills: resolved.skill_registry.iter().map(|s| s.name.clone()).collect(),
          permissions: Some(pb::PermissionPolicy {
              allow: resolved.allow.clone(),
              deny: resolved.deny.clone(),
          }),
          through_seq: 0,   // audit-only; written 0, never read (C1)
      }
  }
  ```
  (Update the doc comment at `src/proto/events.rs:245` `Manifest`→`Agent`.)
- [ ] `src/proto/events.rs:268-271` — rename `manifest` → `agent`, use the renamed arm:
  ```rust
  /// The `Agent` checkpoint event body.
  pub fn agent(resolved: &crate::profile::ResolvedProfile) -> pb::event::Body {
      wire::body(pb::payload::Kind::Agent(agent_msg(resolved)))
  }
  ```
- [ ] `src/proto/events.rs:317-332` — rename `compaction` → `summary`, build `pb::Summary`:
  ```rust
  /// `Summary` — through_seq unset (no seq in legacy data).
  pub fn summary(
      summary: String,
      turns_covered: u32,
      usage: &tx::CompactionUsage,
  ) -> pb::event::Body {
      wire::body(pb::payload::Kind::Summary(pb::Summary {
          summary,
          turns_covered,
          usage: Some(pb::CompactionUsage {
              input: usage.input,
              output: usage.output,
          }),
          through_seq: None,
      }))
  }
  ```
- [ ] Find any caller of `events::compaction(` and rename to `events::summary(`:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -rn "events::compaction\|::compaction(" src/ | grep -v "Kind::Compaction\|CompactionUsage"
  ```
  Rename each hit (expected: compaction-trigger site in the compaction/turn flow).
- [ ] `src/turn.rs:404` — rename the local + builder call:
  ```rust
  let agent = crate::proto::events::agent_msg(&caps);
  ```
  (was `let manifest = ...manifest_msg(&caps);`). Update the comment block at
  `src/turn.rs:401-403` `Manifest`→`Agent`.
- [ ] `src/turn.rs:583-594` — rename the drift-guard block + emit:
  ```rust
  // Emit the live Agent checkpoint iff it differs from the last one in the log
  // (drift guard). First run of a session has no prior Agent → emit.
  {
      let events_for_guard = ingestor.events()?;
      if retrace::active_agent(&events_for_guard) != Some(agent.clone()) {
          emitter.emit(wire::body(payload::Kind::Agent(agent)))?;
      }
  }
  ```
  (`active_agent` defined in Task 4. `payload::Kind::Agent`.)
- [ ] `src/turn.rs:365` — leave `output_schema_retries: 3,` (it's the curate
      `ResolvedProfile`, unrelated to the envelope; unchanged by M4).
- [ ] Do not build yet — read sites (Task 4) still reference old names.

---

## Task 4 — Update Rust read sites + the compat key-rewrite (green for Task 1)

The core I1 fix lands here. Rename the LWW shim, the `Kind::Compaction` matches, and add
`compat_rewrite_keys` at the decode chokepoint.

**Files:** `src/session/retrace.rs`, `src/session/transcript.rs`, `src/proto/wire.rs`,
`src/tools/list_agents.rs`, `src/ui/renderer.rs`, `src/proto/tests.rs`

- [ ] `src/proto/wire.rs` — add the compat rewrite helper (place above `read_events`,
      after the imports near `src/proto/wire.rs:18`):
  ```rust
  /// Back-compat (I1): pre-rename logs carry `"manifest"`/`"compaction"` payload keys.
  /// After the Phase-2 rename the pbjson deserializer only knows `"agent"`/`"summary"`,
  /// and `ignore_unknown_fields` is OFF, so an old key is a hard decode error. pbjson has
  /// no serde-alias support, so we rewrite the legacy keys in the raw JSON line before
  /// decode. Cheap string-level swap on the `payload` object's single kind key.
  fn compat_rewrite_keys(line: &str) -> std::borrow::Cow<'_, str> {
      // Fast path: untouched lines (the overwhelming majority) clone nothing.
      if !line.contains("\"manifest\"") && !line.contains("\"compaction\"") {
          return std::borrow::Cow::Borrowed(line);
      }
      // Parse → rewrite the payload kind key → reserialize. Robust vs. string splicing
      // (won't corrupt a "manifest" substring inside instructions/summary text).
      let mut v: serde_json::Value = match serde_json::from_str(line) {
          Ok(v) => v,
          Err(_) => return std::borrow::Cow::Borrowed(line), // let read_events surface the error
      };
      if let Some(payload) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
          if let Some(inner) = payload.remove("manifest") {
              payload.insert("agent".to_string(), inner);
          }
          if let Some(inner) = payload.remove("compaction") {
              payload.insert("summary".to_string(), inner);
          }
      }
      std::borrow::Cow::Owned(v.to_string())
  }
  ```
- [ ] `src/proto/wire.rs:44-49` — apply it in the decode loop:
  ```rust
      for line in text.lines() {
          if line.trim().is_empty() {
              continue;
          }
          let line = compat_rewrite_keys(line);
          events.push(serde_json::from_str::<Event>(&line)?);
      }
  ```
- [ ] `src/proto/wire.rs` — make the helper reusable by the other two decode sites:
      change `fn compat_rewrite_keys` to `pub(crate) fn compat_rewrite_keys`.
- [ ] `src/tools/list_agents.rs:81` — route the peer-session decode through it:
  ```rust
      .filter_map(|l| {
          let l = crate::proto::wire::compat_rewrite_keys(l);
          serde_json::from_str::<Event>(&l).ok()
      })
  ```
  (Confirm the closure binding `l` is a `&str`; adjust deref as the compiler requires.)
- [ ] `src/ui/renderer.rs:325` — same rewrite before decode:
  ```rust
      let json_line = crate::proto::wire::compat_rewrite_keys(json_line);
      match serde_json::from_str::<pb::Event>(&json_line) {
  ```
  Add a one-line comment: `// compat: rewrite pre-rename manifest/compaction keys (I1)`.
- [ ] `src/session/retrace.rs:132-144` — rename the LWW shim `ActiveManifest` →
      `ActiveAgent`, state `Option<pb::Agent>`, match `Kind::Agent`. **Keep it a plain
      LWW register** (Phase 3a does the fold rewrite):
  ```rust
  /// Last-write-wins register over `Agent` checkpoint events. (Phase 2: LWW shim,
  /// behavior-identical to the old ActiveManifest. The fold/seed rewrite is Phase 3a.)
  pub(crate) struct ActiveAgent;
  impl Projection for ActiveAgent {
      type State = Option<pb::Agent>;
      fn seed(&self) -> Self::State { None }
      fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
          if let payload::Kind::Agent(a) = kind {
              *state = Some(a.clone());
          }
      }
  }
  ```
  Update the module-doc bullet at `src/session/retrace.rs:9` `Manifest`→`Agent`.
- [ ] `src/session/retrace.rs:184-206` — **rename the struct `ActiveMessages` → `ActiveSummary`**
      (mirrors the proto `Compaction`→`Summary` event and the projection-naming rule
      `Active<Event>`). In its `apply`, rename the `Kind::Compaction(c)` arm to
      `Kind::Summary(c)` (body unchanged — still `clear()` + push
      `format!("[Session history summary: {}]", c.summary)`). Update the doc comment at
      `src/session/retrace.rs:184` `Compaction`→`Summary`. Then update **every use site** of
      the struct (grep `ActiveMessages` across `src/`): the `run_projection(&ActiveMessages, …)`
      call in `fold` and the `messages()`/`messages_from_*` accessors all become `ActiveSummary`.
      (Pure rename — behavior-identical. Note: Phase 1 introduced `ActiveMessages::checkpoint`;
      it rides along to `ActiveSummary::checkpoint` here.)
- [ ] `src/session/retrace.rs:520-521` — rename the `RetraceResult` field:
  ```rust
      /// The most recent `Agent` checkpoint payload, if any (LWW-Register).
      pub active_agent: Option<pb::Agent>,
  ```
- [ ] `src/session/retrace.rs:532-548` — in `fold`: update the doc comment list
      (`ActiveManifest`→`ActiveAgent`), rename the local + struct init:
  ```rust
      let active_agent = run_projection(&ActiveAgent, all_events, &mutations);
      ...
      Ok(RetraceResult { messages, enabled_loadables, active_agent, effective_allow, mutations })
  ```
- [ ] `src/session/retrace.rs:558-562` — rename the shim fn:
  ```rust
  /// Most recent `Agent` checkpoint payload (LWW-Register). Shim over the `ActiveAgent`
  /// projection so the drift-guard in run_turn is untouched.
  pub fn active_agent(events: &[Event]) -> Option<pb::Agent> {
      run_projection(&ActiveAgent, events, &HashMap::new())
  }
  ```
- [ ] Find any OTHER reader of `RetraceResult.active_manifest` / `retrace::active_manifest`:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -rn "active_manifest\|\.active_manifest" src/ | grep -v retrace.rs
  ```
  Rename each hit to `active_agent`.
- [ ] `src/session/transcript.rs:136-138` — `last_compaction_index`: rename the match to
      `Kind::Summary(_)`. **Keep the fn name `last_compaction_index`** (it's the message-
      checkpoint index; renaming the fn is cosmetic and out of Phase-2 scope) OR rename to
      `last_summary_index` and fix callers — pick the minimal:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -rn "last_compaction_index" src/
  ```
  If ≤2 callers, rename fn → `last_summary_index`; else keep the name, only swap the
  `Kind::Summary(_)` match. Document the choice in the commit message.
- [ ] `src/proto/tests.rs:101-105` — update the smoke test:
  ```rust
  #[test]
  fn agent_event_exists_resolution_gone() {
      use super::gait::log::v1::{payload, Agent};
      let _ = payload::Kind::Agent(Agent::default());
  }
  ```
- [ ] Build:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/p2-t4.txt; echo "exit: $?"
  grep -E "^error" /tmp/p2-t4.txt | head -20
  ```
  Expected: `exit: 0`, no `^error` lines. If errors remain, they name the few
  test-only `pb::Manifest`/`pb::Compaction` literals fixed in Task 5 — finish those
  first if the lib build itself fails.
- [ ] Run the Task-1 fixture (now green):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test --lib pre_rename_log_decodes 2>&1 > /tmp/p2-t4-fix.txt; echo "exit: $?"
  grep -E "test result|pre_rename" /tmp/p2-t4-fix.txt
  ```
  Expected: `test result: ok. 1 passed`.

---

## Task 5 — Fix the proto-events unit tests + full test sweep

The rename leaves `pb::Manifest`/`pb::Compaction` literals in test modules.

**Files:** `src/proto/events.rs`, `src/session/retrace.rs`

- [ ] `src/proto/events.rs:489-505` — the `manifest`-mapping test: drop the retries write
      and assertion (envelope no longer has the field), rename builder + variant:
  ```rust
      profile.model = "anthropic/x".into();
      profile.instructions = "be terse".into();
      // (output_schema_retries removed from the envelope — M4)

      let body = agent(&profile);
      let r = match kind_of(&body) {
          pb::payload::Kind::Agent(r) => r.clone(),
          other => panic!("expected Agent, got {other:?}"),
      };
      assert_eq!(r.model, "anthropic/x");
      let perms = r.permissions.as_ref().expect("permissions");
      assert_eq!(perms.allow, vec!["read".to_string()]);
      assert_eq!(perms.deny, vec!["shell:rm *".to_string()]);
      assert!(r.mcp_servers.is_empty());
      assert!(r.skills.is_empty());
  }
  ```
  (Delete the `profile.output_schema_retries = 7;` line and the
  `assert_eq!(r.output_schema_retries, Some(7));` line. Rename the test fn
  `manifest_*` → `agent_*` for clarity.)
- [ ] `src/proto/events.rs:507-527` — `manifest_equality_is_mcp_server_order_sensitive`:
      rename → `agent_equality_*`, build `pb::Agent`, drop `output_schema_retries`, add
      `through_seq: 0`, set `instructions: String::new()`:
  ```rust
      let r1 = pb::Agent {
          model: "m".into(),
          instructions: String::new(),
          tools: vec![],
          mcp_servers: vec!["zeta".into(), "alpha".into()],
          skills: vec![],
          permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
          through_seq: 0,
      };
      let r2 = pb::Agent {
          mcp_servers: vec!["alpha".into(), "zeta".into()],
          ..r1.clone()
      };
      assert_ne!(r1, r2, "agent equality is sensitive to mcp_servers order");
  ```
- [ ] `src/session/retrace.rs:733-744` — the two-`Manifest` test: rename
      `Kind::Manifest`→`Kind::Agent`, `pb::Manifest`→`pb::Agent`, drop
      `output_schema_retries: None`, drop `instructions: String::new()` only if it stays a
      valid field (it does — `Agent.instructions` exists), set `through_seq: 0`:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -n "pb::Manifest\|Kind::Manifest\|ActiveManifest\|output_schema_retries" src/session/retrace.rs
  ```
  Fix each: `pb::Manifest {... instructions: String::new(), output_schema_retries: None, }`
  → `pb::Agent { ... instructions: String::new(), through_seq: 0, }`; `run_projection(&ActiveManifest, ...)`
  → `run_projection(&ActiveAgent, ...)`; assertion comments `Manifest`→`Agent`.
  Sites: `src/session/retrace.rs:733-744`, `:783-787`, `:795-799` (and any the grep surfaces).
- [ ] Full test run:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep -E "^error" /tmp/gait-test-output.txt | head -20
  grep -E "FAILED|test result" /tmp/gait-test-output.txt
  ```
  Expected: `exit: 0`, no `^error`, no `FAILED`, all `test result: ok`.
- [ ] Final residue sweep — must be empty except `CompactionUsage` (kept) and the
      `last_compaction_index` fn name if you kept it:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -rn "Manifest\|Kind::Compaction\|pb::Compaction\|::manifest_msg\|output_schema_retries" src/ | grep -v "CompactionUsage\|// \|^.*last_compaction_index"
  ```
  Expected: no output (or only intentional comments). Investigate any hit before commit.

---

## Task 6 — Commit

- [ ] Verify clean build + tests one final time (reuse `/tmp/gait-test-output.txt` from
      Task 5 — do not re-run unless the tree changed since).
- [ ] Stage and commit:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait && git add proto/log.proto gait-rs/src gait-rs/docs/plans/2026-06-12-impl-phase2-rename.md
  git commit -m "$(cat <<'EOF'
feat(proto): rename Manifest→Agent, Compaction→Summary; trim envelope (Phase 2)

- proto: rename the two checkpoint messages and their oneof arms (field numbers
  unchanged: agent=2, summary=17); add Agent.through_seq (audit-only); drop
  output_schema_retries from the envelope (M4 — it stays on ResolvedProfile from
  GaitConfig, read at resolve time).
- rust: update all read/write sites to pb::Agent / Kind::Agent / pb::Summary /
  Kind::Summary; ActiveManifest→ActiveAgent (LWW shim, behavior-identical);
  active_manifest→active_agent.
- back-compat (I1): pbjson has no serde-alias support and its visit_str matches the
  proto field name only, so old "manifest"/"compaction" log keys would fail to decode
  (ignore_unknown_fields is OFF). Add wire::compat_rewrite_keys at the decode
  chokepoint (read_events) and the two other persisted-log decode sites
  (list_agents, renderer). Pre-rename log.jsonl fixture test asserts both legacy keys
  still decode to the new Kind variants and the dropped outputSchemaRetries is ignored.

Phase 2 of docs/plans/2026-06-12-agent-checkpoint-model.md. Pure rename + envelope
trim + back-compat; the fold rewrite (checkpoint(), seed-from-config, ActiveLoadables
deletion) is Phase 3a.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
  ```

---

## Acceptance checklist

- [ ] `proto/log.proto`: `Agent`/`Summary` messages; arms `agent=2`, `summary=17`;
      `Agent.through_seq=7`; no `output_schema_retries` in the envelope.
- [ ] No `pb::Manifest` / `pb::Compaction` / `Kind::Manifest` / `Kind::Compaction` /
      `ActiveManifest` / `manifest_msg` anywhere in `src/` (CompactionUsage excepted).
- [ ] `cargo build` and `cargo test` both `exit: 0`, no `FAILED`.
- [ ] `pre_rename_log_decodes_via_compat_rewrite` passes (the I1 gate).
- [ ] `compat_rewrite_keys` applied at all three persisted-log decode sites
      (`wire::read_events`, `list_agents.rs`, `renderer.rs`).
- [ ] Runtime `output_schema_retries` still flows `GaitConfig → Profile → ResolvedProfile`
      unchanged (only the envelope write was removed).
