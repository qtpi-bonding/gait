//! Diagnostic records — a stream PARALLEL to the event log, never part of it.
//!
//! HARD RULE: this module must NOT import the event proto
//! (`gait_proto::gait::log`, `payload`, `Event`) and must NOT define any
//! `From`/`Into` between `DiagRecord` and an event type. Diagnostics travel as a
//! distinct `signal::Body::Diagnostic` and are written to `diagnostic.jsonl`,
//! never folded by `retrace`. Keeping the event proto out of this file is the
//! structural guarantee that a diagnostic can never become a transcript event.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    TurnBegin,
    AwaitFeedback,
    LlmRequest,
    Connecting,
    AwaitingHeaders,
    AwaitingFirstByte,
    Streaming,
    StreamEnd,
    TurnEnd,
    Unknown,
}

/// How the spine watchdog bounds a phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// Fast phases: bounded by `stall_cap`.
    StallCap,
    /// `streaming`: guard_idle owns idle/absolute; spine applies a generous backstop.
    AbsoluteBackstop,
    /// `await_feedback`: legitimate while the spine has a tool in flight; else stall_cap.
    ToolAware,
}

impl Phase {
    pub fn cap(&self) -> Cap {
        match self {
            Phase::Streaming => Cap::AbsoluteBackstop,
            Phase::AwaitFeedback => Cap::ToolAware,
            _ => Cap::StallCap,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseEvent {
    Start,
    Heartbeat,
    Ok,
    Timeout,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagRecord {
    pub ts: u64,      // unix epoch millis (dependency-free; no chrono in the workspace)
    pub mono_ms: u64, // ms since brain start (ordering, deltas)
    pub session: String,
    pub turn: u32,
    pub phase: Phase,
    pub event: PhaseEvent,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>, // transport phases only (spec fix: omitted on pings)
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Unix epoch millis. Dependency-free; used for DiagRecord.ts.
pub fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// True when GAIT_DEBUG is set to a non-empty value.
pub fn debug_enabled() -> bool {
    std::env::var("GAIT_DEBUG")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

impl DiagRecord {
    /// One compact JSON object, no trailing newline (the writer adds it).
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"event\":\"error\"}".into())
    }
}

// ── PhaseCell + process-global ────────────────────────────────────────────────

#[derive(Clone)]
pub struct PhaseCell(Arc<Mutex<(Phase, Instant)>>);

impl PhaseCell {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new((Phase::TurnBegin, Instant::now()))))
    }
    pub fn set(&self, p: Phase) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *g = (p, Instant::now());
    }
    pub fn current(&self) -> Phase {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).0
    }
    pub fn elapsed_ms(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .elapsed()
            .as_millis() as u64
    }
}
impl Default for PhaseCell {
    fn default() -> Self {
        Self::new()
    }
}

static BRAIN_PHASE: OnceLock<PhaseCell> = OnceLock::new();

/// Install the brain-process phase cell (call ONCE at brain startup). Returns a clone.
pub fn install_brain_phase_cell() -> PhaseCell {
    BRAIN_PHASE.get_or_init(PhaseCell::new).clone()
}

/// Set the current phase. No-op if not installed (spine process / tests).
pub fn set_phase(p: Phase) {
    if let Some(c) = BRAIN_PHASE.get() {
        c.set(p);
    }
}

/// (phase, elapsed_ms) of the installed cell, or None.
pub fn current_phase() -> Option<(Phase, u64)> {
    BRAIN_PHASE.get().map(|c| (c.current(), c.elapsed_ms()))
}

/// Phase ↔ snake_case string (matches the serde rename_all, but explicit for the
/// wire `phase` field on the Diagnostic signal so the spine can route without parsing JSON).
pub fn phase_str(p: Phase) -> &'static str {
    match p {
        Phase::TurnBegin => "turn_begin",
        Phase::AwaitFeedback => "await_feedback",
        Phase::LlmRequest => "llm_request",
        Phase::Connecting => "connecting",
        Phase::AwaitingHeaders => "awaiting_headers",
        Phase::AwaitingFirstByte => "awaiting_first_byte",
        Phase::Streaming => "streaming",
        Phase::StreamEnd => "stream_end",
        Phase::TurnEnd => "turn_end",
        Phase::Unknown => "unknown",
    }
}

pub fn phase_from_str(s: &str) -> Phase {
    match s {
        "turn_begin" => Phase::TurnBegin,
        "await_feedback" => Phase::AwaitFeedback,
        "llm_request" => Phase::LlmRequest,
        "connecting" => Phase::Connecting,
        "awaiting_headers" => Phase::AwaitingHeaders,
        "awaiting_first_byte" => Phase::AwaitingFirstByte,
        "streaming" => Phase::Streaming,
        "stream_end" => Phase::StreamEnd,
        "turn_end" => Phase::TurnEnd,
        _ => Phase::Unknown,
    }
}

impl DiagRecord {
    /// A lightweight phase ping: phase + elapsed only (no attempt/detail).
    /// `mono_ms` is 0 in pings — the brain-start timer is not wired to the ping
    /// task, so setting it to `elapsed_ms` (time-in-phase) would give consumers
    /// wrong deltas when diffing across records.
    pub fn ping(session: &str, turn: u32, model: &str, phase: Phase, elapsed_ms: u64) -> Self {
        Self {
            ts: now_unix_millis(),
            mono_ms: 0,
            session: session.into(),
            turn,
            phase,
            event: PhaseEvent::Heartbeat,
            elapsed_ms,
            attempt: None,
            model: model.into(),
            detail: None,
        }
    }
    /// A terminal failure record (spine-side; session known, turn/model best-effort).
    pub fn terminal(session: &str, phase: Phase, detail: &str) -> Self {
        Self {
            ts: now_unix_millis(),
            mono_ms: 0,
            session: session.into(),
            turn: 0,
            phase,
            event: PhaseEvent::Error,
            elapsed_ms: 0,
            attempt: None,
            model: String::new(),
            detail: Some(detail.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_to_one_json_line() {
        let r = DiagRecord {
            ts: 1_750_000_000_000,
            mono_ms: 14233,
            session: "s3d6bf3d6".into(),
            turn: 4,
            phase: Phase::AwaitingHeaders,
            event: PhaseEvent::Heartbeat,
            elapsed_ms: 45000,
            attempt: Some(1),
            model: "minimax/MiniMax-M3".into(),
            detail: Some("received 0 bytes".into()),
        };
        let line = r.to_json_line();
        assert!(!line.contains('\n'), "exactly one line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["phase"], "awaiting_headers");
        assert_eq!(v["event"], "heartbeat");
        assert_eq!(v["elapsed_ms"], 45000);
    }

    #[test]
    fn fast_phases_use_stall_cap_long_phases_do_not() {
        assert!(matches!(Phase::AwaitingHeaders.cap(), Cap::StallCap));
        assert!(matches!(Phase::Connecting.cap(), Cap::StallCap));
        assert!(matches!(Phase::Streaming.cap(), Cap::AbsoluteBackstop));
        assert!(matches!(Phase::AwaitFeedback.cap(), Cap::ToolAware));
    }
}
