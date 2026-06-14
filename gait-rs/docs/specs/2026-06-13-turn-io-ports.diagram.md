# Turn I/O ports — diagrams

Companion to [`2026-06-13-stream-output-sink.md`](./2026-06-13-stream-output-sink.md).

## 1. Ports & adapters around the log store

The log is the hub. Input adapters feed the input port; the turn reads the store;
the turn's emissions go through one **emit transaction** that commits to the log
and tees a live copy to the output sinks.

```mermaid
flowchart LR
    ARG["ArgSource<br/>(CLI --task)"]
    REPL_IN["ReplSource<br/>(stdin) · phase 2"]
    REMOTE_IN["RemoteSource<br/>(cloud) · phase 3"]

    ARG --> SRC
    REPL_IN --> SRC
    REMOTE_IN --> SRC

    SRC["TurnSource<br/>(input port)"] -->|"to_event(Input)"| APPEND["append event"]
    APPEND --> LOG[("LOG<br/>event store")]
    LOG -->|"read on spawn"| TURN["turn"]

    TURN -->|"emit(body)"| EM["Emitter"]

    subgraph TX["emit = one transaction"]
        direction LR
        EM -->|"1 · durable write (COMMIT)"| LOG
        EM -->|"2 · tee live copy"| CH(["live channel"])
    end

    CH -->|"classify(item)"| SINK["TurnSink<br/>(output port)"]
    SINK -->|"Result"| OUT_STD["stdout"]
    SINK -->|"Trace + Error"| OUT_ERR["stderr"]
    SINK -->|"combo · phase 3"| OUT_CLOUD["cloud"]

    TURN -. "keeps emitting until done" .-> EM
```

## 2. The emit transaction (sequence)

The durable append is the commit; the channel tee is an at-least-once projection
derived from the committed event. They are one call so the log and the live
sinks never diverge — except deltas, which are tee-only and never logged.

```mermaid
sequenceDiagram
    participant T as turn
    participant E as Emitter
    participant L as LOG (store)
    participant C as channel
    participant S as TurnSink

    rect rgb(235, 245, 255)
    Note over T,S: emit(body) — atomic: commit, then project
    T->>E: emit(body)
    E->>L: write_to_transcript (durable)
    Note right of L: COMMIT — event is now authoritative
    E-->>C: send(StreamItem) (best-effort tee)
    C->>S: item(StreamItem)
    S->>S: classify → stdout / stderr / cloud
    end

    Note over T,C: delta(text): tee only — never written to LOG
    T-->>C: send(Delta)
    C->>S: item(Delta) → Trace → stderr
```

## 3. The unified run loop (CLI and REPL converge)

Both `cmd_run` and the REPL become the same loop; only the adapter pair differs.

```mermaid
flowchart TB
    START(["run_loop(source, sink)"]) --> RL["sink.result_line(session_id)"]
    RL --> NEXT{"source.next()"}
    NEXT -->|"Some(Input)"| APP["append to_event(Input) → LOG"]
    APP --> SPAWN["spawn turn (reads LOG)"]
    SPAWN --> DRIVE["drive(rx, sink)"]
    DRIVE --> RES{"TurnResult"}
    RES -->|"Clarify / Inform / Permissions"| NEXT
    RES -->|"Done"| AFTER["after-turn coordination"]
    AFTER --> NEXT
    NEXT -->|"None (EOF / single-shot done)"| END(["exit"])
```
