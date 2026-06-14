# Phase 5 spec — Cloud adapters

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phases 1–4. *Outline only — depends on a transport choice not yet made;
expect a full rewrite when it's real.*

## Goal

Prove the boundary: a remote-control agent drives gait over a network by
implementing one `TurnSource` + one `TurnSink`, with **zero** changes to the turn,
the log, or the other adapters. This is the payoff the whole boundary was shaped
for.

## Design (outline)

- **`RemoteSink: TurnSink`** — serializes each channel item (by `classify`) and
  the final `TurnResult` onto a transport (websocket / HTTP stream / gRPC — TBD).
  A remote observer renders Trace, surfaces Error, and reads the Result envelope
  (`{message, result, changes}`) — the same structure shell/`jq` and workflow
  nodes read.
- **`RemoteSource: TurnSource`** — receives control messages → `event::Body`
  appended to the log; `retrace` folds them. The cloud client is not special: it
  appends like every other driver.
- One bidirectional transport implements both around the same session log.

## Open decisions (deferred)
- **Transport + message format** — not chosen. Drives everything else here.
- **Tee input to the channel** — today input events are appended log-only, so a
  remote observer wouldn't see "user said X" live. For a shared/observed session,
  input events should also publish. This is the one place the input-bypasses-the-
  channel fact (architecture plan, §Architecture note) needs revisiting. Decide
  with the transport.
- **Auth / session ownership / multi-observer** — out of scope until a transport
  exists.

## Why it's cheap once Phases 1–4 land
`TurnSink`/`TurnSource` are agnostic, `classify` is read-time and reusable, the
result envelope is already the universal record, and `retrace` already folds any
appended input. So a cloud driver is "two adapters + a transport," not a turn
rewrite.
