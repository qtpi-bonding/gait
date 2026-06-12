//! Generated proto types — the log format's source of truth.
//!
//! `proto/log.proto` + `proto/state.proto` are compiled by `build.rs` (prost +
//! pbjson) into `$OUT_DIR`. The module tree mirrors the proto package paths
//! (`gait.log.v1` → [`gait::log::v1`], `gait.state.v1` → [`gait::state::v1`]) so
//! prost's cross-package references (state → log item types) resolve.
//!
//! The wire is JSON (proto3 canonical / camelCase) via pbjson, so each line of
//! `transcript.jsonl` is one [`gait::log::v1::Event`].
pub mod gait {
    pub mod log {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.log.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.log.v1.serde.rs"));
        }
    }
    pub mod state {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.state.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.state.v1.serde.rs"));
        }
    }
}

pub mod hash;
pub mod envelope;
pub mod wire;
pub mod events;
pub mod emitter;
pub use emitter::Emitter;

#[cfg(test)]
mod tests;
