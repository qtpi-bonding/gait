# gait-min

`gait-min` is a second binary built from the same `gait-bin` crate as the full
`gait` binary, compiled with a smaller Cargo feature set. It is not a
different codebase, a fork, or a runtime mode switch — it's a compile-time
composition of the same source tree that leaves out entire dependency
subtrees (workflow DAG runner, MCP client, plugin/Code Mode sandboxes, rich
terminal renderer) so the resulting binary is smaller and its build pulls in
fewer crates.

## Build

```bash
# Full gait: the existing default product, unchanged behavior.
cargo build -p gait-bin --bin gait

# gait-min: no workflows, extensions, or rich TUI.
cargo build -p gait-bin --no-default-features --features min --bin gait-min
```

`gait` is built with `full` (the default feature set); `gait-min` requires
`--no-default-features --features min` — there is no default feature
combination that produces the `gait-min` binary, and `full`/`min` are mutually
exclusive per-binary `required-features`, not a runtime flag.

## What's in Min

Min is not a stripped-down demo — it's the full turn engine with a narrower
surface around it. Specifically, Min includes:

- **The CLI** — one-shot turns (`gait-min "<task>"`), session management,
  config, auth, named inference profiles, sandbox controls, permission
  register/enable/disable/load/unload for skills and (disabled) MCP/plugin
  entries in config, and everything else that isn't workflow- or
  rich-TUI-specific.
- **The plain REPL** — `gait-min -i` gets a line-oriented, non-themed REPL
  (no markdown rendering, no syntax highlighting, no background-color
  detection). It still transitively links `crossterm` via `reedline` for
  basic line editing (history, cursor movement) — that's terminal
  input handling, not the rich-TUI renderer this build excludes.
- **Durable, event-sourced sessions** — the same append-only JSONL log,
  the same git-committed session state, the same projections. There is no
  reduced or alternate session format for Min.
- **The permission gate** — `allow`/`deny` pattern matching, interactive
  approval prompts, `--auto-approve`. Identical to full `gait`.
- **The sandbox** — Seatbelt/Landlock kernel-enforced containment, on by
  default per your `gait.jsonc`/CLI flags exactly as in full `gait`.
  `--no-sandbox` remains an explicit, per-invocation opt-out in both builds;
  Min does not change the default, and does not remove the sandbox as a
  compile-time option.
- **Skills** — markdown-based capability files (`skills/<name>/SKILL.md`) are
  **not** gated out. Unlike MCP servers, WASM plugins, and Code Mode, skills
  stay always-on in both builds. They're core-adjacent (plain files loaded
  into the prompt, no extra runtime/sandbox/client dependency), not a heavy
  optional extension system, so there's no dependency cost to excluding them
  and no reason to.

## What's excluded from Min

These are genuinely absent from `gait-min`'s dependency tree, not merely
disabled at runtime — `cargo tree -p gait-bin -e normal --no-default-features
--features min` shows none of them:

- **Workflows** — the `gait-workflow` YAML DAG runner. No `gait-min workflow`
  subcommand, no `/workflow` REPL command, and `gait-workflow` itself is not
  a linked dependency.
- **MCP** — the `rmcp` client crate and everything under `gait-core`'s `mcp`
  feature. No live MCP server connections.
- **Plugins** — WASM plugin loading.
- **Code Mode** — `gait-codemode` and its `wasmtime`/`cranelift` WASI runtime.
- **The rich TUI** — specifically `termimad` (markdown rendering), `syntect`
  (syntax highlighting), and `termbg` (terminal background-color detection).
  As noted above, Min still links `crossterm` transitively through `reedline`
  for plain line editing; that is not the rich-TUI renderer being excluded.
- **ACP** — the Agent Client Protocol adapter (`gait-acp`) is a separate
  binary/crate that `gait-bin` never depends on in either composition; Min
  simply inherits that same absence.
- **The local model backend** — `gait local ...` and the in-process
  llama.cpp runner. This is off by default in *both* `gait` and `gait-min`
  (it builds llama.cpp from source, so it's an explicit `local` feature
  opt-in for either binary, not a full/Min split).

## No session-log backward-compatibility shim

Min introduces no cross-build session-log compatibility layer, and none is
needed. Protobuf's wire format already round-trips unknown fields safely by
default (unrecognized fields are preserved through decode/re-encode rather
than dropped), so a session log written by one build and later read by the
other — or by a future build with more or fewer features — behaves exactly
as protobuf always has. That is the entire extent of cross-build session-log
compatibility this plan introduces: none, because none is required.

## Side effect affecting both builds: `workflow-runs/` bookkeeping

As part of moving ordinary (non-`gait workflow run`) CLI, REPL, and ACP turns
off the `gait-workflow` DAG runner and onto the shared `gait-turn-runner`
crate, `workflow-runs/<run_id>/log.jsonl` is **no longer written for ordinary
turns** — in *either* build, full `gait` included. This is not Min-specific
and not gated by any feature flag; it falls directly out of the runner
extraction (an ordinary turn was previously implemented as a degenerate
one-node workflow run, which is why it used to leave a `workflow-runs/`
trace at all). A real `gait workflow run`/`gait-min workflow run` still
creates a `workflow-runs/<run_id>/` directory and log exactly as before —
only ordinary turns changed, and `gait workflow list` output reflects the
narrower set of runs it now sees.

## Verifying the composition

```bash
make check       # full gait: fmt, clippy, workspace tests
make check-min    # gait-min: fmt, Min feature tests, Min build, dependency-absence check
```

`make check-min`'s dependency-absence check runs
`cargo tree -p gait-bin -e normal --no-default-features --features min` and
fails if `gait-workflow`, `gait-acp`, `termimad`, `syntect`, `termbg`,
`rmcp`, or `wasmtime` appear anywhere in it. `-e normal` excludes
dev-dependency edges, so it reflects what actually ships in the binary, not
what the workspace's test suites happen to pull in.
