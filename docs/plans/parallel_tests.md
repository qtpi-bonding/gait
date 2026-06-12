# gait Parallelism Test Plan

Two suites. **Suite A** tests the Scoped Agents spec (shared codebase working tree, declared
scopes). **Suite B** tests per-session codebase working trees (parallel trees + branches,
persistent checkpoint workdir), including fork.

Conventions:

- `ckpt(S, N)` = session S's checkpoint commit after turn N. `tree(ckpt)` = its committed tree.
- `run_turn(S, edits...)` = drive one turn that performs the given file ops (via a scripted
  fake model or a replayed transcript — deterministic, no live LLM).
- Tags: `[unit]` pure logic, `[int]` spawns real processes/repos, `[kernel]` requires
  Linux ≥5.13 (or macOS) CI runner with sandboxing available.
- ⚠ marks tests that pin a specific bug class already identified during design review.

---

## Suite A — Scoped agents in a shared working tree

### A1. Spawn-time disjointness & scope validation

**A1.1 Disjoint scopes coexist** `[unit]`
```
spawn(A, scope=[src/parser/]); spawn(B, scope=[src/lexer/]) → both succeed
```
Baseline: the feature's reason to exist.

**A1.2 Subtree overlap rejected** `[unit]`
```
spawn(A, [src/parser/]); spawn(B, [src/parser/ast/]) → error, no session.json for B
```
Overlap is ancestor/descendant, not just equality.

**A1.3 ⚠ Sibling-prefix false positive** `[unit]`
```
spawn(A, [src/parse/]); spawn(B, [src/parser/]) → both succeed
spawn(C, [src/parserutil/]) with A,B active → succeeds
```
Pins component-wise comparison. A string `starts_with` implementation fails this.

**A1.4 Trailing-slash and `.`-segment normalization** `[unit]`
```
[src/parser] vs [src/parser/] vs [./src/parser/] → all treated identical for overlap
```

**A1.5 ⚠ Traversal in declaration rejected** `[unit]`
```
spawn(A, [src/parser/../../etc/]) → error (canonicalizes outside codebase root)
spawn(B, [/tmp/x]) → error (absolute path outside codebase)
```

**A1.6 Symlink in declared path rejected** `[unit]`
```
ln -s /elsewhere src/link; spawn(A, [src/link/]) → error
```
Spec rule: git pathspec view and Landlock resolved-path view must not diverge.

**A1.7 Nonexistent scope dir pre-created** `[int]`
```
spawn(A, [src/newmodule/]) → succeeds; dir exists; later PathFd::new(dir) succeeds;
agent can create files there; empty dir invisible to checkpoint until files exist
```
Pins the "agent's job is to create the module" case.

**A1.8 Unscoped blocks scoped, both directions** `[int]`
```
run unscoped S1 (active); spawn(A, [src/parser/]) → rejected
spawn(A, [src/parser/]) active; gait run (unscoped S2 turn) → rejected/queued
```
Implicit `["."]` rule. Without it the whole-tree checkpoint of S1 absorbs A's edits.

**A1.9 Parent lockdown after spawning scoped children** `[int]`
```
parent P (unscoped) spawns A [src/parser/]; P attempts write(README.md) same turn
→ blocked/queued until A finishes; after A finishes → allowed
```
The parent is itself the unscoped concurrent writer.

**A1.10 Parent with declared scope + contained child** `[unit]`
```
P scope [src/]; spawn child [src/parser/] → ok
P scope [src/]; spawn child [tests/]      → rejected (containment: child ⊆ parent)
```

**A1.11 Grandchild containment** `[unit]`
```
A [src/parser/] spawns G [src/lexer/] → rejected
A [src/parser/] spawns G [src/parser/ast/] → ok
```

**A1.12 Sibling disjointness under one parent** `[unit]`
```
P [src/]; children [src/parser/], [src/lexer/] → ok; third child [src/parser/types/] → rejected
```

**A1.13 ⚠ Spawn TOCTOU race** `[int]`
```
two processes concurrently spawn children with overlapping scopes
→ exactly one session.json written; other gets overlap error
```
Pins the workspace-level spawn lock. Run N times; flaky pass = bug.

**A1.14 Shared containment helper property test** `[unit]`
```
forall (scope, path): tier0_in_scope(scope, path) == !disjoint(scope, [path])
```
Pins "one `path_within` helper used by both spawn check and gate" — prevents drift.

### A2. Tier-0 scope gate

**A2.1 In-scope write falls through to normal tiers** `[unit]`
```
scope [src/parser/]; write src/parser/x.rs → reaches deny/allow/prompt as today
```

**A2.2 Out-of-scope write blocked with scope-specific error** `[unit]`
```
write src/lexer/x.rs → blocked; tool_result message contains "outside session scope";
transcript records the blocked call
```
Error identity matters: not a generic deny.

**A2.3 ⚠ Traversal bypass at the gate** `[unit]`
```
write src/parser/../lexer/x.rs → blocked (lexical normalization before comparison)
```
Until Landlock ships, tier 0 is the only enforcement — this is not defense-in-depth, it's the wall.

**A2.4 ⚠ Gate prefix bug** `[unit]`
```
scope [src/parse/]; write src/parser/x.rs → blocked
```
Same bug class as A1.3, at the other call site.

**A2.5 Multiedit atomicity across the boundary** `[unit]`
```
multiedit{src/parser/a.rs ok, src/lexer/b.rs out} → entire call blocked, zero edits applied
```
Pins the decided semantics (block whole call; partial application would leave
non-replayable state in the transcript).

**A2.6 Deny still fires inside scope (tier order)** `[unit]`
```
scope [src/parser/]; deny ["edit:src/parser/locked.rs"]; edit that file → tier-1 blocked
```
Scope gate is tier 0, not a replacement for deny.

**A2.7 ⚠ Auto mode + scope** `[int]`
```
--auto, scope [src/parser/]: write in-scope → auto-approved & executes;
write out-of-scope → blocked
```
Regression for the v1 draft's `deny: ["write"]` catch-all, which blocked everything in auto.

**A2.8 Read tools unrestricted** `[unit]`
```
read/grep/glob/ls/codesearch on out-of-scope paths → succeed
```

**A2.9 Unscoped session: gate inert** `[int]`
```
no scope in session.json → byte-identical permission behavior to pre-feature build
(replay a recorded pre-feature transcript; assert same decisions)
```

### A3. Scoped checkpoint (audit-trail correctness)

**A3.1 ⚠ Snapshot excludes sibling edits** `[int]`
```
A edits src/parser/a.rs while B edits src/lexer/b.rs (interleaved turns)
→ tree(ckpt(A,1)) contains parser paths only; b.rs absent; and vice versa
```
The headline test — the audit-trail lie this whole design kills.

**A3.2 Snapshot excludes user's out-of-scope dirty files** `[int]`
```
user leaves uncommitted edit in README.md; A runs a turn
→ README.md absent from tree(ckpt(A,1))
```

**A3.3 Interleaved turn-over-turn diff purity** `[int]`
```
ckpt(A,1); ckpt(B,1); A edits again; ckpt(A,2)
→ diff(ckpt(A,1), ckpt(A,2)) touches only A's scope, despite B's writes in between
```
Stronger than A3.1: contamination via the *index* between turns, not just one snapshot.

**A3.4 gitignore respected under pathspec** `[int]`
```
src/parser/gen.tmp gitignored; agent's build creates it → absent from snapshot
```
`IndexAddOption::DEFAULT` semantics must survive the `["."]→scope_paths` change.

**A3.5 Deletions round-trip** `[int]`
```
turn 1: old.rs exists; turn 2: agent deletes it; ckpt(A,2) records deletion;
restore→1 resurrects it; restore→2 removes it again
```

**A3.6 Shadow repos share objects via alternates, survive sibling existence** `[int]`
```
A and B both checkpoint; assert each shadow's own object store contains only
new blobs (changed files), unchanged-file blobs resolve via alternates
```

**A3.7 Codebase gc hazard (alternates dissociation)** `[int]`
```
session based on commit X on branch tmp; user: git branch -D tmp && git gc --prune=now
→ attempt restore/diff in shadow repo; pin the outcome
```
Borrowed blobs reachable only from deleted history can vanish under the
borrower's feet. Test documents whether gait repacks/keeps a ref at session
start or accepts-and-documents the hazard. Whatever the answer, it must be deliberate.

**A3.8 Checkpoint writes succeed inside the sandbox** `[kernel]`
```
scoped turn under Landlock completes → ckpt commit exists in checkpoints/<id>/codebase.git
```
Pins the infra-path enumeration (the checkpoint runs *inside* the restricted process).

### A4. Scoped restore

**A4.1 Basic isolation on restore** `[int]`
```
A at turn 3 → restore_to(A, turn 1):
  A's scope files == tree(ckpt(A,1)); B's files untouched; user's out-of-scope dirty edits untouched
```

**A4.2 Tracked ghost removed** `[int]`
```
new_module.rs created turn 2 (checkpointed); restore→1 → file gone
```
FORCE checkout's baseline-vs-target deletion. `git checkout <sha> -- <paths>` semantics
would fail this — pins the libgit2 path.

**A4.3 ⚠ Untracked crash ghost removed** `[int]`
```
simulate crash: write src/parser/half.rs mid-turn, kill before checkpoint;
restore→last ckpt → half.rs deleted by remove_untracked_in_scope
```

**A4.4 ⚠ Consecutive-undo ghost (stale HEAD)** `[int]`
```
F exists only in turn 4's tree; session at turn 7 (F absent);
restore→4 (F reappears); immediately restore→5
→ F must be gone
```
Without the post-restore checkpoint, the second restore diffs target-5 vs baseline-7,
treats F as untracked, and leaves it. The single most likely silent-lie bug in the design.

**A4.5 Post-restore checkpoint + transcript event** `[int]`
```
after any scoped restore: new ckpt commit whose tree == current scope state;
transcript contains a restore event referencing old turn, target turn, new SHA
```
Audit honesty: the undo is itself history.

**A4.6 Restore never touches out-of-scope untracked junk** `[int]`
```
stray notes.txt at repo root; A restores → notes.txt intact
```

**A4.7 Directory create/delete round-trip** `[int]`
```
src/parser/sub/{a,b}.rs deleted by turn 3; restore→1 → files back;
restore→3 → files gone (empty dir may remain — pin the decision either way)
```

**A4.8 Restore vs sibling's running turn** `[int]`
```
B mid-turn; A restores → B's files untouched; B's next ckpt diff unchanged by A's restore
```

**A4.9 Restore blocked during own running turn** `[int]`
```
A turn executing; gait undo A → refused/queued on A's session lock
```

**A4.10 Unscoped restore regression** `[int]`
```
unscoped session: restore_to still reset --hard, full tree, behavior identical to today
```

**A4.11 Unscoped crash ghost (pre-existing gap)** `[int]`
```
unscoped session, crash mid-turn after writing junk.rs, restore_to(last ckpt)
→ does reset --hard remove it? (it does not remove untracked) — pin current behavior
```
Not introduced by this spec, but the scoped work makes the asymmetry visible;
decide whether unscoped restore gains the same untracked cleanup.

### A5. Kernel enforcement

**A5.1 ⚠ Reads unrestricted under sandbox** `[kernel]`
```
scoped agent: cat src/lexer/b.rs, grep across whole tree, cargo build reading everything
→ all succeed
```
Regression for `handle_access(from_all(ABI::LATEST))` — the draft's ruleset
governs reads and would fail this on first contact.

**A5.2 Shell write outside scope blocked** `[kernel]`
```
shell: touch src/lexer/evil.rs → EACCES; file absent
```

**A5.3 Wrapper bypass closed** `[kernel]`
```
shell: bash -c "echo x > src/lexer/y.rs" → blocked at syscall
shell: find src/lexer -name '*.rs' -delete → blocked
```
The original deny-pattern bypass, now closed by the kernel layer.

**A5.4 Grandchild process inheritance** `[kernel]`
```
shell: make → make spawns cc → cc attempts out-of-scope write → blocked
```

**A5.5 In-scope shell writes work** `[kernel]`
```
cargo fmt over src/parser/ → succeeds; out-of-scope files untouched
```

**A5.6 Rename across boundary / ABI-1 Refer policy** `[kernel]`
```
mv src/parser/a.rs src/lexer/ → blocked (any ABI)
mv src/parser/a.rs src/parser/sub/ → ok on ABI≥2 (Refer granted);
on ABI 1 → pin policy: min-ABI=2 fallback to no-kernel-enforcement + transcript event
```

**A5.7 Truncate gap by ABI** `[kernel]`
```
kernel ≥6.2: shell: ": > src/lexer/b.rs" → blocked
kernel 5.19–6.1: same op escapes → assert transcript carries the documented-gap note
```

**A5.8 Delete / mkdir rights enumerated** `[kernel]`
```
rm in-scope ok, rm out-of-scope blocked; mkdir likewise
```
Pins RemoveFile/RemoveDir/MakeDir/MakeReg in the right-set.

**A5.9 Infra paths writable** `[kernel]`
```
under sandbox: cargo build (target/), mktemp (/tmp), transcript append (sessions/<id>/),
checkpoint commit (checkpoints/<id>/codebase.git/) → all succeed
```
One assertion per enumerated infra path; a missing one fails baffling and late otherwise.

**A5.10 ⚠ `.git` writable + worktree still protected** `[kernel]`
```
shell: git status → ok (index.lock); shell: git checkout -- . →
fails partway; assert all out-of-scope files byte-identical before/after
```
Pins the non-obvious interaction: open `.git`, but kernel blocks the worktree
writes git tries to make. Prevents a future "fix" that locks `.git` and breaks git status.

**A5.11 Symlink-out escape closed by kernel, leaks past gate** `[kernel]`
```
agent: ln -s ../../lexer src/parser/escape; write src/parser/escape/x.rs
→ tier-0 gate alone would pass (path looks in-scope); kernel blocks (resolved path)
```
Demonstrates why the layers aren't redundant — and why no-kernel platforms
need the documented caveat.

**A5.12 Fallback honesty** `[int]`
```
mock Landlock-unavailable: interactive run → proceeds + KernelEnforcementUnavailable
event in transcript; --auto run → refuses without --no-kernel-enforcement;
with flag → proceeds + event
```

**A5.13 Nested ruleset tightening** `[kernel]`
```
parent scope [src/], child [src/parser/]: child writes src/lexer/x → blocked
(allowed by parent's ruleset, denied by stacked child ruleset)
```

**A5.14 Spawned MCP server inherits sandbox** `[kernel]`
```
scoped session with --spawn MCP server; server process writes out-of-scope → blocked
```
(`--connect` servers are the documented hole — optionally assert a warning event
when a scoped session has a network MCP enabled.)

### A6. Lifecycle, locks, release

**A6.1 finish releases scope** `[int]`
```
A [src/parser/] finishes → spawn(C, [src/parser/]) succeeds
```

**A6.2 abort releases scope** `[int]`
```
gait abort A → scope free; A excluded from disjointness set
```

**A6.3 ⚠ Crash + stale reclamation** `[int]`
```
kill -9 A's process; spawn(C, [src/parser/]) →
first attempt detects stale pid/heartbeat, marks A stale, succeeds (or succeeds after
explicit gait abort A — pin whichever policy ships); never: permanent deadlock
```

**A6.4 Parent unlock on last child completion** `[int]`
```
P spawns A, B (scoped); A finishes (P still locked); B finishes → P's writes allowed
```

**A6.5 Scope immutable per turn** `[int]`
```
edit session.json scope while A's turn runs → turn continues with the scope read at
turn start; gate, checkpoint, restore all used the same value (no torn reads)
```

**A6.6 Concurrent turns in one session excluded** `[int]`
```
two `gait run --session A` simultaneously → second blocked by session lock file
```

### A7. Trace repo under parallel sessions

**A7.1 Concurrent transcript commits to distinct branches** `[int]`
```
A and B complete turns simultaneously → both commits land on session/A, session/B;
repeat 100×; no lock errors, no lost commits (shared refs/locks in gait-dir .git)
```

**A7.2 Concurrent finish** `[int]`
```
finish A and finish B at the same time → both merged to main (one may retry);
no lost transcript, no half-removed worktree
```

**A7.3 Reconstruction purity** `[int]`
```
after interleaved parallel runs, reconstruct LLM messages for A
→ contains only A's events; B's tool calls never leak into A's context
```

---

## Suite B — Per-session codebase working trees (parallel trees + branches)

Architecture under test: the checkpoint repo made non-bare with a persistent workdir at
`sessions/<id>/code/`, alternates → user codebase objects. Each session edits its own tree.
`finish` delivers changes back (patch or branch — the contract test B5.1 pins). Fork
materializes sibling trees from any turn.

### B1. Materialization & isolation

**B1.1 Tree materializes from codebase HEAD via alternates** `[int]`
```
create session A → sessions/A/code/ tree == codebase HEAD tree;
A's .git/objects near-empty; objects/info/alternates → codebase objects;
codebase repo untouched (no .git/worktrees entry, refs identical)
```
Pins the design promise: alternates borrowing, never `git worktree add` into the user's repo.

**B1.2 Dirty-codebase baseline decision** `[int]`
```
user has uncommitted edit in src/app.rs; create session
→ pin: initial ckpt commit captures dirty state (or clean HEAD only — whichever ships);
session tree matches the pinned choice
```
Either answer is defensible; an accidental answer is not. The initial checkpoint
commit is the session's turn-0 baseline either way.

**B1.3 Full write isolation between sessions** `[int]`
```
A and B both edit src/app.rs (same path, different content) in their own trees
→ no interference; ckpt(A,1) and ckpt(B,1) diverge; user tree byte-identical to before
```
The payoff test: the entire Suite-A scope machinery is unnecessary here.

**B1.4 Ignored/untracked files not materialized** `[int]`
```
user tree has node_modules/ (ignored) and scratch.txt (untracked)
→ session tree contains neither; pin the consequence (deps must be rebuilt per session)
```
Checkpoint-from-codebase respects gitignore today; materialization inherits that.
Documents the operational cost of isolation.

**B1.5 User repo read-only invariant under N sessions** `[int]`
```
run 5 parallel sessions to completion (no finish)
→ user codebase .git/{refs,index,HEAD,config} byte-identical; worktree files byte-identical
```

### B2. Checkpoint mechanics with a persistent workdir

**B2.1 Per-turn commit of own tree** `[int]`
```
run_turn(A, edit x.rs) → ckpt(A,1) exists; tree(ckpt(A,1)) == sessions/A/code/ state
```
Same add_all path as today, workdir persistent instead of `set_workdir` per call.

**B2.2 reset --hard restore is fully safe again** `[int]`
```
A creates ghost.rs turn 2; restore_to(A,1) → ghost.rs gone; B's tree and user tree untouched
```
No pathspec, no FORCE-baseline subtleties, no remove_untracked helper —
isolation dissolves Suite-A's A4 complexity. Assert the simple path stays simple.

**B2.3 ⚠ Consecutive restores with stale HEAD** `[int]`
```
same sequence as A4.4 (restore→4 then restore→5) in a per-session tree
→ correct because reset --hard moves HEAD each time
```
The contrast test to A4.4: documents *why* this mode doesn't need the
post-restore checkpoint for correctness (though the transcript restore event should
still be emitted for audit parity — assert it is).

**B2.4 ⚠ Codebase gc / deleted-history hazard** `[int]`
```
session created from commit X on branch tmp; user: git branch -D tmp; git gc --prune=now
→ checkout/restore/diff in session repo; pin outcome
```
Same hazard as A3.7 but worse here: the *working tree's own base* can dangle, not just
old snapshots. Pin the mitigation: keep a ref/grafted commit in the session repo at
creation, or repack base objects in, or document loudly. Silent corruption is the only
unacceptable answer.

**B2.5 Object dedup at scale** `[int]`
```
large fixture repo (~500MB objects); create 10 sessions
→ disk growth ≈ 10 working trees + ε; sum of session object stores ≪ one repo's objects
```
Loose-bound assertion; protects the alternates wiring from a future refactor that
quietly turns it into 10 full clones.

**B2.6 Crash mid-checkpoint** `[int]`
```
kill -9 during commit → shadow repo opens cleanly next turn (libgit2 atomicity);
workdir intact; next checkpoint succeeds and captures current state
```

**B2.7 Concurrent checkpoints across sessions** `[int]`
```
A and B checkpoint simultaneously (separate shadow repos, shared alternates source)
→ both commits land; repeat under load; no object-store races
```

### B3. Fork

**B3.1 Fork materializes turn-N state** `[int]`
```
A runs 5 turns; fork(A, turn=3) → F:
  F's code tree == tree(ckpt(A,3));
  F's transcript == A's transcript truncated after turn 3 (byte-identical prefix);
  F has its own checkpoint repo; objects resolve (alternates → A's shadow and/or codebase)
```

**B3.2 Fork lineage event** `[int]`
```
F's transcript begins with (or session.json records) fork-origin: {parent: A, turn: 3}
```
Audit: a fork that looks like a spontaneous session lies about provenance.

**B3.3 Parallel divergence** `[int]`
```
post-fork: A runs turns 4–6, F runs its own turn 4, concurrently
→ ckpt(A,6) and ckpt(F,4) diverge; neither tree nor history contains the other's edits;
A's original turns 1–3 commits unchanged
```

**B3.4 Fork of a fork** `[int]`
```
fork(F, turn=3') → G; chain of alternates resolves; G restore→3' works;
deleting/finishing A after forks → pin: F and G still function (see B2.4 hazard —
parent shadow removal must not strand children's borrowed objects)
```
This is the alternates-chain lifetime question. Likely answer: fork copies/repacks
needed objects or holds a ref; the test forces the decision.

**B3.5 Fork while parent mid-turn** `[int]`
```
A executing turn 6; fork(A, turn=3) concurrently
→ allowed; F's tree is exactly ckpt(A,3) regardless of A's in-flight worktree state
```
Fork must read committed history only, never the live workdir.

**B3.6 Fork at turn boundaries** `[int]`
```
fork at turn 0 (pre-first-turn baseline) → tree == session baseline;
fork at latest turn → tree == current committed state;
fork at turn > latest → clean error
```

**B3.7 Undo inside a fork doesn't touch the parent** `[int]`
```
F restore→2 (a turn inherited from A) → A's tree, A's shadow repo, A's transcript untouched
```
Shared-prefix history, independent state.

### B4. Trace repo + nested-repo hygiene

**B4.1 ⚠ Session code tree excluded from trace commits** `[int]`
```
sessions/A/ is a trace-repo worktree; sessions/A/code/ now lives inside it
→ run turns; inspect every trace commit on session/A: contains transcript/session.json,
never any file under code/ (per-session .gitignore gains `code/` next to `codebase.git/`)
```
Forgetting this one line commits the user's entire codebase into the trace repo,
once per turn. Cheapest catastrophic bug in the suite.

**B4.2 Nested-repo command routing** `[int]`
```
run `git status` / `git add` with cwd=sessions/A/code/ → operates on the session repo,
never the trace repo (and vice versa from sessions/A/)
```
Pins GIT_DIR discipline / the info/exclude `.git` trick.

**B4.3 Worktree churn under concurrency** `[int]`
```
spawn 10 sessions concurrently → 10 trace worktrees + branches, no
.git/worktrees lock failures; finish 5 concurrently → clean removal, no orphan
worktree metadata (`git worktree list` in gait-dir matches live sessions)
```

### B5. Finish & delivery (the new user-visible contract)

**B5.1 finish delivers without touching the user worktree** `[int]`
```
A edits files; gait finish A → pin the shipped contract, e.g.:
  branch `gait/A` created in user repo pointing at A's final tree (one explicit write), or
  patch file emitted to outputs
→ artifact applies/merges cleanly onto user HEAD; user *working tree* untouched either way
```

**B5.2 Conflicting finishes surface cleanly** `[int]`
```
A and B both modified the same region of src/app.rs in their trees;
finish A (applies); finish B → conflict reported (patch reject / branch needing merge);
nothing half-applied; B's session state intact for retry
```
The honest cost of isolation: Suite A's "no merge step" claim does not hold here.
This test pins the failure UX, which is the real product surface.

**B5.3 finish after user advanced HEAD** `[int]`
```
user commits unrelated work mid-session; finish A
→ pin: 3-way result vs clear "based on <sha>, rebase needed" messaging; never silent overwrite
```

**B5.4 finish cleanup** `[int]`
```
finish A → session/A branch merged to main (transcript preserved in history);
sessions/A/ worktree removed; checkpoint repo removed; disk reclaimed;
gait sessions no longer lists A
```

**B5.5 finish vs concurrent fork-child** `[int]`
```
A has live fork F; finish A → pin: blocked until F resolves, or allowed with F's
borrowed objects preserved (ties to B3.4); never: F silently corrupted
```

### B6. Composition with Suite A

**B6.1 Scoped children inside one session tree** `[int]`
```
session P (own tree) spawns scoped children A [src/parser/], B [src/lexer/]
→ rerun A3.1 and A4.1 invariants against sessions/P/code/ instead of the user codebase;
Landlock enforcement scope roots at the session tree path
```
One representative composition test; the two features must not assume "the codebase"
is the user's directory.

**B6.2 Undo in one session invisible everywhere** `[int]`
```
A restore→1 → B's tree, F's tree, user tree all byte-identical to before
```
The suite's closing invariant, worth a dedicated cheap assertion even though
B1.3/B3.7 imply it.

---

## Harness primitives worth building first

- `fixture_codebase(spec)` — deterministic repo builder (files, commits, ignored dirs, a deletable branch for the gc tests).
- `scripted_agent(turn → ops)` — fake model driving `gait run` with predetermined tool calls; everything above must run without a live LLM.
- `tree_of(ckpt)` / `assert_tree_only(ckpt, prefixes)` — committed-tree inspection.
- `assert_repo_untouched(path, snapshot)` — byte-level refs/index/worktree comparison for the user repo.
- `with_kill(at_event)` — crash injection keyed on transcript events (mid-turn, mid-checkpoint).
- `kernel_matrix` — CI lanes: no-Landlock, ABI 1, ABI 2, ABI ≥4 (Truncate), macOS Seatbelt.

## If you only write ten first

A3.1, A3.3, A4.4, A4.3, A2.7, A5.1, A1.13, B4.1, B2.4, B3.1 — these pin every silent-lie
bug class identified in review: checkpoint contamination, stale-HEAD ghosts, auto-mode
lockout, read-restricted sandbox, spawn races, trace-repo codebase ingestion, alternates
gc, and fork provenance. Everything else fails loudly; these are the ones that fail by lying.
