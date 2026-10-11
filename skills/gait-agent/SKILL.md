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
gait init <path>
gait init <path> --force
gait link /path/to/gait-workspace
gait config get <key>
gait config set <key> <value>
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
gait inference show <inference-name>
gait inference add <inference-name> --model <provider>/<model-id>
```

Set the workspace default:

```bash
gait config set inference <inference-name>
gait config get inference
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
gait inference select <inference-name> --session <session-id>
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
gait auth status <provider>
gait auth add <provider>
gait auth remove <provider>
gait auth login <provider>
gait auth logout <provider>
gait auth link <provider> <source>
gait auth unlink <provider>
gait models
gait models <filter>
```

## Sessions

```bash
gait sessions
gait sessions --json
gait ps
gait ps --json
gait stats <session-id>
gait stats <session-id> --json
```

Persist a session by merging it into the main codebase:

```bash
gait persist <session-id> --yes
gait persist --all --yes
gait persist <session-id> --json
```

Discard a session without merging it:

```bash
gait discard <session-id> --yes
gait discard --all --yes
gait discard <session-id> --json
```

Inspect or undo turns:

```bash
gait undo <session-id> --list
gait undo <session-id> --turn <turn-number>
gait undo <session-id> --turn <turn-number> --code
gait undo <session-id> --turn <turn-number> --transcript
```

## Permissions and session updates

```bash
gait allow <session-id> --pattern '<pattern>'
gait allow <session-id> --pattern '<pattern>' --scope <once|session|global|agent>
gait allow <session-id> --pattern '<pattern>' --scope agent --agent <agent-name>
gait deny <session-id>
gait unlock <session-id>
gait unlock <session-id> --scope <once|session|global|agent>
gait unlock <session-id> --scope agent --agent <agent-name>
gait reply <session-id> "<message>"
```

Load or unload a registered extension for a session:

```bash
gait load <mcp|skill|plugin> <name> <session-id>
gait unload <mcp|skill|plugin> <name> <session-id>
```

## Extensions

```bash
gait register <mcp|skill|plugin> <name> [options]
gait unregister <mcp|skill|plugin> <name>
gait enable <mcp|skill|plugin> <name>
gait disable <mcp|skill|plugin> <name>
```

Use `gait register --help` and the kind-specific help for registration
options. Scope options are `global`, `agent`, and `session`.

## Workflows and daemons

```bash
gait workflow list
gait workflow validate <workflow-name>
gait workflow run <workflow-name>
gait workflow continue <run-id>
gait workflow abort <run-id>
gait workflow log <run-id>

gait daemons list
gait daemons list --all
gait daemons stop <daemon-name>
```

## Common command options

Most workspace and session commands accept `--gait-dir <path>` or use
`GAIT_DIR`. Commands that operate on a codebase accept `--codebase <path>`.
Commands with machine-readable output accept `--json`.

```bash
gait <command> --help
gait --help
```
