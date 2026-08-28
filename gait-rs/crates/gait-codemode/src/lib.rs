//! Native host boundary for Code Mode.
//!
//! `run()` drives the pinned QuickJS-WASI guest artifact directly through
//! Wasmtime -- see `quickjs.rs` for the actual evaluator (C-ABI marshaling,
//! the `host_call` bridge, and the promise/job-queue driving loop).

use async_trait::async_trait;
use serde_json::Value;
use std::borrow::Cow;
use thiserror::Error;

mod quickjs;
pub mod reference;
pub use reference::{render_reference, ToolReference};

pub const HOST_ABI_MODULE: &str = "env";
pub const HOST_ABI_IMPORT: &str = "host_call";

/// Imports required by the fixed, checked-in guest artifact
/// (`vendor/quickjs-wasi/3.6.0/quickjs.wasm`; see that directory's README).
/// This is the artifact's actual minimal import surface: `env.host_*` calls
/// are QuickJS-NG's own async/module/interrupt hooks (not ambient host
/// capability), and the `wasi_snapshot_preview1` functions are what
/// QuickJS-NG's libc needs for allocation/clock/random plumbing. Gait links
/// every one of these imports to inert, capability-denying stubs at
/// instantiation time — none of them are backed by real filesystem, clock,
/// random, network, or process access. That link-time denial, not the
/// absence of these import *names* from the binary, is what satisfies "no
/// WASI implementation... is granted".
pub const QUICKJS_IMPORTS: &[(&str, &str)] = &[
    ("env", "host_get_timezone_offset"),
    ("env", "host_interrupt"),
    ("env", "host_promise_rejection"),
    ("env", "host_module_normalize"),
    ("env", "host_module_load"),
    ("env", "host_call"),
    ("wasi_snapshot_preview1", "clock_time_get"),
    ("wasi_snapshot_preview1", "fd_close"),
    ("wasi_snapshot_preview1", "fd_fdstat_get"),
    ("wasi_snapshot_preview1", "fd_seek"),
    ("wasi_snapshot_preview1", "fd_write"),
    ("wasi_snapshot_preview1", "random_get"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub timeout_ms: u64,
    pub max_memory_bytes: usize,
    pub max_host_calls: u32,
    pub max_output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_host_calls: 128,
            max_output_bytes: 4 * 1024 * 1024,
        }
    }
}

#[async_trait]
pub trait CapabilityHost: Send + Sync {
    async fn call(&self, namespace: &str, method: &str, args: Value) -> Result<Value, HostError>;
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiSurface {
    pub namespaces: Vec<String>,
}
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HostError {
    #[error("capability {namespace}.{method} is unavailable")]
    Unavailable { namespace: String, method: String },
    #[error("host capability failed: {0}")]
    Failed(String),
}
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CodeModeError {
    #[error("Code Mode host error: {0}")]
    Host(#[from] HostError),
    #[error("Code Mode input limit exceeded")]
    InputLimit,
    #[error("Code Mode output limit exceeded")]
    OutputLimit,
    #[error("Code Mode host-call limit exceeded")]
    HostCallLimit,
    #[error("Code Mode memory limit exceeded")]
    MemoryLimit,
    #[error("Code Mode timed out")]
    Timeout,
    #[error("Code Mode runtime error: {0}")]
    Runtime(String),
}

/// Evaluate `source` as JavaScript inside the pinned QuickJS-WASI guest,
/// bridging `gait.<namespace>.<method>(args)` calls to `host`.
pub async fn run(
    source: &str,
    api: ApiSurface,
    host: &dyn CapabilityHost,
    limits: Limits,
) -> Result<Value, CodeModeError> {
    quickjs::eval(source, api, host, limits, GuestArtifact::embedded().bytes()).await
}

/// The pinned bytes are retained for ABI inspection and adapter development;
/// they are not executable through this crate's public evaluator API.
#[derive(Clone, Copy, Debug)]
pub struct GuestArtifact {
    bytes: &'static [u8],
}
impl GuestArtifact {
    pub const fn embedded() -> Self {
        Self {
            bytes: include_bytes!("../guest/gait_codemode_js.wasm"),
        }
    }
    pub const fn bytes(self) -> &'static [u8] {
        self.bytes
    }
    pub fn module(&self, engine: &wasmtime::Engine) -> Result<wasmtime::Module, wasmtime::Error> {
        wasmtime::Module::new(engine, self.bytes)
    }
    pub fn imports(&self) -> Result<Vec<(String, String)>, wasmtime::Error> {
        let engine = wasmtime::Engine::default();
        Ok(self
            .module(&engine)?
            .imports()
            .map(|i| (i.module().to_owned(), i.name().to_owned()))
            .collect())
    }
}

pub fn bounded_output(value: impl Into<Vec<u8>>, limits: Limits) -> Result<Vec<u8>, &'static str> {
    let value = value.into();
    if value.len() > limits.max_output_bytes {
        Err("Code Mode output limit exceeded")
    } else {
        Ok(value)
    }
}
pub fn abi_name() -> Cow<'static, str> {
    Cow::Borrowed(HOST_ABI_IMPORT)
}
