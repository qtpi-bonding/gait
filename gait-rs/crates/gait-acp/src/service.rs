//! The lifecycle of the gait-acp process as events: `ServiceStarted` when it
//! comes up, `ServiceAlive` every minute, `ServiceStopping` as it goes down.
//! A monitor tells an idle seat from a dead one by these alone.
use std::time::Duration;

use gait_events::gait::events::v1 as ev;

pub const ALIVE_EVERY: Duration = Duration::from_secs(60);

/// Names this process run, so a monitor can pair a start with its stop and
/// alive events and tell a restart from a long run.
fn run_id() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let start_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("gait-acp:{}:{start_ms}", std::process::id())
    })
    .clone()
}

pub fn started() {
    concord_telemetry::event!(ev::ServiceStarted {
        source_record_id: run_id(),
        version: concord_telemetry::version!().to_owned(),
    });
}

pub fn stopping(reason: &str) {
    concord_telemetry::event!(ev::ServiceStopping {
        source_record_id: run_id(),
        reason: reason.to_owned(),
    });
}

/// Emits `ServiceAlive` every [`ALIVE_EVERY`] until the runtime ends.
pub fn spawn_alive() {
    spawn_alive_every(ALIVE_EVERY);
}

fn spawn_alive_every(every: Duration) {
    let began = std::time::Instant::now();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.tick().await; // the first tick is immediate: ServiceStarted covers it
        loop {
            tick.tick().await;
            concord_telemetry::event!(ev::ServiceAlive {
                source_record_id: run_id(),
                uptime_ms: began.elapsed().as_millis() as u64,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_and_stopping_each_emit_one_event() {
        let capture = concord_telemetry::testing::capture_exclusive();
        started();
        stopping("signal");
        assert_eq!(capture.events_where::<ev::ServiceStarted>(|_| true).len(), 1);
        let stopped = capture.single_event_where::<ev::ServiceStopping>(|e| e.reason == "signal");
        assert_eq!(stopped.reason, "signal");
    }

    #[tokio::test]
    async fn alive_repeats_on_the_interval() {
        let capture = concord_telemetry::testing::capture_exclusive();
        spawn_alive_every(Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(250)).await;
        let alive = capture.events_where::<ev::ServiceAlive>(|_| true);
        assert!(alive.len() >= 3, "several ticks in 250 ms, got {}", alive.len());
        assert!(alive[0].uptime_ms >= 40);
        assert!(alive[0].source_record_id.starts_with("gait-acp:"));
    }
}
