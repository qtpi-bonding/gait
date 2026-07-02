//! gait-core's proto surface: the format lives in the `gait-proto` crate; this
//! module re-exports it so existing `crate::proto::…` paths keep resolving, and
//! adds the domain-coupled pieces (`events` constructors, the `Recorder` sink).
mod error;
pub use error::ProtoError;
pub use gait_proto::{envelope, gait, hash, wire, workflow_envelope};

use crate::proto::gait::log::v1::{event, payload, Event};

/// The substantive payload kind for an event, if it carries one.
pub(crate) fn event_kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}

pub mod emitter;
pub mod events;
pub use emitter::Recorder;
