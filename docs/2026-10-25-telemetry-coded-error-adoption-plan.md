# gait: adopting coded-error and concord-telemetry

**Status: plan, rev 2 (after one Sonnet review). Nothing is implemented. No push until gp approves.**
Measured on gait `c88d5641` (main, P-256 identity + option C). Same shape as
grorg's plan (`grorg/docs/2026-10-24-telemetry-coded-error-adoption-plan.md`,
built) and multimatrix's (`multimatrix/docs/2026-10-24-...`, built).

Pins, **both together** (concord-telemetry pins coded-error; a different
coded-error rev gives two copies and error records stop seeing gait's codes;
step 1 checks `cargo tree -d -i coded-error` shows one):
`concord-telemetry` `02968a71148a8faf8e573671e7ac4efa2bcb3d7f`,
`coded-error` `197076449d81a6552d99ae3e54cdf14067bd0151`.

Facts below marked **(verify)** were inferred from a grep and not traced; step 0
turns each into a checked fact before any code is written.

## 0. Shared-library items (for the orchestrator)

| Id | Gap | Proposal |
|---|---|---|
| G1 | coded-error has `grpc`, `http`, `mcp`, `cli`, but no ACP mapping. gait-acp answers JSON-RPC with `respond_with_internal_error(String)` at ~20 sites; multimatrix-bridge, structmux and any other ACP agent will want the same class-to-code mapping and `data` shape. | Write the mapping as a small local function in gait-acp now (`wire_error(&anyhow::Error) -> agent_client_protocol::Error`), and lift it into a coded-error `acp` feature when a second user needs it. Tell me if you would rather it start in my-libs. |
| G2 | The "binaries only" rule vs gait-core emitting events. | Same ruling as grorg's B3: the rule means crates other repos depend on, and language-agnostic or wasm crates, never depend on it. Written as if accepted. |
| G3 | Sealed role processes (`__brain`, hand/`__run-tool`, `__daemon-exit`) cannot export. The hand has no network; the brain is sealed after start; `__daemon-exit` is a separate process launched from the sandboxed hand's shell and inherits its seal. | Decision D1 below. No library change. |

No `status(method)`-style gRPC helper is needed: gait has no gRPC (no `tonic`
anywhere in the workspace).

## 1. What gait looks like today

| Fact | Where |
|---|---|
| Binaries with a `main`: `gait` and `gait-min` (both `gait-bin`, sharing helpers in `gait-bin/src/lib.rs`), `gait-acp`. Test-only bins (`mock_brain`, `mock_brain_acp`, `gait-turn-runner/src/bin`, `xtask`) are out of scope. | `crates/gait-bin/src/{main,min_main}.rs`, `crates/gait-acp/src/main.rs` |
| **No `tracing` at all.** Zero `tracing` dependencies, zero macros, no subscriber. Diagnostics are **233** non-test `eprintln!` (146 in gait-cli, 15 gait-bin, 12 gait-workflow, 11 gait-state, 5 spine, 2 gait-repl, the rest scattered; 134 carry a `[gait]` prefix). Consequence: `RUST_LOG=...,gait_acp=debug,gait_ops=debug,gait_state=debug` in `docker/.env.example:42` names crates that log nothing today. | workspace-wide |
| **Process model.** One binary re-enters itself under sealed role tokens (`__brain`, `__run-tool`, `__daemon-exit`, `__local-runner`), dispatched before clap in `gait_core::__dispatch_role`. `gait-acp` does the same dispatch. The spine (in `gait run` or `gait-acp`) is the unsealed parent; the brain is sealed after start (network allowed); the hand has no network. A child `gait run` is spawned from three sites via `RunLaunch::to_process_argv`: `tools/spawn_agent.rs:218`, `turn/spine/handlers/agent.rs:58`, and `spawn_wake`. | `src/lib.rs:113,141`, `src/launch.rs`, `crates/gait-sandbox/src/sandbox.rs:187` |
| **`stderr` of `gait run` is a machine contract.** `port/output.rs:125` writes the JSONL trace (`llmCallStarted`, `toolCall`, ...) to stderr and stdout carries exactly two JSON lines; the `gait-agent` orchestrator skill parses both. A text log line on stderr would corrupt it. | `src/port/output.rs`, `src/turn/brain.rs:384` |
| `std::process::exit`: **~160** non-test sites. gait-cli `admin.rs` 67, `session.rs` 29, `workflow.rs` 26, `run.rs` 10, `init.rs` 2, `exec.rs` 2, `link.rs` 1, `args.rs` 1; gait-bin `main.rs` 10, `min_main.rs` 7; gait-repl 4; sealed roles `brain.rs` 6, `hand.rs` 3 (`exit(143)` on SIGTERM), `src/lib.rs` 3. The lint cannot see the library-crate ones. | see left |
| Error types (thiserror, all hand-named, none coded): `OpError` (gait-ops: NotFound, Conflict, SessionBusy, ConfirmationRequired, `Other(anyhow)`), `SessionError`, `ProtoError`, `GaitHomeError`, state `AuthError`, gait-auth `AuthError` and `CallbackError`, `ProviderError` (Connect, Timeout, Http{status,retry_after}, Overloaded, Sse, StreamIncomplete, ResponseTooLarge, Json, Other), `FactoryError`, `ClassifyError`, sandbox `ScopePathError`, `WatchError`, workflow `ValidationError` and `ContinueError`, `CapabilityError`, compaction `Error`, `GenerationError` (gait-local), `ToolError` (gait-types), `HostError`/`CodeModeError` (codemode). | `crates/*/src`, `src/turn/` |
| ACP boundary: **~20** `responder.respond_with_internal_error(String)` (handlers.rs 6, lifecycle.rs 14: unknown session, cannot close, failed to replay, plus raw `e.to_string()`), and one custom code: `-32001 prompt already seen by agent` with a `data` object (`idempotency.rs:13-30`) that multimatrix's `PromptError::already_seen_by_agent` relies on **(verify its exact `data` keys against multimatrix)**. Attestation failures reach the caller as `anyhow!("...: {error}")` strings. | `crates/gait-acp/src/{handlers,lifecycle,idempotency,attestation}.rs` |
| CLI boundary: each command prints its own `eprintln!` and calls `exit(1)`. `gait -e` returns a computed code (`exec.rs:233`). There is no mapping from error to exit code. | `crates/gait-cli/src/cli/*.rs` |
| Domain "events" exist only as session-log events (`log.jsonl`, persisted, not telemetry): turn finalization, `AgentUpdate`, `DaemonUpdate`, prompt append. | `src/turn/finalize.rs`, `gait-state/src/session/completion.rs` |
| Harness: concordium greps **no gait log line** (measured on concordium `58a6c9d`, my checkout, which predates wave 2; re-measure on `d938ec4`). The only gait mention in a log match is multimatrix's own `"executing dispatch command"` for `gait workflow run /e2e-fixtures/dormant-hello` (bats 02). | `tests/e2e/`, `docker/` |
| Gate today: `make lint` runs `fmt --check`, `clippy -D warnings`, and a `gait-acp` dependency check. No concord-lint, no OTel allowlist. | `gait-rs/Makefile` |

## 2. Scope and what gets deleted

In scope: `gait`, `gait-min` and `gait-acp` call `concord_telemetry::init` and
return `ExitCode`; gait's error types derive `CodedError` under `gait.*`; ACP
and CLI boundaries map coded errors; four domain events are `Event`-derived
protos asserted with `capture()`; the diagnostics that run in an initialised
process move to `tracing`; trace context crosses ACP and the child-`gait`
spawn; `scripts/check.sh` gates it.

Deleted when replaced (CLAUDE.md standing rule, no shims):

- The ~160 `std::process::exit` sites outside the sealed roles (commands return
  `Result`; `main` maps once). Sealed-role exits stay: their exit code is the
  IPC contract with the spine (143 on SIGTERM).
- The ~20 `respond_with_internal_error(String)` sites and `ALREADY_SEEN_ERROR_CODE`
  as a free constant (it becomes a coded variant with the same wire code).
- `OpError::Other(anyhow)` (callers carry the `anyhow` error directly) and the
  `anyhow!("...: {error}")` flattening of attestation errors.
- The interpolated `eprintln!` diagnostics in processes that have `init` (step 4).
  User-facing CLI output stays `eprintln!`: it is the UI.
- `docker/.env.example` filter names that stay dead (orchestrator).

Out of scope: `ToolError` and the model-facing error feedback strings (they
are prompt text, not a boundary); the persisted `log.jsonl` format and
`log.proto` (unchanged); the Nerve IPC frames except one added field pair
(step 2d); the `openai-codex` removal; daemon-exit events (see D1).

## 3. Steps, in order

Each step is its own commit and builds and tests green alone, through
`cargo-slot`, scoped tests, private `CARGO_TARGET_DIR`. Known failures stay
the two already known (anthropic strict-tool, flaky stream-to-file).

### Step 0: facts (no code)

A short note in the worktree `.superpowers/` answering, each with file:line:
(a) how a brain failure reaches the spine today (`turn/stream.rs:117,125`
prints `[gait] transport_error ...`; where does the typed `ProviderError`
stop?); (b) documented exit-code meanings of `gait run`/`gait -e`/`workflow run`
(manual, `gait-agent` skill, bats 02) so the sysexits change is stated, not
guessed; (c) classification of all 233 `eprintln!` into UI / diagnostic /
error-return; (d) which of the three child-`gait` spawn sites share one
spawn helper; (e) how the spine spawns the hand (`handlers/tool.rs:407`) and
whether the hand is per tool call; (f) (resolved in review: `gait run` handles signals at
`run.rs:316-344` with exit 0; `gait-acp` has none) the exact cancellation path
to replace `process::exit(0)`; (g) multimatrix's reading of the `-32001` `data`; (h)
`cargo deny` result with the new tree (D4).

### Step 1: dependencies, check script, `init`, `ExitCode`

- Workspace deps by git rev: `coded-error`, `concord-telemetry` (`testing` in
  dev-dependencies), and `tracing` (absent today, so every crate that logs
  gains it).

  | crate | coded-error features | concord-telemetry | tracing |
  |---|---|---|---|
  | gait-types, gait-proto, gait-platform, gait-sandbox, gait-codemode-js, gait-turn-runner | none | **no** (type, proto, sandbox and wasm-adjacent crates) | no |
  | gait-state, gait-ops, gait-auth, gait-provider, gait-workflow, gait-watch, gait-identity | default | no | gait-state, gait-ops |
  | gait-core (root `src/`) | default | **yes** (emits `TurnCompleted`, `AgentSpawned`, `AgentReported`) | yes |
  | gait-events (new, step 3) | - | yes (derive) | - |
  | gait-acp | default | yes | yes |
  | gait-cli | `cli` | no (init lives in gait-bin) | no |
  | gait-bin | `cli` | yes | no |

  `gait-types` keeps no dependency on either crate: `ToolError` stays uncoded.
- `gait-bin/src/lib.rs` gains `fn init_telemetry(service: &'static str,
  output: LocalOutput, principal: Option<Principal>) -> Option<Guard>` shared by
  `main.rs` and `min_main.rs`. A failing `init` prints to stderr and continues
  without telemetry; it never aborts a command (a malformed
  `OTEL_EXPORTER_OTLP_ENDPOINT` gives `InitError::Exporter`).
- **`gait` and `gait-min`: `LocalOutput::Off`.** stderr is the JSONL contract
  and the REPL owns the terminal. Records still export to OTLP when the
  endpoint is set. `GAIT_DEBUG` and `diagnostic.jsonl` are unchanged.
- **`gait-acp`: `LocalOutput::Json`** (stderr; stdout is the ACP stream and the
  library never writes there). `principal` is the agent (from the identity
  already loaded in `main`).
- Order in each `main`, all **before any runtime exists** (the exporter's
  construction and the `Guard` drop are not known to be safe inside an async
  context, so `init` stays on the plain thread): parse the agent-dir flag, load
  identity (today's fail-fast), **role dispatch**, then `init`. Role processes
  never call `init` (D1). This needs two code moves, stated here so they are not
  a surprise: (a) in `gait-bin` the role-token checks (`__local-runner`,
  `is_role_argv`, `is_run_argv`) live in `async fn run` under `block_on`
  (`main.rs:34-68`); they are hoisted into the synchronous `main` ahead of the
  runtime build. `__local-runner` is not in `is_role_argv` (`src/lib.rs:113-118`)
  so it gets its own exclusion from `init`. (b) `gait-acp/src/main.rs:49` is
  `#[tokio::main] async fn main()`; it becomes a plain `main` that builds the
  runtime by hand, as `gait-bin` already does.
- `main` returns `ExitCode`; the `Guard` is declared before the runtime so it
  drops after `block_on` returns; the 10 + 7 `exit` sites in `main.rs` and
  `min_main.rs` return a code.
- **Signals.** `gait run` already has a handler (`gait-cli/src/cli/run.rs:316-344`):
  first SIGINT/SIGTERM calls `handle.kill()`, waits 3 s, then
  `std::process::exit(0)`, so a held `Guard` never drops there and the existing
  contract is **exit 0 on signal**. Plan: keep exit 0 (callers, the `gait-agent`
  skill, rely on it), and replace the `process::exit(0)` with a cancellation
  that lets `cmd_run` return `ExitCode::SUCCESS` so the guard flushes. `gait-acp`
  has no signal handling today (no `ctrl_c` or `signal::` in its source); step 1
  adds a SIGTERM/SIGINT arm that ends the ACP loop and returns `ExitCode::SUCCESS`
  (a clean stop, same as `docker stop`), so records flush. No exit code changes
  for signals.
- **`init` failure message.** In `gait run` any non-JSONL stderr line breaks
  the stderr contract, so a failing `init` prints nothing there; it is dropped
  (records simply do not export). In `gait-acp` and the other `gait` commands it
  prints one line to stderr. The step 5 contract test includes a malformed
  `OTEL_EXPORTER_OTLP_ENDPOINT`.
- `scripts/check.sh` (gait has none; `make check` calls it): `cargo fmt
  --check`, `clippy -D warnings` (the Makefile already gates it, so no backlog
  exemption is needed), scoped tests, `concord-lint` installed at the pinned rev
  through the slots, `check-otel-dependents.sh` from a pinned concord-telemetry
  checkout (called, not copied) with `concord-telemetry` allowed, a check that
  the "no" rows of the table above have no `concord-telemetry` in `cargo tree`,
  and a grep that fails on `std::process::exit` anywhere under `gait-cli`,
  `gait-bin`, `gait-repl`, `gait-acp` and gait-core's `src/` outside the sealed-role files (concord-lint misses library crates), with
  an allowlist of the sealed-role files. All cargo through `cargo-slot`.
- `lint-baseline.txt` empty from the start (there are no tracing macros yet);
  `fields.local.toml` with: `turn iterations outcome message_count message_ids
  source key_id parent_session_id child_session_id signed provider model attempt
  phase status_code`. Shared seed already has `session_id agent_id elapsed_ms
  error`.

### Step 2: retype errors, bottom-up

Principle (same as grorg and multimatrix): internal APIs keep `anyhow::Result`;
typed errors are raised with `into_anyhow`/`.coded()` and read once per
boundary; `coded_error::downcast_ref`, never plain `downcast_ref`, after
crossing `anyhow`. Each enum has `ALL_CODES`, checked per crate with
`coded_error::check_codes`, plus one aggregate uniqueness test in `gait-bin`
(which sees every crate).

| Crate | Enum | Namespace | Notes |
|---|---|---|---|
| gait-ops | `OpError` | `gait.op` | `not_found`, `conflict`, `session_busy` (Transient), `confirmation_required` (all Logical except noted); `Other` is dropped |
| gait-state | `SessionError`, `ProtoError`, `GaitHomeError`, `AuthError` | `gait.session`, `gait.log`, `gait.home`, `gait.auth_store` | `unsupported_log_version` and the "removed identity format" parse error are Logical (operator wipes the workspace), io is Infrastructure |
| gait-auth | `AuthError`, `CallbackError` | `gait.auth`, `gait.auth.callback` | Logical for user-fixable (`malformed`, expired pending login), Transient for token endpoint 5xx |
| gait-provider | `ProviderError`, `FactoryError`, `ClassifyError` | `gait.provider`, `gait.provider.factory`, `gait.classify` | class from the **existing** retry classification (located in step 0, not reinvented): Connect, Timeout, Overloaded, StreamIncomplete, Sse and Http 429/5xx are Transient; Http 4xx and a missing API key are Logical; ResponseTooLarge, Json Infrastructure |
| gait-sandbox / gait-watch / gait-workflow / gait-local | `ScopePathError`, `WatchError`, `ValidationError`, `ContinueError`, `GenerationError` | `gait.sandbox`, `gait.watch`, `gait.workflow`, `gait.local` | workflow validation is Logical |
| gait-core | `CapabilityError`, compaction `Error` | `gait.capability`, `gait.compaction` | |
| gait-identity | new `IdentityError` | `gait.identity` | `agent_dir_unloadable`, `missing_keyfile`, `device_key_unreadable`, `delegation_expired` (the current anyhow strings from `IdentityContext::from_env`/`new`); Logical (operator-facing) |
| gait-acp | new `PromptError` | `gait.acp.prompt` | `attestation_required`, `attestation_invalid` (one variant per `message_attestation::AttestationError` kind the code can see: bad signature, unknown key id, not a recipient, replay, stale, empty), `already_seen` (Logical, wire code `-32001` kept), `session_unknown`, `session_busy`, `cannot_close`, `replay_failed`; plus `gait.acp.config` for startup flags (the two `bail!`s in `main.rs`) |
| gait-cli | new `CliError` | `gait.cli` | `no_gait_dir` (the exact existing text), `usage`, `not_found`, `confirmation_declined`, one variant per distinct failure the ~160 `exit(1)` sites print (step 2c audits and groups them; the Display texts are kept byte-exact) |

Class lines: Logical = the request or operator input is wrong or conflicts with
state; Infrastructure = ours (io, decode, sandbox setup, key files); Transient
= retry helps (provider connect/timeout/overloaded, session busy). Unknown only
for genuine uncoded leftovers.

- **2a** (no boundary change): the enums, raise sites, `check_codes`. Existing
  `Display` texts are kept; tests that assert error text keep passing.
- **2b ACP boundary.** the mapping lives in coded-error's `acp` feature (the orchestrator
  writes it, G1 resolved): `acp::AcpError { code: i32, message: String, data:
  serde_json::Value }` with `to_acp_error`, `to_acp_error_anyhow` and
  `to_acp_error_anyhow_redacted(err, source, fixed)`. Defaults: Logical -32602;
  Infrastructure, Unknown and Transient -32603 (clients tell Transient apart by
  `data.class`; -32002 is ACP's ResourceNotFound, so it is not used). A variant
  overrides the code with `#[coded(acp = -32001)]` (standard code or
  -32000..=-32099). `data` is always `{"code","class","fingerprint"}` (the same
  object MCP uses). gait-acp converts with
  `agent_client_protocol::Error::new(code, message).data(data)` and, for
  `already_seen`, inserts `reason: "already_seen_by_agent"` and `idempotency_key`
  into `data` at the call site (multimatrix's `is_already_seen_error` reads
  `data.reason` or the code -32001). The record side calls
  `concord_telemetry::error_anyhow!(&err, source = method, rpc.method = method)`
  with the same `method`, so the response fingerprint equals the record's.
  Message: the redacted form, Logical keeps its Display and every other class
  gets the fixed `"request failed"` (D3). Delete the ~20
  `respond_with_internal_error` calls. Tests: per variant, code/class/wire
  code; one `error` record per rejected prompt carrying no Display text.
- **2c CLI.** Commands in `gait-cli` return `Result<(), anyhow::Error>` (or
  an `ExitCode` where the command computes one: `gait -e`); `main` maps with
  `cli::exit_code_anyhow` (Logical 65, Infrastructure 69, Transient 75,
  Unknown 70) and prints the error chain once to stderr, as `eprintln!` today.
  The CLI emits **no** `error!` record when the failure happened in a process
  that already emitted one (a `gait run` child), to avoid double counting.
  Done per file, one commit each, biggest first: `admin.rs` (67), `session.rs`
  (29), `workflow.rs` (26), `run.rs` (10), then the singletons and gait-repl.
  This is the large mechanical part.
- **2d brain failures cross the Nerve.** The brain is sealed and has no
  `init`, so a coded `ProviderError` would be lost. The terminal-failure frame
  the brain sends gains `error_code` and `error_class` (internal IPC frame; not
  the persisted log). The spine rebuilds the error with
  `coded_error::from_remote` and emits the `error!` record. **(verify in step 0a
  that the frame exists and where the spine reads it.)**

### Step 3: domain events

New crate **`gait-events`**: `proto/gait/events.proto` (package
`gait.events.v1`) and its `build.rs`, exactly as the concord-telemetry README
and grorg/multimatrix wiring (`message_attribute` derive then `concord_event`,
never `type_attribute`; `extern_path` for `.concord_telemetry.v1` in prost and
pbjson; `principal.proto` through `concord-telemetry-proto` as a
build-dependency; per-type `extern_path`s for any `gait.log.v1` enum used).
`log.proto` and every other repo are untouched.

Events, emitted in the **spine / gait-acp process** (the only ones with `init`),
after the write is acknowledged:

| Event | Emitted from | Fields besides `source_record_id` |
|---|---|---|
| `PromptAccepted` | gait-acp, in the `on_durable` callback after the batch is appended (attested) and after the append for a direct prompt | `session_id`, `message_count`, `message_ids` (repeated), `source` (enum: direct, attested), `author` (`Principal`; attested: verified by `VERIFICATION_DELIVERY_ATTESTATION` and the delegation cert, direct: none). `source_record_id` = the first message id. |
| `TurnCompleted` | the spine at turn finalization (`turn/finalize.rs` or `run_lifecycle.rs`, **verify**) | `session_id`, `turn`, `outcome` (enum mirroring the existing terminal status, exhaustive `match`, no wildcard), `iterations`, `elapsed_ms`. `source_record_id` = session id + turn. |
| `AgentSpawned` | the spine's `spawn_agent` handler | `session_id` (parent), `child_session_id`, `agent_name`. |
| `AgentReported` | only at the two gait-core `notify_parent` call sites (`relay.rs:685`, `harness.rs:110`). Never inside gait-state: `notify_parent_unsigned` is also reached from the uninitialised `__daemon-exit` process (`daemon_exit.rs:83`), where an emit would silently no-op. | `parent_session_id`, `child_session_id`, `status` (done/clarify/inform), `signed` (true), `outcome` (woke/busy/already_surfaced/no_parent, mirroring `NotifyOutcome` exhaustively). |

`source_record_id` is a string field; its values are as stated. Not in this
round: the daemon-exit notice (D1: it runs as a separate process launched by the hand's detached shell, so it inherits the seal and cannot
export; when option B's supervisor lands it emits `DaemonReported` with
`signed=false`->`true`), dispatch and tool-call events (debug-level outcomes
with no consumer), `PermissionDecided`.

### Step 4: diagnostics to `tracing`

Only in processes that run `init` (spine, gait-acp, the `gait` CLI process):
the diagnostic `eprintln!`s classified in step 0c move to fixed-message
`tracing` calls with fields (`warn!(session_id = %id, error = %e, "...")`):
about 40-60 sites **(estimate; step 0c counts them)** in gait-acp, gait-state
(`notify_parent`, retrace "could not be verified"), spine, `turn/stream.rs`,
`gait-identity`. User-facing CLI output and everything inside the sealed
brain, hand and daemon-exit stays `eprintln!`. The `[gait] transport_error`
lines move to the spine-side coded record (2d). `error = %e` at non-boundary
`warn!` sites stays plain tracing (design rule 5). The baseline stays empty.

### Step 5: tests with `capture()`

`capture()` panics if a global subscriber is installed; gait installs none, so
it is safe, but `gait-core`'s many integration tests are separate binaries and
the unit tests share one: filter on the unique `session_id`/message id each test
creates, `capture_exclusive()` in a binary of its own where that is impossible.
Tests never call `init`.

- `PromptAccepted` once per accepted prompt (attested and direct), none for a
  rejected or replayed one.
- `TurnCompleted` once with the right outcome.
- `AgentSpawned` and `AgentReported` once each; `signed=true`.
- ACP: a rejected prompt returns the coded `acp::Error` (code, class,
  fingerprint in `data`) and emits one `error` record whose fingerprint equals
  `data.fingerprint` and which carries no Display text; `already_seen` still
  returns -32001 with its original `data` keys.
- CLI: a table test mapping each class to its sysexits code; `no_gait_dir` text
  byte-exact.
- **Contract test:** `gait run` against the mock brain with `RUST_LOG=trace`
  and no OTLP endpoint leaves stderr as pure JSONL and stdout as exactly two JSON
  lines. This guards the `Off` decision and the `gait-agent` skill.

### Step 6: trace propagation

`concord_telemetry::propagation` only; gait writes no `traceparent` parsing.
With no exporter nothing is sent.

- **6a ACP into gait-acp (the important one).** The bridge is long-lived, so a
  `TRACEPARENT` env var on its spawn would give one trace for the agent's whole
  life. Per-request context has to ride the message: `_meta.traceparent` (and
  `tracestate`) on `session/prompt`, next to the attestation key already read
  from `req.meta` (`handlers.rs:181,206`). gait-acp wraps the handler in an
  `acp.prompt` span and calls `set_parent` from `_meta` before entering it.
  `session/new` and `session/load` do the same. **The turn does not run on the
  handler thread:** `handlers.rs:312,374` start it on a `std::thread::spawn` with
  its own current-thread runtime, so the span is carried in explicitly
  (`Span::in_scope` around the thread body, or `.instrument(span)` on the inner
  future); otherwise `PromptAccepted`, `TurnCompleted` and `traceparent()` for
  child spawns lose the parent. A test pins that the `TurnCompleted` record
  carries the trace id of the `_meta.traceparent` it was prompted with (with the
  `testing` layer and an in-memory exporter, or by asserting the span's parent). **Cross-repo: multimatrix-bridge
  must `inject` into `_meta`**; I need the orchestrator to ask multimatrix.
  The attested `_meta` is signed over the body, not over `_meta`, so a trace id
  does not weaken attestation; a spoofed `traceparent` only chooses the trace id
  of its own request (the library's documented limit).
- **6b Child `gait` processes.** Set `TRACEPARENT` on the child's environment at
  the real spawn points, all in the spine process: `turn/spine/handlers/agent.rs`
  (detached `:69`, group `:92`) and `launch::spawn_wake` (`src/launch.rs:178-195`),
  whose callers are `relay.rs:694`, `harness.rs:119` and `turn/finalize.rs:218`
  (initialised spine) and `src/lib.rs:211` (the `__daemon-exit` process, which has
  no `init`, so `traceparent()` is `None` there and that wake starts a fresh trace;
  stated, not fixed). `tools/spawn_agent.rs:225` is **not** a spawn site: it
  builds argv and sends it over the Nerve from the sealed brain, so nothing is set
  there. `cmd_run` opens its root span with `set_parent_from_env`. Agent spawns
  and spine-initiated wakes then join the parent turn's trace.
- **6c Hand and shell tool (recommend, gated on grorg).** The spine builds the
  hand's environment with `env_clear` and an allowlist (`handlers/tool.rs:407`),
  and the shell tool filters again (`tools/shell/mod.rs:214,278,464`). Adding
  `TRACEPARENT` to both allowlists, with the value from the tool-call span,
  lets a `grorg` or `multimatrix` CLI the agent runs join the turn's trace
  **if that CLI calls `set_parent_from_env`**: that is grorg's and multimatrix's
  change, so I would ship 6c only after they confirm it. It is a one-line
  allowlist addition each; no protocol change if the hand is spawned per tool
  call (**verify 0e**).
- Not propagated: spine to brain and hand over the Nerve (no `init` there, so
  nothing to join; revisit if D1 changes). Streaming ACP notifications carry no
  trace id (the library's stated limit).

### Step 7: docs and handoff

Delete what this made stale: the manual's exit-code text if any, the `.env.example`
dead filter names (orchestrator), notes in gait's own P-256 plan about
`respond_with_internal_error`. Regenerate `docs/manual.md` from
`gait-manual/manual.yaml` (it is checked by a test). Update the `gait-agent`
skill note that exit status only means the session ended, now with sysexits.
Hand the orchestrator the list below.

## 4. Decisions that are gp's

- **D1 Sealed roles do not export.** The brain (sealed after start), hand and
  daemon-exit never call `init`; their telemetry is emitted by the spine from
  what crosses the Nerve (2d). Cost: no spans inside brain or hand; the
  daemon-exit event waits for option B's supervisor. Alternative: init the
  brain before the seal so it can export (it has network). Recommendation:
  not now; the spine already sees every outcome.
- **D2 `LocalOutput::Off` for the `gait` CLI.** Because stderr is a machine
  contract. Records reach the collector only when `OTEL_EXPORTER_OTLP_ENDPOINT`
  is set; a developer running `gait` locally sees nothing new.
- **D3 Code, class and fingerprint are not redacted on the ACP boundary; the
  message text is, for non-Logical classes.** The caller is the launching
  bridge inside the trust boundary, but it relays to room members, so an
  Infrastructure `message` (io paths) is replaced by a fixed string while a Logical
  error keeps its own Display. If gait-acp is ever exposed beyond the bridge,
  redact the metadata too.
- **D4 New dependency tree.** OpenTelemetry crates enter an AGPL workspace that
  gates on `cargo deny` licences; the check runs in step 0h and the answer goes in
  the step-1 commit. Compile cost and binary size grow; `gait-min` is meant to be
  small, so step 1 measures the size delta and, if it matters, gates telemetry in
  `gait-min` behind a default-off feature.

## 5. Risks

- **CLI exit codes change** from 1 to sysexits (65/69/70/75) on error. Callers
  to check: bats 02 (multimatrix runs `gait workflow run ...`; any nonzero is
  failure unless it compares to 1: **verify**), the `gait-agent` skill, any
  user script. `gait run`'s existing non-error codes are kept (step 0b).
- **`process::exit` removal is wide** (~160 sites, mostly `admin.rs`). A command
  that printed and exited mid-function must now unwind; the per-file commits keep
  each reviewable, and each file's tests run before the next.
- **The stderr contract** (D2) is the sharpest regression risk; the step 5
  contract test exists for it.
- **Remote-code interning** (`from_remote`) is bounded and advisory; a hostile
  peer cannot grow it past 4096 codes (library limit).
- **Volume.** Events are always on regardless of `RUST_LOG`; `error!` records
  have ERROR severity for every class, so each rejected prompt is an ERROR record.
  gait's turn events are low volume (one per turn, prompt and spawn).
- **Two copies of coded-error** if any gait dependency pins another rev; step 1
  checks `cargo tree -d -i coded-error`.
- **Unsigned daemon notices stay invisible to telemetry** until option B.

## 6. Sizing

| Step | Size |
|---|---|
| 0 facts | small |
| 1 deps, check.sh, init, `ExitCode`, signals | medium |
| 2a enums and raise sites | medium |
| 2b ACP boundary | small-medium (~20 sites) |
| 2c CLI `process::exit` removal | **large** (~160 sites, 15 files) |
| 2d Nerve error fields | small, depends on 0a |
| 3 events | medium (new crate, 4 events) |
| 4 diagnostics | small-medium |
| 5 tests | medium |
| 6 trace propagation | small (6a, 6b), small (6c) |
| 7 docs | small |

Order: 0, 1, 2a, 2b, 2d, 2c, 3, 4, 5, 6, 7. 2c can run in parallel with 3
if two builds are free. No push until gp says so.

## 7. For the orchestrator

- **Harness migration owner:** none needed, because no harness grep on a gait log line exists (below); if re-measuring on `d938ec4` finds one, the orchestrator moves it in the pin-bump commit, as for multimatrix.
- **Harness greps that would break: none found** for gait log text (see §1,
  re-measure on `d938ec4`). Behaviour changes it owns:
  - `docker/.env.example:42` `RUST_LOG` already names `gait_acp`, `gait_ops`,
    `gait_state`; those become live after step 4. gait-acp output on stderr gains
    JSON lines beside today's `gait-acp: ...` text lines; the harness's
    `jq fromjson?` tolerates that.
  - `OTEL_EXPORTER_OTLP_ENDPOINT` for the `worker-gait-acp` service in
    `docker/worker.yml`.
  - Exit codes: bats 02's dispatch command, and `docker/worker-gait-acp/entrypoint.sh`
    (`gait init` before registration; check against current main).
- **Cross-repo asks:** multimatrix-bridge injects `_meta.traceparent` on ACP
  prompts (6a); grorg-cli and multimatrix CLI call `set_parent_from_env` if
  6c is wanted; G1 (an ACP mapping home).
- **Shared-library item:** none blocking. G1 and, later, the `status(method)`
  pair (not needed here) are the candidates.
