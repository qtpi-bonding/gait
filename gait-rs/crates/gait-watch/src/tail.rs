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

    // Phase 2: live tail, in the background. `ever_existed` is captured
    // *here*, synchronously, before spawning — not lazily inside the spawned
    // task on its first poll. `tokio::spawn` only schedules the task; the
    // scheduler may not actually run it until sometime later, and if the
    // caller (or another task) deletes `log_path` in that window, a version
    // of this check living inside `tail_loop` would see the file already
    // gone on its very first observation and wrongly conclude "never
    // existed" — which then never terminates, because that branch waits
    // forever for a file that already came and went. Computing it here
    // instead ties it to a point that happens-before any such race: this
    // function has already just read the backlog from `log_path` above, so
    // whatever `log_path.exists()` says at this exact point is the true
    // state as of a moment `watch_session_log`'s caller cannot get ahead of.
    let ever_existed = log_path.exists();
    let subscription = hub.subscribe(log_path.clone()).await;
    tokio::spawn(tail_loop(
        log_path,
        last_seen,
        ever_existed,
        tx,
        subscription,
    ));

    Ok(UnboundedReceiverStream::new(rx))
}

async fn tail_loop(
    log_path: std::path::PathBuf,
    mut last_seen: usize,
    mut ever_existed: bool,
    tx: mpsc::UnboundedSender<Result<Event, WatchError>>,
    mut subscription: crate::hub::SubscriptionHandle,
) {
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
