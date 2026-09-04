# CLI reference

The binary's full surface, verified against `crates/gait-bin/src/lib.rs` and `crates/gait-cli/src/cli/`. Every command takes `--gait-dir <path>` (defaults to `$GAIT_DIR`, then the current directory); subcommands that need it make it a real flag rather than hidden.

## Top-level modes

| Invocation | Mode |
|---|---|
| `gait "<task>"` (bare positional) | One turn of a session, then exit. |
| `gait -i` / `gait --interactive` | Interactive REPL. |
| `gait -e "<prompt>"` / `gait --exec "<prompt>"` | Ephemeral one-off: no session, no git commit. |

`task` / `-i` / `-e` are mutually exclusive. See [top-level flags](#top-level-flags-for-gait-task-and-gait--i) for the shared flag set.

The CLI is a thin dispatcher over `gait-core`; the Turn I/O Boundary, the run-loop, and the engine live in the library. Frontends other than the CLI (REPL, ACP adapters, embedders) all share that same engine. See [`gait-rs/docs/architecture.md`](../gait-rs/docs/architecture.md).

This reference covers the full `gait` binary's command surface. The `gait-min` binary (same source, a smaller compile-time feature set — see [`gait-rs/docs/gait-min.md`](../gait-rs/docs/gait-min.md)) exposes every command above except `gait workflow ...`; everything else, including the REPL, sessions, config, auth, and skill/plugin/MCP admin commands, is identical. (`gait local ...` is a separate `local` feature opt-in absent from *both* binaries' default builds — not a full/Min distinction.)

---

## `gait init [<path>]`

Scaffold a gait-dir at the current directory (or `<path>`) and make an initial git commit:

- `gait.jsonc` — config (committed)
- `agents/default/AGENT.md` — the agent's role and instructions (committed)
- `USER.md` — your personal instructions (gitignored, not shared)
- `skills/` — installed skills (`skills/<name>/SKILL.md`); committed once you add any
- `.active/` — live, not-yet-persisted session worktrees (gitignored)

`sessions/` is **not** created by `init` — it only comes into existence, as tracked content on `main`, once a session is persisted.

Safe to re-run on an existing gait-dir (idempotent). Use `--force` to allow the gait-dir to live inside your code tree (discouraged; also requires `--allow-intree` to actually run sessions in that layout).

## `gait link <gait-dir>`

Write a `.gaitref` pointer in the current directory naming an existing gait workspace, so a code repo can point at a separate gait-dir without copying `gait.jsonc` into itself. Pairs naturally with `gait init` for the out-of-tree layout:

```bash
gait init ~/gait-home          # one-time setup
gait link ~/gait-home          # inside the code repo, once
gait "<task>" --codebase .    # runs from any directory inside the repo
```

## `gait <task>` — run one turn

```bash
gait "<task>" [top-level flags]
```

Each completed turn is committed to the session's git branch. Interrupted turns are also committed — state is never lost.

## `gait -e "<prompt>"` — ephemeral one-off

One throwaway turn with no session and no git commit. The agent must return either a shell command (printed, and — on a TTY — offered to run after a `y/N` prompt) or a plain text answer. The result is a strict JSON object with exactly one of:

- `{"cmd": "..."}` — a shell command to run
- `{"output": "..."}` — a plain text answer

Stdin is read if piped (concatenated to the prompt, separated by a newline).

## `gait -i` — interactive REPL

See [docs/repl.md](repl.md).

## Top-level flags (for `gait <task>` and `gait -i`)

| Flag | Short | Description |
|---|---|---|
| `--session <id>` | `-s` | Continue an existing session by id. |
| `--model <id>` | `-m` | Model override (`provider/model`). |
| `--agent <name>` | `-a` | Agent file to load. |
| `--inference <name>` | | Pick a named inference profile (see [providers.md](providers.md)). |
| `--auto-approve` | | Auto-approve all tool-permission prompts. The sandbox still applies. |
| `--allow <pat>` | | Pre-approve a tool pattern (repeatable). |
| `--deny <pat>` | | Pre-deny a tool pattern (repeatable). |
| `--unlock <path>` | | Pre-grant sandbox path access (repeatable). |
| `--max-iter <n>` | `-n` | Cap agentic tool-use steps. |
| `--result-schema <file>` | `-R` | JSON Schema file — force structured output. |
| `--mode <name>` | | Activate a mode overlay for this turn. |
| `--sandbox` | | Force the sandbox on (overrides config). |
| `--no-sandbox` | | Force the sandbox off (overrides config). |
| `--allow-intree` | | Run even if the gait-dir is inside the codebase. |
| `--gait-dir <path>` | `-d` | Path to gait-dir (hidden; defaults to `$GAIT_DIR`). |
| `--codebase <path>` | `-c` | Path to codebase root (hidden; defaults to cwd). |

`--sandbox-locked` and `--cascade-group` exist for internal use (set by parent processes) and are hidden.

---

## Subcommands

### Session lifecycle

| Command | What it does |
|---|---|
| `gait sessions --gait-dir <path> [--json]` | List active (unmerged) session ids. |
| `gait ps --gait-dir <path> [--json]` | List sessions currently mounted under `.active/` with their live state (`idle`, `awaiting-permission`, `awaiting-clarify`, `awaiting-inform`, `awaiting-unlock`). |
| `gait persist [<id> | --all] --gait-dir <path> [--yes] [--json]` | Merge a session's worktree onto `main` and tear down its `.active/` mount. `--all` persists every idle session. |
| `gait discard [<id> | --all] --gait-dir <path> [--yes] [--json]` | Tear down a session **without** merging — the inverse of `persist`. |
| `gait stop <id> --gait-dir <path> [--json]` | **Currently a stub** — prints "stop not yet implemented" and exits 1. |
| `gait undo <id> [--turn N | --list] [--code] [--transcript] --gait-dir <path> [--json]` | Restore the codebase, the transcript, or both to a past turn. `--list` prints all turns with checkpoint SHAs. Without `--turn`, defaults to the last turn. |
| `gait stats <id> --gait-dir <path> [--json]` | Print token usage (`input`, `output`, `cache_read`, `cache_write`, `total`). |
| `gait reply <id> <message> --gait-dir <path> [--json]` | Answer a pending clarify/inform exchange from outside a running session. |

### Permissions and sandbox escalation

Approve or reject pending exchanges from outside a session (useful in headless or scripted pipelines).

```bash
gait allow <id> [--pattern <pat>] [--scope <once|agent|global|session>] [--agent <name>] --gait-dir <path> [--json]
gait deny  <id> --gait-dir <path> [--json]
gait unlock <id> [--scope <once|session|global|agent>] [--agent <name>] --gait-dir <path> [--json]
```

`--scope` controls how broadly the approval is saved:

| Scope | Effect |
|---|---|
| `once` (default) | Approves this single tool call only. |
| `session` | Logged in the session, doesn't persist beyond it. |
| `agent` | Adds the pattern to the named agent's frontmatter (requires `--agent`). |
| `global` | Adds the pattern to `gait.jsonc`. |

See [docs/sandbox.md](sandbox.md) for the distinction between **permissions** (gate what the agent *chooses* to do) and **unlock** (gate what the *kernel* will allow).

### Extension management

MCP servers, skills, and WASM plugins. Register and toggle each independently.

```bash
# MCP servers (written to <gait-dir>/mcp/<name>.json, added disabled)
gait register mcp <name> --command <prog> [--args "<args>"]   # stdio MCP
gait register mcp <name> --url <url>                          # HTTP MCP
gait register mcp <name> --spawn <cmd>                        # legacy compat: same as --command
gait register mcp <name> --connect <url>                      # legacy compat: same as --url
gait register mcp <name> [--env KEY=val]...                   # extra env vars

# Skills (inline prompt fragments, written to gait.jsonc — legacy; prefer skills/<name>/SKILL.md on disk)
gait register skill <name> --description <text> --path <file>
gait register skill <name> --description <text> --content <text>

# WASM plugins (copied into <gait-dir>/plugins/<name>/, added disabled)
gait register plugin <name> --wasm <path> [--description <text>]

# Toggle / remove
gait enable   <mcp|skill|plugin> <name> --gait-dir <path>
gait disable  <mcp|skill|plugin> <name> --gait-dir <path>
gait unregister <mcp|skill|plugin> <name> --gait-dir <path>

# Per-session toggle (outside a running turn)
gait load   <mcp|skill|plugin> <name> --session <id> --gait-dir <path>
gait unload <mcp|skill|plugin> <name> --session <id> --gait-dir <path>
```

`gait register` always adds entries **disabled**; run `gait enable` to activate.

WASM plugins are registered and toggled but **not executed at runtime** today — the manifest is loaded, but there's no execution path. Treat plugin as scaffolding.

### Auth and models

```bash
gait auth list                                  # show stored providers
gait auth add <provider>                        # prompt for API key and save
gait auth remove <provider>                     # delete stored key
gait auth status <provider>                     # show whether a provider is configured
gait auth login openai-codex                    # ChatGPT subscription device-code flow
gait auth logout openai-codex                   # remove a subscription
gait auth link openai-codex codex               # use the Codex CLI's existing login
gait auth unlink openai-codex                   # remove only gait's external binding

gait models                                     # list all available models
gait models anthropic                           # filter by substring
```

API keys and non-secret external-source bindings are stored in
`$GAIT_HOME/auth.json`; `GAIT_HOME` defaults to `~/.gait-home`. This global
home is separate from `GAIT_DIR` and is not a gait workspace or Git repository.
Keys can also be set via environment variables (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `MINIMAX_API_KEY`, etc.). Subscription login
(`openai-codex`) uses a device-code flow rather than an API key. Linking to
`codex` stores only a non-secret pointer in gait; credentials remain in Codex's
canonical `$CODEX_HOME/auth.json` file (normally `~/.codex/auth.json`).

`gait models` caches the models.dev catalog at
`$GAIT_HOME/cache/models.json`.

### Named inference profiles

```bash
gait inference list                                          # show all named profiles
gait inference show <name>                                   # show the resolved provider/model
gait inference select <name> --session <id> --gait-dir <p>  # bind a profile to a session
gait inference clear --session <id> --gait-dir <p>          # unbind
```

A named inference profile is a saved `(provider, model, llm_params)` triple. Selecting one for a session injects an `Inference` event into the log, so the choice is replay-complete.

### Config

The general primitive for mutating `gait.jsonc` from the CLI. The REPL's `/model` (and any other slash-command that mutates config) calls the same op under the hood.

```bash
gait config get <key> --gait-dir <path>
gait config set <key> <value> --gait-dir <path>     # JSON-parsed; falls back to string
gait config set model "anthropic/claude-sonnet-4-6"
gait config set max_iter 50
```

Values are JSON-parsed (so `50` becomes a number, `true` becomes a bool, `[...]` becomes an array); anything that doesn't parse falls back to a raw string. Comment-preserving.

### Daemons

```bash
gait daemons list --gait-dir <path> [--all] [--json]    # `--all` includes finished daemons
gait daemons stop <name> --gait-dir <path>              # stop a tracked background daemon
```

`gait daemons` shows the background processes the agent has spawned and registered in the gait-dir. They're distinct from the brain/spine/hand process tree — these are the *application-level* background tasks the agent has run.

### Workflows

Not available in `gait-min` — the `gait-workflow` DAG runner is excluded from that build's dependency tree entirely, not merely disabled. See [docs/workflows.md](workflows.md) for the full YAML reference.

`gait workflow list`/`gait workflow log` only ever see runs started by `gait workflow run`. An ordinary one-shot task or REPL turn does not create a `workflow-runs/` entry, in either build — see [`gait-rs/docs/gait-min.md`](../gait-rs/docs/gait-min.md#side-effect-affecting-both-builds-workflow-runs-bookkeeping).

```bash
gait workflow run <name> [args...] --gait-dir <path>     # run workflows/<name>.yaml
gait workflow validate <name> --gait-dir <path>          # parse + graph-validate without running
gait workflow list --gait-dir <path>                     # show all run ids + their status
gait workflow continue <run-id> [--force] [--force-rerun <node-id>]... --gait-dir <path>
gait workflow abort <run-id> --gait-dir <path>           # mark as failed (terminal event)
gait workflow log <run-id> --gait-dir <path>             # print the run log as JSONL
```

`--gait-dir` is accepted in either position (`gait workflow --gait-dir <p> run <name>` or `gait workflow run <name> --gait-dir <p>`).

`gait workflow continue --force` resumes the **originally logged graph** even if the on-disk YAML has since changed — without it, an edited YAML is a hard error. `--force-rerun <node-id>` accepts the risk of re-running a shell node that was left `Running` by an interrupted run (its prior side effects, if any, are *not* undone).

### Where `--gait-dir` and `--codebase` resolve

Resolution order, in this priority:

1. The explicit `--gait-dir` / `--codebase` flag.
2. The `GAIT_DIR` / `CODEBASE` environment variable.
3. A `.gaitref` file walked up from cwd (`gait link` writes these).
4. The current directory.

If none of these find a gait-dir, commands that need one exit 1 with `[gait] no gait dir: pass --gait-dir <path> or set $GAIT_DIR`.

### Exit codes

Most commands exit 0 on success, 1 on user error or runtime error. Specific codes with meaning:

- `gait workflow run` exits 2 on a `GatePaused` outcome (the workflow is paused at a gate node waiting for `gait workflow continue`).
- `gait workflow run` exits 0 on `NodeOpen` (paused at an interactive prompt node — not an error, just a checkpoint).
- `gait inference show` exits 2 on a missing argument (clap usage error).
