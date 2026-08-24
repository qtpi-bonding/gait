use std::path::Path;

use gait_state::gait::log::v1::Event;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::hub::SessionWatchHub;
use crate::parse::read_complete_events;
use crate::WatchError;

const POLL_FALLBACK_MS: u64 = 200;

/// Stream every `Event` written to a still-running session's log, starting
/// with everything already there (backlog) and then everything appended
/// after (live tail). The stream ends cleanly (no more items, no error) once
/// the watched file disappears — see `tail_loop` for why that's never
/// surfaced as an `Err`.
pub async fn watch_session_log(
    hub: &SessionWatchHub,
    gait_dir: &Path,
    session_id: &str,
) -> Result<UnboundedReceiverStream<Result<Event, WatchError>>, WatchError> {
    let log_path = gait_state::session::live::log_path(gait_dir, session_id);

    // Unbounded, deliberately: Phase 1 below sends the whole backlog before
    // this function returns the stream to its caller, so nothing is polling
    // the receiver yet. A bounded channel's `.send()` would block once full,
    // and with nobody able to drain it, that block would never resolve —
    // an arbitrarily large backlog would deadlock `watch_session_log` itself.
    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, WatchError>>();

    // Phase 1: backlog.
    let backlog = read_complete_events(&log_path)?;
    let last_seen = backlog.len();
    for event in backlog {
        if tx.send(Ok(event)).is_err() {
            return Ok(UnboundedReceiverStream::new(rx));
        }
    }

    // Phase 2: live tail, in the background.
    let subscription = hub.subscribe(log_path.clone()).await;
    tokio::spawn(tail_loop(log_path, last_seen, tx, subscription));

    Ok(UnboundedReceiverStream::new(rx))
}

async fn tail_loop(
    log_path: std::path::PathBuf,
    mut last_seen: usize,
    tx: mpsc::UnboundedSender<Result<Event, WatchError>>,
    mut subscription: crate::hub::SubscriptionHandle,
) {
    // A missing file on the first tick must not be treated as termination:
    // the session may not have written its first event yet.
    let mut ever_existed = log_path.exists();

    loop {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(POLL_FALLBACK_MS),
            subscription.next(),
        )
        .await;

        if !log_path.exists() {
            if ever_existed {
                return;
            }
            continue;
        }
        ever_existed = true;

        match read_complete_events(&log_path) {
            Ok(events) => {
                for event in events.into_iter().skip(last_seen) {
                    last_seen += 1;
                    if tx.send(Ok(event)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        }
    }
}
