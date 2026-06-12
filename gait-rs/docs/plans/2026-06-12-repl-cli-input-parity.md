# REPL/CLI Input Parity & Fixes — Implementation Plan

> **For agentic workers:** task-by-task. Steps use checkbox (`- [ ]`) syntax. Dogfood split noted per task: **[self]** = controller implements (keystone/error-prone); **[gait]** = delegate to the gait agent with intent + a pointer to this spec, never verbatim code; always independently verify (build + test + diff).

**Goal:** Make the `gait chat` REPL and the `gait run` CLI expand input identically, let `chat` resume a session, and give the permission prompt a real two-axis (scope × pattern) grammar.

**Architecture:** One `expand_input(input, &ExpandCtx)` single-pass scanner replaces the two divergent expanders (`expand_at_markers`, `expand_quote_reply`); both call sites build an `ExpandCtx` and call it. No cascade — each region of the *original* input is matched once and its replacement is never re-scanned (so `>(N)` agent text can never trigger `$(...)` shell execution). `chat` gains `--session`. The permission parser becomes a two-axis grammar with create-if-missing persistence.

**Tech Stack:** Rust, existing `repl_input.rs` / `ui/permission.rs` / `session/retrace.rs` / `bin/gait.rs` / `chat.rs`.

---

## Background (confirmed against live code, 2026-06-12)

- **REPL** (`chat.rs:157`) runs only `expand_quote_reply` → `>(N)` works, `$(cmd)`/`$(N)`/`@path` do **not**.
- **CLI** (`gait.rs:253`) runs only `expand_at_markers` → `$(…)`/`@` work, `>(N)` does **not**.
- `>(N)` needs the prior turn's blocks. Those are already persisted: `TurnComplete.blocks: Vec<String>` (`proto/events.rs:216`, written at `turn.rs:752`). The CLI can load them from the log.
- `cmd_chat` (`gait.rs:832`) parses only `--codebase/--gait-dir/--agent`; `run_chat(gait_dir, codebase, agent_name)` always mints a fresh session → **no resume**. `gait run` already supports `--session` (`gait.rs:191`).
- `parse_permission_response` (`ui/permission.rs:40`) matches *exactly* `o`/`s`/`r` or a bare integer; `o`/`s` hardcode `patterns.last()`; a bare number = **Agent** scope (persists to the agent `.md`). No combos. The bare-number → `append_agent_allow` path hits "No such file" when the agent `.md` is absent — the reported "no file" error.

## Final permission grammar

```
⟳ cargo test --all
  [1] shell:cargo test *      (specific)
  [2] shell:cargo *           (broad)
  number = once · s = session · a = agent · r = reject   (e.g. s2, 2s)
```

- `r` / `reject` → Reject
- `<n>` → **Once**, `patterns[n-1]`  (number alone implies once)
- `s` / `a` → Session / Agent at the **broadest** pattern (`patterns.last()`)
- `s<n>` / `<n>s` / `a<n>` / `<n>a` → that scope, `patterns[n-1]`, **order-free**
- `o` / `o<n>` → tolerated silent alias for Once (not advertised)
- **No `g`/global** — global stays edit-the-config-directly.
- anything else → reprompt.

---

## Phase 1 — `gait chat --session` resume  (smallest; unblocks "couldn't continue my chat")

### Task 1: Thread `--session` into chat   **[gait]**

**Files:**
- Modify: `gait-rs/src/bin/gait.rs` — `cmd_chat` (~832) + usage line (103)
- Modify: `gait-rs/src/chat.rs` — `run_chat` signature (~top) and the initial `session_id` binding (~67)

- [ ] **Step 1:** In `cmd_chat`, add an arm `"--session" => { i += 1; session_id = argv.get(i).cloned(); }` (an `Option<String>`, default `None`), matching how `run` parses it at `gait.rs:191`.
- [ ] **Step 2:** Change `run_chat(gait_dir, codebase, agent_name)` → `run_chat(gait_dir, codebase, agent_name, session_id: Option<String>)`. When `Some(id)`, use it; when `None`, mint a fresh id exactly as today (`new_session_id()`). The `shell_history_path` derivation must use the resolved id.
- [ ] **Step 3:** Update the `chat` usage line (`gait.rs:103`) to include `[--session <id>]`.
- [ ] **Step 4:** Build + run the chat-arg tests; manually verify `gait chat --gait-dir … --session <existing>` loads prior history (the session's `log.jsonl` retraces into context).

**Verify (self):** `gait chat … --session <id>` continues a real prior session; omitting `--session` still starts fresh.

---

## Phase 2 — Permission grammar + robust persistence

### Task 2: Two-axis `parse_permission_response`   **[self — keystone]**

**Files:**
- Modify: `gait-rs/src/ui/permission.rs` — `parse_permission_response` (40-63) + menu line (21) + reprompt hint (35)
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Replace the parser body.** Tokenize `input.trim().to_lowercase()` into at most one scope letter (`o|s|a`) + an optional digit run, in any order; reject on any other char or a second letter. Resolve pattern (`patterns[n-1]`, or `patterns.last()` when no digits — `None` if out of range / empty patterns). Resolve scope: `s`→Session, `a`→Agent, `o`→Once, none→Once (digits required). `r`/`reject` short-circuits to Reject. **No Global.**

```rust
fn parse_permission_response(input: &str, patterns: &[String]) -> Option<crate::permission::PermissionChoice> {
    use crate::permission::PermissionChoice;
    let s = input.trim().to_lowercase();
    if s == "r" || s == "reject" { return Some(PermissionChoice::Reject); }
    if s.is_empty() { return None; }

    let mut letter: Option<char> = None;
    let mut digits = String::new();
    for c in s.chars() {
        match c {
            'o' | 's' | 'a' if letter.is_none() => letter = Some(c),
            d if d.is_ascii_digit() => digits.push(d),
            _ => return None,
        }
    }
    let pattern = if digits.is_empty() {
        patterns.last()?.clone()
    } else {
        let n: usize = digits.parse().ok()?;
        if n < 1 || n > patterns.len() { return None; }
        patterns[n - 1].clone()
    };
    Some(match letter {
        Some('s') => PermissionChoice::Session(pattern),
        Some('a') => PermissionChoice::Agent(pattern),
        Some('o') => PermissionChoice::Once(pattern),
        None => { if digits.is_empty() { return None; } PermissionChoice::Once(pattern) }
    })
}
```

- [ ] **Step 2:** Menu line (`permission.rs:21`) → `  number = once · s = session · a = agent · r = reject  (e.g. s2, 2s)`. Reprompt hint (`:35`) → `unrecognized — e.g. 2, s, s2, 2a, r`.
- [ ] **Step 3: Tests.** Keep existing four, fix their expectations to the new defaults, and add:
  - `bare_number_is_once` — `"2"` → `Once(patterns[1])`
  - `session_pattern_either_order` — `"s2"` and `"2s"` → `Session(patterns[1])`
  - `agent_pattern_either_order` — `"a1"` and `"1a"` → `Agent(patterns[0])`
  - `bare_letter_uses_broadest` — `"s"` → `Session(patterns.last())`, `"a"` → `Agent(patterns.last())`
  - `out_of_range_and_junk_are_none` — `"9"`, `"sx"`, `"sa"`, `""` → `None`
  - `reject_short_circuits` — `"r"` → `Reject`
  - NOTE: update `parse_permission_response_number` — bare `"1"`/`"2"` now mean **Once**, not Agent.
- [ ] **Step 4:** Build + test `ui::permission`.

### Task 3: Create-if-missing for Agent/Global persistence (the "no file" fix)   **[gait]**

**Files:**
- Modify: `gait-rs/src/agent_frontmatter.rs` — `append_agent_allow`
- Check: `gait-rs/src/config.rs` — `append_allow_pattern` (only relevant to the config path; verify it already creates the file or make it robust)
- Test: `agent_frontmatter.rs` tests

- [ ] **Step 1:** In `append_agent_allow`, if the agent `.md` does not exist, create its parent dir + an empty file (a bare frontmatter stub `---\nallow: []\n---\n` is fine) before appending, so an Agent-scope approve never fails with "No such file". Read the existing append logic first and match its frontmatter handling.
- [ ] **Step 2:** Add a test `append_agent_allow_creates_missing_file` — point at a non-existent path under a tempdir, append a pattern, assert the file now exists and contains the pattern.
- [ ] **Step 3:** Build + test.

**Verify (self):** at the permission prompt, `a2` on a gait-dir whose `agents/<name>.md` is absent persists cleanly with no warning.

---

## Phase 3 — Unified expander + `>(N)` in the CLI

### Task 4: `last_turn_blocks` projection helper   **[gait]**

**Files:**
- Modify: `gait-rs/src/session/retrace.rs` — add a public fn near the other projections
- Test: `retrace.rs` tests

- [ ] **Step 1:** Add `pub fn last_turn_blocks(events: &[Event]) -> Vec<String>` that iterates events, and returns the `blocks` of the **last** `payload::Kind::TurnComplete(tc)` (clone `tc.blocks`), or `vec![]` if none. Use the existing private `kind(e)` helper pattern (retrace.rs:20).
- [ ] **Step 2:** Test `last_turn_blocks_returns_last_complete` — mint two `TurnComplete` events (use the existing `mint_events` test helper) with distinct `blocks`, assert the helper returns the second's; and `…_empty_when_none` for a log with no `TurnComplete`.
- [ ] **Step 3:** Build + test.

### Task 5: `ExpandCtx` + single-pass `expand_input`   **[self — keystone]**

**Files:**
- Modify: `gait-rs/src/repl_input.rs` — add `ExpandCtx` + `expand_input`; keep `expand_shell_marker`, `resolve_quote_range`, `looks_like_path`; extract the inline `@`-file body into `fn expand_file_marker(token, codebase) -> String`; **delete** `expand_at_markers` and `expand_quote_reply`.
- Test: migrate `expand_tests` + `quote_reply_tests` to drive `expand_input` through a ctx.

- [ ] **Step 1: Add the context + scanner.** One char loop dispatching on sigil against the **original** input (no chaining of outputs):

```rust
pub struct ExpandCtx<'a> {
    pub codebase: &'a std::path::Path,
    pub history: &'a std::path::Path,
    pub last_blocks: &'a [String],
}

/// Single pass over `input`. At each position match one sigil and emit its
/// replacement, advancing past the matched region — replacements are never
/// re-scanned, so `>(N)` (agent text) can't trigger `$(...)` shell execution.
pub fn expand_input(input: &str, ctx: &ExpandCtx) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '$' if chars.get(i + 1) == Some(&'(') => {
                let start = i + 2;
                if let Some(rel) = chars[start..].iter().position(|&c| c == ')') {
                    let inner: String = chars[start..start + rel].iter().collect();
                    out.push_str(&expand_shell_marker(&inner, ctx.history, ctx.codebase));
                    i = start + rel + 1; continue;
                }
                out.push('$'); i += 1;
            }
            '>' if chars.get(i + 1) == Some(&'(') => {
                let start = i + 2;
                if let Some(rel) = chars[start..].iter().position(|&c| c == ')') {
                    let range: String = chars[start..start + rel].iter().collect();
                    let range = range.trim();
                    if range.chars().all(|c| c.is_ascii_digit() || c == '-') {
                        if let Some(q) = resolve_quote_range(range, ctx.last_blocks) {
                            out.push_str(&q); i = start + rel + 1; continue;
                        }
                    }
                }
                out.push('>'); i += 1;
            }
            '@' => {
                let ts = i + 1;
                let te = chars[ts..].iter().position(|c| c.is_whitespace())
                    .map(|p| ts + p).unwrap_or(chars.len());
                let token: String = chars[ts..te].iter().collect();
                if looks_like_path(&token) {
                    out.push_str(&expand_file_marker(&token, ctx.codebase));
                    i = te; continue;
                }
                out.push('@'); i += 1;
            }
            c => { out.push(c); i += 1; }
        }
    }
    out
}

fn expand_file_marker(token: &str, codebase: &std::path::Path) -> String {
    let path = if token.starts_with('/') || token.starts_with('~') {
        std::path::PathBuf::from(token)
    } else { codebase.join(token) };
    match std::fs::read_to_string(&path) {
        Ok(contents) => format!("[file: {}]\n{}", token, contents.trim_end()),
        Err(e) => format!("[file: {} — error: {}]", token, e),
    }
}
```

- [ ] **Step 2: Delete** `expand_at_markers` and `expand_quote_reply` (and their `pub use`/imports).
- [ ] **Step 3: Migrate tests.** Rewrite `expand_tests` to call `expand_input(input, &ExpandCtx { codebase: dir.path(), history: &hist, last_blocks: &[] })`; rewrite `quote_reply_tests` to pass `last_blocks: &blocks()`, `codebase`/`history` to a tempdir/throwaway path. Keep `at_paren_no_longer_expands`. **Add** `no_cascade_quote_then_shell` — `last_blocks = ["$(echo pwned)"]`, input `">(1)"`, assert output contains the literal `$(echo pwned)` and **not** `[shell:` (proves the replacement isn't re-scanned).
- [ ] **Step 4:** Build + test `repl_input`.

### Task 6: Rewire both call sites   **[self]**

**Files:**
- Modify: `gait-rs/src/chat.rs:157`
- Modify: `gait-rs/src/bin/gait.rs:250-255`

- [ ] **Step 1 (REPL):** Replace `let line = expand_quote_reply(&line, &last_turn_blocks);` with
  `let line = crate::repl_input::expand_input(&line, &crate::repl_input::ExpandCtx { codebase: &codebase, history: &shell_history_path, last_blocks: &last_turn_blocks });`
  (REPL now also expands `$(…)`/`@` — the bug fix.)
- [ ] **Step 2 (CLI):** Build `last_blocks` from the log and call `expand_input`:
  ```rust
  let task = task.map(|t| {
      let hist = gait_dir.join("tmp").join(format!("shell-{}.jsonl", &session_id));
      let events = gait_ai::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
          .events().unwrap_or_default();
      let last_blocks = gait_ai::session::retrace::last_turn_blocks(&events);
      gait_ai::repl_input::expand_input(&t, &gait_ai::repl_input::ExpandCtx {
          codebase: &codebase_path, history: &hist, last_blocks: &last_blocks,
      })
  });
  ```
  (`expand_input` returns `String`, so the old `.unwrap_or_else` error arm is gone.)
- [ ] **Step 3:** Build whole workspace + full test suite.

**Verify (self):** in REPL, `@Cargo.toml` and `$(echo hi)` expand; in CLI with `--session <id>`, `>(1)` quotes the prior turn's first block.

---

## Final review

- [ ] Full `cargo test` green (save to file, grep per CLAUDE.md).
- [ ] Manual smoke (after `make install`): chat resume, `@`/`$()` in REPL, `>(N)` in CLI, permission `s2`/`2s`/`a1`/`2`/`r`, and `a<n>` against a missing agent file.
- [ ] `finishing-a-development-branch`.

## Self-review notes
- **Type consistency:** `ExpandCtx` fields (`codebase`, `history`, `last_blocks`) identical at both call sites; `PermissionChoice` already has `Session/Agent/Once/Reject` variants (no `Global` emitted by the prompt).
- **No-cascade** is structural (single pass over original input), not a convention — covered by `no_cascade_quote_then_shell`.
- **Dropped:** `expand_at_markers`, `expand_quote_reply`, the `o`/`s` = always-broadest behavior, bare-number = Agent.
