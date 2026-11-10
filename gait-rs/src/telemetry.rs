//! Process telemetry setup shared by every gait binary (`gait`, `gait-min`,
//! `gait-acp`) and each hidden role subprocess: one place that names the
//! service, initialises the exporter and flushes it before the process ends.

/// The telemetry service name of a process: each hidden role subprocess is its
/// own service, so a backend can tell the sealed brain from the CLI.
pub fn service_for_argv(arg1: Option<&str>, default: &'static str) -> &'static str {
    match arg1 {
        Some("__brain") => "gait-brain",
        Some("__run-tool") => "gait-hand",
        Some("__daemon-exit") => "gait-daemon-exit",
        Some("__local-runner") => "gait-local-runner",
        _ => default,
    }
}

/// Puts the labels of a baggage header on `span`. `carrier::link_from` links a
/// span without adopting the linked span's labels; a span that stands for the
/// linked work (an answer, a daemon wake) adds them itself, here.
#[cfg(feature = "telemetry")]
pub fn adopt_labels(span: &tracing::Span, baggage: Option<&str>) {
    if let Some(baggage) = baggage {
        span.in_scope(|| concord_telemetry::context::add_from_baggage(baggage));
    }
}

#[cfg(feature = "telemetry")]
static GUARD: std::sync::Mutex<Option<concord_telemetry::Guard>> = std::sync::Mutex::new(None);

/// Initialises telemetry for this process and keeps its guard here, where
/// [`flush`] can reach it from code that ends the process with
/// `std::process::exit` (which skips destructors). A failed init is reported
/// on stderr as one JSON line and the process keeps running, so a monitor can
/// alert on the missing `ServiceStarted` event. `local_json` writes JSON log
/// lines to stderr too (`gait-acp` does; the CLI must not disturb its own
/// stderr protocol, and the brain's stdout is the Nerve).
pub fn init(service: &'static str, agent_id: Option<String>, local_json: bool) {
    #[cfg(feature = "telemetry")]
    {
        use concord_telemetry::{Config, LocalOutput, Principal, PrincipalKind};
        let mut config = Config::new(service, concord_telemetry::version!())
            .with_build_info(concord_telemetry::build_info!());
        config.local = if local_json { LocalOutput::Json } else { LocalOutput::Off };
        config.principal = agent_id.map(|id| Principal::new(PrincipalKind::Agent, id));
        match concord_telemetry::init(config) {
            Ok(guard) => {
                if let Ok(mut slot) = GUARD.lock() {
                    *slot = Some(guard);
                }
            }
            Err(error) => {
                #[allow(clippy::print_stderr)]
                {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "level": "error",
                            "code": "gait.telemetry.init_failed",
                            "service": service,
                            "message": error.to_string(),
                        })
                    );
                }
            }
        }
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = (service, agent_id, local_json);
}

/// Exports whatever is buffered and shuts the exporters down. The guard bounds
/// its own wait, so an unreachable collector cannot stall the caller. Call it
/// before sending the brain's `Terminated` signal and before every
/// `std::process::exit`; the spine kills the brain's process group right after
/// it sees `Terminated`. Safe to call twice.
pub fn flush() {
    #[cfg(feature = "telemetry")]
    {
        let guard = GUARD.lock().ok().and_then(|mut slot| slot.take());
        drop(guard);
    }
}

/// Flushes telemetry when dropped: hold it in `main` so a normal return exports
/// what is buffered. Paths that call `std::process::exit` call [`flush`] first.
pub struct FlushGuard;

impl Drop for FlushGuard {
    fn drop(&mut self) {
        flush();
    }
}

#[cfg(test)]
mod tests {
    use super::service_for_argv;

    #[test]
    fn each_role_subprocess_is_its_own_telemetry_service() {
        assert_eq!(service_for_argv(Some("__brain"), "gait"), "gait-brain");
        assert_eq!(service_for_argv(Some("__run-tool"), "gait"), "gait-hand");
        assert_eq!(service_for_argv(Some("__daemon-exit"), "gait"), "gait-daemon-exit");
        assert_eq!(service_for_argv(Some("__local-runner"), "gait"), "gait-local-runner");
        assert_eq!(service_for_argv(Some("run"), "gait-min"), "gait-min");
        assert_eq!(service_for_argv(None, "gait"), "gait");
    }
}

#[cfg(all(test, feature = "telemetry"))]
mod adopt_tests {
    #[test]
    fn every_entry_of_a_baggage_header_lands_on_the_span() {
        // One shared rule (context::add_from_baggage): any valid entry is a
        // label, unknown keys and empty values included.
        let _capture = concord_telemetry::testing::capture_exclusive();
        let span = tracing::info_span!("adopt-probe");
        super::adopt_labels(&span, Some("action_id=A1, request_id=mm-9;prop=1,other=x"));
        drop(span);
        let finished = concord_telemetry::testing::spans()
            .into_iter()
            .rev()
            .find(|s| s.name == "adopt-probe")
            .expect("span finished");
        let has = |key: &str, value: &str| {
            finished.attributes.iter().any(|kv| kv.key.as_str() == key && kv.value.to_string() == value)
        };
        assert!(has("action_id", "A1"), "{:?}", finished.attributes);
        assert!(has("request_id", "mm-9"));
        assert!(has("other", "x"));
    }

    #[test]
    fn no_baggage_adds_nothing() {
        let _capture = concord_telemetry::testing::capture_exclusive();
        let span = tracing::info_span!("adopt-none");
        super::adopt_labels(&span, None);
        drop(span);
    }
}
