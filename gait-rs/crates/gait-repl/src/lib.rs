//! gait-repl: the interactive terminal frontend (REPL, chat loop, view layer).
//! Depends only on gait-core. The `gait` binary's `chat` subcommand enters here.
//!
//! `clippy::print_stderr` is denied crate-wide except inside `ui/` (the one
//! module allowed to touch the terminal) — every other raw stderr write must
//! go through `ReplBuilder::write_raw`, which holds the same `paint_lock` the
//! status-timer thread respects. See docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md.
#![deny(clippy::print_stderr)]

pub mod chat;
pub mod repl;
pub mod theme;
pub mod tools;
#[allow(clippy::print_stderr)]
pub mod ui;

#[cfg(feature = "dev-harness")]
pub mod dev_harness;
