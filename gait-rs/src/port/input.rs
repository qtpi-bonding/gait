use crate::proto::gait::log::v1::{Provenance, event};

/// Core representation of one input message after transport parsing.
/// Input provenance is attested only (a verified author id from ingress);
/// direct signatures are made later, over the durable event, by the agent
/// itself. `rejected_provenance_claim` preserves the distinction between
/// absent metadata and an invalid claim.
#[derive(Debug, Clone)]
pub struct InputEnvelope {
    pub body: event::Body,
    pub provenance: Option<Provenance>,
    pub rejected_provenance_claim: bool,
}

#[derive(Debug, Clone)]
pub enum ProvenanceClaim {
    Absent,
    Rejected,
}

impl InputEnvelope {
    pub fn new(body: event::Body, provenance: Option<Provenance>) -> Self {
        Self {
            body,
            provenance,
            rejected_provenance_claim: false,
        }
    }

    pub fn rejected_claim(body: event::Body) -> Self {
        Self {
            body,
            provenance: None,
            rejected_provenance_claim: true,
        }
    }

    pub fn with_claim(body: event::Body, claim: ProvenanceClaim) -> Self {
        match claim {
            ProvenanceClaim::Absent => Self::new(body, None),
            ProvenanceClaim::Rejected => Self::rejected_claim(body),
        }
    }
}

impl From<(event::Body, Option<Provenance>)> for InputEnvelope {
    fn from((body, provenance): (event::Body, Option<Provenance>)) -> Self {
        Self::new(body, provenance)
    }
}

/// Confirmation run once an `Input` batch is durable: after every message of the
/// batch is appended to the log. A failure is logged and never removes the
/// appended events. If the append itself fails, the closure is dropped unrun.
pub type OnDurable = Box<dyn FnOnce() -> anyhow::Result<()> + Send>;

/// What the session source wants core to do next.
///
/// `Input` appends bodies and then runs a model turn. `SessionUpdate` appends
/// durable session-control bodies without launching a model turn. `Resume`
/// runs a turn that folds the existing log with no new input. `None` from
/// [`TurnSource::next`] means no more work.
pub enum TurnStart {
    Input {
        /// One core input per message; a batch preserves message/provenance
        /// pairing and records malformed claims for later log folding.
        bodies: Vec<InputEnvelope>,
        on_durable: Option<OnDurable>,
    },
    SessionUpdate(Vec<event::Body>),
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
                body.map(|b| TurnStart::Input {
                    bodies: vec![(b, None).into()],
                    on_durable: None,
                })
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
            TurnStart::Input { ref bodies, .. } => match &bodies[0].body {
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
