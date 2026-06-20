//! The gait log format — generated proto types + wire plumbing.
//!
//! `proto/log.proto` + `proto/state.proto` are compiled by `build.rs` (prost +
//! pbjson) into `$OUT_DIR`. The module tree mirrors the proto package paths so
//! prost's cross-package references resolve. The wire is JSON (proto3 canonical
//! / camelCase) via pbjson — each `log.jsonl` line is one `gait::log::v1::Event`.
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
    pub mod workflow {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.workflow.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.workflow.v1.serde.rs"));
        }
    }
}

pub mod envelope;
pub mod hash;
pub mod wire;

#[cfg(test)]
mod tests;
