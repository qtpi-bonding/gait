# Feedback index

All open items from the 2026-06-09 session. Items are tentative — some are confirmed bugs,
others are gut reactions or design sketches. Check before acting on any of them.

Legend: **[bug]** confirmed or suspected | **[feature]** new capability | **[prompt]** system-prompt/preamble change | **[investigate]** diagnosis needed first | **[open-q]** unresolved design question | **[ux]** UX/label issue | **[closed]** not a real bug

Each item lists a **Feedback doc** (the original report/discussion to read for context) and a **Code** location (the file to open when acting on it).

---

## Group A — Output / turn contract

These all touch the `output` tool shape or the turn-termination model.

| ID | Item | Decision / Status | Feedback doc | Code |
|----|------|-------------------|--------------|------|
| A1 | **[feature]** Rename `output.summary` → `output.message` | ✅ done — renamed across `OutputParams`, `TurnOutput`, `TurnResult::Done`, and all callsites/tests | [prompt-wishlist §1](2026-06-09-prompt-wishlist.md), [turn-1 §1](2026-06-09-turn-1-discussion.md) | `gait-rs/src/tools/output.rs:8-9` |
| A2 | **[prompt]** Make `output` the required terminator; plain text is a degraded fallback, not co-equal | ✅ done — plan doc written (`turn-terminator-redesign.md`), preamble landed | [prompt-wishlist §2](2026-06-09-prompt-wishlist.md), [turn-1 §2](2026-06-09-turn-1-discussion.md) | `gait-rs/src/turn.rs:463-484` |
| A3 | **[open-q]** Hard vs soft retry limit on `output_schema` validation | ✅ done — `output_schema_retries` field in `GaitConfig` and `TurnOpts`, default 3 | [turn-1 discussion](2026-06-09-turn-1-discussion.md) | `gait-rs/src/turn.rs` |
| A4 | **[open-q]** Should `message` be required (non-empty)? | ✅ done — enforced at schema level | [turn-1 discussion](2026-06-09-turn-1-discussion.md) | `gait-rs/src/tools/output.rs` |

> Note: A referenced `turn-terminator-redesign.md` does not exist anywhere in the repo. A1 and A2
> need that plan doc written before implementation.

---

## Group B — System prompt / preamble

| ID | Item | Decision / Status | Feedback doc | Code |
|----|------|-------------------|--------------|------|
| B1 | **[prompt]** Add "what is gait" preamble to agent system prompt | ✅ done — full preamble landed in `gait-rs/prompts/gait-agent.md` | [prompt-wishlist §6](2026-06-09-prompt-wishlist.md), [preamble-proposal](2026-06-09-preamble-proposal.md) | `gait-rs/prompts/gait-agent.md` |
| B2 | **[prompt]** Document `output` is always permitted (never gated) | ✅ done — covered in preamble | [prompt-wishlist §5](2026-06-09-prompt-wishlist.md) | `gait-rs/prompts/gait-agent.md` |
| B3 | **[prompt]** Always emit `<loadable-index>` even when empty | ✅ done — `build_loadable_index` now returns `(none enabled)` block instead of empty string | [prompt-wishlist §4](2026-06-09-prompt-wishlist.md) | `gait-rs/src/turn.rs` (`build_loadable_index`) |
| B4 | **[prompt]** Document transcript event types | ✅ done — folded into B1 preamble | [prompt-wishlist §7](2026-06-09-prompt-wishlist.md) | `gait-rs/prompts/gait-agent.md` |
| B5 | **[prompt]** Tool discovery nudge for clarify/inform/spawn_agent | ✅ done — folded into B1 preamble | [prompt-wishlist §8](2026-06-09-prompt-wishlist.md) | `gait-rs/prompts/gait-agent.md` |
| B6 | **[prompt]** Add anti-patterns ("Don't") list to prompt | ✅ done — folded into B1 preamble | [prompt-wishlist §9](2026-06-09-prompt-wishlist.md) | `gait-rs/prompts/gait-agent.md` |

---

## Group C — Confirmed / suspected bugs

| ID | Item | Severity | Status | Feedback doc | Code |
|----|------|----------|--------|--------------|------|
| C1 | **[bug]** `edit` tool rejects all input shapes: "invalid type: map, expected a sequence" | HIGH | ✅ **closed** — root cause was `$ref` schema that broke MiniMax deserialization. Fixed in `ed1badd` (inline schemas + tolerant lines deser). `list_agents.rs` confirms the fix landed. | [bug-edit-tool](bug-2026-06-09-edit-tool-shape-rejected.md) | `gait-rs/src/tools/edit.rs:50` |
| C2 | **[bug]** After a `[r]` reject, the model gets no follow-up turn — REPL returns to input silently | MEDIUM | ✅ **done** — `break 'spawn` → `continue 'spawn` at `chat.rs:580`. Deny event is already in the transcript; re-entering the spawn loop gives the model a turn to react. | [bug-permission-reject](bug-2026-06-09-permission-reject-drops-reply.md) | `gait-rs/src/chat.rs:576-583` |
| C3 | **[bug]** `edit` range-replace (`pos..end`) drops a brace and reports success — no post-edit balance/parse guard. M3 set `end` one line past the fn close; file no longer compiles, agent reported done. Sibling of the closed C1 (different failure on the now-working tool). | HIGH | open — add a delimiter-balance (or tree-sitter) guard after `apply_hashline_edits`; the existing `end` guard only catches a *missing* `end`, not a wrong one. | [bug-edit-range-replace](bug-2026-06-10-edit-range-replace-drops-brace.md) | `gait-rs/src/tools/edit.rs:67`, `gait-rs/src/hashline.rs` |

---

## Group D — Investigations

| ID | Item | Outcome | Feedback doc | Code |
|----|------|---------|--------------|------|
| D1 | Dirty-files list shows gitignored paths | **not a gait bug** — `get_dirty_files` calls `repo.statuses(None)` which respects `.gitignore`. Files reported (node_modules, dist, etc.) were almost certainly *tracked* in that project — tracked files bypass `.gitignore` regardless. Nothing to fix in gait. | [prompt-wishlist §3](2026-06-09-prompt-wishlist.md) | `gait-rs/src/git.rs:25-35` |
| D2 | Numbered permission options (1/2/3) not persisting between turns | ✅ done — `agent_path` now defaults to `<workspace>/agent.md` when no `--agent` flag given. `append_agent_allow` creates the file with YAML frontmatter on first use. | [repl-cli-feedback §4](2026-06-09-repl-cli-feedback.md) | `gait-rs/src/chat.rs:355-357,444,525` |

---

## Group E — REPL / CLI features (need specs before implementing)

| ID | Item | Effort | Status | Feedback doc | Code |
|----|------|--------|--------|--------------|------|
| E1 | **[feature]** Turn rewind (`gait rewind`) via git shadow db | Medium | ✅ done — `gait rewind` command implemented; shadow db checkpoints codebase per turn | [repl-cli-feedback §1](2026-06-09-repl-cli-feedback.md) | `gait-rs/src/` (rewind module) |
| E2 | **[feature]** Ctrl+C to abort in-progress turn in `gait chat` | Medium | open — needs cancellation token refactor in `run_turn`, new `TurnResult::Cancelled` variant | [repl-cli-feedback §3](2026-06-09-repl-cli-feedback.md) | `gait-rs/src/turn.rs:435-450` |
| E3 | **[feature]** REPL output line numbering + `>(N)` quote-reply syntax | Medium | ✅ done — dim gutter numbers on agent output; `>(N)` / `>(N-M)` / `>(N) message` expands to quoted block | [repl-cli-feedback §2](2026-06-09-repl-cli-feedback.md) | `gait-rs/src/chat.rs`, `gait-rs/src/render.rs` |

---

## Group F — Permission system features (need specs before implementing)

| ID | Item | Effort | Status | Feedback doc | Code |
|----|------|--------|--------|--------------|------|
| F1 | **[feature]** Session-scoped allow list (between per-turn `o` and global `gait.jsonc`) | Large | design space open — D2 is a symptom of this gap: numbered options claim to save to agent file but silently no-op when no agent file exists. Precedence, storage, UX of prompt menu all need deciding. | [repl-cli-feedback §5](2026-06-09-repl-cli-feedback.md) | `gait-rs/src/permission.rs`, `gait-rs/src/chat.rs:103-134` |

---

## Suggested action order

1. ✅ **B3** — always emit `<loadable-index>`.
2. ✅ **D2** — default `agent_path` to `agent.md`.
3. ✅ **A1** — rename `output.summary` → `output.message`.
4. ✅ **C1** — edit tool schema bug (closed; fixed in ed1badd).
5. ✅ **C2** — model turn after rejection (`break` → `continue 'spawn`).
6. ✅ **A3** — `output_schema_retries`, default 3, overridable.
7. ✅ **A4** — `message` non-empty enforced at schema level.
8. ✅ **A2 + B1–B6** — `turn-terminator-redesign.md` written; full preamble landed.
9. ✅ **E1** — `gait rewind` implemented.
10. ✅ **E3** — line numbering + `>(N)` quote-reply.
11. **E2** — Ctrl+C abort in `gait chat`. Needs cancellation token refactor + spec.
12. **F1** — Session-scoped permissions. Needs design spec.
