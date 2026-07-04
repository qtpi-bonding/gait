//! Golden-transcript test harness for `run_turn` (Stage 0 / Task 1).
//!
//! Pins the *current* (pre-refactor) behavior of `run_turn()` by driving it with a
//! deterministic [`GoldenProvider`] and comparing the emitted event **bodies**
//! against a committed snapshot under `src/turn/snapshots/`. The body sequence
//! stays stable across Sessions, allow/deny sets, and UUIDs — those are stripped
//! from the snapshot — so a refactor that changes an inner block but produces a
//! different event body at the same call site fails the test loudly.
//!
//! Inconsistencies that exist today (e.g. cancellation skips `post_turn`, Fail
//! paths skip event+commit+drain) are **recorded verbatim** in the snapshots —
//! this suite is the regression baseline for Stages 1–5 of the
//! `run_turn()` decomposition plan. Fixing the inconsistencies is Stage 6 and
//! is out of scope for this task.
//!
//! Provider capability summary (per plan Step 4):
//! - yields a scripted sequence of `StreamChunk`s;
//! - can yield a scripted `ProviderError` variant (StreamIncomplete / Timeout /
//!   Overloaded) on demand;
//! - can **await the cancellation token** at a scripted yield point — unlike
//!   the existing `ScriptedProvider`/`MockProvider`/`FakeProvider` doubles in
//!   `tests/` and `src/turn/tests/integration.rs`, which only yield Ok(...).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_stream::stream;
use tempfile::TempDir;

use crate::config::{LlmParamsConfig, PreviewPolicy, SandboxConfig};
use crate::proto::gait::log::v1::{event, payload};
use crate::proto::wire;
use crate::proto::Recorder;
use crate::test_utils::init_git_repo;
use crate::tools::ToolCtx;
use crate::tools::{ReadTool, ShellTool, StructuredDoneTool, WriteTool};
use crate::types::{Tool, ToolDef};
use gait_provider::provider::{ChunkStream, Provider, ProviderError, StreamChunk, TimeoutPhase};

use crate::turn::TurnInput;

// ── GoldenProvider ────────────────────────────────────────────────────────────

/// One scripted step in a provider response.
///
/// `Ok` yields `chunks` then closes. `Err` yields exactly one `ProviderError`
/// (terminates the stream immediately). `WaitCancel` parks the stream until
/// the caller cancels the supplied token — the deterministic equivalent of
/// "user Esc's mid-stream" — then yields no further items.
#[derive(Clone)]
enum GoldenStep {
    Ok(Vec<StreamChunk>),
    /// `ProviderError` doesn't implement `Clone` (it wraps `serde_json::Error`
    /// via `#[from]`), so scenarios pass a constructor fn instead of a value.
    Err(fn() -> ProviderError),
    WaitCancel,
}

/// In-crate `Provider` test double for the golden transcript suite.
///
/// Differs from the other scripted doubles (`MockProvider`,
/// `ScriptedProvider`, `FakeProvider`) in two ways the plan calls out:
/// 1. It can yield any `ProviderError` variant, so the four stream-error
///    return sites (`turn/mod.rs:949–961`) can be exercised deterministically.
/// 2. Steps can `WaitCancel`, parking the stream until the cancellation token
///    passed via `run_turn` — `cancel_during_connect` (`~:921`) and
///    `cancel_mid_stream` (`~:1023`) only fire inside `tokio::select!` poll
///    loops. A pure synchronous stream trips the *start-of-loop* `is_cancelled()`
///    check at `~:783` and never reaches those sites.
struct GoldenProvider {
    steps: Vec<GoldenStep>,
    call_idx: AtomicUsize,
}

impl GoldenProvider {
    fn new(steps: Vec<GoldenStep>) -> Self {
        Self {
            steps,
            call_idx: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Provider for GoldenProvider {
    fn model_id(&self) -> &str {
        "golden"
    }

    async fn stream(
        &self,
        _system: &str,
        _messages: &[crate::types::Message],
        _tools: &[Box<dyn Tool>],
        _params: &LlmParamsConfig,
        _tail_context: Option<&str>,
    ) -> Result<ChunkStream, ProviderError> {
        let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .get(idx)
            .cloned()
            .unwrap_or(GoldenStep::Ok(vec![]));
        Ok(match step {
            GoldenStep::Ok(chunks) => Box::pin(stream! {
                for c in chunks { yield Ok(c); }
            }),
            GoldenStep::Err(e) => Box::pin(stream! {
                yield Err::<StreamChunk, ProviderError>(e());
            }),
            GoldenStep::WaitCancel => Box::pin(stream! {
                // Park forever until the caller's `tokio::select!` arm picks up
                // cancellation. `crate::turn::mod::cancel_token_fut` resolves when
                // the turn's token is cancelled — we never yield anything before
                // then, so the test stays deterministic.
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                if false {
                    yield Ok(StreamChunk::Done);
                }
            }),
        })
    }
}

// ── ProviderError constructors for the four stream-error scenarios ───────────

fn err_stream_incomplete() -> ProviderError {
    ProviderError::StreamIncomplete { received_bytes: 42 }
}

fn err_timeout() -> ProviderError {
    ProviderError::Timeout {
        phase: TimeoutPhase::Ttfb,
    }
}

fn err_overloaded() -> ProviderError {
    ProviderError::Overloaded
}

fn err_other() -> ProviderError {
    ProviderError::Other("synthetic other".into())
}

// ── BodyKind — the deterministic event discriminant ──────────────────────────

/// Discriminant used for snapshot comparison.
///
/// Snapshots are emitted in order, one entry per event body the brain emitted.
/// `Raw` and `Replace`/`Redact` mutations are stripped (UUID-only). Per-event
/// `id`s, `seq`s, `parents`, timestamps, and `content_hash` are likewise
/// stripped — they're bookkeeping, not behavior.
///
/// `Error` carries the `phase` enum string so test goldens still distinguish
/// git/llm/tool/init failures without binding to formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyKind {
    SessionStart,
    UserText {
        content: String,
    },
    AssistantText {
        content: String,
    },
    ToolCall {
        call_id: String,
        name: String,
    },
    ToolResult {
        call_id: String,
    },
    Permissions,
    Clarify,
    Inform,
    Unlock,
    CancellationSignal,
    ThinkingBlock,
    Error {
        phase: String,
    },
    HookRun,
    DaemonUpdate,
    AgentUpdate,
    ClarifyConvergence,
    InformConvergence,
    Cwd,
    Toggle,
    ModelChange,
    Summary,
    SandboxApplied,
    ContextStatus,
    /// A mutation event (Raw/Replace/Redact) folded away — sequence-presence
    /// only. Empty string keeps snapshots stable when IDs drift.
    Mutation,
    Raw {
        type_str: String,
    },
    Inject {
        role: String,
    },
    Unknown,
}

fn body_kind(b: &event::Body) -> BodyKind {
    let p = match b {
        event::Body::Payload(p) => p,
        event::Body::Replace(_) => return BodyKind::Mutation,
        event::Body::Redact(_) => return BodyKind::Mutation,
    };
    let k = match &p.kind {
        Some(k) => k,
        None => return BodyKind::Unknown,
    };
    match k {
        payload::Kind::SessionStart(_) => BodyKind::SessionStart,
        payload::Kind::UserText(u) => BodyKind::UserText {
            content: u.content.clone(),
        },
        payload::Kind::AssistantText(a) => BodyKind::AssistantText {
            content: a.content.clone(),
        },
        payload::Kind::ToolCall(tc) => BodyKind::ToolCall {
            call_id: tc.call_id.clone(),
            name: tc.name.clone(),
        },
        payload::Kind::ToolResult(tr) => BodyKind::ToolResult {
            call_id: tr.call_id.clone(),
        },
        payload::Kind::Permissions(_) => BodyKind::Permissions,
        payload::Kind::Clarify(_) => BodyKind::Clarify,
        payload::Kind::Inform(_) => BodyKind::Inform,
        payload::Kind::Unlock(_) => BodyKind::Unlock,
        payload::Kind::CancellationSignal(_) => BodyKind::CancellationSignal,
        payload::Kind::ThinkingBlock(_) => BodyKind::ThinkingBlock,
        payload::Kind::Error(e) => BodyKind::Error {
            phase: error_phase_to_string(e.phase),
        },
        payload::Kind::HookRun(_) => BodyKind::HookRun,
        payload::Kind::DaemonUpdate(_) => BodyKind::DaemonUpdate,
        payload::Kind::AgentUpdate(_) => BodyKind::AgentUpdate,
        payload::Kind::ClarifyConvergence(_) => BodyKind::ClarifyConvergence,
        payload::Kind::InformConvergence(_) => BodyKind::InformConvergence,
        payload::Kind::Cwd(_) => BodyKind::Cwd,
        payload::Kind::Toggle(_) => BodyKind::Toggle,
        payload::Kind::ModelChange(_) => BodyKind::ModelChange,
        payload::Kind::Summary(_) => BodyKind::Summary,
        payload::Kind::SandboxApplied(_) => BodyKind::SandboxApplied,
        payload::Kind::ContextStatus(_) => BodyKind::ContextStatus,
        payload::Kind::Inject(i) => BodyKind::Inject {
            role: i.role.clone(),
        },
        payload::Kind::Raw(r) => BodyKind::Raw {
            type_str: r.r#type.clone(),
        },
        payload::Kind::DaemonStart(_) | payload::Kind::DaemonStop(_) => BodyKind::DaemonUpdate,
        payload::Kind::Agent(_)
        | payload::Kind::PermissionsReply(_)
        | payload::Kind::TurnComplete(_)
        | payload::Kind::ClarifyReply(_)
        | payload::Kind::InformReply(_)
        | payload::Kind::UnlockReply(_) => BodyKind::Unknown,
    }
}

fn error_phase_to_string(p: i32) -> String {
    use crate::proto::gait::log::v1::ErrorPhase;
    match p {
        v if v == ErrorPhase::Llm as i32 => "llm".into(),
        v if v == ErrorPhase::Tool as i32 => "tool".into(),
        v if v == ErrorPhase::Git as i32 => "git".into(),
        v if v == ErrorPhase::Init as i32 => "init".into(),
        _ => "unspecified".into(),
    }
}

// ── Snapshot I/O ─────────────────────────────────────────────────────────────

const SNAPSHOT_DIR: &str = "src/turn/snapshots";

fn render_snapshot(name: &str, bodies: &[BodyKind], result_kind: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n", name));
    out.push_str(&format!("# result: {}\n", result_kind));
    out.push_str(&format!("# events: {}\n", bodies.len()));
    out.push_str("---\n");
    for (i, b) in bodies.iter().enumerate() {
        out.push_str(&format!("{:03}: {}\n", i, render_body(b)));
    }
    out
}

fn render_body(b: &BodyKind) -> String {
    match b.clone() {
        BodyKind::SessionStart => "SessionStart".into(),
        BodyKind::UserText { content } => format!(r#"UserText "{}""#, content),
        BodyKind::AssistantText { content } => format!(r#"AssistantText "{}""#, content),
        BodyKind::ToolCall { call_id, name } => {
            format!(r#"ToolCall "{}" "{}""#, call_id, name)
        }
        BodyKind::ToolResult { call_id } => format!(r#"ToolResult "{}""#, call_id),
        BodyKind::Permissions => "Permissions".into(),
        BodyKind::Clarify => "Clarify".into(),
        BodyKind::Inform => "Inform".into(),
        BodyKind::Unlock => "Unlock".into(),
        BodyKind::CancellationSignal => "CancellationSignal".into(),
        BodyKind::ThinkingBlock => "ThinkingBlock".into(),
        BodyKind::Error { phase } => format!("Error phase={}", phase),
        BodyKind::HookRun => "HookRun".into(),
        BodyKind::DaemonUpdate => "DaemonUpdate".into(),
        BodyKind::AgentUpdate => "AgentUpdate".into(),
        BodyKind::ClarifyConvergence => "ClarifyConvergence".into(),
        BodyKind::InformConvergence => "InformConvergence".into(),
        BodyKind::Cwd => "Cwd".into(),
        BodyKind::Toggle => "Toggle".into(),
        BodyKind::ModelChange => "ModelChange".into(),
        BodyKind::Summary => "Summary".into(),
        BodyKind::SandboxApplied => "SandboxApplied".into(),
        BodyKind::ContextStatus => "ContextStatus".into(),
        BodyKind::Mutation => "Mutation".into(),
        BodyKind::Raw { type_str } => format!(r#"Raw "{}""#, type_str),
        BodyKind::Inject { role } => format!(r#"Inject role={}"#, role),
        BodyKind::Unknown => "Unknown".into(),
    }
}

fn snapshot_path(name: &str) -> std::path::PathBuf {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    here.join(SNAPSHOT_DIR).join(format!("{}.snap", name))
}

fn read_snapshot(name: &str) -> Option<String> {
    let p = snapshot_path(name);
    std::fs::read_to_string(&p).ok()
}

fn write_snapshot(name: &str, content: &str) {
    let p = snapshot_path(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir snapshots");
    }
    std::fs::write(&p, content).expect("write snapshot");
}

fn diff_snapshots(name: &str, got_bodies: &[BodyKind], got_result: &str) -> String {
    let got = render_snapshot(name, got_bodies, got_result);
    match read_snapshot(name) {
        Some(want) if want == got => String::new(),
        Some(want) => {
            let mut s = String::new();
            s.push_str(&format!("snapshot mismatch for `{}`:\n", name));
            s.push_str("--- want ---\n");
            s.push_str(&want);
            s.push_str("--- got  ---\n");
            s.push_str(&got);
            s
        }
        None => format!(
            "no snapshot for `{}`; under `cargo test -- --bless` write it.\n--- got ---\n{}",
            name, got
        ),
    }
}

// ── Test driver ───────────────────────────────────────────────────────────────

/// Test inputs the harness needs to drive a single scenario.
struct Fixture {
    ws: TempDir,
    session_id: String,
    provider: Arc<GoldenProvider>,
    tools: Vec<Box<dyn Tool>>,
    allow: Vec<String>,
    deny: Vec<String>,
    input: TurnInput,
    /// Lives on `ResolvedProfile`, not `TurnInput` — set via `for_test_with_schema`.
    result_schema: Option<serde_json::Value>,
}

impl Fixture {
    fn new(provider: GoldenProvider, tools: Vec<Box<dyn Tool>>, auto: bool) -> Self {
        let ws = TempDir::new().expect("tempdir");
        init_git_repo(ws.path());
        Self {
            ws,
            session_id: "sgolden01".into(),
            provider: Arc::new(provider),
            tools,
            allow: vec![],
            deny: vec![],
            input: TurnInput {
                auto,
                max_iter: None,
                preview_policy: PreviewPolicy::default(),
                cancel: None,
                sandbox_on: false,
                sandbox_cfg: SandboxConfig::default(),
                relay: None,
                initial_git: None,
                daemons: vec![],
            },
            result_schema: None,
        }
    }

    fn repos(&self) -> crate::git::Repos {
        crate::git::Repos::production()
    }

    fn record_kind(&self) -> String {
        match self.run() {
            Ok(t) => match t {
                crate::turn::TurnResult::Done { .. } => "Done".into(),
                crate::turn::TurnResult::Permissions => "Permissions".into(),
                crate::turn::TurnResult::Clarify { .. } => "Clarify".into(),
                crate::turn::TurnResult::Inform { .. } => "Inform".into(),
                crate::turn::TurnResult::Unlock { .. } => "Unlock".into(),
                crate::turn::TurnResult::Cancelled => "Cancelled".into(),
                crate::turn::TurnResult::Failed { .. } => "Failed".into(),
            },
            Err(_) => "Err".into(),
        }
    }

    fn run(&self) -> anyhow::Result<crate::turn::TurnResult> {
        // Builds the session + emitter + ResolvedProfile around a fresh
        // `run_turn` call, then blocks on it. These are plain `#[test]` fns
        // (not `#[tokio::test]`), so there is no ambient runtime to conflict with.
        let gait_dir = self.ws.path().to_path_buf();
        let codebase_path = self.ws.path().to_path_buf();
        let session_id = self.session_id.clone();
        let provider: Arc<dyn Provider> = self.provider.clone();
        let tools = self.tools.clone_for_test();
        let allow = self.allow.clone();
        let deny = self.deny.clone();
        let input = self.input.clone();
        let repos = self.repos();
        let result_schema = self.result_schema.clone();
        let rt = tokio::runtime::Runtime::new().expect("build tokio runtime");
        rt.block_on(async move {
            let session = crate::session::Session {
                id: session_id,
                gait_dir,
                codebase_path,
                assigned_by: None,
                repos,
            };
            let caps = crate::profile::ResolvedProfile::for_test_with_schema(
                provider,
                tools,
                allow,
                deny,
                result_schema,
            );
            let (tx, _rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
            let emitter = Recorder::for_session(&session.gait_dir, &session.id, tx);
            crate::turn::run_turn(&session, &emitter, caps, input).await
        })
    }

    fn bodies_from_log(&self) -> Vec<BodyKind> {
        let tx_path = self
            .ws
            .path()
            .join("sessions")
            .join(&self.session_id)
            .join("log.jsonl");
        let events = wire::read_events(&tx_path).unwrap_or_default();
        events
            .iter()
            .filter_map(|e| e.body.as_ref().map(body_kind))
            .collect()
    }
}

/// Helper trait: cheaply clone `Vec<Box<dyn Tool>>` for the
/// `run_turn_test_call` helper without consuming the fixture.
trait CloneForTest {
    fn clone_for_test(&self) -> Vec<Box<dyn Tool>>;
}
impl CloneForTest for Vec<Box<dyn Tool>> {
    fn clone_for_test(&self) -> Vec<Box<dyn Tool>> {
        // We cannot `clone()` `Box<dyn Tool>` (trait objects aren't
        // generally cloneable). For the limited set of tools the harness
        // constructs, build fresh instances of the same shape.
        // Note: name-based reconstruction for the two `ToolDef` tools
        // (closing-the-gap), plus read/write/shell/structured-done
        // instances with the canonical ctx.
        let ctx = ToolCtx::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp"),
            "sgolden01".to_string(),
        );
        let names: Vec<&str> = self.iter().map(|t| t.name()).collect();
        let mut out: Vec<Box<dyn Tool>> = Vec::new();
        for n in names {
            match n {
                "read" => out.push(Box::new(ReadTool::new(ctx.clone()))),
                "write" => out.push(Box::new(WriteTool::new(ctx.clone()))),
                "shell" => out.push(Box::new(ShellTool::new(ctx.clone()))),
                "structured_done" => out.push(Box::new(StructuredDoneTool::new())),
                "clarify" => out.push(Box::new(crate::tools::ClarifyTool::new())),
                "inform" => out.push(Box::new(crate::tools::InformTool::new())),
                // ToolDef tools are recreated with a no-op closure. This is
                // safe because the harness only uses ToolDef shapes for
                // tools it doesn't actually execute — see scenario bodies
                // that pass empty `tools` or all-allow ToolDefs.
                other => out.push(Box::new(noop_tool_def(other))),
            }
        }
        out
    }
}

fn noop_tool_def(name: &str) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: format!("noop {}", name),
        parameters: serde_json::json!({"type": "object"}),
        execute: Box::new(|_args| Box::pin(async move { Ok(serde_json::Value::Null) })),
        permission: None,
    }
}

fn run_scenario(name: &str, fixture: Fixture) {
    // `record_kind` executes the turn (writing the log); `bodies_from_log`
    // must run after, or it reads a log that hasn't been written yet.
    let kind = fixture.record_kind();
    let bodies = fixture.bodies_from_log();
    let diff = diff_snapshots(name, &bodies, &kind);
    // Either explicit bless or first run (no snapshot yet): write & pass.
    if std::env::var("GOLDEN_BLESS").is_ok() || read_snapshot(name).is_none() {
        write_snapshot(name, &render_snapshot(name, &bodies, &kind));
        return;
    }
    if !diff.is_empty() {
        panic!("{diff}");
    }
}

fn make_done_scenario(sid: &str, tools: Vec<Box<dyn Tool>>, provider: GoldenProvider) -> Fixture {
    let mut fx = Fixture::new(provider, tools, true);
    fx.session_id = sid.to_string();
    fx
}

// ── Scenarios ────────────────────────────────────────────────────────────────
//
// One per exit kind (the plan's Step 5). Scenarios produce *all* events the
// brain emits on that path — including the documented inconsistencies we are
// pinning (per the Stage-6 carve-out) — and the `TurnResult` discriminant.

// 1. done_implicit — implicit-done path at the inline-text handling branch.
#[test]
fn golden_done_implicit() {
    let provider = GoldenProvider::new(vec![GoldenStep::Ok(vec![
        StreamChunk::Text("hello world".into()),
        StreamChunk::Done,
    ])]);
    let fx = make_done_scenario("sgdone01", vec![], provider);
    run_scenario("done_implicit", fx);
}

// 2. done_structured — output tool fires ExecutionSignal::TurnComplete.
#[test]
fn golden_done_structured() {
    let provider = GoldenProvider::new(vec![GoldenStep::Ok(vec![
        StreamChunk::ToolCallComplete {
            tool_call_id: "tc1".into(),
            tool_name: "structured_done".into(),
            args: serde_json::json!({"message": "task complete", "result": {"ok": true}}),
        },
        StreamChunk::Done,
    ])]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(StructuredDoneTool::new())];
    let fx = make_done_scenario("sgdone02", tools, provider);
    run_scenario("done_structured", fx);
}

// 3. permissions — supervised (auto:false) request to a tool not in `allow`.
#[test]
fn golden_permissions() {
    let provider = GoldenProvider::new(vec![GoldenStep::Ok(vec![
        StreamChunk::ToolCallComplete {
            tool_call_id: "tcperm".into(),
            tool_name: "shell".into(),
            args: serde_json::json!({"cmd": "echo hi", "reason": "x"}),
        },
        StreamChunk::Done,
    ])]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(ShellTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "sgperm01".to_string(),
    )))];
    let mut fx = Fixture::new(provider, tools, false);
    fx.session_id = "sgperm01".into();
    fx.allow = vec!["read".into()];
    run_scenario("permissions", fx);
}

// 4. clarify — ExecutionSignal::Clarify.
#[test]
fn golden_clarify() {
    // `structured_done` always signals TurnComplete — Clarify comes from the
    // dedicated `clarify` tool (ExecutionSignal::Clarify).
    let provider = GoldenProvider::new(vec![GoldenStep::Ok(vec![
        StreamChunk::ToolCallComplete {
            tool_call_id: "tcc".into(),
            tool_name: "clarify".into(),
            args: serde_json::json!({
                "items": [{"id": "i1", "content": "?", "why": "changes approach", "kind": "is", "options": ["a","b"]}]
            }),
        },
        StreamChunk::Done,
    ])]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::ClarifyTool::new())];
    let fx = make_done_scenario("sgclarify01", tools, provider);
    run_scenario("clarify", fx);
}

// 5. inform — ExecutionSignal::Inform.
#[test]
fn golden_inform() {
    // `structured_done` always signals TurnComplete — Inform comes from the
    // dedicated `inform` tool (ExecutionSignal::Inform).
    let provider = GoldenProvider::new(vec![GoldenStep::Ok(vec![
        StreamChunk::ToolCallComplete {
            tool_call_id: "tci".into(),
            tool_name: "inform".into(),
            args: serde_json::json!({
                "items": [{"id": "i1", "content": "heads up", "context": "ctx"}]
            }),
        },
        StreamChunk::Done,
    ])]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::InformTool::new())];
    let fx = make_done_scenario("sginform01", tools, provider);
    run_scenario("inform", fx);
}

// 6. unlock_precheck — supervised write outside the sandbox ceiling. Pre-check
//    returns Unlock before the tool runs.
#[test]
fn golden_unlock_precheck() {
    let provider = GoldenProvider::new(vec![GoldenStep::Ok(vec![
        StreamChunk::ToolCallComplete {
            tool_call_id: "tcup".into(),
            tool_name: "write".into(),
            args: serde_json::json!({
                "file_path": "/etc/passwd",
                "content": "noop"
            }),
        },
        StreamChunk::Done,
    ])]);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(WriteTool::new(ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "sgunlock01".to_string(),
    )))];
    let mut fx = Fixture::new(provider, tools, false);
    fx.session_id = "sgunlock01".into();
    fx.allow = vec!["write".into()];
    fx.input.sandbox_on = true;
    run_scenario("unlock_precheck", fx);
}

// 7. cancel_early — cancellation observed at the top-of-loop cancel guard
//    (~:783). With `cancel_token_fut` resolving before any LLM stream starts,
//    the brain emits CancellationSignal, commits, and returns Cancelled — but
//    (per current inconsistencies the snapshot pins) **omits** `post_turn`.
#[test]
fn golden_cancel_early() {
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let provider = GoldenProvider::new(Vec::new());
    let mut fx = Fixture::new(provider, vec![], true);
    fx.session_id = "sgcancelearly01".into();
    fx.input.cancel = Some(cancel);
    run_scenario("cancel_early", fx);
}

// 8. cancel_mid_stream — cancellation observed during a chunked `stream()`
//    wait (`~:1023`). Uses the `WaitCancel` arm so the producer hangs until the
//    token fires.
#[test]
fn golden_cancel_mid_stream() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_for_task = cancel.clone();
    let provider = GoldenProvider::new(vec![GoldenStep::WaitCancel]);
    let mut fx = Fixture::new(provider, vec![], true);
    fx.session_id = "sgcancelmid01".into();
    fx.input.cancel = Some(cancel_for_task.clone());
    // Trigger cancellation halfway through. The stream will be parked; the
    // turn loop's `tokio::select!` arm will resolve first.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        cancel.cancel();
    });
    run_scenario("cancel_mid_stream", fx);
}

// 9. stream_error — all four `ProviderError` stream-error variants. We test
//    each in its own scenario so each can pin its own body sequence.
#[test]
fn golden_stream_error_stream_incomplete() {
    let provider = GoldenProvider::new(vec![GoldenStep::Err(err_stream_incomplete)]);
    let mut fx = Fixture::new(provider, vec![], true);
    fx.session_id = "sgerr1".into();
    run_scenario("stream_error_stream_incomplete", fx);
}

#[test]
fn golden_stream_error_timeout() {
    let provider = GoldenProvider::new(vec![GoldenStep::Err(err_timeout)]);
    let mut fx = Fixture::new(provider, vec![], true);
    fx.session_id = "sgerr2".into();
    run_scenario("stream_error_timeout", fx);
}

#[test]
fn golden_stream_error_overloaded() {
    let provider = GoldenProvider::new(vec![GoldenStep::Err(err_overloaded)]);
    let mut fx = Fixture::new(provider, vec![], true);
    fx.session_id = "sgerr3".into();
    run_scenario("stream_error_overloaded", fx);
}

#[test]
fn golden_stream_error_other() {
    let provider = GoldenProvider::new(vec![GoldenStep::Err(err_other)]);
    let mut fx = Fixture::new(provider, vec![], true);
    fx.session_id = "sgerr4".into();
    run_scenario("stream_error_other", fx);
}

// 10. schema_exhausted — schema retries exhaust in auto mode. We exhaust
//     `done_nudges_remaining` (= output_schema_retries=3 → 4 plain-text
//     responses), so the brain fails with
//     "subagent ended without calling structured_done". Pure Err path; no
//     event, no commit, no drain — pinned as the documented inconsistency.
#[test]
fn golden_schema_exhausted() {
    let responses: Vec<GoldenStep> = (0..4)
        .map(|_| {
            GoldenStep::Ok(vec![
                StreamChunk::Text("text only".into()),
                StreamChunk::Done,
            ])
        })
        .collect();
    let provider = GoldenProvider::new(responses);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(StructuredDoneTool::new())];
    let mut fx = Fixture::new(provider, tools, true);
    fx.session_id = "sgschexh".into();
    fx.result_schema = Some(serde_json::json!({"type": "object"}));
    run_scenario("schema_exhausted", fx);
}

// 11. max_steps — runs to `max_iter: 2`, both iterations call a Continue-signal
//     tool (never structured_done) so the loop never exits early. Errors out
//     (`anyhow::bail!` at ~:1941). No event, no commit, no drain — pinned.
//     Plain-text responses (no tool call) would instead hit the *implicit
//     done* path after step 1 — that path is already covered by
//     `done_implicit`, so it can't also stand in for max-steps exhaustion.
#[test]
fn golden_max_steps() {
    let ctx = ToolCtx::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
        "sgmax".to_string(),
    );
    let responses: Vec<GoldenStep> = (0..2)
        .map(|i| {
            GoldenStep::Ok(vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: format!("tcr{i}"),
                    tool_name: "read".into(),
                    args: serde_json::json!({"file_path": "/tmp/does-not-exist.txt"}),
                },
                StreamChunk::Done,
            ])
        })
        .collect();
    let provider = GoldenProvider::new(responses);
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(ReadTool::new(ctx))];
    let mut fx = Fixture::new(provider, tools, true);
    fx.session_id = "sgmax".into();
    fx.allow = vec!["read".into()];
    fx.input.max_iter = Some(2);
    run_scenario("max_steps", fx);
}
