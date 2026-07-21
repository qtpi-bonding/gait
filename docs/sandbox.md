# Sandbox

Two things called "sandbox" in gait, doing different jobs:

- **Permissions** gate what the *agent chooses* to do. A tool call matching a `deny[]` pattern is never executed; one matching an `allow[]` pattern runs immediately; everything else is queued for the user to approve. (See [Permissions](#permissions) below.)
- **The kernel sandbox** gates what the *process is physically able* to do. Configured in `gait.jsonc`'s `sandbox` block; enforced by Seatbelt on macOS, Landlock on Linux. Independent of permissions — `--auto-approve` skips the human approval prompt, but the kernel seal still applies.

The threat model and the brain/spine/hand process layout that makes the kernel seal work are in [`gait-rs/docs/sandbox-security.md`](../gait-rs/docs/sandbox-security.md). This doc is the config reference.

## Three-layer config model

A tool call is denied for exactly one of three reasons, and whichever it is, the transcript gets a `SandboxDenied` event naming it instead of a bare "operation not permitted" from the tool:

1. **The global ceiling is closed** (`sandbox.on = false` or the dimension not granted by `readable` / `writable` / `network`).
2. **The per-tool floor is `false`** for that dimension (`sandbox.tools.<tool>.<dim>`).
3. **(For `shell` only)** no `sandbox.shell_whitelist` entry matched.

```jsonc
"sandbox": {
  "on": true,

  // Layer 1 — global ceiling: the outer bound nothing below can exceed.
  "readable": ["~/.pub-cache"],
  "writable":  ["/tmp"],
  "network": "off",             // "off" | "*" | ["host1", "host2"]
                                 // Only "*" means all-hosts; any other bare
                                 // string is a config error.

  // Layer 2 — per-tool floor: what each builtin tool gets before any
  // shell_whitelist grant. Omit a tool to keep its built-in default.
  "tools": {
    "shell":    { "reads": true, "writes": true, "network": false },
    "webfetch": { "network": true }
  },

  // Layer 3 — shell-only overrides: widen shell's floor for one exact
  // single-segment command shape. Compound commands (cd &&, |, ;) never
  // match on purpose — use the shell tool's cwd param instead of `cd &&`.
  "shell_whitelist": [
    { "pattern": "flutter test *",    "grants": ["network"] },
    { "pattern": "flutter pub get *", "grants": ["network"] }
  ]
}
```

A whitelist match never pauses the turn — it auto-grants the listed dimension and emits a `SandboxWhitelistMatch` event recording which command, which pattern, and which dimension was granted. Distinct from a human-driven `gait unlock` decision; the two paths never conflate in the log.

## `--unlock` (CLI / REPL) — granting more at runtime

A turn can pause in `awaiting-unlock` state if a tool call exceeds the granted scope. The CLI form:

```bash
gait unlock <session-id> [--scope once|session|global|agent] [--agent <name>] --gait-dir <path> [--json]
```

`--scope` controls how broadly the grant is saved:

| Scope | Effect |
|---|---|
| `once` (default) | Logged in the session, doesn't persist beyond it. |
| `session` | Logged in the session, doesn't persist beyond it. (Same as `once` today; reserved for a future per-session target.) |
| `global` | Writes the grant into `gait.jsonc`. |
| `agent` | Writes the grant into the named agent's frontmatter (requires `--agent`). |

In the REPL, `/unlock` does the same thing with the current session pre-filled.

You can also pre-grant scope at the start of a turn with `--unlock <path>` (repeatable) on the top-level flags — those paths are added to the writable/readable list for the whole turn.

## `--sandbox` and `--no-sandbox`

Force the sandbox on or off for a single turn, overriding `gait.jsonc`'s `sandbox.on` default:

```bash
gait --sandbox    "<task>"     # force on, regardless of config
gait --no-sandbox "<task>"     # force off
```

`--sandbox-locked` is set by parent processes (subagent spawns) and forces the sandbox on in a way the child cannot override — even a child's own `--no-sandbox` is a no-op when the parent set `--sandbox-locked`.

## Permissions

Permissions are orthogonal to the sandbox — they gate tool calls before the kernel ever sees them, based on the `<tool>:<cmd>` pattern. Three tiers:

| Tier | How set | Effect |
|---|---|---|
| **Deny** | `deny[]` in `gait.jsonc` or agent frontmatter | Always blocked — never prompted. Emits a `PermissionDenied` event. |
| **Allow** | `allow[]` in `gait.jsonc` or agent frontmatter | Runs immediately without prompting. |
| **Ask** | Everything else | Queued and presented to the user (or auto-approved with `--auto-approve`). |

Patterns are matched against `<tool>:<cmd>` — e.g. `shell:rm *`, `shell:git push *`, `read:/etc/*`. The pattern is anchored at the prefix; `shell:rm *` matches `rm anything`.

## Approving or denying a pending request from outside the session

The REPL prompts inline; the CLI form for non-interactive / scripted use:

```bash
gait allow <id> [--pattern <pat>] [--scope <once|agent|global|session>] [--agent <name>] --gait-dir <path> [--json]
gait deny  <id> --gait-dir <path> [--json]
```

`gait ps --gait-dir <path> --json` gives you the session id and live state of every mounted session, so a script can poll, see which are `awaiting-permission`, and reply to them.

## Extending gait

Four ways to add capability without changing gait's source:

| Mechanism | What it adds | Where it lives | Managed with |
|---|---|---|---|
| **MCP servers** | New tools, proxied from an external MCP server. | `<gait-dir>/mcp/<name>.json` | `gait register/enable/disable/unregister mcp` |
| **Skills** | Prompt fragments appended to the system prompt. | `<gait-dir>/skills/<name>/SKILL.md` (frontmatter: `name`, `description`; body = markdown injected verbatim) | Auto-loaded from disk. The `gait register skill` / inline `gait.jsonc` `skills` array path still works but is deprecated. |
| **Hooks** | Shell-command side effects at fixed points in the turn loop. | `hooks` array in `gait.jsonc` or agent frontmatter | See [docs/hooks.md](hooks.md). |
| **WASM plugins** | Scaffolding only — manifest is loaded, but there is no execution path yet. | `<gait-dir>/plugins/<name>/PLUGIN.md` | `gait register/enable/disable/unregister plugin` writes the manifest. An enabled plugin currently has no runtime effect. |

For deeper custom logic than a hook or skill can express, `spawn_agent` (a builtin tool) lets a running turn delegate work to a whole new gait session with its own agent/model/tool config. See [docs/workflows.md](workflows.md) for the higher-level orchestration layer on top of that.
