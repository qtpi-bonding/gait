//! The gait log format — generated proto types + wire plumbing.
//!
//! `proto/log.proto` + `proto/state.proto` are compiled by `build.rs` (prost +
//! pbjson) into `$OUT_DIR`. The module tree mirrors the proto package paths so
//! prost's cross-package references resolve. The wire is JSON (proto3 canonical
//! / camelCase) via pbjson — each `log.jsonl` line is one `gait::log::v1::Event`.
pub mod gait {
    pub mod log {
        // pbjson-generated code (included via include!), which we cannot edit.
        #[allow(clippy::large_enum_variant)] // generated prost code
        #[allow(clippy::useless_borrows_in_formatting)]
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.log.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.log.v1.serde.rs"));
        }
    }
    pub mod state {
        // pbjson-generated code (included via include!), which we cannot edit.
        #[allow(clippy::useless_borrows_in_formatting)]
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.state.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.state.v1.serde.rs"));
        }
    }
    pub mod workflow {
        // pbjson-generated code (included via include!), which we cannot edit.
        #[allow(clippy::large_enum_variant)] // generated prost code
        #[allow(clippy::useless_borrows_in_formatting)]
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.workflow.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.workflow.v1.serde.rs"));
        }
    }
    pub mod sandbox {
        // pbjson-generated code (included via include!), which we cannot edit.
        #[allow(clippy::large_enum_variant)] // generated prost code
        #[allow(clippy::useless_borrows_in_formatting)]
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.sandbox.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.sandbox.v1.serde.rs"));
        }
    }
}

pub mod envelope;
pub mod hash;
pub mod schema_doc;
pub mod textproto;
pub mod wire;
pub mod workflow_envelope;

#[cfg(test)]
mod tests;

/// Encoded `FileDescriptorSet` from build.rs, including imports and source info.
pub const DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/proto_descriptor.bin"));
