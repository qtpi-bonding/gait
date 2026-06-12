#!/usr/bin/env bash
#
# edit-syntax-check.sh — fast, per-file syntax guard for gait's `edit` tool.
#
# Wire it as a post_tool hook on `edit` in gait.jsonc:
#
#   "hooks": [
#     { "event": "post_tool", "tool": "edit",
#       "run": "/abs/path/to/edit-syntax-check.sh \"$GAIT_ARG_FILE_PATH\"" }
#   ]
#
# gait passes the edited file as $GAIT_ARG_FILE_PATH (also accepted as $1). On a
# non-zero exit, gait appends this script's output to the tool result, so the
# model sees the breakage and fixes it on its next turn — turning a silent
# corruption (C3: a dropped brace reported as success) into a recoverable error.
#
# Design rules:
#   - PARSE, don't compile. We only need to catch structural breakage (e.g. a
#     dropped delimiter), which is a parse error. `cargo check` is deliberately
#     NOT used: it is whole-crate and slow. Per-file parsers run in milliseconds.
#   - Language-agnostic via extension dispatch — gait edits Rust, JS, Python, … .
#   - Never mutate the file (checks are read-only; we emit to stdout/throwaway).
#   - Degrade gracefully: an unknown extension, a missing checker, or a missing
#     file all PASS (exit 0). The guard never blocks an edit on its own absence.

set -u

file="${1:-${GAIT_ARG_FILE_PATH:-}}"
[ -n "$file" ] && [ -f "$file" ] || exit 0   # nothing to check — pass

have() { command -v "$1" >/dev/null 2>&1; }

# fail <lang> <error-output> — print a model-readable message and exit non-zero.
fail() { printf '%s syntax error in %s:\n%s\n' "$1" "$file" "$2"; exit 1; }

case "$file" in
  *.rs)
    have rustfmt || exit 0
    # --emit stdout parses the file and prints the formatted result; it exits
    # non-zero ONLY on a parse error (e.g. "unclosed delimiter"), not on style.
    err=$(rustfmt --emit stdout "$file" 2>&1 >/dev/null) || fail rust "$err" ;;
  *.js|*.mjs|*.cjs)
    have node || exit 0
    err=$(node --check "$file" 2>&1) || fail javascript "$err" ;;
  *.py)
    have python3 || exit 0
    err=$(python3 -m py_compile "$file" 2>&1) || fail python "$err" ;;
  *.json)
    have python3 || exit 0
    err=$(python3 -m json.tool "$file" /dev/null 2>&1) || fail json "$err" ;;
  *.sh|*.bash)
    have bash || exit 0
    err=$(bash -n "$file" 2>&1) || fail shell "$err" ;;
  *)
    exit 0 ;;   # unknown type — don't block the edit
esac

exit 0
