//! Compatibility entry point for gait-core integration tests.
//!
//! The implementation is owned by `gait-testkit`; this target exists only so
//! Cargo exposes `CARGO_BIN_EXE_mock_brain` to gait-core's legacy tests.
mod implementation {
    include!("../../crates/gait-testkit/src/mock_brain.rs");
}

fn main() {
    implementation::run();
}
