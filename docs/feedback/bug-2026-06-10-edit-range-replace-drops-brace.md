# Bug: `edit` range-replace (`pos..end`) silently drops a brace — no post-edit balance guard

**Status:** Confirmed from THREE independent real dispatches (2026-06-10 ×2, 2026-06-11;
see the "Second live reproduction" and "Third reproduction" sections below — it reproduced
while gait was the *development driver*, not just an archebuild node, and on 2026-06-11 the
model self-recovered from it). gait session `secf94eb9`
(MiniMax-M3), editing `crates/grorg-core/src/graph.rs` while running as an
archebuild node executor (grorg dogfood).
**Severity:** High. The edit produced a file that does not compile (unbalanced
braces), and the agent reported success (`output`: "Updated `graph.rs`"). Nothing
between the bad edit and the green result caught it.
**Affects:** `edit` tool, the multi-line range-replace path (`op: "replace"` with
`pos` + `end`). Sibling of the known MiniMax "duplication snowball" already guarded
in `edit.rs` (session `s60ec2202`), but a *distinct* failure: there the model omits
`end`; here the model supplies `end` and sets it one line too far.

## What happens

M3 added two fields to a struct and the matching initializer entries, then closed
the edit with a **range replace whose `end` anchor included the enclosing function's
closing `}`** — but the replacement `lines` ended at `})`, omitting that brace. The
brace was deleted. Result: `Graph::from_snapshot` is left unterminated and the next
method nests inside it.

```
$ cargo check -p grorg-core
error: this file contains an unclosed delimiter
   --> crates/grorg-core/src/graph.rs:320:3
 70 | impl Graph {
    |            - unclosed delimiter
 71 |     pub fn from_snapshot(snapshot: GraphSnapshot) -> Result<Self> {
    |                                                                   - this delimiter might not be properly closed...
```

The agent's own transcript shows it believed it succeeded — its final `output`
message reads "Updated `crates/grorg-core/src/graph.rs`: 1. Added `NodeIndex`…
2. Added two new fields…", with no indication the file no longer parses.

## The exact edit sequence (transcript `secf94eb9`)

All four edits "succeeded" (each returned `Edited … (N line(s) net change)`):

1. `replace pos 6#QP` → `use petgraph::graph::{DiGraph, NodeIndex};` (net 0). Fine.
2. `replace pos 56#MQ`, `lines` = one array element containing the whole struct body
   with embedded `\n`, **no `end`**. → **Rejected** with the existing helpful guard:
   *"the replacement repeats the anchored line … and then adds 12 more line(s), but
   with no `end` ref a replace swaps only that single line."* Good — this is the
   `s60ec2202` guard working.
3. `append pos 61#RB` → inserts the two new struct fields (net +6). Fine.
4. `replace pos 260#RH` (net +7), then `replace pos 255#NN end 269#SW` (net +6) —
   **two overlapping range-replaces of the initializer block**, the replacement
   ending at `        })`. The original lines 255–269 contained both `})` *and* the
   function's closing `}`; the range covered the `}` but the replacement text did
   not re-emit it. The brace is gone.

Two compounding factors:
- **Off-by-one on `end`.** `end: 269#SW` reached one line past the struct
  literal's `})` to the function's `}`; the replacement stopped at `})`.
- **Overlapping duplicate edits in one turn.** The model issued the initializer
  replace twice (`260#RH`, then `255#NN..269#SW`). Anchors renumber after each
  applied edit, so the second range targeted shifted lines — exactly the kind of
  drift that turns an off-by-one into a dropped delimiter.

## Why the existing guard didn't fire

`edit.rs` already steers the model toward `end` for multi-line replaces and has a
regression test for the "omit `end`, snowball duplicates" pattern. That guard keys
on *missing* `end`. Here `end` is present but wrong, so the guard passes and the
edit applies. There is **no post-apply check that the file's delimiter balance (or
parse) is unchanged/valid**, so a brace-dropping range replace is indistinguishable
from a correct one at the tool layer.

## Reproduction

1. A file with a function whose body ends `…})\n    }` (struct-literal return then
   fn close), with anchors from a fresh `read`.
2. `edit` with `op: replace`, `pos` = the start of the initializer, `end` = the ref
   of the function's closing `}`, and `lines` ending at `})` (omitting the `}`).
3. Tool returns `Edited … (N line(s) net change)` — success.
4. File no longer compiles / no longer parses (unbalanced `{`).

Deterministic variant (no model): any range replace whose `[pos, end]` span contains
a closing delimiter that the `lines` payload does not reproduce.

## Code

- `gait-rs/src/tools/edit.rs:67` — `apply_hashline_edits(&old_content, &params.edits)`
  then reports `"{} line(s) net change"` with no validity check.
- `gait-rs/src/hashline.rs` — `apply_hashline_edits` / `HashlineEditOp` (the actual
  range splice; where a balance/parse guard would live).
- Existing guard + regression: `gait-rs/src/tools/edit.rs:44-47` (the `end` steer)
  and the `s60ec2202` test at `:161-188`.

## Suggested fix (hypothesis — yours to decide)

- **Post-apply balance guard.** After splicing, compare a cheap delimiter-balance
  signature (`{}`, `()`, `[]` counts, ignoring strings/comments) of the spliced
  region — or the whole file — before vs after. If an edit changes balance in a way
  that leaves the file unbalanced, reject with a message naming the delimiter, the
  same way the `end` guard already coaches. For language-aware codebases a
  tree-sitter parse-error check would be stronger, but a bracket-count delta catches
  this class cheaply and language-agnostically.
- **Echo the deleted span on range replaces.** When `end` is set, include the exact
  lines being removed in the result (`removed: 255#NN..269#SW →\n<those lines>`), so
  an off-by-one is visible to the model on the next turn instead of silent.
- **Optionally** flag overlapping/duplicate edits to the same region within one call
  as suspicious (the anchor-drift trap that turned this off-by-one into a drop).

## Second live reproduction (gait as dev driver, session `s026c179b`)

Later the same day, gait/M3 was driving normal subagent-style development directly
(not via archebuild) — one `gait run` per plan task, editing
`crates/grorg-core/src/dispatch.rs`. The task was a mechanical "insert a function and
some tests into an existing file." gait's `edit` again left the file with an **unclosed
delimiter** (`error: this file contains an unclosed delimiter`, pointing at the
`decide_transition` fn). This confirms the bug is not specific to the archebuild
harness — it reproduces in plain `gait run`.

Two further observations from this run:

1. **The model could not self-recover.** After the bad edit, M3 spent ~10 turns trying
   to fix it by *manually counting opening vs closing braces* ("35 opens vs 34 closes …
   there's still 1 missing closing brace"), reading and re-reading the file, never
   converging. A balance/parse signal from the tool (suggested fix #1) is exactly the
   information it was trying — and failing — to reconstruct by hand. The tool knows the
   splice; the model is guessing.
2. **A network error ended the turn mid-repair:**
   `[gait] error: stream error: Network error: error sending request for url
   (https://api.minimax.io/anthropic/v1/messages)`. Unrelated to the edit bug, but worth
   noting: a transient MiniMax API stream error killed the session with the file in a
   broken, uncommitted state. (The grorg controller caught the non-compiling file on its
   own `cargo test` gate, reverted, and finished the task by hand — so nothing broken
   was committed.)

Net: the controller's independent compile/test gate is what kept broken code out of the
tree both times. The edit-tool guard (suggested fix #1) would let the *agent* catch it
itself instead of relying on the caller.

## Third reproduction — but self-recovered (2026-06-11, driven dev)

Reproduced again while gait/M3 drove a subagent task inserting a method into an existing
`impl` block (`crates/grorg-ops/src/dispatch.rs`). gait's first `edit` "accidentally
consumed the closing `}` of the preceding method along with the anchor line," leaving the
new method unclosed — the same range-replace-drops-a-brace failure. **The new datapoint: this time M3 recovered on its own.** It hit the `cargo`
"unclosed delimiter" error, correctly diagnosed that the prior method's `}` was missing,
and re-inserted it with a targeted `append` after `Ok(orphan_ids)` — converging in a
couple of turns rather than the ~10-turn flailing + network-death of the first live run.

Two takeaways:
- The bug itself is **unchanged** — a `pos..end` range replace still silently drops the
  delimiter; nothing at the tool layer flagged it. The compile error (from the *caller's*
  build, not the edit tool) is still what surfaced it.
- But the recovery is much better than the earlier run. Whether that's M3 variance or a
  prompt/tool improvement, it suggests the model *can* use a balance signal when it gets
  one — which is exactly what suggested fix #1 (a tool-side balance guard) would provide
  proactively instead of relying on a downstream `cargo` invocation that may not exist.

(Aside, same run: the task text passed to gait contained a normal `DATABASE_URL`, but
gait's transcript rendered it mangled — `postgres://postgres:postgres[file: localhost...]`
— and it sensibly fell back to the test's own connection string. Minor, not filed; flagging
in case the task-string rendering is dropping `@host` segments.)

---

**Project:** gait (`gait-rs`, `edit` tool)
**Filed:** 2026-06-10
**Type:** bug — confirmed + reproduced
**Filed by:** grorg agent (Rust), while dogfooding archegraph with gait configured
as the archebuild node executor (MiniMax-M3). The model's off-by-one is the trigger;
the reported gap is that the `edit` tool applied and reported success on an edit that
left the file syntactically unbalanced, with no guard to catch it.
**Related:** C1 (`bug-2026-06-09-edit-tool-shape-rejected.md`, closed) was the
schema-rejection bug; this is a *different* failure on the now-working tool. The
archegraph side has its own report (the build's `post_dag` `cargo check` gate was
skipped, so this non-compiling output passed as `Completed`) —
`~/Documents/archegraph/docs/feedback/2026-06-10-build-post-dag-chain-skipped-noncompiling-passes.md`.
The two together are why broken code reached a green build.
