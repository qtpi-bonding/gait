# Observability

gait-rs's own action items from
[`an internal design note`](an internal design note)
(a cross-repo discipline convention shared with grorg, archegraph, and
multimatrix — see that doc's "gait input" section for the full reasoning).
This page is where that convention's per-repo checklist actually landed.

## Checking whether a gait-acp turn is still alive

gait-rs has no `tracing`/`log`/`slog` dependency anywhere in the workspace.
Instead, every turn writes a structured JSONL diagnostic stream, gated
behind `GAIT_DEBUG`:

```bash
GAIT_DEBUG=1 <however gait-acp is launched>
```

While `GAIT_DEBUG` is set, each session's turn writes heartbeat records to:

```text
<gait-dir>/.active/<session-id>/sessions/<session-id>/diagnostic.jsonl
```

Each line is one `DiagRecord`:
`{ts, mono_ms, session, turn, phase, event, elapsed_ms, attempt, model,
agent_id, detail}`. `phase` is one of `turn_begin`, `await_feedback`,
`llm_request`, `streaming`, `reasoning`, `stream_end`, `turn_end`. This is
the supported way to check turn liveness from outside the gait-acp process
without instrumenting anything new — if a turn looks stuck from an ACP
client's side, tail this file for the session in question:

```bash
tail -f <gait-dir>/.active/<session-id>/sessions/<session-id>/diagnostic.jsonl
```

A heartbeat lands roughly every 15 seconds while a turn is in flight (see
the ping task in `src/turn/brain.rs`), so a turn "stuck" on the same phase
across several consecutive heartbeats is a genuine stall in that phase; a
turn shorter than ~15s may show only its first (`turn_begin`) heartbeat
before completing, which is expected, not a gap.

`agent_id` is populated only when the turn's triggering message arrived via
a verified multimatrix delivery (`Provenance.attested_author_agent_id`) —
it lets a neighbor's `grep agent_id` line up gait's diagnostic lines with
multimatrix's and grorg's own logs for the same causal chain, per the
correlation-id convention in the concordat doc. Ordinary (non-delivery)
turns omit the field entirely.

`diagnostic.jsonl` is off by default and never created unless `GAIT_DEBUG`
is set — this is deliberate. It's a per-session file inside gait's own
workspace, not a live stream; leaving it on unconditionally would mean
creating and growing a file per session forever.

## Why gait-acp never logs to stdout

`gait-acp`'s stdout is the live [ACP](https://agentclientprotocol.com)
JSON-RPC transport to whatever client is driving it (multimatrix-bridge, an
editor, etc.). Writing log lines there would corrupt the protocol stream,
not supplement it. Any gait-acp logging goes to stderr or to
`diagnostic.jsonl`, never stdout — this rules out the "long-lived binaries
emit JSON to stdout" half of the cross-repo convention for gait
specifically; see the concordat doc for the full reasoning.

## `RUST_LOG`

Not applicable. There is no `tracing`/`log` crate in the gait-rs workspace,
so there is no `EnvFilter` to misconfigure and no filter string to document.
`GAIT_DEBUG=1` (above) is the one on/off switch that exists today.
