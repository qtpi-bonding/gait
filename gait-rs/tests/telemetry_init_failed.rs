//! A failed telemetry init is reported through the shared one-line report
//! (`concord_telemetry::report_init_failure`): code `telemetry_init_failed`,
//! no gait-specific code. Its own test binary, because the subscriber is
//! global; the second `init` in a child process is the failure.
#![cfg(feature = "telemetry")]

const CHILD: &str = "GAIT_TELEMETRY_INIT_FAILED_CHILD";

#[test]
fn second_init_reports_the_shared_init_failure_line() {
    if std::env::var_os(CHILD).is_some() {
        gait_core::telemetry::init("gait-init-failed-test", None, false);
        gait_core::telemetry::init("gait-init-failed-test", None, false);
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "second_init_reports_the_shared_init_failure_line",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|line| line.starts_with("{\"level\""))
        .unwrap_or_else(|| panic!("no init-failure line on stderr: {stderr}"));
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(value["level"], "error");
    assert_eq!(value["code"], "telemetry_init_failed", "{line}");
    assert_eq!(value["service"], "gait-init-failed-test");
    assert!(
        value["message"].as_str().is_some_and(|m| !m.is_empty()),
        "{line}"
    );
}
