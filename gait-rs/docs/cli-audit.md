# gait CLI audit

## Entry / main

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait init [path]` | scaffold gait-dir: config, agent file, gitignore, initial commit. idempotent. | `git init` | — | `gait init` (unchanged) |
| `gait run --task "..." [--session] [--auto] [--model] [--allow] [--deny] [--max-iter]` | non-interactive single turn. no `--session` = new session. `--session` = continue. exits when done. | — | `make`, `sh -c "..."` | `gait "task" [--session] [--auto] [--model] [--allow] [--deny] [--max-iter]` — drop `run`, task is bare positional |
| `gait chat [--session] [--agent]` | interactive REPL. `/n` new session, `/q` exit. | — | `python -i`, `node` (bare) | `gait -i [--session] [--agent]` — drop `chat` subcommand; `-i` is the only REPL entry point |
| `gait -e "request"` | ephemeral one-off. no session, no log. model returns a shell cmd (offers to run) or plain text. | — | `perl -e`, `python -c` | `gait -e "request"` (unchanged) |

---

## Session

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait sessions [--gait-dir]` | list all sessions: branch + worktree + state. like `git branch` but richer. | — | — | `gait sessions` (unchanged); add `--json` |
| `gait status <id> [--json]` | show session runtime state: idle / awaiting-permission / awaiting-clarify / awaiting-unlock | — | `ps`, `systemctl status` | `gait ps [--json]` — no id, lists only live/running sessions with runtime state inline. distinct from `sessions` (which shows all) |
| `gait finish <id>` | merge session worktree back into main, remove checkpoint db | — | — | `gait persist <id>` — merge + cleanup; `gait discard <id>` — throw away without merging |
| `gait undo <id> --turn N [--code] [--transcript]` | restore codebase and/or transcript to a prior turn. `--list` shows turns. | — | — | `gait undo` (unchanged); add `--json` |
| `gait cost <id> [--json]` | show token usage for the session (input / output / cache read / cache write) | — | — | `gait stats <id> [--json]` — expand to include timing per turn, wall-clock total |
| _(missing)_ | cancel a live running session | — | `kill` | add `gait stop <id>` |

---

## Permission flow

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait allow <id> [--pattern] [--scope once\|session\|agent\|global]` | approve pending tool permission request. scope controls whether it persists to config. | — | `chmod +x`, `sudo` (grant) | add `session` scope (was missing; matches `unlock`) |
| `gait deny <id> [--scope once\|session\|agent\|global]` | reject pending tool permission request | — | `chmod -x` (revoke) | add `session` scope; add `--json` |
| `gait unlock <id> [--scope once\|session\|global\|agent]` | grant sandbox path access (separate from tool permission). scope controls persistence. | — | `chown`, `setfacl` | add `--json` |

---

## Extension management

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait register mcp\|skill\|plugin <name> [flags]` | add an mcp server, skill, or wasm plugin to the gait-dir. starts disabled. | `git remote add` | `apt install`, `brew install` | |
| `gait unregister mcp\|skill\|plugin <name>` | remove an mcp server, skill, or plugin | `git remote remove` | `apt remove`, `brew uninstall` | |
| `gait enable mcp\|skill\|plugin <name>` | activate a registered extension (sets enabled: true) | — | `systemctl enable` | |
| `gait disable mcp\|skill\|plugin <name>` | deactivate without removing | — | `systemctl disable` | |

---

## Auth / models

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait auth list\|add\|remove [provider]` | manage API keys per provider. `add` prompts interactively. | `git credential` | `keychain`, `pass` | |
| `gait models [filter]` | fetch and list available models, optionally filtered | — | — | |

---

## Workflows

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait workflow run <name> [args...]` | run a named workflow yaml | — | `make <target>` | |
| `gait workflow list` | list defined workflows | — | `make -p` (list targets) | |
| `gait workflow continue <id>` | resume a paused workflow at its gate | — | — | |
| `gait workflow abort <id>` | cancel a running workflow | — | `kill` | |
| `gait workflow log <id>` | show workflow run log | — | `journalctl`, `tail -f` | |

---

## Daemons

| command | what it does | git analog | unix analog | rename |
|---|---|---|---|---|
| `gait daemons [--gait-dir]` | list running background processes started by the agent | — | `ps aux`, `systemctl list-units` | |

---

## Flags

Flags across entry commands. `gait "task"` = scripted run, `gait -i` = REPL, `gait -e` = ephemeral. `gait "task"` and `gait -e` should have identical flag sets where applicable.

Bare positional task conflicts with subcommand names are resolved with `--`: `gait -- "sessions are broken"`.

| flag | what it does | `gait "task"` | `gait -i` | `gait -e` | short | rename |
|---|---|---|---|---|---|---|
| positional | task prompt text | ✓ | — | ✓ | — | |
| `--session` | continue an existing session by id | ✓ | ✓ | — | `-s` | |
| `--model` | override model (provider/model string) | ✓ | ✓ | ✓ | `-m` | add to `-i`; make consistent |
| `--agent` | agent file to load | ✓ | ✓ | — | `-a` | drop from `-e` (breaks ephemeral contract) |
| `--auto` | auto-approve all permission requests | ✓ | — | ✓ | — | add to `-e`; long form only |
| `--allow` | pre-approve tool pattern (repeatable) | ✓ | — | ✓ | — | add to `-e`; long form only |
| `--deny` | pre-deny tool pattern (repeatable) | ✓ | — | ✓ | — | add to `-e`; long form only |
| `--max-iter` | cap agentic steps | ✓ | — | — | `-n` | freed from `--deny` |
| `--result-schema` | path to structured output schema | ✓ | — | — | `-R` | |
| `--unlock` | pre-grant sandbox path access (repeatable) | ✓ | ✓ | ✓ | — | add; replaces `--no-sandbox`; drop short form |
| `--json` | machine-readable output | where applicable | where applicable | — | — | add to all commands that output structured data |
| `--sandbox` | enable sandbox | ✓ | — | — | — | remove — sandbox is default on |
| `--no-sandbox` | disable sandbox | ✓ | — | — | — | remove — use `--unlock` |
| `--gait-dir` | path to gait directory (global) | ✓ hidden | ✓ hidden | — | `-d` | |
| `--codebase` | path to codebase root (global) | ✓ hidden | ✓ hidden | — | `-c` | |
