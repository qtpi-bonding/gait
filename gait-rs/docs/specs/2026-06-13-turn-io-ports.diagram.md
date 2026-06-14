# Turn I/O ports — diagrams

Companion to [`2026-06-13-stream-output-sink.md`](./2026-06-13-stream-output-sink.md).

## 1. Ports & adapters around the log store

The log is the hub. Input adapters feed the input port; the turn reads the store;
the turn's emissions go through one **record transaction** — `commit` to the log
then `publish` the event to the channel; sinks `classify` it at read time (in lockstep).

```mermaid
flowchart LR
    ARG["ArgSource<br/>(CLI --task)"]
    REPL_IN["ReplSource<br/>(stdin) · phase 2"]
    REMOTE_IN["RemoteSource<br/>(cloud) · phase 3"]

    ARG --> SRC
    REPL_IN --> SRC
    REMOTE_IN --> SRC

    SRC["TurnSource<br/>(input port)"] -->|"to_event(Input)"| APPEND["append (wire::append)"]
    APPEND --> LOG[("LOG<br/>event store")]
    LOG -->|"read on spawn"| TURN["turn"]

    TURN -->|"record(body)"| REC["Recorder"]

    subgraph TX["record = one transaction (lockstep)"]
        direction LR
        REC -->|"1 · commit (durable append)"| LOG
        REC -->|"2 · publish event (StreamItem)"| CH(["live channel"])
    end

    CH -->|"classify(item) at read"| SINK["TurnSink<br/>(output port)"]
    SINK -->|"Result"| OUT_STD["stdout"]
    SINK -->|"Trace + Error"| OUT_ERR["stderr"]
    SINK -->|"combo · phase 3"| OUT_CLOUD["cloud"]

    TURN -. "publish_delta (live only) · keeps recording until done" .-> REC
```

## 2. The record transaction (sequence)

`record` = commit then publish, in lockstep. The durable append is the commit;
the channel publish is an at-least-once projection of the committed event,
carrying its class. One call, so the log and the live sinks never diverge —
except deltas, which go through `publish_delta` (publish only, never committed).

```mermaid
sequenceDiagram
    participant T as turn
    participant R as Recorder
    participant L as LOG (store)
    participant C as channel
    participant S as TurnSink

    rect rgb(235, 245, 255)
    Note over T,S: record(body) — always both, in lockstep
    T->>R: record(body)
    R->>L: commit (durable append)
    Note right of L: COMMIT — event is now authoritative
    R-->>C: publish event (StreamItem, best-effort)
    C->>S: item(StreamItem)
    S->>S: classify(item) at read → stderr (Trace/Error)
    end

    Note over T,C: publish_delta(text): publish only — never committed
    T-->>C: publish Delta
    C->>S: item → Trace → stderr
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
