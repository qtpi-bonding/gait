# Forgecode Review

Reviewed June 2026: https://github.com/tailcallhq/forgecode

## What it is

Forge is a competing Rust agentic CLI — 131K LOC, 26 crates, TUI, 30+ providers, semantic search, MCP, telemetry. A full application, not a library. Nothing to import.

## Key validation

Forge made the same core architectural call we did: **no external Rust AI SDK crate, own the HTTP layer directly** via `reqwest` + hand-rolled SSE. Their provider layer and our `gait-ai` crate are structurally identical. Good signal.

## Worth borrowing (by reading, not importing)

- **`gix` over `git2`** — use gitoxide when we get to git integration. More actively maintained, pure Rust, better API.
- **`forge_eventsource_stream`** — their SSE parsing is more battle-tested than `eventsource-stream 0.2`. Worth reviewing if we hit edge cases.
- **`FsSearch` / `FSPatch` tool logic** — their grep (ripgrep integration) and edit tools are close equivalents to gait's. Good reference for the tools phase.
- **Provider edge cases** — their Anthropic/OpenAI provider impls handle some edge cases (token caching headers, Bedrock ID sanitization, doom-loop detection) worth knowing about.

## Doesn't apply

TUI, semantic search, telemetry, OAuth, Bedrock/Google auth, triple-layer domain/infra/app architecture — all out of scope for gait's lean philosophy.

## Conclusion

Right call to build `gait-ai` ourselves. Forge confirms the approach works at production scale. Primary actionable: **use `gix` not `git2`** in the next phase.
