#![cfg(feature = "telemetry")]

#[test]
fn report_emits_one_coded_error_record_without_the_text() {
    let cap = concord_telemetry::testing::capture_exclusive();
    let error = gait_cli::cli::error::CliError::not_found("secret-token-xyz");
    gait_bin::report(&error);
    let records = cap.errors_where(|record| record.code == "gait.cli.not_found");
    assert_eq!(records.len(), 1);
    assert!(!records[0]
        .fields
        .values()
        .any(|value| format!("{value:?}").contains("secret-token-xyz")));
}
