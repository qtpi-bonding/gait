# gait-rs agent guidelines

You are implementing features in **gait-rs** — a Rust port of the gait CLI agent runner.

## Codebase layout

The codebase root is `gait-rs/`. Key files:
- `src/bin/gait.rs` — CLI entry point (cmd_run, cmd_init, cmd_auth, etc.)
- `src/turn.rs` — core turn loop (`run_turn`, `TurnOpts`, permission gating)
- `src/session/transcript.rs` — `AnyEvent` enum, JSONL helpers
- `src/session/context.rs` — `reconstruct_messages` from transcript
- `src/tools/mod.rs` — `builtin_tools()`, tool registry
- `src/tools/*.rs` — individual tool implementations
- `src/config.rs` — `GaitConfig`, `ConfigTool` parsed from gait.jsonc
- `src/factory.rs` — `load_provider(model_str)` → `Box<dyn Provider>`
- `src/anthropic/` — Anthropic SSE streaming client
- `tests/` — integration tests

## Layout note

The codebase root is the `rs-port/` directory (not `gait-rs/`). All file paths for read/write/edit are relative to `rs-port/`. The Rust source lives under `gait-rs/` within that root. Plans live under `docs/superpowers/plans/`.

## Commands

- **Run all tests**: `cargo_test` tool, or `shell` with `cd gait-rs && cargo test 2>&1 | tail -30`
- **Check compilation**: `cargo_build` tool, or `shell` with `cd gait-rs && cargo build 2>&1 | grep '^error'`
- **Run specific test**: `shell` with `cd gait-rs && cargo test <test_name> 2>&1 | tail -10`

## Key patterns

- Tool implementations: see `src/tools/output.rs` for the minimal stateless pattern
- Tools that scope to codebase: see `src/tools/read.rs` for how `codebase_path` is used
- Adding a new tool: create `src/tools/foo.rs`, register in `src/tools/mod.rs`, add to `builtin_tools()`
- All tests use TDD pattern: write failing test → implement → verify passing → commit

## Invariants

- Always run `cargo test` after completing each task to verify no regressions
- Never skip steps in the plan — follow TDD order exactly
- Commit after each task with a meaningful message
- Do not modify `gait.jsonc` model field — it must always be `minimax/MiniMax-M1-80k`

## Working on a plan

When given a plan file, follow it task-by-task:
1. Read the task, understand what it requires
2. Write the failing test first
3. Run tests to confirm failure
4. Implement the code
5. Run tests to confirm passing
6. Commit
7. Move to the next task

Call `output()` when the entire plan is complete with a summary of what was implemented.
