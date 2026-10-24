---
name: gait-manual
description: Explain how to use and configure Gait.
manual-source-hash: 2a10f1ec05fbb99543136690640d480252c929093228f873ceb7066e9ba13332
---

# Gait Manual

## Getting started

Gait is an AI coding agent with a command-line interface and an interactive REPL.

Use the CLI to initialize a Gait directory, run one-off tasks, or continue a session.
Use the REPL when you want an interactive coding session.

### CLI commands

#### `gait init`

```text
Usage: init [OPTIONS] [PATH]

Arguments:
  [PATH]  Target gait-dir (default: current directory)

Options:
      --force   Create the workspace even if it would live inside a codebase git repo. (In-tree is discouraged: running such a workspace also needs `--allow-intree`, since the agent could otherwise write to its own config. Prefer out-of-tree: `gait init ~/gait-home && gait link ~/gait-home`.)
      --pretty
  -h, --help    Print help
```

### REPL commands

#### `/help`

Show this command guide

Aliases: /?


#### `/info`

Model, session, gait-dir, codebase


## Running a task

Every task runs one of three ways: a bare positional (`gait "fix the login bug"`) starts
or continues the default session; `-e`/`--exec` runs a single hands-free request with no
session and no file/repo access; `-i`/`--interactive` opens the REPL. These three are
mutually exclusive.

`--auto-approve`, `--allow`, `--deny`, and `--unlock` control tool permissions for the
run. `--inference` selects a named model; `-a`/`--agent` loads an agent file; `-s`/`--session`
continues an existing session by id; `-n`/`--max-iter` caps agentic steps.

`-R`/`--result-schema` (a file) and `--result-schema-json` (inline) declare a JSON Schema
the final `structured_done` result must match; the two are mutually exclusive. Under `-e`,
the run's workspace lives under a scratch temp directory that is deleted on exit —
`--gait-dir` is *not* where an ephemeral run writes, and its diagnostics are lost with it
unless `GAIT_DEBUG=1` is set, which keeps the scratch directory and prints its path.

### CLI commands

#### `gait`

AI coding agent

```text
AI coding agent

Usage: gait [OPTIONS] [TASK]
       gait <COMMAND>

Commands:
  manual
  init
  link        Point the current directory at an existing gait workspace
  sessions
  ps
  stop
  persist
  discard
  undo
  stats
  log
  allow
  deny
  unlock
  reply
  config      `gait config <set|get|show> ...` — the general external-CLI equivalent of any REPL slash command that reads or mutates `gait.jsonc`. `set`/`get` call the generic `gait_ops::config::set`/`get` (the comment-safe jsonc mutators); `show` calls the shared `gait_ops::config::resolve` op. gait-dir-wide, not session-scoped. Mirrors `WorkflowArgs`/`cmd_workflow`'s action+rest shape exactly
  register    `gait register mcp|skill|plugin <name> [flags]`
  unregister  `gait unregister mcp|skill|plugin <name>`
  load        `gait load <kind> <name> --session <id>` — append a Toggle(enabled=true) event to a session's log outside a running turn
  unload      `gait unload <kind> <name> --session <id>` — append a Toggle(enabled=false) event to a session's log outside a running turn
  enable      `gait enable mcp|skill|plugin <name>`
  disable     `gait disable mcp|skill|plugin <name>`
  auth        `gait auth list|add|remove|login|status|logout|link|unlink [provider] [source]`
  models      `gait models [filter]`
  inference   `gait inference list|show|select|clear [name] --session <id>`
  mode        `gait mode list|show|add|select|clear [name] --session <id>`
  workflow    `gait workflow <run|list|continue|abort|log>` subcommand
  daemons
  help        Print this message or the help of the given subcommand(s)

Arguments:
  [TASK]  Task to run (bare positional: `gait "fix the login bug"`)

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
          Override the in-tree safety guard (design §7): run even though the gait-dir is inside the codebase. The agent could then write to its own config/permissions/audit log — prefer `gait init ~/gait-home && gait link ~/gait-home` instead
      --agent-dir <PATH>
          Agent identity directory. Sets GAIT_AGENT_DIR for this process and every child it spawns
  -h, --help
          Print help
  -V, --version
          Print version
```

## Sessions

Sessions preserve turns, tool activity, and workspace state.

Start a fresh session when you want a clean conversation. Use undo when you need to
restore an earlier turn or inspect available checkpoints.

A session's worktree lives under `<gait-dir>/.active/<id>/` until `gait persist` merges
it onto `main` or `gait discard` tears it down without merging (unrecoverable).
`gait ps` shows each mounted session's live state: `idle`, `awaiting-permission`,
`awaiting-clarify`, `awaiting-inform` or `awaiting-unlock`. `gait daemons` lists the
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

### CLI commands

#### `gait sessions`

```text
Usage: sessions [OPTIONS] --gait-dir <GAIT_DIR>

Options:
      --gait-dir <GAIT_DIR>  [env: GAIT_DIR]
      --pretty
      --last <LAST>          Show only the N most-recently-updated sessions
  -h, --help                 Print help
```

### REPL commands

#### `/new`

Start a fresh session

Aliases: /n


#### `/undo [N]`

Restore code + transcript to the end of turn N (no N: list turns)

Aliases: /u


#### `/unlock [once|session|global|agent]`

Unlock a held session


## Permissions and modes

Every tool call is classified in order: deny (hard floor, wins even under
`--auto-approve`) → allow (pattern match) → ask (pauses for a human, or
for a client's permission UI). `--allow`/`--deny` on the command line set
the per-invocation baseline; `gait.jsonc`'s `allow`/`deny` fields set the
persistent global baseline.

A **mode** (`<gait_dir>/modes/<name>/MODE.md`) is a named, switchable
overlay of `allow`/`deny`/`steer`/`description` on top of that baseline —
`--mode <name>` on `run` activates one for a session; `gait mode
select/clear <name> --session <id>` switches it mid-session. `gait mode
add <name> --allow <pattern> --deny <pattern> --steer <text>
--description <text>` authors a new one; `gait mode list`/`gait mode
show <name>` inspect what's authored. A mode's `deny` unions with the
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

`gait allow <session-id> --pattern <pattern> --scope <global|mode|session|once>`
answers a pending permission gate. `--scope mode` persists to the
session's *currently active* mode's `MODE.md` — it errors if no mode
is active (there is nowhere to write to). `--scope global` persists to
`gait.jsonc`. `--scope session`/`--scope once` are log-only, no file
write.

### CLI commands

#### `gait allow`

```text
Usage: allow [OPTIONS] --gait-dir <GAIT_DIR> <SESSION_ID>

Arguments:
  <SESSION_ID>

Options:
      --pattern <PATTERN>
      --scope <SCOPE>
      --gait-dir <GAIT_DIR>  [env: GAIT_DIR]
      --pretty
  -h, --help                 Print help
```

#### `gait unlock`

```text
Usage: unlock [OPTIONS] --gait-dir <GAIT_DIR> <SESSION_ID>

Arguments:
  <SESSION_ID>

Options:
      --scope <SCOPE>        Grant scope: once (default, log only) | session (log only) | global (→ gait.jsonc) | agent (→ agent file) [default: once]
      --agent <AGENT>        Agent name for agent-scoped grants (resolves to agents/<name>.md)
      --gait-dir <GAIT_DIR>  [env: GAIT_DIR]
      --pretty
  -h, --help                 Print help
```

#### `gait mode`

`gait mode list|show|add|select|clear [name] --session <id>`

```text
`gait mode list|show|add|select|clear [name] --session <id>`

Usage: mode [OPTIONS] --gait-dir <GAIT_DIR> [ACTION] [NAME]

Arguments:
  [ACTION]
  [NAME]

Options:
      --gait-dir <GAIT_DIR>        [env: GAIT_DIR]
      --allow <ALLOW>              Repeatable allow pattern for the `add` action. Ignored by other actions
      --deny <DENY>                Repeatable deny pattern for the `add` action. Ignored by other actions
      --steer <STEER>              Freeform steering text for the `add` action. Ignored by other actions [default: ""]
      --description <DESCRIPTION>  Human-readable description for the `add` action. Ignored by other actions [default: ""]
      --session <SESSION>
      --pretty
  -h, --help                       Print help
```

## Inferences and providers

Inferences name the models Gait can use. Provider credentials are managed separately
from inference selection.

Use the CLI for durable configuration and the REPL commands for inspecting or changing
the current interactive session.

API keys and non-secret external-source bindings live in `$GAIT_HOME/auth.json`
(`GAIT_HOME` defaults to `~/.gait-home`). That home is machine-local and separate from
the gait-dir, which is the Git-backed workspace holding configuration and sessions.
Keys can also come from environment variables such as `ANTHROPIC_API_KEY` and
`OPENAI_API_KEY`. Official ChatGPT plan sign-in uses `openai-signin`:
run `gait auth login openai-signin`, open the authorization URL, and paste the
resulting callback URL when prompted. For a later process, use
`gait auth login openai-signin --complete <url>` (or `--complete -` to read one
trimmed line from stdin). Sign out with `gait auth logout openai-signin`, adding
`--forget` when switching accounts. If the provider cannot revoke a session, disconnect
the app in ChatGPT Settings. The legacy `openai-codex` device-code path is unofficial;
linking it to `codex` stores only a pointer, and the credentials stay in Codex's own
`$CODEX_HOME/auth.json`. `gait models` caches the models.dev catalog in
`$GAIT_HOME/cache/models.json`.

`gait config set <key> <value>` edits one key of `gait.jsonc` and keeps its comments.
The value is parsed as JSON (`50` is a number, `true` a bool, `[...]` an array) and
falls back to a plain string. Registering and enabling extensions rewrite the file as
plain JSON and drop comments, so prefer `config set` for single keys.

### CLI commands

#### `gait inference`

`gait inference list|show|select|clear [name] --session <id>`

```text
`gait inference list|show|select|clear [name] --session <id>`

Usage: inference [OPTIONS] --gait-dir <GAIT_DIR> [ACTION] [NAME]

Arguments:
  [ACTION]
  [NAME]

Options:
      --gait-dir <GAIT_DIR>
          [env: GAIT_DIR]
      --model <MODEL>
          Model for the `add` action, in `provider/id` form (e.g. `opencode/kimi-k2.6`). Ignored by other actions
      --compaction-threshold-tokens <COMPACTION_THRESHOLD_TOKENS>
          Required compaction threshold for the `add` action
      --session <SESSION>

      --pretty

  -h, --help
          Print help
```

#### `gait auth`

`gait auth list|add|remove|login|status|logout|link|unlink [provider] [source]`

```text
`gait auth list|add|remove|login|status|logout|link|unlink [provider] [source]`

Usage: auth [OPTIONS] [ACTION] [PROVIDER] [SOURCE]

Arguments:
  [ACTION]    Action: list (default), add, remove, login, status, logout, link, unlink
  [PROVIDER]  Provider name
  [SOURCE]    External source name (used with link)

Options:
      --home <HOME>          Override the external CLI's home directory
      --pretty
      --complete <COMPLETE>  Complete a pending openai-signin login with a callback URL (or `-` for stdin)
      --forget               Forget the openai-signin registration when logging out
  -h, --help                 Print help
```

#### `gait models`

`gait models [filter]`

```text
`gait models [filter]`

Usage: models [OPTIONS] [FILTER]

Arguments:
  [FILTER]  Optional substring filter for model ids

Options:
      --pretty
  -h, --help    Print help
```

### REPL commands

#### `/inference [name|default|show <name>|add <name> <provider/id>]`

List, show, add, select, or clear a named inference


#### `/auth [list|status <provider>|link <provider> <source> [--home <path>]|unlink <provider>]`

List, inspect, link, or unlink credentials


## Workflows and extensions

Workflows run repeatable sequences of prompts and operations. MCP tools and plugins can
be enabled for a session when they are available in the configured Gait directory.

`gait register` always adds an entry disabled; run `gait enable` to activate it. WASM
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

### CLI commands

#### `gait workflow`

`gait workflow <run|list|continue|abort|log>` subcommand

```text
`gait workflow <run|list|continue|abort|log>` subcommand

Usage: workflow [OPTIONS] [ACTION] [REST]...

Arguments:
  [ACTION]   Action: run | list | continue | abort | log
  [REST]...  Remaining args (workflow name, id, …)

Options:
      --gait-dir <GAIT_DIR>  Gait dir (falls back to $GAIT_DIR) [env: GAIT_DIR]
      --pretty               Applies to `list`/`abort`/`validate` only — `run`/`continue` stream turn output through `CliSink` (always compact, no pretty option) and `log` streams one compact JSON object per event (NDJSON; pretty would break line-per-event streaming)
  -h, --help                 Print help
```

#### `gait enable`

`gait enable mcp|skill|plugin <name>`

```text
`gait enable mcp|skill|plugin <name>`

Usage: enable [OPTIONS] <KIND> <NAME>

Arguments:
  <KIND>  Kind: mcp | skill | plugin
  <NAME>  Extension name

Options:
      --gait-dir <GAIT_DIR>  Gait dir (falls back to $GAIT_DIR) [env: GAIT_DIR]
      --pretty
  -h, --help                 Print help
```

#### `gait disable`

`gait disable mcp|skill|plugin <name>`

```text
`gait disable mcp|skill|plugin <name>`

Usage: disable [OPTIONS] <KIND> <NAME>

Arguments:
  <KIND>  Kind: mcp | skill | plugin
  <NAME>  Extension name

Options:
      --gait-dir <GAIT_DIR>  Gait dir (falls back to $GAIT_DIR) [env: GAIT_DIR]
      --pretty
  -h, --help                 Print help
```

### REPL commands

#### `/workflow <name> [args...]`

Run a workflow


## The REPL

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

## Where things live

`--gait-dir` and `--codebase` resolve in this order: the explicit flag, then the
`GAIT_DIR` / `CODEBASE` environment variable, then a `.gaitref` file found by walking
up from the current directory (`gait link` writes these), then the current directory.
If no gait-dir is found, commands that need one exit 1 with `no gait dir: pass
--gait-dir <path> or set $GAIT_DIR`.

`--agent-dir <path>` names the agent identity directory whose keys sign direct events.
It sets `GAIT_AGENT_DIR` for the process and every child it spawns, so brain
processes read the same identity. If the directory cannot be loaded, gait exits 1 at
startup and names the directory.

Known gap: the detached-daemon exit hook runs in the sandboxed hand, which cannot read
the agent directory, so it cannot sign. Its completion notice is appended unsigned; it
still wakes an idle parent, and the parent shows it as `[daemon:<name>] [unverified]`.
The planned fix is a supervisor outside the sandbox that runs the hook and signs.

Most commands exit 0 on success and 1 on a user or runtime error. A clap usage error
(for example a missing argument) exits 2.

A session's log is `<gait-dir>/sessions/<id>/log.jsonl`, its in-flight worktree is
`<gait-dir>/.active/<id>/`, and configuration is `<gait-dir>/gait.jsonc`.

## Skills

Skills are reusable, markdown-based capability files. Register, load, enable, or unload a
skill with `--kind skill` (see `enable`/`disable`/`register`/`unregister`/`load`/`unload`)
or the REPL's `/skill` command — this works in every Gait build.

### REPL commands

#### `/skill [load|unload <name>]`

List, load, or unload skills/mcp/plugins


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
- `tool_timeout_secs` — Wall-clock timeout (seconds) for a single ripgrep subprocess invocation backed by grep/codesearch/ls. On expiry the tool returns an error result instead of blocking the turn on a hung `rg` process — the same unbounded-subprocess risk `shell_timeout_secs` guards against, so it shares that default. NOT sized to preempt the spine's watchdog: the watchdog's phase-duration cap is already suppressed while tools are running (see `SpinePhase::HandsPhase` in `watchdog_breach`, `turn/spine.rs`) and its only live check during a tool call is brain ping-liveness, which is unaffected by how long a tool takes. User-set only; the agent never picks it.
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
