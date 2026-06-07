# Rust Port Plan

## Why

- Truly static binary — no Bun/Node runtime dependency
- `git2` bundled — no `git` CLI required on the host
- Better distribution story: single download, runs anywhere
- Faster startup

## When

Not now. Reason: design is still in flux (transcript schema, workflow system). Port after:
1. Workflow design is stable (spec is done — see `workflow-design.md`)
2. Transcript schema is settled (currently at v2)
3. A release worth distributing

**Workflows will be implemented in Rust only.** Do not implement the workflow runner in TypeScript. The TypeScript build is feature-frozen at single-turn + structured output. All new primitives (workflow, branch worktrees, gate) go in Rust.

## Architecture Decision

Do NOT use a framework (Rig, Awaken, AISDK-rs). Own the agentic loop directly.

gait's loop is ~200 lines:
```
send(messages + tools) → parse response → if tool_calls: execute, append, repeat → else: done
```

This is where clarify/inform/permissions/output hook in — owning it is a feature.

## Minimal Deps

```toml
tokio       = { features = ["full"] }
reqwest     = { features = ["stream", "json"] }
serde       = { features = ["derive"] }
serde_json  = "*"
schemars    = "*"     # tool JSON schema from structs
git2        = "*"     # replaces all git execSync calls
clap        = { features = ["derive"] }
async-openai = "*"   # OpenAI-compatible providers (MiniMax, Groq, Mistral, OpenRouter, etc.)
```

Plus a thin custom Anthropic SSE client (~150 lines) for Anthropic-specific streaming.

## Provider Coverage

- `async-openai` covers ~80% of providers (all OpenAI-compatible endpoints)
- Custom Anthropic client covers the rest
- No framework needed for either

## What NOT to Use

- **Rig** (0xPlaygrounds) — 7.5k stars, production-proven, but includes RAG pipeline, vector stores, embedding abstractions, agent "character" system. gait needs none of this. Pay compile time and binary weight for nothing.
- **Awaken** — general-purpose agent runtime, daemon-based, no git, no built-in tools. Fundamentally different architecture.
- **AISDK (lazy-hq)** — closest Vercel AI SDK equivalent in Rust, but only 232 stars. Not mature enough to depend on.
