# gait-workflow audit — findings (2026-07-18)

Audit of `crates/gait-workflow/src/` (multi-node workflow orchestrator) before trusting it for real multi-node runs. Two parallel passes: concurrency/isolation correctness, and control-flow/data correctness. Findings below, most severe first. None of these are fixed yet — this doc exists so the list survives before we get to each one.

Status legend: `[ ]` open, `[x]` fixed (link the plan/commit when closed).

## Findings

- [x] **1. No graph validation at all — cyclic/dangling references silently "complete" or silently no-op.**
  No cycle detection on `depends_on`, no check that `branch.then/else/cases`, `goto`, or `on_error.goto` reference real node ids (`grep -rn "fn validate"` in the crate returns nothing). A cyclic or dangling `depends_on` doesn't error or hang — `compute_ready_nodes` (`runner.rs:168-186`) never marks the deadlocked nodes ready, the main loop's ready-set goes empty, and it falls straight through to `emit_workflow_complete` → `Ok(WorkflowOutcome::Completed)` (`runner.rs:671-727, 1007-1021`) having run **zero** of the stuck nodes. A misspelled `on_error.goto` recovery target has the same shape: the failing node is still marked `Error` and unblocks its dependents as if nothing happened, but the recovery node never runs (`runner.rs:980-1002`). Dangling `branch`/plain `goto` targets are the same — silent no-op, no error surfaced (`runner.rs:807-870`). A multi-way `cases` branch with no matching case and no `else` is indistinguishable in the log from an intentionally-terminal path (`exec_branch.rs:7-12`, `runner.rs:833-834`).
  **Fix direction (agreed next step):** load-time graph validation — cycle detection + reference-existence check — before a workflow is allowed to run at all.
  **Fixed:** `docs/superpowers/plans/2026-07-18-workflow-graph-validation.md` — `validate_graph` in `crates/gait-workflow/src/validate.rs`, enforced in `run_workflow`. Known remaining gap (documented in the design spec's Non-goals, not closed by this fix): a `depends_on` target reachable only via `on_error.goto` that never runs still reproduces the same silent-fake-complete symptom.

- [x] **2. No resume idempotency — a killed-mid-node workflow re-runs that node's side effects on resume.**
  `WorkflowState::open_node()` (`state.rs:54-69`) exists specifically to detect "the node that was in-flight when we got interrupted" but is only referenced from tests — never called from `runner.rs`. `sync_completed` (`runner.rs:242-251`) only treats `{Done, Skipped, Error, Paused}` as completed; a node stuck at `Running` from a mid-execution crash is neither completed nor in-flight in the fresh process, so `compute_ready_nodes` just re-enqueues and re-runs it from scratch. For `exec_shell.rs` there is zero side-effect tracking — a `git commit`/`rm -rf`/deploy script that already partially or fully ran will run again on `gait workflow continue`. Not documented anywhere as "resume replays the interrupted node."
  **Fixed (shell nodes only):** `docs/superpowers/plans/2026-07-18-workflow-resume-and-subprocess.md` — `run_workflow` now refuses to re-enqueue a shell node left `Running` by an interrupted run unless the caller passes `--force-rerun <node-id>` (`gait workflow continue`). Scoped to shell nodes only — interactive prompt-node and gate-node suspension is unaffected (that `Running` state is a normal, expected resume signal, not a crash artifact). This is detect-and-refuse, not true idempotency: side effects already run before the crash are still not undone.

- [x] **3. Concurrent shell nodes share one worktree with zero locking, and bypass sandbox containment entirely.**
  Two nodes with no `depends_on` relationship run as real concurrent OS subprocesses in the identical `run_worktree` directory (`exec_shell.rs:36-54`, seeded via `runner.rs:565`). The only parallel test (`smoke_parallel_workflow_completes`, `tests/integration_workflow.rs:334`) uses two `echo`s with no shared-file interaction, so it cannot catch a real race between two file-writing/git-committing shell nodes. Separately: shell nodes call raw `tokio::process::Command` and never enroll in the `Containment`/cascade-group mechanism (`register_pid`/`configure_enrolled` are never called from this path) — prompt nodes get sandboxed via the spine/hand seal, shell nodes get nothing, and this asymmetry isn't documented anywhere in `exec_shell.rs`.
  **Won't-fix, by design (decided 2026-07-18):** treated as the same class of problem as two parallel Makefile recipes writing the same file — it's the workflow author's responsibility to not schedule conflicting shell nodes in parallel (or to add an explicit `depends_on` between them), not something gait auto-detects. The sandbox-containment half of this finding (shell nodes never enroll in the `Containment`/cascade-group mechanism prompt nodes get) remains a real, separate architectural gap, but is out of scope for this decision and not tracked as fixed — only the "shared worktree race" half is being closed as a non-issue.

- [x] **4. Timed-out or cancelled subprocesses leak — no `kill_on_drop`.**
  `exec_shell.rs:44-54` wraps `Command::output()` in `tokio::time::timeout`, but the `Child` future has no `kill_on_drop` set (unlike `src/tools/grep.rs`/`ls.rs`/`codesearch.rs`/`mcp/client.rs`, which all do). On timeout, or when a sibling node's failure causes the scheduler to `anyhow::bail!` and drop the whole `FuturesUnordered` task pool (`runner.rs:634`, `:922`), every other in-flight subprocess — shell and prompt-node brain spawns alike — is orphaned rather than reaped. Contradicts the "cascade group contains spawned subprocesses so nothing escapes reaping" design intent for this specific path.
  **Fixed:** `docs/superpowers/plans/2026-07-18-workflow-resume-and-subprocess.md` — `exec_shell_node` (`crates/gait-workflow/src/exec_shell.rs`) now sets `.kill_on_drop(true)` on its `Command`, matching the existing pattern in `src/tools/grep.rs`/`ls.rs`/`codesearch.rs`/`src/mcp/client.rs`.

- [ ] **5. Variable substitution is unescaped string splicing into `sh -c`.**
  `vars.rs::substitute()` does plain string/regex replacement with no escaping; `exec_shell.rs:40` feeds the result straight into `Command::new("sh").arg("-c").arg(&cmd)`. A prior node's freeform output (LLM text, a JSON field) containing shell metacharacters (`` ` ``, `$(...)`, `;`, an unbalanced quote) lands raw in a downstream shell node's command line — no quoting, no `--` boundary. Not an adversarial scenario, just the ordinary case of one node's output corrupting a sibling's command.

- [ ] **6. Branch conditions fail open on unresolvable expressions.**
  `vars.rs::substitute` replaces a variable reference to a node that never produced output with an empty string (`.unwrap_or_default()`, `vars.rs:79/92/102`). In `eval_bool_expr` (`vars.rs:126-148`), an expression like `"$test.exit == 0"` where `test` never ran substitutes to `" == 0"` — the empty-lhs guard skips every operator match, and the function falls through to its final line, `!matches!(expr, "" | "0" | "false")`, which returns **`true`** for `" == 0"`. A malformed/incomplete condition (missing dependency, untaken path) silently routes down the `then` branch instead of erroring — there's no way to distinguish "condition true" from "condition unparseable."

- [ ] **7. Hooks run inline in the scheduler with no timeout — one hanging hook stalls every parallel node.**
  `run_hook` (`hooks.rs:3-24`) is awaited directly inside the single-threaded scheduling loop (`runner.rs:357-361` before-hook; `:792-797/907-910/927-930/961-964` after/error hooks), not inside the `FuturesUnordered` task pool, and has no timeout. A hanging or slow hook on any one node blocks the scheduler from progressing *any* other ready node, including unrelated parallel branches — defeating the fan-out concurrency the rest of the design provides.

- [ ] **8. Run log is unrecoverable if the process is killed mid-write.**
  `read_events` (`run_log.rs:72-85`) does `serde_json::from_str::<WorkflowEvent>(line)?` per line, propagating on the first parse error. A crash mid-`writeln!` (`run_log.rs:234`) leaves a truncated last line; the next `gait workflow continue` fails to parse that line and errors out reconstructing **any** state — not even a partial resume from before the truncated event. Compounds with #2: even if `open_node()` were wired up, it couldn't help if the log itself won't parse.

- [ ] **9. Minor: silent data loss on log replay.**
  `state.rs:110`: `serde_json::from_str(&nc.json_output).ok()` silently drops a parse error, turning a corrupted/truncated `json_output` field into a silent `None` on replay instead of a visible error. Matters most in combination with #8 (a torn write producing a line that parses as a valid `WorkflowEvent` shell but has corrupted inner payload).

- [ ] **10. Minor: `RunOverrides` sharing across a multi-node initial ready-set has no type-level guard.**
  `runner.rs:648-669`'s initial ready-node seeding passes the same `run_overrides` (`cascade: Option<CascadeKillHandle>`, `fs_delegate`, `profile`, ...) to every node in `initially_ready`. Currently safe only because `single_prompt_run` (the sole caller that populates non-default `RunOverrides`) always constructs a one-node `Workflow` — nothing enforces that invariant if a future caller runs a multi-node graph with non-`None` overrides. Acknowledged informally in a comment (`runner.rs:91-98`) but not backed by the type system.

- [ ] **11. Note, not a bug: gate nodes are unconditional pauses, not evaluated approvals.**
  `exec_gate.rs` is 19 lines: substitutes the message string and always returns the pause sentinel. No expression evaluation, no approval predicate. Worth confirming this matches intent — "gate" reads as conditional but isn't.

## Confirmed NOT an issue (checked, ruled out)

- Cross-node variable leakage during concurrency: `VarContext.node_outputs` is a snapshot clone of only-`Done` nodes taken at enqueue time (`runner.rs:369-379`) — two racing nodes never see each other's in-flight output. Correctly scoped.
- No `todo!()`/`unimplemented!()`/`#[allow(dead_code)]` anywhere in the audited files.
- Sandbox `Containment` (cascade group / cgroup) TOCTOU gap exists in `gait-sandbox` but is currently dormant — nothing in `gait-workflow` shares one `Containment` instance across concurrently-spawning callers today.

## Next step

Fixing #1 (graph validation) first — cheapest fix, and it converts the worst failure class ("workflow silently lies about completing") into a load-time rejection, which also makes several of the other findings (#5, #6 in particular) easier to reason about once malformed graphs can't reach the scheduler at all.
