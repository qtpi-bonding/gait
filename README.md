# gait

A turn-based AI coding agent. State lives in an append-only JSONL log committed to git after every turn — there is no daemon, no server, no UI process holding your session in RAM.

The name is a backronym: **g**AI**t**. Also just a word for the way something moves.

![gait REPL: two turns edit main.py, then /undo 1 rolls the second one back](docs/demo/demo.gif)

*Two turns in the REPL, then `/undo 1` restores the code and the transcript to the end of turn 1. Recorded with [`docs/demo/demo.tape`](docs/demo/demo.tape).*

---

## Why it's shaped this way

Most agentic harnesses are **daemon-shaped**: one long-lived process owns every session as an in-memory object and multiplexes turns as tasks inside it. That process is the source of truth, which is why those systems need a separate "load/restore" step to survive a restart — and why the process running your model's tool calls is the same process holding your editor connection.

gait's bet is the opposite. **One turn is one sealed process that exits.** Each turn spawns a fresh `gait __brain` subprocess that reads the log from disk, runs a single turn, writes its events back, and dies. The durable truth is 100% on disk; nothing authoritative lives in RAM between turns.

The cost is a per-turn spawn. The payoff:

- **Hard isolation** — each turn's model + tool execution runs in its own kernel-sealed process, not the process holding your terminal/editor connection.
- **Crash safety** — kill anything mid-turn; the next invocation rebuilds from the log. No "lost session."
- **One source of truth** — CLI, REPL, and any future editor bridge read the *same* on-disk sessions with zero coordination, because none of them own session state.
- **Composability** — N concurrent sessions is just N sealed process-trees, the same mechanism subagents already use.

A long-lived *frontend* (the REPL, or an editor bridge speaking [ACP](https://agentclientprotocol.com)) is still allowed — it has to be, to hold an interactive connection open. But it is a **stateless driver**: it routes input to per-turn brains and streams their events back, holding nothing the disk doesn't already hold. The daemonless invariant is about *turns*, not the frontend.

Under the sealed path, each turn runs as three processes with different trust levels: a **spine** (the parent; sole disk reader/writer, capability authority), a **brain** (sealed; no disk, only the LLM network), and a per-tool-call **hand** (kernel-sealed to the scope of the call). The full threat model and seal mechanics: [docs/sandbox-security.md](gait-rs/docs/sandbox-security.md). The engine-internals architecture: [gait-rs/docs/architecture.md](gait-rs/docs/architecture.md).

---

## Prerequisites

- **Rust** (stable, edition 2021 — built/tested with 1.92) and **Cargo**. Install via [rustup](https://rustup.rs) if you don't have it.
- **macOS (Apple Silicon or Intel) or Linux.** The sandbox is kernel-enforced per platform — Seatbelt on macOS, Landlock on Linux — and works out of the box on Apple Silicon. Windows is untested and not currently supported.

  The `git` CLI is **not** required at runtime — gait talks to git exclusively through `git2`/libgit2. The build shells out to `git rev-parse HEAD` to stamp the binary with a build hash but degrades gracefully to `"unknown"` if `git` isn't on `PATH`.

- An **API key** for at least one model provider, or a local OpenAI-compatible server. See [docs/providers.md](docs/providers.md). Add it after install with `gait auth add <provider>`, or just set `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` in the environment.

## Install

```bash
cd gait-rs
cargo build --release
cp target/release/gait /usr/local/bin/gait
```

Or `make install` from inside `gait-rs/` (edit `Makefile` to set `INSTALL_DIR`).

The installed binary is small (tens of MB). The `target/` directory is several GB of intermediate Cargo build cache — `cargo clean` after copying the binary out is fine.

### Full gait vs. gait-min

`gait` and `gait-min` are the same source tree built with different Cargo feature sets — not a fork, not a runtime mode:

```bash
# Full gait: the existing default product
cargo build -p gait-bin --bin gait

# gait Min: no workflows, extensions, or rich TUI
cargo build -p gait-bin --no-default-features --features min --bin gait-min
```

Min keeps the CLI, the plain REPL, durable event-sourced sessions, the permission gate, the sandbox, and skills; it drops the `gait-workflow` DAG runner, MCP, plugins, Code Mode, the rich-TUI renderer (`termimad`/`syntect`/`termbg`), and the local model backend from its dependency tree entirely — see [gait-rs/docs/gait-min.md](gait-rs/docs/gait-min.md) for exactly what's in, what's out, and why.

## Quickstart

```bash
# 1. Initialize a gait-dir
mkdir my-workspace && cd my-workspace
gait init

# 2. Add an API key
gait auth add anthropic

# 3. Run one turn (bare positional task)
gait "add a health check endpoint to src/server.ts" --gait-dir . --codebase /path/to/your/repo

# 4. Set GAIT_DIR to skip the flag
export GAIT_DIR=$PWD
gait "run the test suite and fix any failures" --codebase /path/to/your/repo

# 5. Continue the same session
gait --session s1a2b3c4 "keep going"

# 6. Non-interactive (auto-approve all permissions; the sandbox still applies)
gait "..." --auto-approve

# 7. Interactive REPL instead of one-shot turns
gait -i

# 8. Ephemeral one-off (no session, no git — prints a shell cmd or an answer)
gait -e "what's the current git branch?"
```

Global credentials and the models cache live under `$GAIT_HOME`, which defaults
to `~/.gait-home`. This is separate from `GAIT_DIR`: the former is machine-local
state, while the latter is the Git-backed Gait workspace holding configuration
and session history.

Driving gait from another coding agent (as a dispatched subagent) or a human REPL session? See [`skills/gait-agent/SKILL.md`](skills/gait-agent/SKILL.md) — a portable, agent-agnostic skill covering both.

## Checkpoints and undo

Every turn is reversible. gait snapshots your codebase's working tree at the start and end of each turn and commits the transcript to git alongside it, so you can roll the code, the transcript, or both back to any earlier turn.

Snapshots live in a bare shadow repository inside the session directory, not in your project's `.git`. gait never writes to your repository's git history, branches or index. The command is `gait undo`; see [docs/cli.md](docs/cli.md) for its options and [docs/repl.md](docs/repl.md) for the REPL equivalents. The on-disk layout is in [gait-rs/docs/architecture.md](gait-rs/docs/architecture.md).

## Modes

| Invocation | Mode |
|---|---|
| `gait "<task>"` (bare positional) | Runs one turn of a session and exits. |
| `gait -i` / `gait --interactive` | Interactive REPL — iterate turn-by-turn in one process. |
| `gait -e "<prompt>"` / `gait --exec "<prompt>"` | Ephemeral one-off: no session, no git commit. |

Plus subcommands (`gait <subcommand>`) for everything else. See [docs/cli.md](docs/cli.md) for the full command reference.

### Top-level flags (apply to `gait <task>` and `gait -i`)

| Flag | Description |
|---|---|
| `-s, --session <id>` | Continue an existing session by id. |
| `-m, --model <id>` | Model override (`provider/model`). |
| `-a, --agent <name>` | Agent file to load. |
| `--auto-approve` | Auto-approve all tool-permission prompts. The sandbox ceiling still applies (kernel-enforced). |
| `--allow <pat>` | Pre-approve a tool pattern (repeatable). |
| `--deny <pat>` | Pre-deny a tool pattern (repeatable). |
| `--unlock <path>` | Pre-grant sandbox path access (repeatable). |
| `-n, --max-iter <n>` | Cap agentic tool-use steps. |
| `-R, --result-schema <file>` | JSON Schema file — force structured output for this turn. |
| `--inference <name>` | Pick a named inference profile (see [docs/providers.md](docs/providers.md)). |
| `--mode <name>` | Activate a mode overlay for this turn. |
| `--sandbox` / `--no-sandbox` | Force the sandbox on/off for this turn. (Off by config default only — `--sandbox-locked` overrides either, set by parent processes.) |
| `--allow-intree` | Run even if the gait-dir is inside the codebase. Use sparingly. |

`--gait-dir`/`--codebase` are hidden flags — they default to `$GAIT_DIR`/`$CODEBASE`/cwd, and most subcommands below take them explicitly.

---

## Documentation

- **[docs/cli.md](docs/cli.md)** — every subcommand, every flag, including the ones not on the top-level.
- **[docs/repl.md](docs/repl.md)** — REPL slash-commands and input syntax.
- **[docs/providers.md](docs/providers.md)** — provider registry, model strings, local models, named inference profiles.
- **[docs/hooks.md](docs/hooks.md)** — lifecycle hooks, the `GAIT_*` env vars they see, event semantics.
- **[docs/workflows.md](docs/workflows.md)** — workflow YAML reference: node types, variable substitution, loops, gates, resume.
- **[docs/sandbox.md](docs/sandbox.md)** — the three-layer sandbox config, `--unlock`, `awaiting-unlock`, shell whitelist.
- **[gait-rs/docs/sandbox-security.md](gait-rs/docs/sandbox-security.md)** — brain/spine/hand process model, threat model, what's enforced vs. not.
- **[gait-rs/docs/architecture.md](gait-rs/docs/architecture.md)** — engine internals: crate layout, event log, projections, ops boundary, embedding API.
- **[gait-rs/docs/gait-min.md](gait-rs/docs/gait-min.md)** — the `gait-min` compile-time composition: what's in, what's out, and how it's verified.
- **[gait-rs/docs/embedding.md](gait-rs/docs/embedding.md)** — driving gait from another program over the Turn I/O Boundary.

## Config: `gait.jsonc`

JSONC (JSON with `//` and `/* */` comments). The full schema lives in
`gait-rs/crates/gait-state/src/config/`; a minimal version:

```jsonc
{
  "model": "anthropic/claude-sonnet-4-6",
  "allow": ["read", "glob", "grep", "ls", "clarify", "inform"],
  "deny":  ["shell:git push *"],

  "sandbox": {
    "on": true,
    "readable": ["$CODEBASE"],
    "writable":  ["$CODEBASE"],
    "network": false
  }
}
```

Other fields that exist but have no user-facing example above (all optional, with sane defaults): `compaction_threshold`, `max_iter`, `output_schema_retries` (default 3), `preview` (`on-ask` | `always` | `off`), `result_schema`, `shell_timeout_secs` / `tool_timeout_secs` (default 300s each), `llm_params` (temperature/max_tokens/top_p plus per-provider extras), `show_thinking`.

**Heads up:** several `gait register`/`enable`/`disable` operations rewrite this file as plain JSON and strip comments in the process. Use `gait config set <key> <value>` to mutate a single key without losing comments.

See [docs/sandbox.md](docs/sandbox.md) for the full sandbox schema, including `sandbox.tools` (per-tool floor) and `sandbox.shell_whitelist` (auto-grant shell patterns).

## Built-in tools

All tools go through the permission gate. Tools in `allow` run immediately; others prompt the user (or auto-approve with `--auto-approve`). The sandbox applies underneath the permission gate regardless of `allow`.

| Tool | What it does |
|---|---|
| `read` | Read a file |
| `write` | Write a file (creates or overwrites) |
| `glob` | Glob file paths |
| `grep` | Regex search over file contents |
| `ls` | List a directory |
| `codesearch` | Semantic code search |
| `webfetch` | Fetch a URL |
| `shell` | Run a shell command |
| `structured_done` | Signal turn completion with a structured result |
| `spawn_agent` | Delegate a subtask to a new child session |
| `list_agents` | List active child sessions and their status |
| `clarify` / `inform` | Pause the turn to ask/tell the user something |
| `annotate` | Attach an annotation to the transcript |
| `prune` | Drop old context from the working transcript |
| `enable` | Toggle an MCP server/skill/plugin on for the rest of the turn |

For deeper custom logic, see the [extending section in docs/sandbox.md](docs/sandbox.md#extending-gait-mcp-skills-hooks-wasm) (MCP, skills, hooks) and [docs/workflows.md](docs/workflows.md) (spawn_agent, workflows).

---

## Development

```bash
cd gait-rs
cargo test --workspace    # run all tests
cargo build --release     # release build
make fmt                  # format the workspace
make lint                 # rustfmt --check + clippy -D warnings
make check                # lint + test (what CI/the hook enforce)
make check-min             # gait-min composition: fmt, Min tests/build, dependency-absence check
```

Two git hooks live in [`.githooks/`](.githooks/): `pre-commit` runs `cargo fmt --check` (Rust sources/manifests only); `pre-push` runs `cargo clippy -D warnings` (heavier, runs once before pushing). Activate with:

```bash
make -C gait-rs hooks      # or: git config core.hooksPath .githooks
```

Bypass with `git commit --no-verify` / `git push --no-verify`.

## License

gait is free software, licensed under the **GNU Affero General Public License, version 3 or later** (`AGPL-3.0-or-later`). See [LICENSE](LICENSE) for the full text and [NOTICE](NOTICE) for third-party attributions.

If you run a modified version as a network service, the AGPL requires you to also offer your users the source code for that modified version.

## Acknowledgements

UX ideas adapted from [aichat](https://github.com/sigoden/aichat) (MIT OR Apache-2.0).
