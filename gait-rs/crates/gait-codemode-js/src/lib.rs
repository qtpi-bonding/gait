//! Intentionally metadata-only.
//!
//! rquickjs cannot cross-compile to `wasm32-unknown-unknown` (its C build
//! needs libc headers that target does not provide), so this crate does not
//! build the Code Mode guest. The real guest is the vendored QuickJS-WASI
//! artifact copied by `cargo xtask codemode-guest`; see
//! `vendor/quickjs-wasi/*/README.md` and `gait-rs/docs/architecture.md`.
