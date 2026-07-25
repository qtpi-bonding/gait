use crate::proto::gait::log::v1::event;

/// What begins the next turn. Either a new input event to append to the log
/// before folding (a `user_turn` from `--task`/a REPL line, or a reply body), or
/// a bare `Resume` — run a turn that folds the existing log with no new input
/// (e.g. `gait run` with no `--task`, or a harness re-invocation). `None` from
/// [`TurnSource::next`] means no more turns.
pub enum TurnStart {
    Input(Vec<event::Body>),
    Resume,
}

/// The port that yields turns to [`run_loop`]. **`?Send` by contract, not
/// omission.** The run loop is *awaited on the caller's own task*, never
/// `tokio::spawn`ed across threads, so no `Send` bound is needed anywhere in the
/// turn/workflow path: the REPL's view state is single-threaded (`Rc<RefCell>`),
/// and the workflow runner handles interactive nodes sequentially with automated
/// work (see `runner::handle_open_node`). **Invariant a maintainer must not
/// break:** do not spawn a `TurnSource`/`TurnDriver` onto another thread — drive
/// them inline. If you ever need cross-thread execution, revisit
/// `architecture-mapping/spec-05-interactive-suspension.md` first.
#[async_trait::async_trait(?Send)]
pub trait TurnSource {
    /// The next turn to run, or `None` at EOF (single-shot done / quit / disconnect).
    async fn next(&mut self) -> Option<TurnStart>;
}

/// CLI source: yields exactly one turn — `Input(user_turn)` when `--task` was
/// given, else `Resume` (fold from the log) — then `None`.
pub struct ArgSource {
    start: Option<TurnStart>,
}
impl ArgSource {
    /// `Some(body)` → one `Input` turn; `None` → one `Resume` turn.
    pub fn new(body: Option<event::Body>) -> Self {
        Self {
            start: Some(
                body.map(|b| TurnStart::Input(vec![b]))
                    .unwrap_or(TurnStart::Resume),
            ),
        }
    }
}
#[async_trait::async_trait(?Send)]
impl TurnSource for ArgSource {
    async fn next(&mut self) -> Option<TurnStart> {
        self.start.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::log::v1::{event, payload};

    #[tokio::test]
    async fn arg_source_yields_task_input_once_then_none() {
        let body = crate::proto::events::user_text("hi".into());
        let mut s = ArgSource::new(Some(body));
        match s.next().await.expect("first yield") {
            TurnStart::Input(ref bodies) => match &bodies[0] {
                event::Body::Payload(p) => {
                    assert!(matches!(p.kind, Some(payload::Kind::UserText(_))))
                }
                _ => panic!("expected Input(user_turn)"),
            },
            _ => panic!("expected Input(user_turn)"),
        }
        assert!(s.next().await.is_none(), "second next() must be None");
    }

    #[tokio::test]
    async fn arg_source_no_task_yields_resume_once_then_none() {
        let mut s = ArgSource::new(None);
        assert!(
            matches!(s.next().await, Some(TurnStart::Resume)),
            "no --task must still run one fold turn"
        );
        assert!(s.next().await.is_none(), "second next() must be None");
    }
}
