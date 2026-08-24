use std::path::Path;

use gait_state::gait::log::v1::Event;

use crate::WatchError;

/// Read every *complete* (newline-terminated) event line from `path`.
///
/// Unlike `gait_state::session::log::read_events`, this tolerates a file
/// that's mid-write: a trailing line with no `\n` is treated as "not yet
/// fully written" and silently dropped, not as corruption. A complete line
/// (ends in `\n`) that fails to parse, or whose event version doesn't match,
/// is a real error.
///
/// A missing file yields an empty `Vec`, matching `read_events`'s own
/// behavior for a session that hasn't written anything yet.
pub fn read_complete_events(path: &Path) -> Result<Vec<Event>, WatchError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)?;

    // split('\n') on a newline-terminated string yields a trailing "" element;
    // drop it. On a non-newline-terminated string, the last element is the
    // partial trailing line — drop it too, since it isn't complete yet. Either
    // way, popping the last element is correct.
    let mut lines: Vec<&str> = raw.split('\n').collect();
    if !lines.is_empty() {
        lines.pop();
    }

    let mut events = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<Event>(line)?;
        if event.version != gait_state::envelope::LOG_VERSION {
            return Err(gait_state::session::SessionError::UnsupportedLogVersion {
                found: event.version,
                expected: gait_state::envelope::LOG_VERSION,
            }
            .into());
        }
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_raw(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    /// A real, correctly-serialized `Event` line, built through gait-state's own
    /// mint path rather than hand-typed JSON — the wire format is pbjson-generated
    /// camelCase over `proto/log.proto`'s actual fields (`contentHash`, `parents`,
    /// `ts`, ...), not something worth reproducing by hand and risking drift.
    fn valid_event_line() -> String {
        let body = gait_state::proto::events::session_start("s1".to_string(), None, None, false);
        let event =
            gait_state::envelope::mint(body, None, gait_state::envelope::EventScope::Session);
        serde_json::to_string(&event).unwrap()
    }

    #[test]
    fn empty_file_yields_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_raw(tmp.path(), "log.jsonl", "");
        let events = read_complete_events(&path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn missing_file_yields_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.jsonl");
        let events = read_complete_events(&path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn complete_valid_line_parses_successfully() {
        let tmp = tempfile::tempdir().unwrap();
        let complete = valid_event_line();
        let path = write_raw(tmp.path(), "log.jsonl", &format!("{complete}\n"));
        let events = read_complete_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn trailing_partial_line_is_dropped_not_errored() {
        let tmp = tempfile::tempdir().unwrap();
        let complete = valid_event_line();
        // A real prefix of valid JSON, with no trailing newline — simulates an
        // in-progress `writeln!` that hasn't finished yet.
        let partial_fragment = &complete[..complete.len() / 2];
        let contents = format!("{complete}\n{partial_fragment}");
        let path = write_raw(tmp.path(), "log.jsonl", &contents);
        let events = read_complete_events(&path).unwrap();
        assert_eq!(
            events.len(),
            1,
            "only the complete, newline-terminated line should parse"
        );
    }

    #[test]
    fn only_a_partial_first_line_yields_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        let complete = valid_event_line();
        let partial_fragment = &complete[..complete.len() / 2];
        let path = write_raw(tmp.path(), "log.jsonl", partial_fragment);
        let events = read_complete_events(&path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn malformed_complete_line_is_a_real_error() {
        let tmp = tempfile::tempdir().unwrap();
        // Ends in \n (complete) but is not valid JSON — a real corruption, not a partial write.
        let path = write_raw(tmp.path(), "log.jsonl", "not valid json at all\n");
        let result = read_complete_events(&path);
        assert!(
            result.is_err(),
            "a complete but malformed line must error, not be silently dropped"
        );
    }
}
