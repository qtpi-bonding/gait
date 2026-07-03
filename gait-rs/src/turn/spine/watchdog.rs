//! Watchdog: liveness detection for the relay loop (ping grace, phase caps,
//! absolute backstop). See `WatchdogCfg::default` for the env knobs
//! (`GAIT_TEST_WATCHDOG_PING_MS`) that shrink these for integration tests.

use crate::diagnostic::Phase;

#[derive(Clone, Copy)]
pub(crate) struct WatchdogCfg {
    pub ping_ms: u64,
    pub absolute_backstop_ms: u64, // guard_idle_sse absolute_cap(600s) + 120s margin
}

impl Default for WatchdogCfg {
    fn default() -> Self {
        // `GAIT_TEST_WATCHDOG_PING_MS` shrinks the ping interval for integration tests
        // so the watchdog fires in seconds. Never set in production.
        let ping_ms = std::env::var("GAIT_TEST_WATCHDOG_PING_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15_000);
        Self {
            ping_ms,
            absolute_backstop_ms: 720_000,
        }
    }
}

/// Whether hands are currently running. Suppresses phase-duration caps in the
/// watchdog; the ping check (brain-wedged detection) stays active in all phases.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpinePhase {
    /// Brain is thinking / calling the LLM — no tool tasks running.
    BrainPhase,
    /// One or more tool tasks are in flight (spine spawned tokio tasks for them).
    HandsPhase,
}

/// Pure decision. `since_last_ping_ms = None` until the first ping (cold-start grace:
/// both caps disarmed). Pass `spine_phase` from the relay loop.
pub(crate) fn watchdog_breach(
    cfg: &WatchdogCfg,
    _phase: Phase,
    phase_elapsed_ms: u64,
    since_last_ping_ms: Option<u64>,
    spine_phase: SpinePhase,
) -> Option<&'static str> {
    let since = since_last_ping_ms?;
    // Always live: fires even in HandsPhase (brain-wedged detection).
    if since > 2 * cfg.ping_ms {
        return Some("no diagnostic ping (brain wedged)");
    }
    // Dead-man backstop: suppressed while tools are running.
    // guard_idle_sse fires at 600s; this is a 120s buffer above that.
    // Only reachable if guard_idle_sse itself fails to fire (runtime bug).
    if matches!(spine_phase, SpinePhase::BrainPhase) && phase_elapsed_ms > cfg.absolute_backstop_ms
    {
        return Some("absolute backstop exceeded");
    }
    None
}

/// Owns the watchdog's mutable ping/phase state for one relay turn.
/// Replaces the loose `last_phase`/`last_phase_elapsed_ms`/`last_ping_at`
/// bindings in the relay loop with a single object (see relay.rs).
/// NOTE: `watchdog_breach` currently ignores `last_phase` (its param is
/// `_phase`; the caps key off `last_phase_elapsed_ms` + `spine_phase`). It is
/// carried for signature parity / future use, not because it is load-bearing —
/// preserve the existing behavior exactly, do not "fix" this.
#[allow(dead_code)] // adopted by relay.rs in a later stage
pub(crate) struct Watchdog {
    cfg: WatchdogCfg,
    last_phase: crate::diagnostic::Phase,
    last_phase_elapsed_ms: u64,
    /// None until the first ping → cold-start grace (watchdog disarmed).
    last_ping_at: Option<std::time::Instant>,
}

#[allow(dead_code)] // adopted by relay.rs in a later stage
impl Watchdog {
    pub(crate) fn new() -> Self {
        Self {
            cfg: WatchdogCfg::default(),
            last_phase: crate::diagnostic::Phase::TurnBegin,
            last_phase_elapsed_ms: 0,
            last_ping_at: None,
        }
    }

    pub(crate) fn ping_ms(&self) -> u64 {
        self.cfg.ping_ms
    }

    /// Record a Diagnostic ping: refresh phase/elapsed and arm the timer.
    pub(crate) fn observe(&mut self, phase: crate::diagnostic::Phase, elapsed_ms: u64) {
        self.last_phase = phase;
        self.last_phase_elapsed_ms = elapsed_ms;
        self.last_ping_at = Some(std::time::Instant::now());
    }

    /// Check for a breach at a timeout tick. Delegates to `watchdog_breach`.
    pub(crate) fn breach(&self, spine_phase: SpinePhase) -> Option<&'static str> {
        let since = self.last_ping_at.map(|t| t.elapsed().as_millis() as u64);
        watchdog_breach(
            &self.cfg,
            self.last_phase,
            self.last_phase_elapsed_ms,
            since,
            spine_phase,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wd() -> WatchdogCfg {
        WatchdogCfg {
            ping_ms: 15_000,
            absolute_backstop_ms: 720_000,
        }
    }

    #[test]
    fn absolute_backstop_fires_past_720s() {
        // BrainPhase past the 720s backstop is a breach even with a fresh ping.
        assert!(watchdog_breach(
            &wd(),
            Phase::Streaming,
            730_000,
            Some(1_000),
            SpinePhase::BrainPhase
        )
        .is_some());
    }

    #[test]
    fn no_ping_cap_fires_when_wedged() {
        // streaming is "long", but no ping for >2*ping_ms means the runtime is wedged.
        assert!(watchdog_breach(
            &wd(),
            Phase::Streaming,
            10_000,
            Some(31_000),
            SpinePhase::BrainPhase
        )
        .is_some());
    }

    #[test]
    fn streaming_with_fresh_pings_is_not_a_breach() {
        assert!(watchdog_breach(
            &wd(),
            Phase::Streaming,
            120_000,
            Some(2_000),
            SpinePhase::BrainPhase
        )
        .is_none());
    }

    #[test]
    fn hands_phase_suppresses_backstop_but_not_ping_check() {
        // Absolute backstop suppressed while hands are running.
        assert!(watchdog_breach(
            &wd(),
            Phase::AwaitFeedback,
            730_000,
            Some(2_000),
            SpinePhase::HandsPhase
        )
        .is_none());
        // Ping check still fires even in HandsPhase (brain-wedged detection).
        assert!(watchdog_breach(
            &wd(),
            Phase::AwaitFeedback,
            200_000,
            Some(31_000),
            SpinePhase::HandsPhase
        )
        .is_some());
        // BrainPhase past absolute backstop → breach.
        assert!(watchdog_breach(
            &wd(),
            Phase::Streaming,
            730_000,
            Some(2_000),
            SpinePhase::BrainPhase
        )
        .is_some());
    }

    #[test]
    fn no_ping_yet_disarms_both_caps() {
        // Before the first ping (None), neither cap fires (cold-start grace).
        assert!(watchdog_breach(
            &wd(),
            Phase::Streaming,
            730_000,
            None,
            SpinePhase::BrainPhase
        )
        .is_none());
    }

    #[test]
    fn fresh_pings_do_not_save_a_capped_stream() {
        let cfg = wd();
        // ping just arrived (since=0) but elapsed crossed absolute backstop.
        assert!(watchdog_breach(
            &cfg,
            Phase::Streaming,
            720_001,
            Some(0),
            SpinePhase::BrainPhase
        )
        .is_some());
    }
}
