---
name: gait-manual
description: Explain how to use and configure Gait.
manual-source-hash: 65c22a0e5cdeba633ec18ee64f0c655f1cf1f1d2842c549695d704ff2a23dc5e
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
      --force  Create the workspace even if it would live inside a codebase git repo. (In-tree is discouraged: running such a workspace also needs `--allow-intree`, since the agent could otherwise write to its own config. Prefer out-of-tree: `gait init ~/gait-home && gait link ~/gait-home`.)
  -h, --help   Print help
```

### REPL commands

#### `/help`

Show this command guide

Aliases: /?


#### `/info`

Model, session, gait-dir, codebase


## Sessions

Sessions preserve turns, tool activity, and workspace state.

Start a fresh session when you want a clean conversation. Use undo when you need to
restore an earlier turn or inspect available checkpoints.

### CLI commands

#### `gait sessions`

```text
Usage: sessions [OPTIONS] --gait-dir <GAIT_DIR>

Options:
      --gait-dir <GAIT_DIR>  [env: GAIT_DIR]
      --json
  -h, --help                 Print help
```

### REPL commands

#### `/new`

Start a fresh session

Aliases: /n


#### `/undo [N]`

Undo the last turn

Aliases: /u


#### `/unlock [once|session|global|agent]`

Unlock a held session


## Inferences and providers

Inferences name the models Gait can use. Provider credentials are managed separately
from inference selection.

Use the CLI for durable configuration and the REPL commands for inspecting or changing
the current interactive session.

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
      --gait-dir <GAIT_DIR>  [env: GAIT_DIR]
      --model <MODEL>        Model for the `add` action, in `provider/id` form (e.g. `opencode/kimi-k2.6`). Ignored by other actions
      --session <SESSION>
  -h, --help                 Print help
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
      --home <HOME>  Override the external CLI's home directory
  -h, --help         Print help
```

#### `gait models`

`gait models [filter]`

```text
`gait models [filter]`

Usage: models [FILTER]

Arguments:
  [FILTER]  Optional substring filter for model ids

Options:
  -h, --help  Print help
```

### REPL commands

#### `/inference [name|default|show <name>|add <name> <provider/id>]`

List, show, add, select, or clear a named inference


#### `/auth [list|status <provider>|link <provider> <source> [--home <path>]|unlink <provider>]`

List, inspect, link, or unlink credentials


## Workflows and extensions

Workflows run repeatable sequences of prompts and operations. Skills, MCP tools, and
plugins can be enabled for a session when they are available in the configured Gait
directory.

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
  -h, --help                 Print help
```

### REPL commands

#### `/workflow <name> [args...]`

Run a workflow


#### `/skill [load|unload <name>]`

List, load, or unload skills/mcp/plugins
