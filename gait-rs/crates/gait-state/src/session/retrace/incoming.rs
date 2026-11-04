//! Model-facing text of a verified incoming message.
//!
//! The log keeps the structured fields of an attested `Message`
//! (`IncomingMessage`), not rendered text. Fold re-renders them here with the
//! renderer version recorded on each message, so a format change cannot
//! silently rewrite how old sessions replay. Rendering is delegated to
//! `message_attestation::render_message`, the one renderer shared with
//! structmuxd, which escapes every attribute and the body.
use std::sync::OnceLock;

use gait_proto::gait::log::v1::{Event, IncomingMessage};
use message_attestation::{MessageView, Trust, render_message};

/// The agent id this process runs as, for `to="you"` and `mentions-you`.
/// Installed once by the entry point that owns the identity; a second install
/// is ignored. Without one, no target or mention renders as "you".
static PROCESS_SELF_ID: OnceLock<String> = OnceLock::new();

pub fn install_process_self_id(id: impl Into<String>) {
    let _ = PROCESS_SELF_ID.set(id.into());
}

/// True when the message carries the structured fields of a verified Message.
pub(crate) fn is_structured(m: &IncomingMessage) -> bool {
    !m.message_id.is_empty()
}

/// The rendered text of a structured, non-rejected incoming message, or `None`
/// when the message cannot be rendered (no verified author on the event, or the
/// stored fields no longer pass the Message rules). A message that cannot be
/// rendered is left out of the model's context rather than shown unlabeled.
pub(crate) fn rendered(ev: &Event, m: &IncomingMessage) -> Option<String> {
    if m.rejected_provenance_claim || !is_structured(m) {
        return None;
    }
    let author = ev.provenance.iter().find_map(|p| p.author_id.as_deref())?;
    let view = MessageView {
        message_id: &m.message_id,
        author_id: author,
        to: &m.to,
        via: &m.via,
        reply_to: m.reply_to.as_deref(),
        references: &m.references,
        mentions: &m.mentions,
        body: &m.content,
    };
    let self_id = PROCESS_SELF_ID.get().map(String::as_str).unwrap_or("");
    match render_message(&view, self_id, Trust::Verified, m.render_version) {
        Ok(text) => Some(text),
        Err(error) => {
            eprintln!(
                "[gait] incoming message {:?} was not rendered into context: {error}",
                m.message_id
            );
            None
        }
    }
}
