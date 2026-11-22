---
name: gait-manual
description: Explain how to use and configure Gait.
---

# gait

AI coding agent

## Exit codes

| Exit | Meaning |
|---|---|
| 0 | ok |
| 2 | usage |
| 65 | logical |
| 69 | infrastructure |
| 70 | unknown |
| 75 | transient |

## Guides

### Getting started

Gait is an AI coding agent with a command-line interface and an interactive REPL.

Use the CLI to initialize a Gait directory, run one-off tasks, or continue a session.
Use the REPL when you want an interactive coding session.

#### REPL commands

- `/help` — Show this command guide (aliases: /?)
- `/info` — Model, session, gait-dir, codebase

Commands: `init`

### Running a task

Every task runs one of three ways: a bare positional (`gait "fix the login bug"`) starts
or continues the default session; `-e`/`--exec` runs a single hands-free request with no
session and no file/repo access; `-i`/`--interactive` opens the REPL. These three are
mutually exclusive.

`--auto-approve`, `--allow`, `--deny`, and `--unlock` control tool permissions for the
run. `--inference` selects a named model; `-a`/`--agent` loads an agent file; `-s`/`--session`
continues an existing session by id; `-n`/`--max-iter` caps agentic steps.

`-R`/`--result-schema` (a file) and `--result-schema-json` (inline) declare a JSON Schema
the final `structured_done` result must match; the two are mutually exclusive. Under `-e`,
the turn's workspace lives under a scratch temp directory that is deleted on exit —
`--gait-dir` is *not* where an ephemeral turn writes, and its diagnostics are lost with it
unless `GAIT_DEBUG=1` is set, which keeps the scratch directory and prints its path.

#### `gait`

```text
The turn's own flags. The registry hands over every argument after the leading globals, so a global given after the task (`gait "x" --gait-dir D`) is read here too

Usage: gait [OPTIONS] [TASK]

Arguments:
  [TASK]
          Task to run (bare positional: `gait "fix the login bug"`)

Options:
  -i, --interactive
          Interactive REPL mode

  -e, --exec <EXEC>
          Ephemeral one-off request

  -s, --session <SESSION>
          Continue an existing session by id

      --inference <INFERENCE>
          Named inference selection

      --verified-goal <VERIFIED_GOAL>
          Verified goal: fixed command run when a turn attempts completion to verify its goal

  -a, --agent <AGENT>
          Agent file to load

      --auto-approve
          auto-approve all tool-permission prompts; the sandbox ceiling still applies (kernel-enforced)

      --allow <ALLOW>
          Pre-approve tool pattern (repeatable)

      --deny <DENY>
          Pre-deny tool pattern (repeatable)

      --unlock <UNLOCK>
          Pre-grant sandbox path access (repeatable)

  -n, --max-iter <MAX_ITER>
          Cap agentic steps

  -R, --result-schema <RESULT_SCHEMA>
          Path to structured output schema

      --result-schema-json <RESULT_SCHEMA_JSON>
          Inline JSON structured output schema

      --allow-intree
          Override the in-tree safety guard (design §7): run even though the gait-dir is inside the codebase
```

### Sessions

Sessions preserve turns, tool activity, and workspace state.

Start a fresh session when you want a clean conversation. Use undo when you need to
restore an earlier turn or inspect available checkpoints.

A session's worktree lives under `<gait-dir>/.active/<id>/` until `gait persist` merges
it onto `main` or `gait discard` tears it down without merging (unrecoverable).
`gait ps` shows each mounted session's live state: `idle`, `awaiting-permission`,
`awaiting-clarify`, `awaiting-inform` or `awaiting-unlock`. `gait processes` lists the
background processes the agent started, which are separate from the turn's own
processes.

**Undo.** A "turn" in undo is one request you sent, and turn N is the state right
after that request finished, so restoring to turn 2 after three requests rolls back
the third. Gait snapshots the codebase at the start and end of each request in a
shadow repository inside the session directory; it never writes to your project's
`.git`. Undo can restore the code, the transcript, or both. Turn 0 restores the code
to before the first request and is code-only, because the transcript has nothing
earlier to go back to. In the REPL an undo never starts a model turn; after a
code-only undo (`/uc`) the next message you send carries a one-line note
(`[undo] code restored to turn N`) so the model knows the files changed under it.

#### REPL commands

- `/new` — Start a fresh session (aliases: /n)
- `/undo [N]` — Restore code + transcript to the end of turn N (no N: list turns) (aliases: /u)
- `/unlock [once|session|global|agent]` — Unlock a held session

Commands: `sessions`

### Permissions and modes

Every tool call is classified in order: deny (hard floor, wins even under
`--auto-approve`) → allow (pattern match) → ask (pauses for a human, or
for a client's permission UI). `--allow`/`--deny` on the command line set
the per-invocation baseline; `gait.jsonc`'s `allow`/`deny` fields set the
persistent global baseline.

A **mode** (`<gait_dir>/modes/<name>/MODE.md`) is a named, switchable
overlay of `allow`/`deny`/`steer`/`description` on top of that baseline —
`--mode <name>` on `run` activates one for a session; `gait mode
select --id <name> --session <id>` (or `gait mode clear --session <id>`) switches it mid-session. `gait mode
add --id <name> --allow <pattern> --deny <pattern> --steer <text>
--description <text>` authors a new one; `gait mode list`/`gait mode
show --id <name>` inspect what's authored. A mode's `deny` unions with the
baseline deny (deny is always monotonic — nothing ever un-denies);
its `allow` unions with the baseline allow. `steer` is a freeform
instruction folded into the model's context while the mode is active.
The file is markdown: YAML frontmatter holds `description`, `allow`,
`deny` and `classify_policy`; the body is the `steer`.

A mode can also set `classify_policy` (`inference`, `risk_threshold`,
`max_consecutive_denials`) to auto-resolve "ask" decisions via a fast
external classifier instead of always pausing — scoped to interactive
sessions where a human is present as the fallback. It always decides
(approve or deny), never falls back to a per-item prompt; the only
escalation path is `max_consecutive_denials` consecutive denials,
which stops trusting the classifier for the rest of the turn. A denied
call shows the same plain "permission denied" text any deny shows the
model; the specific axis (`confidentiality_risk`/`integrity_risk`/
`availability_risk`) that tripped is recorded in the session log's
`ClassifyDecision` event, not in the model-facing text.

`gait allow --id <session-id> --pattern <pattern> --scope <global|mode|session|once>`
answers a pending permission gate. `--scope mode` persists to the
session's *currently active* mode's `MODE.md` — it errors if no mode
is active (there is nowhere to write to). `--scope global` persists to
`gait.jsonc`. `--scope session`/`--scope once` are log-only, no file
write.

Commands: `allow`, `unlock`, `mode`

### Inferences and providers

Inferences name the models Gait can use. Provider credentials are managed separately
from inference selection.

Use the CLI for durable configuration and the REPL commands for inspecting or changing
the current interactive session.

API keys and non-secret external-source bindings live in `$GAIT_HOME/auth.json`
(`GAIT_HOME` defaults to `~/.gait-home`). That home is machine-local and separate from
the gait-dir, which is the Git-backed workspace holding configuration and sessions.
Keys can also come from environment variables such as `ANTHROPIC_API_KEY` and
`OPENAI_API_KEY`. Official ChatGPT plan sign-in uses `openai-signin`:
run `gait auth login --id openai-signin`, open the authorization URL, and paste the
resulting callback URL when prompted. For a later process, use
`gait auth login --id openai-signin --complete <url>` (or `--complete -` to read one
trimmed line from stdin). Sign out with `gait auth logout --id openai-signin`, adding
`--forget` when switching accounts. If the provider cannot revoke a session, disconnect
the app in ChatGPT Settings. The legacy `openai-codex` device-code path is unofficial;
linking it to `codex` stores only a pointer, and the credentials stay in Codex's own
`$CODEX_HOME/auth.json`. `gait models` caches the models.dev catalog in
`$GAIT_HOME/cache/models.json`.

`gait config set --id <key> --value <value>` edits one key of `gait.jsonc` and keeps its comments.
The value is parsed as JSON (`50` is a number, `true` a bool, `[...]` an array) and
falls back to a plain string. Registering and enabling extensions rewrite the file as
plain JSON and drop comments, so prefer `config set` for single keys.

#### REPL commands

- `/inference [name|default|show <name>|add <name> <provider/id>]` — List, show, add, select, or clear a named inference
- `/auth [list|status <provider>|link <provider> <source> [--home <path>]|unlink <provider>]` — List, inspect, link, or unlink credentials

Commands: `inference`, `auth`, `models`

### Workflows and extensions

Workflows run repeatable sequences of prompts and operations. MCP tools and plugins can
be enabled for a session when they are available in the configured Gait directory.

`gait register` always adds an entry disabled; run `gait enable --kind <kind> --id <name>` to activate it. WASM
plugins are registered and toggled but not executed today: the manifest loads and
there is no execution path, so treat plugins as scaffolding.

`gait workflow continue --force` resumes the originally logged graph even if the YAML
on disk has changed; without it an edited YAML is a hard error. `--force-rerun
<node-id>` accepts re-running a shell node that an interrupted run left `Running`;
its earlier side effects are not undone. `gait workflow list` and `log` only see runs
started by `gait workflow run`, never ordinary tasks or REPL turns. `gait workflow
run` exits 2 when paused at a gate node and 0 when paused at an interactive prompt
node.

`enable`/`disable`/`register`/`unregister`/`load`/`unload` also accept a `skill` kind —
that usage works in every Gait build (see the Skills guide), since skills are not part of
this extensions capability.

#### `gait workflow`

```text
Run, continue, inspect or abort a workflow (list, run, continue, log, abort, validate)

Usage: gait workflow [OPTIONS] [ACTION] [ARGS]...

Arguments:
  [ACTION]
          What to do: list, run, continue, log, abort or validate.

  [ARGS]...
          The workflow id or file and any options the action takes.

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.

          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

#### REPL commands

- `/workflow <name> [args...]` — Run a workflow

Commands: `enable`, `disable`

### The REPL

`gait -i` starts a fresh session (or resumes one with `--session <id>`) and runs
request by request in one process. The REPL holds no session state of its own; each
turn runs in a fresh process and the log in the gait-dir is the source of truth, so
if the REPL dies mid-turn, `gait -i --session <id>` continues from the log.

Input syntax, expanded before the message is sent:

- `!cmd` runs a shell command locally and captures its output into shell history.
- `$(N)` pastes shell history entry N; `$(cmd)` runs `cmd` and inlines its output.
- `>(N)` quotes line N of the last agent response; `>(N-M)` quotes a range.
- `@path` inlines the contents of a file.

`--verified-goal <command>` sets a verifier for the session. It runs after every
completion attempt and must exit 0 before the turn can complete; a failure feeds back
into the same turn so the model can fix it. The command is fixed at session start.

### Where things live

`--gait-dir` and `--codebase` resolve in this order: the explicit flag, then the
`GAIT_DIR` / `CODEBASE` environment variable, then a `.gaitref` file found by walking
up from the current directory (`gait link` writes these), then the current directory.
If no gait-dir is found, commands that need one exit 65 with `no gait dir: pass
--gait-dir <path> or set $GAIT_DIR`.

`--identity-dir <path>` names the agent identity directory whose keys sign direct events.
It sets `GAIT_IDENTITY_DIR` for the process and every child it spawns, so brain
processes read the same identity. If the directory cannot be loaded, gait exits 65 at
startup and names the directory.

Known gap: the detached-process exit hook runs in the sandboxed hand, which cannot read
the identity directory, so it cannot sign. Its completion notice is appended unsigned; it
still wakes an idle parent, and the parent shows it as `[process:<name>] [unverified]`.
The planned fix is a supervisor outside the sandbox that runs the hook and signs.

Most commands exit 0 on success. A rejected command exits with a code by error class:
65 when the input or request is wrong, 69 when gait itself failed, 75 when retrying
may help, 70 for an unclassified error; a clap usage error exits 2.

A session's log is `<gait-dir>/sessions/<id>/log.jsonl`, its in-flight worktree is
`<gait-dir>/.active/<id>/`, and configuration is `<gait-dir>/gait.jsonc`.

### Skills

Skills are reusable, markdown-based capability files. Register, load, enable, or unload a
skill with `--kind skill` (see `enable`/`disable`/`register`/`unregister`/`load`/`unload`)
or the REPL's `/skill` command — this works in every Gait build.

#### REPL commands

- `/skill [load|unload <name>]` — List, load, or unload skills/mcp/plugins

## Reference

### Config fields

- `allow` — Tool-name / pattern allowlist.
- `deny` — Tool-name / pattern denylist.
- `hooks` — Lifecycle hook definitions (event, tool filter, shell command).
- `hooks[].event` — Lifecycle event: `pre_turn`, `pre_step`, `post_step`, `pre_tool`, `post_tool`, or `post_turn`.
- `hooks[].run` — Shell command to run; all `GAIT_*` environment variables are set before execution.
- `hooks[].tool` — Optional tool-name filter, applicable to `pre_tool` and `post_tool`; absent means all tools.
- `inference` — The selected model, e.g. "anthropic/claude-sonnet-4-6".
- `max_iter` — Max turn iterations before giving up. Defaults to 100 when absent.
- `output_schema_retries` — Max retries when an LLM result fails output_schema validation.
- `preview` — When to render edit/write previews. Defaults to on-ask.
- `result_schema` — JSON Schema that `result` in `structured_done` must conform to. If set, harness validates and retries on mismatch (up to output_schema_retries).
- `sandbox` — Sandbox policy for shell/process/file execution. Strict-by-default (`on: true`, empty allow-lists, network off) so that engagement is safe unless the user explicitly opts in.
- `sandbox.deny` — Deny patterns layered inside `readable`/`writable`. Present in a user's `gait.jsonc` (scaffolded by `gait init`), authoritative once there — see spec §3. Missing sub-fields (not present at all, distinct from an empty array) fall back to `default_deny_patterns()` via the function-based serde default above.
- `sandbox.deny.read` — Patterns denied within the read scope. This list IS the effective list — no hidden merge with built-in defaults once present in a user's config.
- `sandbox.deny.write` — Patterns denied within the write scope.
- `sandbox.network` — Network policy applied to sandboxed processes: `false`/`off` disables, `*` allows all, or an array of hostnames.
- `sandbox.on` — Whether the sandbox is engaged. Default: true (strict).
- `sandbox.readable` — Paths the sandbox allows reading from.
- `sandbox.require` — Fail a tool call instead of running it unsandboxed when the kernel seal cannot be applied (Landlock missing or disabled, or the apply fails). Default: false. Read from the hand's local config only, never from the brain's request.
- `sandbox.shell_whitelist` — Shell-only command patterns pre-authorized for extra sandbox scope. Grants may also include `unsandbox` to run the matching hand without its kernel seal; the brain remains sandboxed. Only `shell` tool calls are ever checked against this list — see `whitelist_match` in gait-state::permissions::matching.
- `sandbox.shell_whitelist[].grants` — Extra scope granted by a match, e.g. `network`; `unsandbox` runs the matching hand without its kernel seal.
- `sandbox.shell_whitelist[].pattern` — Shell command pattern to match.
- `sandbox.tools` — Per-tool overrides of the built-in sandbox floor (see `gait_sandbox::ceiling::access_for_tool` for the defaults each tool gets when not listed here). Unset fields on an entry keep the built-in default for that dimension.
- `sandbox.tools.<name>.local_bind` — Override whether this tool may use local bind when set.
- `sandbox.tools.<name>.network` — Override whether this tool may use the network when set.
- `sandbox.tools.<name>.reads` — Override whether this tool may read when set.
- `sandbox.tools.<name>.writes` — Override whether this tool may write when set.
- `sandbox.writable` — Paths the sandbox allows writing to.
- `shell_timeout_secs` — Wall-clock timeout (seconds) for a foreground `shell` command. On expiry the command's process subtree is SIGKILLed and the tool returns exit 124 with a nudge to use `detached:true`. User-set only; the agent never picks it.
- `show_thinking` — Whether to render thinking/reasoning blocks in the terminal output. This is a pure UI preference — ThinkingBlock events are ALWAYS written to the session log regardless of this flag. Default: false.
- `tool_timeout_secs` — Wall-clock timeout (seconds) for a single ripgrep subprocess invocation backed by grep/ls. On expiry the tool returns an error result instead of blocking the turn on a hung `rg` process — the same unbounded-subprocess risk `shell_timeout_secs` guards against, so it shares that default. NOT sized to preempt the spine's watchdog: the watchdog's phase-duration cap is already suppressed while tools are running (see `SpinePhase::HandsPhase` in `watchdog_breach`, `turn/spine.rs`) and its only live check during a tool call is brain ping-liveness, which is unaffected by how long a tool takes. User-set only; the agent never picks it.
- `tools` — Config-defined shell-command tools.
- `tools[].cmd` — Shell command to run.
- `tools[].cwd` — Working directory for the command; defaults to `codebase`.
- `tools[].description` — Description shown to the model.
- `tools[].name` — Name used to invoke the tool.

### Inference profile: `inferences/<name>/inference.textproto`

A protobuf text-format `gait.log.v1.Inference`. The profile name is the directory name; do not set `name`.

```
model: "anthropic/claude-sonnet-4-6"
compaction_threshold_tokens: 128000
llm_params { anthropic { effort: "high" } }
```

- `model` (string) — provider/model-id to invoke
- `llm_params` (LlmParams, optional) — provider-specific request settings
- `compaction_threshold_tokens` (uint64) — token budget before compaction

`llm_params`:

- `max_tokens` (uint32, optional) — universal override; Anthropic: max_tokens; OpenAI: max_completion_tokens
- `temperature` (float, optional) — Anthropic: 0-1; OpenAI/Gemini: 0-2
- `top_p` (float, optional) — 0-1; all providers
- `anthropic` (AnthropicLlmParams, one of `provider_params`)
- `openai` (OpenAILlmParams, one of `provider_params`)
- `gemini` (GeminiLlmParams, one of `provider_params`)
- `local` (LocalLlmParams, one of `provider_params`)

`llm_params.anthropic`:

- `top_k` (uint32, optional)
- `effort` (string, optional) — "low"|"medium"|"high"|"xhigh"|"max"
- `thinking` (AnthropicThinking, optional)
- `strict_tools` (bool, optional) — require strict tool-schema handling

`llm_params.openai`:

- `reasoning_effort` (string, optional) — "none"|"minimal"|"low"|"medium"|"high"|"xhigh"
- `frequency_penalty` (float, optional) — penalize repeated tokens
- `presence_penalty` (float, optional) — penalize tokens already present
- `seed` (int64, optional) — deterministic sampling seed
- `stop` (string, repeated) — stop sequences
- `strict_tools` (bool, optional) — require strict tool-schema handling

`llm_params.gemini`:

- `top_k` (int32, optional)
- `candidate_count` (int32, optional)
- `stop_sequences` (string, repeated)
- `seed` (int32, optional)
- `frequency_penalty` (float, optional)
- `presence_penalty` (float, optional)
- `thinking` (GeminiThinking, optional)

`llm_params.anthropic.thinking`:

- `type` (AnthropicThinkingType) — Values: `ANTHROPIC_THINKING_TYPE_UNSPECIFIED`, `ANTHROPIC_THINKING_TYPE_ADAPTIVE`, `ANTHROPIC_THINKING_TYPE_DISABLED`, `ANTHROPIC_THINKING_TYPE_ENABLED`.
- `budget_tokens` (uint32, optional)
- `display` (AnthropicThinkingDisplay) — Values: `ANTHROPIC_THINKING_DISPLAY_UNSPECIFIED`, `ANTHROPIC_THINKING_DISPLAY_SUMMARIZED`, `ANTHROPIC_THINKING_DISPLAY_OMITTED`.

`llm_params.gemini.thinking`:

- `include_thoughts` (bool, optional)
- `budget` (int32, one of `depth`)
- `level` (GeminiThinkingLevel, one of `depth`) — Values: `GEMINI_THINKING_LEVEL_UNSPECIFIED`, `GEMINI_THINKING_LEVEL_MINIMAL`, `GEMINI_THINKING_LEVEL_LOW`, `GEMINI_THINKING_LEVEL_MEDIUM`, `GEMINI_THINKING_LEVEL_HIGH`.

`llm_params.local`:

- `model_path` (string) — filesystem path to the developer-owned model
- `context_tokens` (uint32, optional) — model context window in tokens
- `gpu_layers` (uint32, optional) — number of layers to offload to the GPU

### Mode: `modes/<name>/MODE.md`

Markdown with YAML frontmatter. The frontmatter keys below are `gait.log.v1.Mode` fields; the markdown body is the steer.

```
---
description: Read-only review
allow: [read, glob]
deny: ["shell:git push *"]
---
Review the code. Do not edit files.
```

- `deny` (string, repeated)
- `description` (string) — human-readable label for UIs; behaviorally inert
- `allow` (string, repeated) — permission-gate allow patterns, unioned into effective_allow
- `classify_policy` (ClassifyPolicy, optional)

`classify_policy`:

- `inference` (string) — named inference profile to call (an inference.textproto entry)
- `risk_threshold` (double) — deny if any *_risk noul >= this value
- `max_consecutive_denials` (uint32) — circuit breaker: stop trusting the classifier and fall back to a real pause after this many denials in a row this turn

### Agent file: `agents/<name>/AGENT.md`

YAML frontmatter, then the markdown body (the agent's role prompt).

- `inference` — Name of an inference profile the agent runs with.
- `mcp` — Whitelist of registered MCP servers the agent may see (default-deny: absent means none).
- `plugins` — Whitelist of registered plugins the agent may see (default-deny: absent means none).
- `skills` — Whitelist of registered skills the agent may see (default-deny: absent means none).

### Skill file: `skills/<name>/SKILL.md`

YAML frontmatter, then the markdown body (the skill's instructions; a skill with an empty body is ignored).

- `description` — Description shown to the model.
- `model_invocable` — Whether the model may invoke this skill; omitted values default to true.
- `name` — Required non-empty skill name.

## Other commands

These run from the command line only. They are not ops: no JSON, MCP or typed call reaches them.

### gait workflow

CLI only, not an op.

```text
Run, continue, inspect or abort a workflow (list, run, continue, log, abort, validate)

Usage: gait workflow [OPTIONS] [ACTION] [ARGS]...

Arguments:
  [ACTION]
          What to do: list, run, continue, log, abort or validate.

  [ARGS]...
          The workflow id or file and any options the action takes.

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

### turn

CLI only, not an op.

```text
Run a task: `gait "fix the login bug"`, `-i` for the REPL, `-e` for a one-off request.
A first word that is a command runs the command; put `--` before a task that starts with one.
```

## Commands

### gait sessions

```text
List the active sessions, most recently updated first.

Usage: gait sessions [OPTIONS]

Options:
      --last <last>
          Show only the N most recently updated sessions.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli, typed

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait ps

```text
Show each mounted session and its live state.

Usage: gait ps [OPTIONS]

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait stats

```text
Show a session's token use, split into agent work and compaction.

Usage: gait stats [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli, typed

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait log

```text
Show the outcome of a session's last completed run: its message, checkpoint, and the files and tools it touched.

Usage: gait log [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait undo

```text
Go back to an earlier turn of a session, or list the turns with `--list`.

Usage: gait undo [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --turn <turn>
          The turn to go back to, as numbered by `--list`. Turn 0 is the state before the first request and restores code only.

      --code
          Undo the code only.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --transcript
          Undo the transcript only.

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

      --list
          List the turns and their checkpoints instead of undoing.

  -h, --help
          Print help (see a summary with '-h')

Rules:
  --turn conflicts with --list
```

Access: writes, destructive

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait persist

```text
Merge a session onto main and remove its worktree, or all idle sessions with `--all --yes`.

Usage: gait persist [OPTIONS]

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --all
          Widen the set: for `processes list`, also finished processes; for `persist` and `discard`, every idle session that is not running (needs `--yes`).

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --yes
          Confirm acting on all sessions.

      --dry-run
          Show what would be acted on and change nothing.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait discard

```text
Remove a session's worktree without merging; unrecoverable. `--all --yes --force` does it for all idle sessions.

Usage: gait discard [OPTIONS]

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --all
          Widen the set: for `processes list`, also finished processes; for `persist` and `discard`, every idle session that is not running (needs `--yes`).

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --yes
          Confirm acting on all sessions.

      --dry-run
          Show what would be acted on and change nothing.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --force
          Override a safety check: `init` may create the gait directory inside a codebase's git repository (a turn there then needs `--allow-intree`), `discard --all` may discard more than one session, `manual install` replaces an existing manual.

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes, destructive

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait allow

```text
Approve the permission requests a session is waiting on.

Usage: gait allow [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pattern <pattern>
          The tool pattern to approve; without it the pending requests are approved as asked.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --scope <scope>
          Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.

          Possible values:
          - once:    For this turn only (the default)
          - session: For the life of the session
          - global:  Saved to gait.jsonc
          - mode:    Saved to the active mode's MODE.md

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait deny

```text
Refuse the permission requests a session is waiting on.

Usage: gait deny [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli, typed

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait unlock

```text
Approve the sandbox unlock requests a session is waiting on.

Usage: gait unlock [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --scope <scope>
          Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.

          Possible values:
          - once:    For this turn only, logged but not saved (the default)
          - session: For the life of the session, logged but not saved
          - global:  Saved to gait.jsonc
          - agent:   Saved to the agent file named by `--agent`

      --agent <agent>
          The agent config to change, for `--scope agent`: its name for `unlock`, the path of its `.md` file for `register` and `unregister`.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait reply

```text
Answer the single clarify or inform question a session is waiting on.

Usage: gait reply [OPTIONS] --id <id> --message <message>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --message <message>
          The answer to the session's single pending question.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |

### gait auth list

```text
List the configured providers and what kind of credential each has; never any key text.

Usage: gait auth list [OPTIONS]

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth status

```text
Show whether a provider has a credential and of what kind, without revealing it.

Usage: gait auth status [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth add

```text
Store an API key for a provider. Use `--key-file -` for scripts; otherwise the terminal prompts without echo.

Usage: gait auth add [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --key-file <key_file>
          Read the API key from this file, or from stdin with `-`. Asked for on a terminal when left out.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth link

```text
Use another tool's credentials for a provider without copying them.

Usage: gait auth link [OPTIONS] --id <id> --source <source>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --source <source>
          The tool whose credentials are used, for example `codex`.

      --compact
          Print the JSON output on one line.

      --home <home>
          Override that tool's home directory.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth login

```text
Sign in to a subscription provider. For scripts, complete `openai-signin` with `--callback-file -`.

Usage: gait auth login [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --callback-file <callback_file>
          Finish a pending `openai-signin` login with the URL you were redirected to, or `-` to read it from stdin.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes, network

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth logout

```text
Sign out of a subscription provider. API keys are removed with `auth remove`.

Usage: gait auth logout [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --forget
          For `openai-signin`: also forget its registration.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes, network, destructive

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth remove

```text
Remove a stored API key. Removing a provider with no key still succeeds; the id is absent_ok.

Usage: gait auth remove [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes, destructive

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait auth unlink

```text
Drop the link to another tool's credentials; that tool's own credentials are not touched.

Usage: gait auth unlink [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait models

```text
List the models of the models.dev catalog, optionally filtered by `--filter`.

Usage: gait models [OPTIONS]

Options:
      --filter <filter>
          Keep only models whose provider, id or name contains this text (case-insensitive).

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads, network

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait inference list

```text
List the named inference profiles with their models.

Usage: gait inference list [OPTIONS]

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait inference show

```text
Show one inference profile.

Usage: gait inference show [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait inference add

```text
Write a new inference profile: a model and the token count at which compaction begins.

Usage: gait inference add [OPTIONS] --id <id> --model <model> --compaction-threshold-tokens <compaction_threshold_tokens>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --model <model>
          The model, in `provider/id` form, for example `opencode/kimi-k2.6`.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --compaction-threshold-tokens <compaction_threshold_tokens>
          Begin compacting the conversation when it reaches this many tokens.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait inference select

```text
Use an inference profile for a session from its next turn.

Usage: gait inference select [OPTIONS] --id <id> --session <session>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --session <session>
          The session id the change applies to.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait inference clear

```text
Put a session back on the default inference.

Usage: gait inference clear [OPTIONS] --session <session>

Options:
      --session <session>
          The session id the change applies to.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait mode list

```text
List the named modes with their patterns.

Usage: gait mode list [OPTIONS]

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait mode show

```text
Show one mode.

Usage: gait mode show [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait mode add

```text
Write a new mode from allow and deny patterns, steering text and a description.

Usage: gait mode add [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --allow <allow>
          A tool pattern the mode allows (repeatable).

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --deny <deny>
          A tool pattern the mode denies (repeatable).

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --steer <steer>
          Free-form steering text added to the prompt.

      --description <description>
          A one-line description; required when registering a skill.

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait mode select

```text
Use a mode for a session from its next turn.

Usage: gait mode select [OPTIONS] --id <id> --session <session>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --session <session>
          The session id the change applies to.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait mode clear

```text
Put a session back on the default mode.

Usage: gait mode clear [OPTIONS] --session <session>

Options:
      --session <session>
          The session id the change applies to.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait config get

```text
Read one config key; a known key that is not set is null, an unknown key is not found.

Usage: gait config get [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait config set

```text
Set one config key, keeping the file's comments and every other key.

Usage: gait config set [OPTIONS] --id <id> --value <value>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --value <value>
          The value: JSON (a number, boolean, array or object) or plain text.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait config show

```text
Show the effective config: defaults and gait.jsonc, with the skills and MCP servers found on disk.

Usage: gait config show [OPTIONS]

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait processes list

```text
List the running background processes, or all of them with `--all`.

Usage: gait processes list [OPTIONS]

Options:
      --all
          Widen the set: for `processes list`, also finished processes; for `persist` and `discard`, every idle session that is not running (needs `--yes`).

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait processes stop

```text
Stop a running process by name and record the stop.

Usage: gait processes stop [OPTIONS] --id <id>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes, destructive

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait register

```text
Register an MCP server, a skill or a plugin. An MCP server and a skill start disabled.

Usage: gait register [OPTIONS] --id <id> --kind <kind>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --kind <kind>
          The kind of extension.

          Possible values:
          - mcp:    A Model Context Protocol server
          - skill:  A skill: a SKILL.md of instructions
          - plugin: A WebAssembly plugin

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --url <url>
          The URL of an MCP server that is already running.

      --command <command>
          The program that starts an MCP server.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --args <args>
          The arguments for `--command`, as one string split on spaces.

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

      --description <description>
          A one-line description; required when registering a skill.

      --content <content>
          The text of a skill, given inline.

      --path <path>
          A file to read a skill from.

      --wasm <wasm>
          The `.wasm` file of a plugin.

      --env <KEY=VALUE>
          An environment variable for the MCP server, as `KEY=value` (repeatable).

      --scope <scope>
          Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.

          Possible values:
          - global:  The gait directory, for every session (the default)
          - agent:   The agent file named by `--agent`
          - session: One session's log, named by `--session`

      --agent <agent>
          The agent config to change, for `--scope agent`: its name for `unlock`, the path of its `.md` file for `register` and `unregister`.

      --session <session>
          The session id the change applies to.

  -h, --help
          Print help (see a summary with '-h')

Rules:
  give at most one of --url, --command
  give at most one of --content, --path
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait unregister

```text
Remove a registered extension.

Usage: gait unregister [OPTIONS] --id <id> --kind <kind>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --kind <kind>
          The kind of extension.

          Possible values:
          - mcp:    A Model Context Protocol server
          - skill:  A skill: a SKILL.md of instructions
          - plugin: A WebAssembly plugin

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --scope <scope>
          Where the change applies. When not given: `once` for `allow` and `unlock`, `global` for `register` and `unregister`.

          Possible values:
          - global:  The gait directory, for every session (the default)
          - agent:   The agent file named by `--agent`
          - session: One session's log, named by `--session`

      --agent <agent>
          The agent config to change, for `--scope agent`: its name for `unlock`, the path of its `.md` file for `register` and `unregister`.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

      --session <session>
          The session id the change applies to.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes, destructive

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait enable

```text
Switch an extension on in the gait directory's config, for sessions that start later.

Usage: gait enable [OPTIONS] --id <id> --kind <kind>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --kind <kind>
          The kind of extension.

          Possible values:
          - mcp:    A Model Context Protocol server
          - skill:  A skill: a SKILL.md of instructions
          - plugin: A WebAssembly plugin

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait disable

```text
Switch an extension off in the gait directory's config.

Usage: gait disable [OPTIONS] --id <id> --kind <kind>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --kind <kind>
          The kind of extension.

          Possible values:
          - mcp:    A Model Context Protocol server
          - skill:  A skill: a SKILL.md of instructions
          - plugin: A WebAssembly plugin

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait load

```text
Load an extension into one session from its next turn.

Usage: gait load [OPTIONS] --id <id> --kind <kind> --session <session>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --kind <kind>
          The kind of extension.

          Possible values:
          - mcp:    A Model Context Protocol server
          - skill:  A skill: a SKILL.md of instructions
          - plugin: A WebAssembly plugin

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --session <session>
          The session id the change applies to.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait unload

```text
Unload an extension from one session.

Usage: gait unload [OPTIONS] --id <id> --kind <kind> --session <session>

Options:
      --id <id>
          The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.

      --kind <kind>
          The kind of extension.

          Possible values:
          - mcp:    A Model Context Protocol server
          - skill:  A skill: a SKILL.md of instructions
          - plugin: A WebAssembly plugin

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --session <session>
          The session id the change applies to.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help (see a summary with '-h')
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait init

```text
Create a gait directory at `--gait-dir`: config, a default agent, skills and sessions, as a git repository.

Usage: gait init [OPTIONS]

Options:
      --force
          Override a safety check: `init` may create the gait directory inside a codebase's git repository (a turn there then needs `--allow-intree`), `discard --all` may discard more than one session, `manual install` replaces an existing manual.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait link

```text
Point the current directory at the gait directory `--gait-dir` by writing a `.gaitref` file.

Usage: gait link [OPTIONS]

Options:
      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait manual show

```text
Print the manual: guides, reference and every command, as Markdown, or with `--json` as descriptors.

Usage: gait manual show [OPTIONS]

Options:
      --json
          Return every operation's descriptor, schema, exit codes and error codes as JSON instead of the Markdown manual.

      --pretty
          Indent the JSON output.

      --text
          Print the text form instead of JSON.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: reads

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

### gait manual install

```text
Write the manual to `skills/gait-manual/SKILL.md` in the gait directory, for agents to read.

Usage: gait manual install [OPTIONS]

Options:
      --force
          Override a safety check: `init` may create the gait directory inside a codebase's git repository (a turn there then needs `--allow-intree`), `discard --all` may discard more than one session, `manual install` replaces an existing manual.

      --pretty
          Indent the JSON output.

      --compact
          Print the JSON output on one line.

      --gait-dir <gait_dir>
          The gait directory to use. Without it, `$GAIT_DIR` and then a `.gait/` directory or `.gaitref` file found walking up from the current directory are used.
          
          [env: GAIT_DIR]

      --identity-dir <identity_dir>
          Agent identity directory. Sets GAIT_IDENTITY_DIR for this process and every child it spawns.

  -h, --help
          Print help
```

Access: writes

Paths: cli

| Code | Class | Exit |
|---|---|---|
| gait.op.not_found | logical | 65 |
| gait.op.conflict | logical | 65 |
| gait.op.session_busy | transient | 75 |
| gait.op.confirmation_required | logical | 65 |
| gait.op.invalid | logical | 65 |
| gait.cli.usage | logical | 65 |
| gait.cli.invalid_input | logical | 65 |
| gait.cli.not_found | logical | 65 |
| gait.cli.failed | infrastructure | 69 |

## Library codes

| Code | Class | Exit |
|---|---|---|
| op_registry.usage.missing | logical | 2 |
| op_registry.usage.invalid | logical | 2 |
| op_registry.usage.unknown_key | logical | 2 |
| op_registry.usage.repeated_key | logical | 2 |
| op_registry.usage.one_of | logical | 2 |
| op_registry.usage.requires_if | logical | 2 |
| op_registry.usage.conflicts | logical | 2 |
| op_registry.usage.at_least_one | logical | 2 |
| op_registry.usage.stdin_twice | logical | 2 |
| op_registry.cli.unknown_command | logical | 2 |
| op_registry.call.unknown_operation | logical | 65 |
| op_registry.call.wrong_shape | logical | 65 |
| op_registry.call.outside_audience | logical | 65 |
| op_registry.call.wrong_path | logical | 65 |
