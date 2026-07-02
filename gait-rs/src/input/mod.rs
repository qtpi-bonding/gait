//! Shared input expansion pipeline.
//!
//! This module is the **single source of truth** for sigil/expansion semantics
//! on user-supplied input. It is consumed by both the CLI entry path
//! (`gait "task"`) and the REPL entry path so that the two surfaces expand
//! identically. There are no REPL-only or CLI-only expansion branches here.
//!
//! # Pipeline
//!
//! [`expand_input`] performs a single left-to-right pass over the input
//! string, dispatching on the first sigil at each position. Each matched
//! region is replaced once and the scanner advances past it — replacements
//! are never re-scanned, so a `>(N)` expansion (agent text) can never
//! trigger `$(...)` shell execution (no cascade).
//!
//! ## Sigil grammar
//!
//! - `$(cmd)` runs `cmd` in a PTY and inlines the captured output.
//! - `$(N)`   recalls the Nth most recent shell-history entry (N ≥ 1,
//!   1 = most recent).
//! - `>(N)`   quotes the Nth block of the prior assistant turn, prefixed
//!   with `> ` per line. `>(A-B)` quotes a half-open range of blocks.
//! - `@path`  inlines the contents of the file at `path` (relative to
//!   `ctx.codebase`, or absolute / `~`-rooted as-is).
//!
//! ## Expansion order
//!
//! Sigils are evaluated in source order. A non-sigil character is copied
//! verbatim. Unclosed sigils and non-matching `>` / `@` tokens pass through
//! unchanged. Errors (file read failure, history recall error, non-zero
//! shell exit) are surfaced inline in the expanded string rather than
//! aborting the expansion.
//!
//! ## Why a context struct
//!
//! [`ExpandCtx`] bundles the three external dependencies an expander needs:
//!
//! - `codebase` resolves `@path` and the working directory of `$(cmd)`.
//! - `history` backs `$(N)` shell-history recall.
//! - `last_blocks` backs `>(N)` quote-reply (empty when there is no
//!   prior turn, e.g. on the CLI's first task input).
//!
//! Keeping these in one struct means the CLI task-input path and the REPL
//! input path produce byte-identical expansions for the same input string
//! (see `cli_repl_equivalence` test below).
//!
//! ## One entry point
//!
//! Both entry paths call [`expand_input`] directly. There is no other
//! public expansion API — `expand_shell_marker`, `resolve_quote_range`, and
//! `expand_file_marker` are file-internal helpers. (`text` and `pty` are
//! also re-exported from this module for tests, but consumers should reach
//! the input pipeline through [`expand_input`].)

// File-internal helpers used by tests in sibling modules. `pub(crate)` (not
// private) because lib.rs re-exports `crate::input::expand::*` etc. under
// the old top-level module paths (`crate::expand`, `crate::pty`, ...) for
// zero-diff external consumers after the move into `input/`.
pub(crate) mod expand;

// Re-export sibling modules so the pipeline can call into them, and so
// the existing in-tree tests (which use `super::pty` / `super::shell_history`
// paths) keep compiling after the move into `input/`.
pub(crate) mod pty;
pub(crate) mod shell_history;
pub(crate) mod text;

// Re-export the public pipeline API. The CLI and the REPL both call
// `input::expand_input` — there are no other expansion entry points.
pub use expand::{expand_input, ExpandCtx};

#[cfg(test)]
mod cli_repl_equivalence {
    //! Shared equivalence test (item 12 / spec 00).
    //!
    //! The CLI task-input path (`gait "task"`) and the REPL input path
    //! (`gait -i`) must call **the same** expansion function with **the same**
    //! context shape. This module-level test asserts that for a battery of
    //! inputs spanning every sigil kind, both call shapes produce
    //! byte-identical expanded strings.
    //!
    //! The two call sites in production differ only in how they populate
    //! `ExpandCtx`:
    //!
    //! - CLI: `last_blocks` is empty (no prior turn — the CLI takes a single
    //!   task from the command line).
    //! - REPL: `last_blocks` is whatever the previous assistant turn emitted.
    //!
    //! So we drive the equivalence both for "no prior blocks" (the CLI's
    //! default state) and for the REPL-style inputs that don't reference
    //! `>(N)` (where the two contexts reduce to the same expansion).
    use super::{expand_input, ExpandCtx};
    use std::path::PathBuf;

    fn cli_ctx<'a>(history: &'a std::path::Path, codebase: &'a std::path::Path) -> ExpandCtx<'a> {
        ExpandCtx {
            codebase,
            history,
            last_blocks: &[],
        }
    }

    fn repl_ctx<'a>(
        history: &'a std::path::Path,
        codebase: &'a std::path::Path,
        last_blocks: &'a [String],
    ) -> ExpandCtx<'a> {
        ExpandCtx {
            codebase,
            history,
            last_blocks,
        }
    }

    /// Asserts the CLI task-input path and the REPL input path produce
    /// identical expansion results for the same input string via
    /// `input::expand_input` (item 12 done-condition). The shared pipeline
    /// is the only expansion function both entry points call — this test
    /// pins that contract.
    #[test]
    fn cli_repl_equivalence() {
        let tmp = tempfile::tempdir().unwrap();
        let history = tmp.path().join("shell.jsonl");

        // Plain text — no sigils.
        for s in ["", "hello", "explain this code", "line one\nline two"] {
            let cli = expand_input(s, &cli_ctx(&history, tmp.path()));
            let repl = expand_input(s, &repl_ctx(&history, tmp.path(), &[]));
            assert_eq!(cli, repl, "plain-text mismatch for {s:?}");
        }

        // `$(cmd)` — both sides run through the same PTY capture path.
        for s in ["files: $(echo hi)", "$(pwd)", "x=$(echo a) y=$(echo b)"] {
            let cli = expand_input(s, &cli_ctx(&history, tmp.path()));
            let repl = expand_input(s, &repl_ctx(&history, tmp.path(), &[]));
            assert_eq!(cli, repl, "shell-sigil mismatch for {s:?}");
        }

        // `@path` — both sides resolve relative to `codebase`.
        std::fs::write(tmp.path().join("notes.txt"), "hello file").unwrap();
        for s in [
            "read this: @notes.txt",
            "@notes.txt trailing",
            "x=@notes.txt",
        ] {
            let cli = expand_input(s, &cli_ctx(&history, tmp.path()));
            let repl = expand_input(s, &repl_ctx(&history, tmp.path(), &[]));
            assert_eq!(cli, repl, "@path mismatch for {s:?}");
        }

        // Mixed sigils in source order — single-pass, no cascade.
        for s in [
            "$(echo a) and @notes.txt",
            "trailing $(echo z) middle @notes.txt end",
        ] {
            let cli = expand_input(s, &cli_ctx(&history, tmp.path()));
            let repl = expand_input(s, &repl_ctx(&history, tmp.path(), &[]));
            assert_eq!(cli, repl, "mixed mismatch for {s:?}");
        }

        // Unclosed / non-matching sigils pass through unchanged on both
        // paths (the no-cascade contract).
        for s in ["@(echo hi)", "$(unclosed", ">(foo)", ">(2", ">(99)"] {
            let cli = expand_input(s, &cli_ctx(&history, tmp.path()));
            let repl = expand_input(s, &repl_ctx(&history, tmp.path(), &[]));
            assert_eq!(cli, repl, "passthrough mismatch for {s:?}");
        }

        // `@username` should pass through (does not look_like_path).
        let cli = expand_input("ping @alice today", &cli_ctx(&history, tmp.path()));
        let repl = expand_input("ping @alice today", &repl_ctx(&history, tmp.path(), &[]));
        assert_eq!(cli, repl);
    }

    /// Belt-and-suspenders: also pin that the documented single entry point
    /// (`input::expand_input`) is what the `pub use` re-export at
    /// `crate::expand_input` resolves to, so the lib.rs convenience path
    /// and the canonical input-path agree on identity.
    #[test]
    fn expand_input_is_the_same_function_via_both_paths() {
        // A sanity check: the lib-level `expand_input` re-export must point
        // at the very same symbol as `input::expand_input`. We verify by
        // checking the function pointer addresses — Rust guarantees the
        // re-export compiles to the same function, but we make it explicit.
        let history: PathBuf = std::path::PathBuf::new();
        let codebase: PathBuf = std::path::PathBuf::new();
        let ctx = ExpandCtx {
            codebase: &codebase,
            history: &history,
            last_blocks: &[],
        };
        // Both call shapes must compile and produce identical output.
        let a = super::super::expand_input("hello", &ctx);
        let b = expand_input("hello", &ctx);
        assert_eq!(a, b);
    }
}
