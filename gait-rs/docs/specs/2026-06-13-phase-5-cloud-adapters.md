---
status: superseded
---

# Phase 5 spec — Cloud adapters (SUPERSEDED)

**Superseded 2026-06-14 by [`2026-06-13-phase-5-embedding-api.md`](./2026-06-13-phase-5-embedding-api.md).**

The transport decision reframed Phase 5 from a *network* transport to an
**in-process embedding API** (drive gait as a Rust library by implementing the I/O
ports). A network transport (websocket / HTTP / socket) remains a valid *future*
follow-up — it is just another `TurnSource` + `TurnSink` pair over `embed::run_loop`,
addable with zero core change once the embedding API is the stable surface. The
original outline is kept in git history.
