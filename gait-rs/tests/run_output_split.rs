//! End-to-end check of the `gait run` output split (Phase 1):
//! the live event stream and errors go to **stderr**; the final turn result is
//! the **only** thing on **stdout**, as one proto `RunResult` line
//! (`{"complete":{…}}`) carrying `changes`. Error *events* must not flip the exit.

use gait_core::port::output::CliSink;
use gait_core::port::output::TurnSink;
use gait_core::proto::gait::log::v1::{event, stream_item, RunResult, StreamItem};
use gait_core::proto::{envelope, events};

fn item(body: event::Body) -> StreamItem {
    StreamItem {
        item: Some(stream_item::Item::Event(envelope::mint(
            body,
            None,
            envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        ))),
    }
}

#[test]
fn stdout_is_only_the_result_stream_goes_to_stderr() {
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let mut sink = CliSink::new(&mut out, &mut err, false);

    // Live stream: a tool_call (Trace) and an error event (Error) — both to stderr.
    sink.item(&item(events::tool_call(
        "c1".into(),
        "read".into(),
        &serde_json::json!({"path": "a.rs"}),
    )));
    sink.item(&item(events::error("boom".into(), "tool")));

    // Error EVENTS route to stderr but do NOT set `failed` (hard-failures-only policy).
    assert!(!sink.failed, "an error event must not flip the exit code");

    // The authoritative payload → stdout, exactly once.
    sink.result(&RunResult {
        run_id: "r1".into(),
        state: Some(
            gait_core::proto::gait::log::v1::run_result::State::Complete(
                gait_core::proto::gait::log::v1::RunComplete {
                    summary: Some(gait_core::proto::gait::log::v1::RunSummary {
                        changes: Some(gait_core::proto::gait::log::v1::ChangeSummary {
                            added: vec![gait_core::proto::gait::log::v1::FileChange {
                                path: "new.rs".into(),
                                ..Default::default()
                            }],
                            modified: vec![gait_core::proto::gait::log::v1::FileChange {
                                path: "a.rs".into(),
                                ..Default::default()
                            }],
                            file_count: 2,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
        ),
        ..Default::default()
    });

    let _ = sink; // release the &mut borrows on out/err
    let out_s = String::from_utf8(out).unwrap();
    let err_s = String::from_utf8(err).unwrap();

    // stdout: exactly one line, parsing as the proto RunResult `complete` state with changes.
    let lines: Vec<&str> = out_s.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "stdout must be exactly one line, got: {out_s:?}"
    );
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert!(
        v.get("complete").is_some(),
        "stdout must be a complete RunResult: {v}"
    );
    assert_eq!(
        v["complete"]["summary"]["changes"]["added"][0]["path"],
        "new.rs"
    );
    assert_eq!(
        v["complete"]["summary"]["changes"]["modified"][0]["path"],
        "a.rs"
    );

    // stderr: carries the live trace (the tool_call and the error), never the result.
    assert!(
        err_s.contains("read"),
        "tool_call should be on stderr: {err_s:?}"
    );
    assert!(
        err_s.contains("boom"),
        "error should be on stderr: {err_s:?}"
    );
    assert!(
        !err_s.contains("\"complete\""),
        "the result must not appear on stderr: {err_s:?}"
    );
}
