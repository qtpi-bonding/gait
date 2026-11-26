---
name: gait-agent
description: Use the Gait command-line interface to run turns, select inference profiles, manage sessions, permissions, extensions, and workflows.
---

# Gait CLI

Gait runs one turn per invocation. A gait workspace contains `gait.jsonc`,
inference profiles, agent files, and session history. The target repository is
passed separately as `--codebase`.

Set the workspace path once:

```bash
export GAIT_DIR=/path/to/gait-workspace
```

Use `--gait-dir <path>` instead of `GAIT_DIR` when needed. Use
`--codebase <path>` to select the repository the agent can access.

## Workspace

```bash
gait init --gait-dir <path>
gait init --gait-dir <path> --force
gait link --gait-dir /path/to/gait-workspace
gait config get --id <key>
gait config set --id <key> --value <value>
```

`gait init` creates a workspace. `gait link` writes a `.gaitref` in the current
directory. `gait config` reads or updates `gait.jsonc`.

## Inference profiles

An inference profile is stored in
`inferences/<inference-name>/inference.textproto`:

```textproto
model: "<provider>/<model-id>"
compaction_threshold_tokens: 128000
```

```bash
gait inference list
gait inference show --id <inference-name>
gait inference add --id <inference-name> --model <provider>/<model-id> \
  --compaction-threshold-tokens 128000
```

Set the workspace default:

```bash
gait config set --id inference --value <inference-name>
gait config get --id inference
```

Use a profile for one turn:

```bash
gait --inference <inference-name> \
  --codebase /path/to/codebase \
  "<task>"
```

Use a profile for an ephemeral request without a session:

```bash
gait -e --inference <inference-name> \
  --codebase /path/to/codebase \
  "<prompt>"
```

Select or clear the profile recorded for an active session:

```bash
gait inference select --id <inference-name> --session <session-id>
gait inference clear --session <session-id>
```

The `--inference` flag selects a profile for the invocation. The `inference`
config value supplies the default when the flag is absent.

## Turns

Run a new turn:

```bash
gait --inference <inference-name> \
  --codebase /path/to/codebase \
  "<task>"
```

Continue a session:

```bash
gait --session <session-id> \
  --codebase /path/to/codebase \
  "<task>"
```

Run with permission and execution flags:

```bash
gait --auto-approve \
  --allow '<pattern>' \
  --deny '<pattern>' \
  --unlock /path/to/file \
  --max-iter <count> \
  --codebase /path/to/codebase \
  "<task>"
```

Use `gait --help` for all top-level flags.

## Authentication and models

```bash
gait auth list
gait auth status --id <provider>
gait auth add --id <provider>             # prompts without echo in a terminal
gait auth add --id <provider> --key-file -
gait auth remove --id <provider>
gait auth login --id <provider>
gait auth login --id openai-signin --callback-file -
gait auth logout --id <provider>
gait auth link --id <provider> --source <source>
gait auth unlink --id <provider>
gait models
gait models --filter <filter>
```

## Sessions

```bash
gait sessions
gait ps
gait stats --id <session-id>
gait log --id <session-id>
```

Persist a session by merging it into the main codebase:

```bash
gait persist --id <session-id> --yes
gait persist --all --yes
```

Discard a session without merging it:

```bash
gait discard --id <session-id> --yes
gait discard --all --yes --force
```

Inspect or undo turns:

```bash
gait undo --id <session-id> --list
gait undo --id <session-id> --turn <turn-number>
gait undo --id <session-id> --turn <turn-number> --code
gait undo --id <session-id> --turn <turn-number> --transcript
```

## Permissions and session updates

```bash
gait allow --id <session-id> --pattern '<pattern>'
gait allow --id <session-id> --pattern '<pattern>' --scope <once|session|global|agent>
gait allow --id <session-id> --pattern '<pattern>' --scope agent --agent <agent-name>
gait deny --id <session-id>
gait unlock --id <session-id>
gait unlock --id <session-id> --scope <once|session|global|agent>
gait unlock --id <session-id> --scope agent --agent <agent-name>
gait reply --id <session-id> --message "<message>"
```

Load or unload a registered extension for a session:

```bash
gait load --kind <mcp|skill|plugin> --id <name> --session <session-id>
gait unload --kind <mcp|skill|plugin> --id <name> --session <session-id>
```

## Extensions

```bash
gait register --kind <mcp|skill|plugin> --id <name> [options]
gait unregister --kind <mcp|skill|plugin> --id <name>
gait enable --kind <mcp|skill|plugin> --id <name>
gait disable --kind <mcp|skill|plugin> --id <name>
```

Use `gait register --help` and the kind-specific help for registration
options. Scope options are `global`, `agent`, and `session`.

## Workflows and processes

```bash
gait workflow list
gait workflow types
gait workflow run <workflow-name>
gait workflow continue <run-id>
gait workflow abort <run-id>
gait workflow log <run-id>

gait processes list
gait processes list --all
gait processes stop --id <process-name>
```

## Common command options

Most workspace and session commands accept `--gait-dir <path>` or use
`GAIT_DIR`. Commands that operate on a codebase accept `--codebase <path>`.
Every command prints JSON; lists are `{"items":[...]}`. Add `--pretty` for indented output.

```bash
gait <command> --help
gait --help
```
