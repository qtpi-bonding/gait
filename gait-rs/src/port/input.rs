use crate::proto::gait::log::v1::event;

#[async_trait::async_trait]
pub trait TurnSource {
    /// Next input event, or None at EOF (single-shot done / quit / disconnect).
    async fn next(&mut self) -> Option<event::Body>;
}

/// CLI source: yields a single pre-built `user_turn` body (from `--task`), then None.
/// `None` task (a `--auto` resume) yields nothing — the turn folds from the log.
pub struct ArgSource { body: Option<event::Body> }
impl ArgSource {
    pub fn new(body: Option<event::Body>) -> Self { Self { body } }
}
#[async_trait::async_trait]
impl TurnSource for ArgSource {
    async fn next(&mut self) -> Option<event::Body> { self.body.take() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::log::v1::{event, payload};

    #[tokio::test]
    async fn arg_source_yields_task_once_then_none() {
        // ArgSource::new(Some(body)) yields it once, then None.
        let body = crate::proto::events::user_turn("hi".into(), None, None);
        let mut s = ArgSource::new(Some(body));
        let first = s.next().await.expect("first yield");
        assert!(matches!(first, event::Body::Payload(p) if matches!(p.kind, Some(payload::Kind::UserTurn(_)))));
        assert!(s.next().await.is_none(), "second next() must be None");
    }

    #[tokio::test]
    async fn arg_source_no_task_is_none() {
        assert!(ArgSource::new(None).next().await.is_none());
    }
}
