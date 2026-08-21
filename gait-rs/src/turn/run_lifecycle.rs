use std::collections::HashSet;

use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::{payload, Event};

#[derive(Debug, Clone)]
pub(crate) struct ActiveRun {
    pub run_id: String,
    pub index: u32,
    pub baseline_checkpoint: Option<String>,
    pub turns: Vec<pb::TurnResult>,
    pub unmatched_turn: Option<pb::TurnStart>,
    pub start_seq: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct StartedTurn {
    pub run: ActiveRun,
    pub turn_id: String,
    pub turn_index: u32,
    pub started_ms: u64,
}

pub(crate) fn begin_turn(
    log: &crate::turn::SessionLog,
    git: &crate::turn::SessionGit,
    codebase: &std::path::Path,
    trigger: Vec<pb::event::Body>,
    trigger_provenance: Option<pb::Provenance>,
) -> anyhow::Result<StartedTurn> {
    use crate::proto::envelope::EventScope;

    let mut events = log.events();
    if let Some(active) = latest_active_run(&events)? {
        if !active.can_resume() && (!active.turns.is_empty() || active.unmatched_turn.is_some()) {
            repair_incomplete_tail(log, git, codebase, &active)?;
            events = log.events();
        }
    }
    let active = latest_active_run(&events)?;
    let run = match active {
        Some(active) if active.can_resume() || active.turns.is_empty() => active,
        Some(active) => anyhow::bail!(
            "run {} has an incomplete lifecycle tail that must be repaired",
            active.run_id
        ),
        None => {
            let index = events
                .iter()
                .filter_map(|event| match crate::turn::event_kind(event) {
                    Some(payload::Kind::RunStart(start)) => Some(start.index),
                    _ => None,
                })
                .max()
                .map_or(0, |index| index + 1);
            let baseline = git
                .checkpoint(
                    codebase,
                    crate::git::checkpoint::CheckpointBoundary::RunBaseline { run: index },
                )
                .ok()
                .filter(|sha| !sha.is_empty());
            let run_id = uuid::Uuid::new_v4().to_string();
            let start = log.append_body(
                crate::proto::events::run_start(pb::RunStart {
                    run_id: run_id.clone(),
                    index,
                    baseline_checkpoint: baseline.clone(),
                }),
                EventScope::Run { run: index },
            )?;
            ActiveRun {
                run_id,
                index,
                baseline_checkpoint: baseline,
                turns: Vec::new(),
                unmatched_turn: None,
                start_seq: start.seq,
            }
        }
    };

    if run.unmatched_turn.is_some() {
        anyhow::bail!("run {} has an unmatched TurnStart", run.run_id);
    }
    let turn_index = run.next_turn_index();
    let turn_id = uuid::Uuid::new_v4().to_string();
    let turn_start = log.append_body(
        crate::proto::events::turn_start(pb::TurnStart {
            turn_id: turn_id.clone(),
            index: turn_index,
            trigger_event_id: None,
        }),
        EventScope::Turn {
            run: run.index,
            turn: turn_index,
        },
    )?;
    let mut trigger_id = None;
    let mut provenance = trigger_provenance;
    for body in trigger {
        let event = match provenance.take() {
            Some(p) => log.append_body_with_provenance(
                body,
                EventScope::Turn {
                    run: run.index,
                    turn: turn_index,
                },
                p,
            )?,
            None => log.append_body(
                body,
                EventScope::Turn {
                    run: run.index,
                    turn: turn_index,
                },
            )?,
        };
        trigger_id.get_or_insert(event.id);
    }
    if trigger_id.is_some() {
        // TurnStart is immutable, so trigger linkage is defined by adjacency and
        // scoped coordinates. The optional field is reserved for externally
        // pre-minted triggers.
    }
    Ok(StartedTurn {
        run,
        turn_id,
        turn_index,
        started_ms: turn_start.ts,
    })
}

fn repair_incomplete_tail(
    log: &crate::turn::SessionLog,
    git: &crate::turn::SessionGit,
    codebase: &std::path::Path,
    active: &ActiveRun,
) -> anyhow::Result<()> {
    use crate::proto::envelope::EventScope;
    use pb::turn_result::Outcome;

    let mut end_seq = log.last_event().map_or(active.start_seq, |event| event.seq);
    let final_outcome = if let Some(unmatched) = &active.unmatched_turn {
        let now = wall_ms();
        let result = pb::TurnResult {
            turn_id: unmatched.turn_id.clone(),
            index: unmatched.index,
            usage: Some(Default::default()),
            timing: Some(pb::ExecutionTiming {
                started_ms: now,
                ended_ms: Some(now),
                active_duration: Some(Default::default()),
            }),
            outcome: Some(Outcome::Failed(pb::TurnFailed {
                reason: "coordinator_recovery".into(),
                message: "recovered an unmatched TurnStart after coordinator restart".into(),
            })),
            ..Default::default()
        };
        end_seq = log
            .append_body(
                crate::proto::events::turn_result(result.clone()),
                EventScope::Turn {
                    run: active.index,
                    turn: unmatched.index,
                },
            )?
            .seq;
        result.outcome.unwrap()
    } else {
        active
            .turns
            .last()
            .and_then(|turn| turn.outcome.clone())
            .ok_or_else(|| anyhow::anyhow!("run {} has no recoverable tail", active.run_id))?
    };

    let checkpoint = git
        .checkpoint(
            codebase,
            crate::git::checkpoint::CheckpointBoundary::RunFinal { run: active.index },
        )
        .ok()
        .filter(|sha| !sha.is_empty());
    let dirty_files = crate::git::codebase::CodebaseRepo::dirty_files(
        &crate::git::codebase::GitCodebaseRepo,
        codebase,
    );
    let final_state = pb::RunEndState {
        codebase: Some(pb::CodebaseState {
            hash: crate::git::codebase::CodebaseRepo::head_hash(
                &crate::git::codebase::GitCodebaseRepo,
                codebase,
            )
            .unwrap_or_default(),
            dirty: !dirty_files.is_empty(),
            dirty_files,
        }),
        checkpoint: checkpoint.clone(),
    };
    let scope = EventScope::Run { run: active.index };
    match final_outcome {
        Outcome::Done(_) => {
            let changes = match (active.baseline_checkpoint.as_deref(), checkpoint.as_deref()) {
                (Some(baseline), Some(final_checkpoint)) => {
                    git.diff_between(baseline, final_checkpoint).ok().flatten()
                }
                _ => None,
            };
            let summary = crate::turn::run_summary::build(
                &log.events(),
                active.index,
                active.start_seq,
                end_seq,
                changes,
            );
            log.append_body(
                crate::proto::events::run_complete(pb::RunComplete {
                    final_state: Some(final_state),
                    summary: Some(summary),
                }),
                scope,
            )?;
        }
        Outcome::Cancelled(_) => {
            log.append_body(
                crate::proto::events::run_cancelled(pb::RunCancelled {
                    final_state: Some(final_state),
                }),
                scope,
            )?;
        }
        Outcome::Failed(_) => {
            log.append_body(
                crate::proto::events::run_failed(pb::RunFailed {
                    final_state: Some(final_state),
                }),
                scope,
            )?;
        }
        Outcome::Pause(_) => anyhow::bail!("paused runs are resumed, not repaired"),
    }
    Ok(())
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

impl ActiveRun {
    pub fn next_turn_index(&self) -> u32 {
        self.turns.len() as u32
    }

    pub fn can_resume(&self) -> bool {
        self.unmatched_turn.is_none()
            && matches!(
                self.turns.last().and_then(|turn| turn.outcome.as_ref()),
                Some(pb::turn_result::Outcome::Pause(_))
            )
    }
}

pub(crate) fn latest_active_run(events: &[Event]) -> anyhow::Result<Option<ActiveRun>> {
    let Some((start_event, start)) = events.iter().rev().find_map(|event| {
        let payload::Kind::RunStart(start) = crate::turn::event_kind(event)? else {
            return None;
        };
        Some((event, start))
    }) else {
        return Ok(None);
    };

    let run = start_event
        .run
        .ok_or_else(|| anyhow::anyhow!("RunStart {} has no run coordinate", start_event.id))?;
    if run != start.index {
        anyhow::bail!(
            "RunStart {} coordinate {} does not match index {}",
            start_event.id,
            run,
            start.index
        );
    }
    if start.run_id.is_empty() {
        anyhow::bail!("RunStart {} has an empty run_id", start_event.id);
    }

    let terminal = events.iter().any(|event| {
        event.run == Some(run)
            && matches!(
                crate::turn::event_kind(event),
                Some(payload::Kind::RunComplete(_))
                    | Some(payload::Kind::RunCancelled(_))
                    | Some(payload::Kind::RunFailed(_))
            )
    });
    if terminal {
        return Ok(None);
    }

    let mut turns = Vec::new();
    let mut starts = Vec::new();
    for event in events.iter().filter(|event| event.run == Some(run)) {
        match crate::turn::event_kind(event) {
            Some(payload::Kind::TurnStart(turn)) => starts.push(turn),
            Some(payload::Kind::TurnResult(turn)) => turns.push(turn),
            _ => {}
        }
    }
    turns.sort_by_key(|turn| turn.index);
    starts.sort_by_key(|turn| turn.index);
    for (expected, turn) in turns.iter().enumerate() {
        if turn.index != expected as u32 || turn.turn_id.is_empty() {
            anyhow::bail!("run {} has invalid durable turn sequence", start.run_id);
        }
    }
    for (expected, turn) in starts.iter().enumerate() {
        if turn.index != expected as u32 || turn.turn_id.is_empty() {
            anyhow::bail!("run {} has invalid TurnStart sequence", start.run_id);
        }
        if let Some(result) = turns.get(expected) {
            if result.turn_id != turn.turn_id {
                anyhow::bail!("TurnStart/TurnResult id mismatch at turn {expected}");
            }
        }
    }
    if starts.len() > turns.len() + 1 {
        anyhow::bail!(
            "run {} has multiple unmatched TurnStart events",
            start.run_id
        );
    }

    Ok(Some(ActiveRun {
        run_id: start.run_id,
        index: start.index,
        baseline_checkpoint: start.baseline_checkpoint,
        unmatched_turn: (starts.len() == turns.len() + 1).then(|| starts.last().unwrap().clone()),
        turns,
        start_seq: start_event.seq,
    }))
}

pub(crate) fn fold_run_result(events: &[Event], run: u32) -> anyhow::Result<pb::RunResult> {
    let (start_event, start) = events
        .iter()
        .find_map(|event| {
            if event.run != Some(run) {
                return None;
            }
            let payload::Kind::RunStart(start) = crate::turn::event_kind(event)? else {
                return None;
            };
            Some((event, start))
        })
        .ok_or_else(|| anyhow::anyhow!("run {run} has no RunStart"))?;

    let mut turns: Vec<pb::TurnResult> = events
        .iter()
        .filter(|event| event.run == Some(run))
        .filter_map(|event| match crate::turn::event_kind(event) {
            Some(payload::Kind::TurnResult(result)) => Some(result),
            _ => None,
        })
        .collect();
    turns.sort_by_key(|turn| turn.index);

    let terminal = events
        .iter()
        .filter(|event| event.run == Some(run))
        .find_map(|event| match crate::turn::event_kind(event) {
            Some(payload::Kind::RunComplete(result)) => {
                Some((event, pb::run_result::State::Complete(result)))
            }
            Some(payload::Kind::RunCancelled(result)) => {
                Some((event, pb::run_result::State::Cancelled(result)))
            }
            Some(payload::Kind::RunFailed(result)) => {
                Some((event, pb::run_result::State::Failed(result)))
            }
            _ => None,
        });
    let state = terminal
        .as_ref()
        .map(|(_, state)| state.clone())
        .unwrap_or_else(|| pb::run_result::State::Paused(pb::RunPaused {}));

    let usage = sum_run_usage(&turns);
    let active_duration = if turns.iter().all(|turn| turn.timing.is_some()) {
        sum_active_duration(turns.iter().map(|turn| turn.timing.as_ref().unwrap()))
    } else {
        None
    };
    let timing = Some(pb::ExecutionTiming {
        started_ms: start_event.ts,
        ended_ms: terminal.as_ref().map(|(event, _)| event.ts),
        active_duration,
    });
    let result = pb::RunResult {
        run_id: start.run_id,
        turns,
        usage,
        timing,
        state: Some(state),
    };
    validate_run_result(&result)?;
    Ok(result)
}

fn sum_run_usage(turns: &[pb::TurnResult]) -> Option<pb::RunTokenUsage> {
    let mut total = pb::RunTokenUsage::default();
    for turn in turns {
        let usage = turn.usage.as_ref()?;
        total.input += usage.input;
        total.output += usage.output;
        total.cache_read += usage.cache_read;
        total.cache_write += usage.cache_write;
    }
    Some(total)
}

fn sum_active_duration<'a>(
    timings: impl Iterator<Item = &'a pb::ExecutionTiming>,
) -> Option<pbjson_types::Duration> {
    let mut seconds = 0i64;
    let mut nanos = 0i64;
    for timing in timings {
        let duration = timing.active_duration.as_ref()?;
        seconds += duration.seconds;
        nanos += i64::from(duration.nanos);
    }
    seconds += nanos / 1_000_000_000;
    nanos %= 1_000_000_000;
    Some(pbjson_types::Duration {
        seconds,
        nanos: nanos as i32,
    })
}

pub(crate) fn validate_run_result(result: &pb::RunResult) -> anyhow::Result<()> {
    if result.run_id.is_empty() || result.turns.is_empty() {
        anyhow::bail!("RunResult requires a run_id and at least one turn");
    }
    let mut ids = HashSet::new();
    for (expected, turn) in result.turns.iter().enumerate() {
        if turn.index != expected as u32 || turn.turn_id.is_empty() || !ids.insert(&turn.turn_id) {
            anyhow::bail!("turn ids and indices must be non-empty, unique, and contiguous");
        }
        let outcome = turn
            .outcome
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("turn {} has no outcome", turn.index))?;
        if matches!(
            outcome,
            pb::turn_result::Outcome::Done(_) | pb::turn_result::Outcome::Pause(_)
        ) && turn.steps.is_empty()
        {
            anyhow::bail!("done and paused turns require at least one step");
        }
        for (step_index, step) in turn.steps.iter().enumerate() {
            if step.index != step_index as u32 {
                anyhow::bail!("step indices must be contiguous within a turn");
            }
        }
        if expected + 1 < result.turns.len()
            && !matches!(outcome, pb::turn_result::Outcome::Pause(_))
        {
            anyhow::bail!("every non-final turn must be paused");
        }
    }

    let final_outcome = result
        .turns
        .last()
        .and_then(|turn| turn.outcome.as_ref())
        .unwrap();
    let valid_state = matches!(
        (&result.state, final_outcome),
        (
            Some(pb::run_result::State::Paused(_)),
            pb::turn_result::Outcome::Pause(_)
        ) | (
            Some(pb::run_result::State::Complete(_)),
            pb::turn_result::Outcome::Done(_)
        ) | (
            Some(pb::run_result::State::Cancelled(_)),
            pb::turn_result::Outcome::Cancelled(_)
        ) | (
            Some(pb::run_result::State::Failed(_)),
            pb::turn_result::Outcome::Failed(_)
        )
    );
    if !valid_state {
        anyhow::bail!("run state does not match the final turn outcome");
    }

    let expected_usage = sum_run_usage(&result.turns);
    if result.usage != expected_usage {
        anyhow::bail!("run usage does not equal the sum of turn usage");
    }
    Ok(())
}

#[cfg(test)]
fn scope_is_valid(event: &Event) -> bool {
    let expected = match crate::turn::event_kind(event) {
        Some(payload::Kind::SessionStart(_))
        | Some(payload::Kind::Summary(_))
        | Some(payload::Kind::Registry(_))
        | Some(payload::Kind::Toggle(_))
        | Some(payload::Kind::Inference(_))
        | Some(payload::Kind::Mode(_))
        | Some(payload::Kind::MessageAccept(_)) => 0,
        Some(payload::Kind::RunStart(_))
        | Some(payload::Kind::RunComplete(_))
        | Some(payload::Kind::RunCancelled(_))
        | Some(payload::Kind::RunFailed(_)) => 1,
        Some(payload::Kind::TurnStart(_))
        | Some(payload::Kind::TurnResult(_))
        | Some(payload::Kind::UserText(_))
        | Some(payload::Kind::ClarifyReply(_))
        | Some(payload::Kind::InformReply(_))
        | Some(payload::Kind::PermissionsReply(_))
        | Some(payload::Kind::SandboxUnlockReply(_)) => 2,
        Some(_) => 3,
        None => return false,
    };
    match expected {
        0 => event.run.is_none() && event.turn.is_none() && event.step.is_none(),
        1 => event.run.is_some() && event.turn.is_none() && event.step.is_none(),
        2 => event.run.is_some() && event.turn.is_some() && event.step.is_none(),
        _ => event.run.is_some() && event.turn.is_some() && event.step.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::envelope::{mint, EventScope};
    use tempfile::TempDir;

    fn push(events: &mut Vec<Event>, body: pb::event::Body, scope: EventScope) {
        events.push(mint(body, events.last(), scope));
    }

    fn turn(index: u32, outcome: pb::turn_result::Outcome, usage: u64) -> pb::TurnResult {
        pb::TurnResult {
            turn_id: format!("t{index}"),
            index,
            steps: vec![pb::StepResult {
                index: 0,
                ..Default::default()
            }],
            usage: Some(pb::TurnTokenUsage {
                output: usage,
                ..Default::default()
            }),
            timing: Some(pb::ExecutionTiming {
                active_duration: Some(pbjson_types::Duration {
                    seconds: 1,
                    nanos: 0,
                }),
                ..Default::default()
            }),
            outcome: Some(outcome),
            ..Default::default()
        }
    }

    #[test]
    fn paused_then_resumed_complete_folds_one_aggregate() {
        let mut events = Vec::new();
        push(
            &mut events,
            crate::proto::events::run_start(pb::RunStart {
                run_id: "run-a".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        );
        push(
            &mut events,
            crate::proto::events::turn_start(pb::TurnStart {
                turn_id: "t0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Turn { run: 0, turn: 0 },
        );
        push(
            &mut events,
            crate::proto::events::turn_result(turn(
                0,
                pb::turn_result::Outcome::Pause(pb::TurnPause {
                    reason: pb::PauseReason::Clarify as i32,
                    request_event_id: "q1".into(),
                }),
                3,
            )),
            EventScope::Turn { run: 0, turn: 0 },
        );
        push(
            &mut events,
            crate::proto::events::turn_start(pb::TurnStart {
                turn_id: "t1".into(),
                index: 1,
                ..Default::default()
            }),
            EventScope::Turn { run: 0, turn: 1 },
        );
        push(
            &mut events,
            crate::proto::events::turn_result(turn(
                1,
                pb::turn_result::Outcome::Done(Default::default()),
                5,
            )),
            EventScope::Turn { run: 0, turn: 1 },
        );
        push(
            &mut events,
            crate::proto::events::run_complete(Default::default()),
            EventScope::Run { run: 0 },
        );

        let result = fold_run_result(&events, 0).unwrap();
        assert_eq!(result.turns.len(), 2);
        assert_eq!(result.usage.unwrap().output, 8);
        assert_eq!(result.timing.unwrap().active_duration.unwrap().seconds, 2);
        assert!(matches!(
            result.state,
            Some(pb::run_result::State::Complete(_))
        ));
    }

    #[test]
    fn unmatched_turn_start_is_recoverable_tail_state() {
        let mut events = Vec::new();
        push(
            &mut events,
            crate::proto::events::run_start(pb::RunStart {
                run_id: "run-a".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        );
        push(
            &mut events,
            crate::proto::events::turn_start(pb::TurnStart {
                turn_id: "t0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Turn { run: 0, turn: 0 },
        );
        let active = latest_active_run(&events).unwrap().unwrap();
        assert_eq!(active.unmatched_turn.as_ref().unwrap().turn_id, "t0");
        assert!(!active.can_resume());
    }

    #[test]
    fn scope_validation_rejects_partial_or_over_scoped_lifecycle_events() {
        let run = mint(
            crate::proto::events::run_start(Default::default()),
            None,
            EventScope::Run { run: 0 },
        );
        let turn = mint(
            crate::proto::events::turn_start(Default::default()),
            None,
            EventScope::Turn { run: 0, turn: 0 },
        );
        let step = mint(
            crate::proto::events::tool_call("c".into(), "read".into(), &serde_json::json!({})),
            None,
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        assert!(scope_is_valid(&run));
        assert!(scope_is_valid(&turn));
        assert!(scope_is_valid(&step));

        let bad_run = mint(
            crate::proto::events::run_start(Default::default()),
            None,
            EventScope::Turn { run: 0, turn: 0 },
        );
        let bad_step = mint(
            crate::proto::events::tool_call("c".into(), "read".into(), &serde_json::json!({})),
            None,
            EventScope::Turn { run: 0, turn: 0 },
        );
        assert!(!scope_is_valid(&bad_run));
        assert!(!scope_is_valid(&bad_step));
    }

    #[test]
    fn begin_turn_repairs_unmatched_start_then_opens_a_new_run() {
        let log = crate::turn::SessionLog::ephemeral(Vec::new());
        log.append_body(
            crate::proto::events::run_start(pb::RunStart {
                run_id: "run-a".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        )
        .unwrap();
        log.append_body(
            crate::proto::events::turn_start(pb::TurnStart {
                turn_id: "t0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Turn { run: 0, turn: 0 },
        )
        .unwrap();

        let codebase = TempDir::new().unwrap();
        let started = begin_turn(
            &log,
            &crate::turn::SessionGit::null(),
            codebase.path(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(started.run.index, 1);
        assert_eq!(started.turn_index, 0);
        let events = log.events();
        assert!(events.iter().any(|event| {
            event.run == Some(0)
                && matches!(
                    crate::turn::event_kind(event),
                    Some(payload::Kind::TurnResult(result))
                        if matches!(result.outcome, Some(pb::turn_result::Outcome::Failed(_)))
                )
        }));
        assert!(events.iter().any(|event| {
            event.run == Some(0)
                && matches!(
                    crate::turn::event_kind(event),
                    Some(payload::Kind::RunFailed(_))
                )
        }));
    }

    #[test]
    fn begin_turn_repairs_missing_terminal_then_opens_a_new_run() {
        let log = crate::turn::SessionLog::ephemeral(Vec::new());
        log.append_body(
            crate::proto::events::run_start(pb::RunStart {
                run_id: "run-a".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        )
        .unwrap();
        log.append_body(
            crate::proto::events::turn_start(pb::TurnStart {
                turn_id: "t0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Turn { run: 0, turn: 0 },
        )
        .unwrap();
        log.append_body(
            crate::proto::events::turn_result(turn(
                0,
                pb::turn_result::Outcome::Done(Default::default()),
                1,
            )),
            EventScope::Turn { run: 0, turn: 0 },
        )
        .unwrap();

        let codebase = TempDir::new().unwrap();
        let started = begin_turn(
            &log,
            &crate::turn::SessionGit::null(),
            codebase.path(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(started.run.index, 1);
        assert!(log.events().iter().any(|event| {
            event.run == Some(0)
                && matches!(
                    crate::turn::event_kind(event),
                    Some(payload::Kind::RunComplete(_))
                )
        }));
    }

    #[test]
    fn begin_turn_resumes_paused_run_without_a_terminal() {
        let log = crate::turn::SessionLog::ephemeral(Vec::new());
        log.append_body(
            crate::proto::events::run_start(pb::RunStart {
                run_id: "run-a".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        )
        .unwrap();
        log.append_body(
            crate::proto::events::turn_start(pb::TurnStart {
                turn_id: "t0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Turn { run: 0, turn: 0 },
        )
        .unwrap();
        log.append_body(
            crate::proto::events::turn_result(turn(
                0,
                pb::turn_result::Outcome::Pause(pb::TurnPause {
                    reason: pb::PauseReason::Clarify as i32,
                    request_event_id: "question".into(),
                }),
                1,
            )),
            EventScope::Turn { run: 0, turn: 0 },
        )
        .unwrap();

        let codebase = TempDir::new().unwrap();
        let started = begin_turn(
            &log,
            &crate::turn::SessionGit::null(),
            codebase.path(),
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(started.run.index, 0);
        assert_eq!(started.turn_index, 1);
        assert!(!log.events().iter().any(|event| {
            event.run == Some(0)
                && matches!(
                    crate::turn::event_kind(event),
                    Some(payload::Kind::RunComplete(_))
                        | Some(payload::Kind::RunCancelled(_))
                        | Some(payload::Kind::RunFailed(_))
                )
        }));
    }
}
