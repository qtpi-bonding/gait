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

#[derive(Debug, PartialEq, Eq)]
pub enum LastTurn {
    /// The log holds none of these messages.
    NoRecord,
    /// A later turn finished or was cancelled; nothing to retry.
    Served,
    /// No later turn succeeded; run a turn over the existing log.
    Unserved,
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
            Some(Kind::TurnResult(result)) => Some(&result.outcome),
            _ => None,
        })
        .next_back();
    let served = matches!(last, Some(outcome) if !matches!(outcome, Some(Outcome::Failed(_))));
    if served {
        LastTurn::Served
    } else {
        LastTurn::Unserved
    }
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
    fn a_failed_turn_leaves_the_messages_unserved() {
        assert_eq!(
            last_turn_over(&[start(1), incoming(2, "m1"), failed(3)], &ids()),
            LastTurn::Unserved
        );
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
        assert_eq!(last_turn_over(&log, &ids()), LastTurn::Unserved);
    }

    #[test]
    fn a_paused_turn_that_was_resumed_and_failed_is_unserved() {
        let paused = result(3, Outcome::Pause(Default::default()));
        let log = [incoming(2, "m1"), paused, start(4), failed(5)];
        assert_eq!(last_turn_over(&log, &ids()), LastTurn::Unserved);
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
            LastTurn::Unserved
        );
    }

    #[test]
    fn a_result_before_the_messages_does_not_count() {
        let done = result(1, Outcome::Done(Default::default()));
        assert_eq!(
            last_turn_over(&[done, incoming(2, "m1")], &ids()),
            LastTurn::Unserved
        );
    }
}
