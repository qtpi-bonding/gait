---
name: gait-agent
description: Use gait — this repo's turn-based AI coding agent — either as a dispatchable subagent for scoped exploration/implementation tasks, or interactively via its REPL. Use when a task calls for a second coding agent to explore/implement against a codebase in the background, or when a human wants to drive gait turn-by-turn themselves.
---

# gait

gait is a stateless, turn-based coding agent: one invocation = one turn (read/edit/run/commit), then it exits. No daemon, no server. Every turn is committed to a session's git branch, so nothing is lost if it's interrupted. Full reference: [README.md](../../README.md).

There are two ways to use it:

1. **As a subagent** — another agent (you) dispatches `gait "<task>"` in the background and reads the result when it exits. Good for well-scoped exploration or implementation work you want to hand off.
2. **Interactively** — a human runs `gait -i` and drives it turn-by-turn in a REPL.

## One-time setup

Every gait invocation needs a **gait-dir** (holds config + session history) and a **codebase** (the repo being worked on — they can be the same directory, or the gait-dir can live elsewhere and point at any repo via `--codebase`).

```bash
mkdir -p /path/to/gait-workspace && cd /path/to/gait-workspace
gait init                       # scaffolds gait.jsonc, agents/default/AGENT.md, etc.
gait auth add anthropic         # or openai / minimax / any supported provider
export GAIT_DIR=/path/to/gait-workspace   # lets you drop --gait-dir from every call below
```

Check `gait.jsonc`'s `model` field is set to a provider you've authenticated (`gait models` lists what's available).

---

## Using gait as a subagent

Dispatch it in the background — it runs to completion on its own and exits; there's nothing to poll.

```bash
gait "Read src/auth.rs and src/session.rs, summarize the session lifecycle and what events get written to the transcript. Call structured_done() with your findings." \
  --gait-dir /path/to/gait-workspace \
  --codebase /path/to/target/repo \
  --auto-approve
```

Notes:

- **The task is a bare positional string** — there's no `--task` flag.
- **`--codebase` is the sandbox root** — gait cannot read outside it. If the task needs to read from one directory and edit another, pass their common ancestor as `--codebase`.
- **`--auto-approve` skips human permission prompts** but not the kernel-level sandbox (`readable`/`writable`/`network` in `gait.jsonc`). For a task that writes files or runs shell commands, also pass explicit `--allow` patterns (e.g. `--allow 'write:*' --allow 'shell:*' --allow 'edit_replace:*'`) — treat broad `shell:*`/`write:*` grants as a real capability escalation and confirm with the user before using them, the same way you would for any other broad auto-approval.
- **One task per invocation.** gait has no memory between processes; give it everything it needs in the prompt (absolute paths, the exact test/build command, the commit message) and keep each dispatch to one logical change. Continue related work in the same session with `--session <id>` (printed at start as `{"type":"session_id","id":"..."}`) rather than starting fresh each time.
- **Always end the prompt** with an explicit completion instruction, e.g. `Call structured_done() when done.` — otherwise you can't tell success from a silent stall.
- **Prefer intent over verbatim code.** Describe the goal, name the files, point it at a spec/plan to read rather than pasting code blocks. If the change is too tricky or error-prone to describe this way, it's a sign you should do it yourself instead of delegating it.

After it exits, verify independently — don't trust "done" at face value:

```bash
git -C /path/to/target/repo log --oneline -3
gait ps --gait-dir /path/to/gait-workspace --json      # list active sessions + their state
gait stats <session-id> --gait-dir /path/to/gait-workspace   # token usage / cost
```

When you're satisfied with a session's result, merge its transcript into the gait-dir's history (or drop it if the run didn't pan out):

```bash
gait persist <session-id> --gait-dir /path/to/gait-workspace   # merge + keep
gait discard <session-id> --gait-dir /path/to/gait-workspace   # tear down, no merge
```

Multiple sessions can run in parallel against the same codebase as long as they touch disjoint files — each has its own lock, log, and shadow checkpoint repo. Don't run `cargo build`/`test`-equivalents from two parallel sessions at once; they'll fight over shared build artifacts. Run the full build/test suite yourself, once, after all parallel sessions land.

---

## Using gait interactively (REPL)

For a human driving gait turn-by-turn instead of one-shot dispatch:

```bash
gait -i --gait-dir /path/to/gait-workspace --codebase /path/to/target/repo
```

Inside the REPL:

| Command | Aliases | What it does |
|---|---|---|
| `/help` | `/?` | Show the command guide |
| `/info` | | Show model, session, gait-dir, codebase |
| `/model` | | Show or switch model (`provider/id`) |
| `/new` | `/n` | Start a fresh session |
| `/undo` | `/u` | Undo the last turn (code + transcript) |
| `/sandbox` | | Toggle the sandbox |
| `/unlock` | | Grant access when a turn is paused on a sandbox boundary |
| `/workflow` | | Run a workflow |
| `/exit` | `/quit`, `/q` | Exit the REPL |

Message syntax:

| Syntax | What it does |
|---|---|
| `!cmd` | Run a shell command locally, captured into shell history |
| `$(N)` / `$(cmd)` | Paste shell history entry N / inline a fresh command's output |
| `>(N)` / `>(N-M)` | Quote line(s) from the last agent response |
| `@path` | Inline a file's contents into the message |

Resume a prior REPL session later with `gait -i --session <id>`.
