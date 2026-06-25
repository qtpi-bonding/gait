//! Diagnostic records — a stream PARALLEL to the event log, never part of it.
//!
//! HARD RULE: this module must NOT import the event proto
//! (`gait_proto::gait::log`, `payload`, `Event`) and must NOT define any
//! `From`/`Into` between `DiagRecord` and an event type. Diagnostics travel as a
//! distinct `signal::Body::Diagnostic` and are written to `diagnostic.jsonl`,
//! never folded by `retrace`. Keeping the event proto out of this file is the
//! structural guarantee that a diagnostic can never become a transcript event.

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

impl DiagRecord {
    /// One compact JSON object, no trailing newline (the writer adds it).
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"event\":\"error\"}".into())
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
