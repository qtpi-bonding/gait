# REPL

`gait -i` starts the interactive REPL. It begins a fresh session (or resumes one with `--session <id>`) and runs turn-by-turn in one process. The REPL is a *stateless driver* — it routes your input to a fresh per-turn brain and streams events back, holding nothing on disk that the brain doesn't already hold. The actual turn engine is the same one the CLI uses.

## Slash commands

Source of truth: `crates/gait-repl/src/repl/commands.rs::COMMANDS`. The REPL and the CLI share op-level logic for everything below; the slash form is just an in-REPL invocation.

### General

| Command | Aliases | What it does |
|---|---|---|
| `/help` | `/?` | Show this command guide. |
| `/info` | | Show model, session, gait-dir, codebase. |
| `/auth` | | List or inspect credentials; link/unlink external CLI authentication. |
| `/exit` | `/quit`, `/q` | Exit the REPL. |

### Shell

| Command | Aliases | What it does |
|---|---|---|
| `/sh` | `/e` | One-off: describe a shell command, optionally run it. Same effect as `gait -e` but in REPL context. |

### Inference and mode

| Command | What it does |
|---|---|
| `/inference` | List or select a named inference profile (see [providers.md](providers.md#named-inference-profiles)). |
| `/mode` | Show or switch the active mode overlay. |

### Session

| Command | What it does |
|---|---|
| `/new` (`/n`) | Start a fresh session. |
| `/workflow` | Run a workflow. |
| `/sandbox` | Toggle the sandbox. |
| `/unlock` | Unlock a held (`awaiting-unlock`) session. |
| `/skill` | List, load, or unload skills / MCP servers / plugins. |

### Undo

| Command | What it does |
|---|---|
| `/undo [N]` (`/u`) | Restore code and transcript to the end of turn N. With no N, list the turns and change nothing. |
| `/uc [N]` | Restore code only to the end of turn N. |
| `/ut [N]` | Restore transcript only to the end of turn N. |

A turn here is one request you sent, and turn N is the state right after it finished, so `/undo 2` after three turns rolls back turn 3. `/uc 0` restores the code to before the first turn; `0` is code-only, because the transcript has nothing earlier to go back to.

### External authentication

The REPL uses the same non-secret auth-source operations as the CLI:

```text
/auth
/auth status openai-codex
/auth link openai-codex codex
/auth link openai-codex codex --home /custom/codex/home
/auth unlink openai-codex
```

Linking selects Codex's existing login; it does not copy or revoke Codex's
credentials.

---

## Input syntax (works in any message)

| Syntax | What it does |
|---|---|
| `!cmd` | Run a shell command locally; output is captured into shell history. |
| `$(N)` | Paste shell history entry `N` into your message. |
| `$(cmd)` | Run `cmd` and inline its output into your message before sending. |
| `>(N)` | Quote line `N` from the last agent response (supports ranges `>(N-M)`). |
| `@path` | Inline the contents of a file at `path` into your message. |

These are expanded before the message is sent — useful for giving the agent the output of a command you just ran, or pointing it at a specific file without retyping its contents.

## Where the REPL writes

The REPL is a thin frontend over the same engine the CLI uses. State lives in the gait-dir:

- **Session log** — `<gait-dir>/sessions/<id>/log.jsonl` (the source of truth).
- **In-flight worktree** — `<gait-dir>/.active/<id>/` (gitignored, removed on `gait persist` or `gait discard`).
- **Config** — `<gait-dir>/gait.jsonc`.

Global credentials and the models cache do not live in the gait-dir. They use
`$GAIT_HOME/auth.json` and `$GAIT_HOME/cache/models.json`; `GAIT_HOME` defaults
to `~/.gait-home`.

If the REPL dies mid-turn, the turn is committed to the log and the brain subprocess is reaped by the OS. Re-running `gait -i --session <id>` continues from where the log left off.

## Configured verifiers

Pass `--verified-goal <command>` at REPL startup to set a verifier for the session. When configured, the verifier runs after every completion attempt to gate the turn. The command is resolved at session startup and remains fixed for all turns in that session — so a session that starts with a verifier keeps it, and one that starts without keeps that snapshot too.

For example:

```bash
gait -i --verified-goal "cargo test -p my-crate"
```

means every turn in that REPL session will require `cargo test -p my-crate` to exit 0 before allowing completion. Verifier failures produce same-turn feedback describing the failure, and the model can continue generating to address it.
