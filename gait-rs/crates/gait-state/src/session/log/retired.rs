//! Event kinds that an older gait wrote and this version no longer reads.
//!
//! There is no migration: a log holding one of these names cannot be opened,
//! and the reader says so with a coded error (see
//! `SessionError::RetiredEventKind`) instead of a bare parse failure.

use crate::session::SessionError;

/// The retired oneof keys, in the camelCase the writer used and the
/// snake_case the reader also accepted.
const RETIRED_KINDS: [&str; 6] = [
    "daemonStart",
    "daemon_start",
    "daemonStop",
    "daemon_stop",
    "daemonUpdate",
    "daemon_update",
];

/// The retired kind the `payload` of this (already failed to parse) log line
/// carries, if it is one.
pub(super) fn retired_kind(line: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = value.get("payload")?.as_object()?;
    RETIRED_KINDS
        .iter()
        .copied()
        .find(|kind| payload.contains_key(*kind))
}

/// The error for a log line that did not parse: the coded retired-kind error
/// when the line carries a retired kind, the plain parse error otherwise.
/// `line_number` is 1-based.
pub(super) fn parse_failure(
    path: &std::path::Path,
    line_number: usize,
    line: &str,
    parse: serde_json::Error,
) -> SessionError {
    match retired_kind(line) {
        Some(kind) => SessionError::RetiredEventKind {
            path: path.display().to_string(),
            line: line_number,
            kind: String::from(kind),
        },
        None => SessionError::Parse(parse),
    }
}

#[cfg(test)]
mod tests {
    use super::retired_kind;

    #[test]
    fn a_retired_kind_is_named_from_the_payload_key() {
        let camel = r#"{"version":3,"payload":{"daemonUpdate":{"name":"build"}}}"#;
        assert_eq!(retired_kind(camel), Some("daemonUpdate"));
        let snake = r#"{"payload":{"daemon_stop":{}}}"#;
        assert_eq!(retired_kind(snake), Some("daemon_stop"));
    }

    #[test]
    fn other_lines_are_not_retired_kinds() {
        assert_eq!(retired_kind(r#"{"payload":{"nonsense":{}}}"#), None);
        assert_eq!(retired_kind(r#"{"payload":{"processUpdate":{}}}"#), None);
        assert_eq!(retired_kind("not json"), None);
    }
}
