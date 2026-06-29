mod error;
pub use error::SessionError;

pub mod completion;
pub mod daemons;
pub mod ingestor;
pub mod log;
pub mod retrace;
pub use ingestor::Ingestor;
pub mod session_meta;
pub mod session_obj;
pub use log::*;
pub use retrace::messages_from_path;
pub use session_meta::{from_events as session_meta_from_events, read_session_meta, SessionMeta};
pub use session_obj::Session;

/// Generate a fresh session id: `s` + first 8 hex chars of a v4 UUID.
pub fn new_session_id() -> String {
    format!(
        "s{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_has_expected_format() {
        let id = new_session_id();
        assert!(id.starts_with('s'), "got: {}", id);
        assert_eq!(id.len(), 9, "got: {}", id);
    }
}
