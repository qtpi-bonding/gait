//! Decides, from the session log, whether a redelivery of already-seen messages
//! should run a turn. The turn that first ran over them may have failed (a
//! provider 429, an expired credential) after its messages were already durable
//! and marked seen; the redelivery then carries nothing new, and without this
//! check it would be answered with a silent no-op.
//!
//! The rule looks only at what came after the latest matching `IncomingMessage`:
//! if the last `TurnResult` there is not a failure (done, paused, cancelled) the
//! messages were served or deliberately stopped; if it is a failure, or there is
//! none, they are unserved. No (run, turn) matching is used: a retry runs as a `Resume` turn in
//! a new run, so scope matching would stop retrying after the first failed retry.
//! A turn that failed before the brain spawned writes no `TurnResult` at all;
//! that case also counts as unserved. The caller has already refused the request
//! when a turn is live, so a tail without a result is a crash or early failure.
use gait_core::proto::event_kind;
use gait_core::proto::gait::log::v1::{Event, payload::Kind, turn_result::Outcome};

/// The turn that failed over a set of messages, as its log result recorded it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FailedTurn {
    pub started_ms: u64,
    pub error_code: String,
    pub error_class: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LastTurn {
    /// The log holds none of these messages.
    NoRecord,
    /// A later turn finished or was cancelled; nothing to retry.
    Served,
    /// No later turn succeeded; run a turn over the existing log. Carries the
    /// failed turn when one wrote a failed result (none after a hard crash).
    Unserved(Option<FailedTurn>),
}

pub fn last_turn_over(events: &[Event], message_ids: &[String]) -> LastTurn {
    let latest = events
        .iter()
        .filter(|event| match event_kind(event) {
            Some(Kind::IncomingMessage(message)) => message_ids.contains(&message.message_id),
            _ => false,
        })
        .map(|event| event.seq)
        .max();
    let Some(latest) = latest else {
        return LastTurn::NoRecord;
    };
    // The last result after the latest matching message decides: a Pause that
    // was later resumed and failed must not count as served.
    let last = events
        .iter()
        .filter(|event| event.seq > latest)
        .filter_map(|event| match event_kind(event) {
            Some(Kind::TurnResult(result)) => Some(result),
            _ => None,
        })
        .next_back();
    match last.map(|result| (result, &result.outcome)) {
        Some((_, Some(Outcome::Failed(_)))) | None => {}
        Some(_) => return LastTurn::Served,
    }
    LastTurn::Unserved(last.and_then(|result| match &result.outcome {
        Some(Outcome::Failed(failed)) => Some(FailedTurn {
            started_ms: result.timing.as_ref().map_or(0, |timing| timing.started_ms),
            error_code: failed.error_code.clone().unwrap_or_default(),
            error_class: failed.error_class.clone().unwrap_or_default(),
        }),
        _ => None,
    }))
}

/// `RedeliveryRetried`: the retry is a typed event, linked to the new turn by
/// the trace (the turn runs under the prompt span this is emitted in) and to
/// the failed turn by its start time, which its `TurnCompleted` carries too.
pub fn emit_retried(
    session_id: &str,
    delivery_id: &str,
    deliverer: &str,
    failed: Option<FailedTurn>,
) {
    let failed = failed.unwrap_or_default();
    concord_telemetry::event!(gait_events::gait::events::v1::RedeliveryRetried {
        // A delivery can be retried more than once, and a hard-crashed turn has
        // no start time: the wall clock keeps every retry's id distinct.
        source_record_id: format!(
            "{session_id}:{delivery_id}:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        ),
        gait_session_id: session_id.to_owned(),
        delivery_id: delivery_id.to_owned(),
        deliverer: deliverer.to_owned(),
        failed_started_ms: failed.started_ms,
        error_code: failed.error_code,
        error_class: failed.error_class,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::log::v1 as pb;

    fn event(seq: u64, kind: Kind) -> Event {
        Event {
            seq,
            body: Some(pb::event::Body::Payload(pb::Payload { kind: Some(kind) })),
            ..Default::default()
        }
    }

    fn incoming(seq: u64, id: &str) -> Event {
        event(
            seq,
            Kind::IncomingMessage(pb::IncomingMessage {
                message_id: id.into(),
                ..Default::default()
            }),
        )
    }

    fn result(seq: u64, outcome: Outcome) -> Event {
        event(
            seq,
            Kind::TurnResult(pb::TurnResult {
                outcome: Some(outcome),
                ..Default::default()
            }),
        )
    }

    fn failed(seq: u64) -> Event {
        result(
            seq,
            Outcome::Failed(pb::TurnFailed {
                reason: "error".into(),
                ..Default::default()
            }),
        )
    }

    fn start(seq: u64) -> Event {
        event(seq, Kind::TurnStart(pb::TurnStart::default()))
    }

    fn ids() -> Vec<String> {
        vec!["m1".to_owned()]
    }

    #[test]
    fn messages_absent_from_the_log_have_no_record() {
        assert_eq!(
            last_turn_over(&[incoming(1, "other"), failed(2)], &ids()),
            LastTurn::NoRecord
        );
        assert_eq!(last_turn_over(&[], &ids()), LastTurn::NoRecord);
    }

    #[test]
    fn the_failed_turn_is_reported_with_its_code_and_start() {
        let failed = event(
            3,
            Kind::TurnResult(pb::TurnResult {
                timing: Some(pb::ExecutionTiming { started_ms: 77, ..Default::default() }),
                outcome: Some(Outcome::Failed(pb::TurnFailed {
                    reason: "error".into(),
                    error_code: Some("gait.provider.http".into()),
                    error_class: Some("transient".into()),
                    ..Default::default()
                })),
                ..Default::default()
            }),
        );
        assert_eq!(
            last_turn_over(&[incoming(2, "m1"), failed], &ids()),
            LastTurn::Unserved(Some(FailedTurn {
                started_ms: 77,
                error_code: "gait.provider.http".into(),
                error_class: "transient".into(),
            }))
        );
    }

    #[test]
    fn a_failed_turn_leaves_the_messages_unserved() {
        let log = [start(1), incoming(2, "m1"), failed(3)];
        assert!(matches!(last_turn_over(&log, &ids()), LastTurn::Unserved(Some(_))));
    }

    #[test]
    fn a_finished_turn_serves_them() {
        let done = result(3, Outcome::Done(Default::default()));
        assert_eq!(
            last_turn_over(&[start(1), incoming(2, "m1"), done], &ids()),
            LastTurn::Served
        );
    }

    #[test]
    fn a_paused_turn_is_not_retried() {
        let paused = result(3, Outcome::Pause(Default::default()));
        assert_eq!(
            last_turn_over(&[incoming(2, "m1"), paused], &ids()),
            LastTurn::Served
        );
    }

    #[test]
    fn a_cancelled_turn_is_not_retried() {
        let cancelled = result(3, Outcome::Cancelled(Default::default()));
        assert_eq!(
            last_turn_over(&[incoming(2, "m1"), cancelled], &ids()),
            LastTurn::Served
        );
    }

    #[test]
    fn a_failed_retry_is_still_unserved() {
        // The retry is a Resume turn in a new run: its own failed result has no
        // IncomingMessage, and must not make the messages look superseded.
        let log = [incoming(2, "m1"), failed(3), start(4), failed(5)];
        assert!(matches!(last_turn_over(&log, &ids()), LastTurn::Unserved(_)));
    }

    #[test]
    fn a_paused_turn_that_was_resumed_and_failed_is_unserved() {
        let paused = result(3, Outcome::Pause(Default::default()));
        let log = [incoming(2, "m1"), paused, start(4), failed(5)];
        assert!(matches!(last_turn_over(&log, &ids()), LastTurn::Unserved(_)));
    }

    #[test]
    fn a_later_successful_turn_serves_them() {
        let done = result(6, Outcome::Done(Default::default()));
        let log = [incoming(2, "m1"), failed(3), start(4), failed(5), done];
        assert_eq!(last_turn_over(&log, &ids()), LastTurn::Served);
    }

    #[test]
    fn a_turn_that_never_wrote_a_result_is_unserved() {
        assert_eq!(
            last_turn_over(&[start(1), incoming(2, "m1")], &ids()),
            LastTurn::Unserved(None)
        );
    }

    #[test]
    fn a_result_before_the_messages_does_not_count() {
        let done = result(1, Outcome::Done(Default::default()));
        assert!(matches!(last_turn_over(&[done, incoming(2, "m1")], &ids()), LastTurn::Unserved(_)));
    }
}
