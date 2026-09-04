//! gait-repl: the interactive terminal frontend (REPL, chat loop, view layer).
//! Depends only on gait-core. The `gait` binary's `chat` subcommand enters here.
//!
//! `clippy::print_stderr` is denied crate-wide except inside `ui/` (the one
//! module allowed to touch the terminal) — every other raw stderr write must
//! go through `ReplBuilder::write_raw`, which holds the same `paint_lock` the
//! status-timer thread respects. See docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md.
#![deny(clippy::print_stderr)]

#[cfg(feature = "rich-tui")]
pub mod chat;
#[allow(clippy::print_stderr)]
pub mod plain;
pub mod repl;
#[cfg(feature = "rich-tui")]
pub mod syntax;
#[cfg(feature = "rich-tui")]
pub mod theme;
pub mod tools;
#[allow(clippy::print_stderr)]
pub mod ui;

// Frame-scripting primitives (Script/Turn/Frame + the mock-brain wire
// format + a fresh-gait-dir fixture) shared by both REPL flavors' e2e test
// harnesses. No ReplBuilder/Theme dependency, so this compiles under
// `dev-harness` alone with `rich-tui` off — see turn_script.rs's doc comment.
#[cfg(feature = "dev-harness")]
pub mod turn_script;

// Drives the real REPL turn-engine path through the themed ReplBuilder /
// ReplSource / ReplSink stack (`chat.rs`'s own path) so it can capture and
// annotate what the rich renderer would have painted — not meaningful
// without `rich-tui`. The plain REPL has its own, simpler turn-engine test
// harness in `plain.rs` that only needs `dev-harness` (for `mock_brain`).
#[cfg(all(feature = "dev-harness", feature = "rich-tui"))]
pub mod dev_harness;
