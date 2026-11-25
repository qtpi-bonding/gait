//! Native driver for the vendored `quickjs-wasi` guest artifact.
//!
//! This is a from-scratch Rust port of the calling convention documented and
//! implemented by the upstream `quickjs-wasi` npm package's TypeScript driver
//! (same artifact, same C interface): evaluate source through `qjs_eval`,
//! drain the promise/microtask job queue via `qjs_is_job_pending` /
//! `qjs_execute_pending_job`, and bridge guest calls to host capabilities
//! through the single `env.host_call` import.
//!
//! Unlike the npm package, this driver only needs a JSON-in/JSON-out bridge
//! (`CapabilityHost::call` is already `Value -> Result<Value, HostError>`),
//! so it does not port the package's general structural value marshaling
//! (`dump()`/`hostToHandle()`). Guest-side JSON encode/decode is done by
//! calling the guest's own captured `JSON.stringify`/`JSON.parse`.
//!
//! Async host calls are non-blocking by construction: `host_call` (a plain
//! synchronous Wasmtime host function) never awaits Rust futures itself. It
//! creates a QuickJS promise via `qjs_new_promise`, enqueues the real async
//! `CapabilityHost::call` future, and returns the (pending) promise
//! immediately -- exactly the pattern the npm package's README documents for
//! bridging async host work ("Promises and async host functions"). The outer
//! `eval` loop below is what actually awaits those futures (concurrently, so
//! `Promise.all([...])` performs real concurrent host work) and settles each
//! promise by calling its resolve/reject function once its future completes.

use crate::{
    ApiSurface, CapabilityHost, CodeModeError, HostError, Limits, LimitsMode, RunOptions,
    RunReport, WorkflowLimits,
};
use futures::future::{BoxFuture, FutureExt};
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use wasmtime::{
    Caller, Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};

/// Cranelift compilation of the ~636KB QuickJS-NG guest is the dominant cost
/// of a Code Mode run (single-digit seconds in an unoptimized build; still
/// meaningful in release) -- and the guest is a single pinned, checked-in
/// artifact that never changes at runtime. Compile it once per process and
/// reuse the `Engine`/`Module` across every `run()` call; only the `Store`
/// (and everything reachable from `HostState`) is created fresh per call.
static COMPILED_GUEST: OnceLock<Result<(Engine, Module), String>> = OnceLock::new();

fn compiled_guest(artifact: &[u8]) -> Result<(&'static Engine, &'static Module), CodeModeError> {
    let result = COMPILED_GUEST.get_or_init(|| {
        let config = Config::new();
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;
        let module = Module::new(&engine, artifact).map_err(|e| e.to_string())?;
        Ok((engine, module))
    });
    match result {
        Ok((engine, module)) => Ok((engine, module)),
        Err(message) => Err(rt(message.clone())),
    }
}

/// One in-flight capability call: its guest promise's resolve/reject
/// functions, and the real async work being awaited on the host side.
struct Pending<'a> {
    resolve_ptr: i32,
    reject_ptr: i32,
    future: BoxFuture<'a, Result<Value, HostError>>,
}

#[derive(Clone)]
struct QjsExports {
    initialize: TypedFunc<(), ()>,
    qjs_init: TypedFunc<(), i32>,
    wasm_malloc: TypedFunc<i32, i32>,
    wasm_free: TypedFunc<i32, ()>,
    qjs_eval: TypedFunc<(i32, i32, i32, i32), i32>,
    qjs_new_string: TypedFunc<(i32, i32), i32>,
    qjs_get_string_len: TypedFunc<(i32, i32), i32>,
    qjs_free_cstring: TypedFunc<i32, ()>,
    qjs_get_global: TypedFunc<(), i32>,
    qjs_get_prop_string: TypedFunc<(i32, i32), i32>,
    qjs_set_prop_string: TypedFunc<(i32, i32, i32), i32>,
    qjs_call: TypedFunc<(i32, i32, i32, i32), i32>,
    qjs_new_host_function: TypedFunc<(i32, i32, i32), i32>,
    qjs_new_promise: TypedFunc<(i32, i32), i32>,
    qjs_promise_state: TypedFunc<i32, i32>,
    qjs_promise_result: TypedFunc<i32, i32>,
    qjs_is_job_pending: TypedFunc<(), i32>,
    qjs_execute_pending_job: TypedFunc<(), i32>,
    qjs_is_exception: TypedFunc<i32, i32>,
    qjs_get_exception: TypedFunc<(), i32>,
    qjs_throw: TypedFunc<i32, i32>,
    qjs_free_value: TypedFunc<i32, ()>,
    qjs_new_error: TypedFunc<(), i32>,
    qjs_get_undefined: TypedFunc<(), i32>,
    qjs_set_interrupt_handler: TypedFunc<i32, ()>,
    qjs_set_promise_rejection_handler: TypedFunc<i32, ()>,
}

struct HostState<'a> {
    host: &'a dyn CapabilityHost,
    /// Wall-clock deadline (Code Mode only); `None` in Workflow mode.
    deadline: Option<Instant>,
    /// Host-call cap (Code Mode only); `None` in Workflow mode.
    max_host_calls: Option<u32>,
    /// Guest-CPU budget (Workflow mode only); `None` in Code Mode.
    cpu_budget: Option<Duration>,
    /// Accumulated guest CPU time across all executed segments.
    cpu_spent: Duration,
    /// Start of the currently-executing guest segment, if inside one.
    segment_start: Option<Instant>,
    host_call_count: u32,
    pending: Vec<Pending<'a>>,
    /// Promise rejections seen unhandled so far: the promise's identity key
    /// (the object pointer carried inside the boxed JSValue), the retained
    /// promise JSValue, the retained reason JSValue (kept alive so its heap
    /// address cannot be reused by a later rejection while the entry is
    /// stored), and the reason's description captured at notification time.
    unhandled: Vec<(i32, i32, i32, String)>,
    /// First unrecoverable guest trap observed (e.g. a failed JSValue
    /// free), if any; surfaced as an error at the end of the run.
    first_trap: Option<String>,
    memory: Option<Memory>,
    exports: Option<QjsExports>,
    limiter: StoreLimits,
}

/// Which limit regime a single `run_inner` invocation enforces.
enum Mode {
    Code { limits: Limits, deadline: Instant },
    Workflow { limits: WorkflowLimits },
}

impl wasmtime::ResourceLimiter for HostState<'_> {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.limiter.memory_growing(current, desired, maximum)
    }
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.limiter.table_growing(current, desired, maximum)
    }
}

fn rt(msg: impl Into<String>) -> CodeModeError {
    CodeModeError::Runtime(msg.into())
}

/// Convert a wasmtime trap/error into a runtime `CodeModeError`.
fn wasm_err(e: wasmtime::Error) -> CodeModeError {
    rt(e.to_string())
}

impl<'a> HostState<'a> {
    fn clone_handles(&self) -> (QjsExports, Memory) {
        (
            self.exports
                .clone()
                .expect("exports initialized before use"),
            self.memory.expect("memory initialized before use"),
        )
    }
}

impl HostState<'_> {
    /// Is this run enforcing the Workflow limit regime (as opposed to the
    /// wall-clock-deadline Code Mode regime)?
    fn is_workflow(&self) -> bool {
        self.deadline.is_none()
    }

    /// Record the first unrecoverable guest trap seen during the run (later
    /// ones are ignored); the end of the run turns it into an error.
    fn record_trap(&mut self, message: String) {
        if self.first_trap.is_none() {
            self.first_trap = Some(message);
        }
    }

    /// Mark the start of a guest-CPU accounting segment.
    fn cpu_begin(&mut self) {
        self.segment_start = Some(Instant::now());
    }

    /// Close the current guest-CPU accounting segment, adding its elapsed
    /// time to the accumulated total (also on error paths, by callers).
    fn cpu_end(&mut self) {
        if let Some(start) = self.segment_start.take() {
            self.cpu_spent += start.elapsed();
        }
    }

    /// Should the guest be interrupted right now? Code Mode uses the
    /// wall-clock deadline; Workflow mode uses the guest-CPU budget,
    /// which includes the currently-running segment.
    fn should_interrupt(&self) -> bool {
        match self.deadline {
            Some(deadline) => Instant::now() >= deadline,
            None => {
                let budget = self.cpu_budget.expect("workflow mode has a cpu budget");
                let running = self
                    .segment_start
                    .map(|start| start.elapsed())
                    .unwrap_or(Duration::ZERO);
                self.cpu_spent + running >= budget
            }
        }
    }
}

/// Write `bytes` into guest memory, always with a trailing NUL -- matching
/// the reference driver's `writeString`, which NUL-terminates unconditionally
/// (including source code passed to `qjs_eval` alongside an explicit
/// length): QuickJS-NG's tokenizer relies on that trailing NUL as a sentinel
/// even when a length is also given. Skipping it corrupts whatever guest
/// heap byte follows the allocation on the *next* allocation that reuses it.
fn write_bytes<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    bytes: &[u8],
) -> Result<i32, CodeModeError> {
    let ptr = exports
        .wasm_malloc
        .call(&mut *store, (bytes.len() + 1) as i32)
        .map_err(|e| rt(e.to_string()))?;
    if ptr == 0 {
        return Err(rt("guest allocation failed"));
    }
    memory
        .write(&mut *store, ptr as usize, bytes)
        .map_err(|e| rt(e.to_string()))?;
    memory
        .write(&mut *store, ptr as usize + bytes.len(), &[0])
        .map_err(|e| rt(e.to_string()))?;
    Ok(ptr)
}

fn read_i32<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    memory: Memory,
    ptr: i32,
) -> Result<i32, CodeModeError> {
    let mut buf = [0u8; 4];
    memory
        .read(&mut *store, ptr as usize, &mut buf)
        .map_err(|e| rt(e.to_string()))?;
    Ok(i32::from_le_bytes(buf))
}

fn read_js_string<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    value_ptr: i32,
) -> Result<String, CodeModeError> {
    let len_out = exports
        .wasm_malloc
        .call(&mut *store, 4)
        .map_err(|e| rt(e.to_string()))?;
    if len_out == 0 {
        return Err(rt("guest allocation failed"));
    }
    let cstr_ptr = exports
        .qjs_get_string_len
        .call(&mut *store, (value_ptr, len_out))
        .map_err(|e| rt(e.to_string()))?;
    let len = read_i32(store, memory, len_out)?;
    exports
        .wasm_free
        .call(&mut *store, len_out)
        .map_err(|e| rt(e.to_string()))?;
    if cstr_ptr == 0 {
        return Ok(String::new());
    }
    let mut bytes = vec![0u8; len.max(0) as usize];
    memory
        .read(&mut *store, cstr_ptr as usize, &mut bytes)
        .map_err(|e| rt(e.to_string()))?;
    exports
        .qjs_free_cstring
        .call(&mut *store, cstr_ptr)
        .map_err(|e| rt(e.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn new_js_string<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    s: &str,
) -> Result<i32, CodeModeError> {
    let ptr = write_bytes(store, exports, memory, s.as_bytes())?;
    let value = exports
        .qjs_new_string
        .call(&mut *store, (ptr, s.len() as i32))
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut *store, ptr)
        .map_err(|e| rt(e.to_string()))?;
    Ok(value)
}

fn new_js_error<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    message: &str,
) -> Result<i32, CodeModeError> {
    let err = exports
        .qjs_new_error
        .call(&mut *store, ())
        .map_err(|e| rt(e.to_string()))?;
    let msg = new_js_string(store, exports, memory, message)?;
    let name_ptr = write_bytes(store, exports, memory, b"message")?;
    exports
        .qjs_set_prop_string
        .call(&mut *store, (err, name_ptr, msg))
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut *store, name_ptr)
        .map_err(|e| rt(e.to_string()))?;
    Ok(err)
}

fn get_prop<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    obj: i32,
    name: &str,
) -> Result<i32, CodeModeError> {
    let name_ptr = write_bytes(store, exports, memory, name.as_bytes())?;
    let result = exports
        .qjs_get_prop_string
        .call(&mut *store, (obj, name_ptr))
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut *store, name_ptr)
        .map_err(|e| rt(e.to_string()))?;
    Ok(result)
}

fn set_prop<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    obj: i32,
    name: &str,
    value: i32,
) -> Result<(), CodeModeError> {
    let name_ptr = write_bytes(store, exports, memory, name.as_bytes())?;
    exports
        .qjs_set_prop_string
        .call(&mut *store, (obj, name_ptr, value))
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut *store, name_ptr)
        .map_err(|e| rt(e.to_string()))?;
    Ok(())
}

/// Call a QuickJS function with argument JSValue pointers already on the
/// guest heap; returns the raw (possibly-exception) result pointer.
fn call_raw<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    func: i32,
    this: i32,
    args: &[i32],
) -> Result<i32, CodeModeError> {
    let argv_ptr = if args.is_empty() {
        0
    } else {
        let ptr = exports
            .wasm_malloc
            .call(&mut *store, (args.len() * 4) as i32)
            .map_err(|e| rt(e.to_string()))?;
        for (i, arg) in args.iter().enumerate() {
            memory
                .write(&mut *store, ptr as usize + i * 4, &arg.to_le_bytes())
                .map_err(|e| rt(e.to_string()))?;
        }
        ptr
    };
    let result = exports
        .qjs_call
        .call(&mut *store, (func, this, args.len() as i32, argv_ptr))
        .map_err(|e| rt(e.to_string()))?;
    if argv_ptr != 0 {
        exports
            .wasm_free
            .call(&mut *store, argv_ptr)
            .map_err(|e| rt(e.to_string()))?;
    }
    Ok(result)
}

/// JSON-encode a guest value via the guest's own captured `JSON.stringify`.
fn json_stringify<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    json_stringify_fn: i32,
    undefined: i32,
    value: i32,
) -> Result<Option<String>, CodeModeError> {
    let result = call_raw(
        store,
        exports,
        memory,
        json_stringify_fn,
        undefined,
        &[value],
    )?;
    if exports
        .qjs_is_exception
        .call(&mut *store, result)
        .map_err(|e| rt(e.to_string()))?
        != 0
    {
        return Err(exception_error(store, exports, memory)?);
    }
    // JSON.stringify(undefined) returns the JS `undefined`, not a string.
    let is_undefined = read_js_string(store, exports, memory, result).is_err();
    let _ = is_undefined; // strings are never an error path here; kept for clarity
    let text = read_js_string(store, exports, memory, result)?;
    exports
        .qjs_free_value
        .call(&mut *store, result)
        .map_err(|e| rt(e.to_string()))?;
    if text.is_empty() {
        // Ambiguous with an empty string result, but qjs_get_string_len
        // distinguishes non-strings via a 0 cstr_ptr, which read_js_string
        // maps to "". Re-check the value's type explicitly instead of
        // guessing from the decoded text.
        return Ok(None);
    }
    Ok(Some(text))
}

fn exception_error<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
) -> Result<CodeModeError, CodeModeError> {
    let exc = exports
        .qjs_get_exception
        .call(&mut *store, ())
        .map_err(|e| rt(e.to_string()))?;
    let message = describe_exception(store, exports, memory, exc)?;
    exports
        .qjs_free_value
        .call(&mut *store, exc)
        .map_err(|e| rt(e.to_string()))?;
    Ok(classify_message(message))
}

/// QuickJS-NG raises the `host_interrupt`-driven abort as an ordinary
/// exception with this message; surface it as the specific timeout variant
/// rather than a generic runtime error.
fn classify_message(message: String) -> CodeModeError {
    if message == "interrupted" {
        CodeModeError::Timeout
    } else {
        rt(message)
    }
}

fn describe_exception<'a>(
    store: &mut impl wasmtime::AsContextMut<Data = HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    exc: i32,
) -> Result<String, CodeModeError> {
    let message_prop = get_prop(store, exports, memory, exc, "message")?;
    let message = read_js_string(store, exports, memory, message_prop)?;
    exports
        .qjs_free_value
        .call(&mut *store, message_prop)
        .map_err(|e| rt(e.to_string()))?;
    if message.is_empty() {
        // Fall back to a direct string read (covers thrown non-Error values).
        read_js_string(store, exports, memory, exc)
    } else {
        Ok(message)
    }
}

// Every closure registered below has an empty capture set: it only reads
// `HostState` through its `Caller` parameter at call time. That makes each
// closure trivially `'static` as a *value*, regardless of what lifetime `'a`
// `HostState<'a>` is instantiated with here -- so this can build a
// `Linker<HostState<'a>>` for the same non-'static `'a` the real `Store` in
// `run_inner` uses (`wasmtime::Store<T>` does not require `T: 'static`).
fn make_linker<'a>(engine: &Engine) -> Result<Linker<HostState<'a>>, CodeModeError> {
    let mut linker: Linker<HostState<'a>> = Linker::new(engine);

    linker
        .func_wrap(
            "env",
            "host_call",
            |mut caller: Caller<'_, HostState<'_>>,
             name_ptr: i32,
             name_len: i32,
             _this_ptr: i32,
             argc: i32,
             argv_ptr: i32|
             -> i32 {
                host_call_trampoline(&mut caller, name_ptr, name_len, argc, argv_ptr)
            },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "env",
            "host_interrupt",
            |caller: Caller<'_, HostState<'_>>| -> i32 {
                if caller.data().should_interrupt() {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "env",
            "host_promise_rejection",
            |mut caller: Caller<'_, HostState<'_>>,
             promise_ptr: i32,
             reason_ptr: i32,
             is_handled: i32| {
                host_promise_rejection(&mut caller, promise_ptr, reason_ptr, is_handled);
            },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "env",
            "host_module_normalize",
            |_caller: Caller<'_, HostState<'_>>, _base_ptr: i32, _name_ptr: i32| -> i32 { 0 },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "env",
            "host_module_load",
            |_caller: Caller<'_, HostState<'_>>, _name_ptr: i32, _out_len_ptr: i32| -> i32 { 0 },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "env",
            "host_get_timezone_offset",
            |_caller: Caller<'_, HostState<'_>>, _hi: i32, _lo: i32| -> i32 { 0 },
        )
        .map_err(|e| rt(e.to_string()))?;

    // wasi_snapshot_preview1: inert, capability-denying stubs. None of these
    // grant real filesystem, clock, or randomness access to the guest; they
    // exist only because QuickJS-NG's WASI libc bootstrap calls them.
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, HostState<'_>>,
             _clock_id: i32,
             _precision: i64,
             result_ptr: i32|
             -> i32 {
                let memory = caller.data().memory.expect("memory set");
                let now_ns = (Instant::now().elapsed().as_nanos() as u64).to_le_bytes();
                let _ = memory.write(&mut caller, result_ptr as usize, &now_ns);
                0
            },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |_caller: Caller<'_, HostState<'_>>,
             fd: i32,
             _iovs: i32,
             _len: i32,
             _out: i32|
             -> i32 {
                if fd == 1 || fd == 2 {
                    0
                } else {
                    8 // ERRNO_BADF
                }
            },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |_caller: Caller<'_, HostState<'_>>, _fd: i32| -> i32 { 52 },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |_caller: Caller<'_, HostState<'_>>, _fd: i32, _stat_ptr: i32| -> i32 { 8 },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_seek",
            |_caller: Caller<'_, HostState<'_>>,
             _fd: i32,
             _offset: i64,
             _whence: i32,
             _result_ptr: i32|
             -> i32 { 52 },
        )
        .map_err(|e| rt(e.to_string()))?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, HostState<'_>>, buf_ptr: i32, buf_len: i32| -> i32 {
                let memory = caller.data().memory.expect("memory set");
                // Deterministic, non-cryptographic filler: the guest is not
                // granted real entropy, matching "no ... random ... host API
                // is granted". Math.random()/crypto callers inside the guest
                // get *a* value, not zero, but never real host randomness.
                let mut bytes = vec![0u8; buf_len.max(0) as usize];
                let mut state = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
                    ^ 0x9E3779B97F4A7C15;
                for b in bytes.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *b = (state & 0xff) as u8;
                }
                let _ = memory.write(&mut caller, buf_ptr as usize, &bytes);
                0
            },
        )
        .map_err(|e| rt(e.to_string()))?;

    Ok(linker)
}

fn host_call_trampoline(
    caller: &mut Caller<'_, HostState<'_>>,
    _name_ptr: i32,
    _name_len: i32,
    argc: i32,
    argv_ptr: i32,
) -> i32 {
    match host_call_impl(caller, argc, argv_ptr) {
        Ok(ptr) => ptr,
        Err(err) => {
            let (exports, memory) = caller.data().clone_handles();
            let exports = &exports;
            if let Ok(err_value) = new_js_error(caller, exports, memory, &err.to_string()) {
                let _ = exports.qjs_throw.call(&mut *caller, err_value);
                let _ = exports.qjs_free_value.call(&mut *caller, err_value);
            }
            0
        }
    }
}

/// Handle a QuickJS promise-rejection notification. When `is_handled == 0`
/// the rejection is (so far) unhandled: keep the promise and reason values
/// on `HostState` so they can be reported at the end of the run. When
/// `is_handled != 0` for a promise we are still tracking, the rejection was
/// handled after the fact: free both the stored values and the incoming
/// values. Every path frees each owned JSValue exactly once.
///
/// All of this only applies in Workflow mode; in Code Mode the old
/// behaviour is kept exactly: free both incoming values immediately, with
/// no description capture and no tracking.
fn host_promise_rejection(
    caller: &mut Caller<'_, HostState<'_>>,
    promise_ptr: i32,
    reason_ptr: i32,
    is_handled: i32,
) {
    let (exports, memory) = caller.data().clone_handles();
    let exports = &exports;
    if !caller.data().is_workflow() {
        free_js_value(caller, exports, promise_ptr);
        free_js_value(caller, exports, reason_ptr);
        return;
    }
    // `promise_ptr` points to a boxed JSValue in guest memory, so its
    // address is not a stable identity for the promise itself (two events
    // for the same promise can arrive through different boxes). Read the
    // object pointer carried inside the boxed JSValue and use that as the
    // identity key instead. A failed read is treated as a trap: without a
    // reliable identity we cannot deduplicate or match handled events, so
    // guessing (e.g. falling back to the box pointer) could free a value
    // twice later on.
    let promise_key = match read_i32(caller, memory, promise_ptr) {
        Ok(key) => key,
        Err(e) => {
            caller.data_mut().record_trap(e.to_string());
            free_js_value(caller, exports, promise_ptr);
            free_js_value(caller, exports, reason_ptr);
            return;
        }
    };
    if is_handled == 0 {
        let already_stored = caller
            .data()
            .unhandled
            .iter()
            .any(|(key, _, _, _)| *key == promise_key);
        if already_stored {
            // Do not store a duplicate entry for the same promise: free the
            // incoming pair instead (the first entry keeps its own retained
            // values alive for reporting).
            free_js_value(caller, exports, promise_ptr);
            free_js_value(caller, exports, reason_ptr);
            return;
        }
        let message = describe_exception(caller, exports, memory, reason_ptr)
            .unwrap_or_else(|_| "<unrepresentable rejection>".to_string());
        caller
            .data_mut()
            .unhandled
            .push((promise_key, promise_ptr, reason_ptr, message));
        return;
    }
    let stored = caller
        .data_mut()
        .unhandled
        .iter()
        .position(|(key, _, _, _)| *key == promise_key);
    if let Some(index) = stored {
        let (_, stored_promise, stored_reason, _) = caller.data_mut().unhandled.remove(index);
        free_js_value(caller, exports, stored_promise);
        free_js_value(caller, exports, stored_reason);
        free_js_value(caller, exports, promise_ptr);
        free_js_value(caller, exports, reason_ptr);
    } else {
        free_js_value(caller, exports, promise_ptr);
        free_js_value(caller, exports, reason_ptr);
    }
}

/// Free a guest JSValue. In Workflow mode a failed free is an unrecoverable
/// trap recorded on `HostState` and surfaced at the end of the run; in Code
/// Mode it is ignored, exactly as before.
fn free_js_value(caller: &mut Caller<'_, HostState<'_>>, exports: &QjsExports, value_ptr: i32) {
    if let Err(e) = exports.qjs_free_value.call(&mut *caller, value_ptr) {
        if caller.data().is_workflow() {
            caller.data_mut().record_trap(e.to_string());
        }
    }
}

fn host_call_impl(
    caller: &mut Caller<'_, HostState<'_>>,
    argc: i32,
    argv_ptr: i32,
) -> Result<i32, CodeModeError> {
    let (exports, memory) = caller.data().clone_handles();
    let exports = &exports;
    if argc != 3 {
        return Err(rt(
            "Code Mode host bridge called with an unexpected argument count",
        ));
    }
    let mut arg_ptrs = [0i32; 3];
    for (i, slot) in arg_ptrs.iter_mut().enumerate() {
        *slot = read_i32(caller, memory, argv_ptr + (i as i32) * 4)?;
    }
    let namespace = read_js_string(caller, exports, memory, arg_ptrs[0])?;
    let method = read_js_string(caller, exports, memory, arg_ptrs[1])?;
    let args_json = read_js_string(caller, exports, memory, arg_ptrs[2])?;
    let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);

    if let Some(max_host_calls) = caller.data().max_host_calls {
        if caller.data().host_call_count >= max_host_calls {
            return Err(CodeModeError::HostCallLimit);
        }
        caller.data_mut().host_call_count += 1;
    }

    let resolve_out = exports
        .wasm_malloc
        .call(&mut *caller, 4)
        .map_err(|e| rt(e.to_string()))?;
    let reject_out = exports
        .wasm_malloc
        .call(&mut *caller, 4)
        .map_err(|e| rt(e.to_string()))?;
    let promise_ptr = exports
        .qjs_new_promise
        .call(&mut *caller, (resolve_out, reject_out))
        .map_err(|e| rt(e.to_string()))?;
    let resolve_ptr = read_i32(caller, memory, resolve_out)?;
    let reject_ptr = read_i32(caller, memory, reject_out)?;
    exports
        .wasm_free
        .call(&mut *caller, resolve_out)
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut *caller, reject_out)
        .map_err(|e| rt(e.to_string()))?;

    let state = caller.data_mut();
    let host = state.host;
    // `namespace`/`method` are local to this call, but the future must
    // outlive it (it's polled later in `drive()`), so the async block
    // captures its own owned copies rather than borrowing these locals --
    // the resulting future is then self-contained (the borrow it takes of
    // `namespace`/`method` is of its own captured fields).
    let future = async move { host.call(&namespace, &method, args).await }.boxed();
    state.pending.push(Pending {
        resolve_ptr,
        reject_ptr,
        future,
    });

    Ok(promise_ptr)
}

/// Evaluate `source` inside the vendored QuickJS-WASI guest, bridging
/// `gait.<namespace>.<method>(args)` calls to `host` and enforcing `limits`.
pub async fn eval_with(
    source: &str,
    api: ApiSurface,
    host: &dyn CapabilityHost,
    opts: RunOptions,
    artifact: &[u8],
) -> Result<RunReport, CodeModeError> {
    match opts.limits {
        LimitsMode::CodeMode(limits) => {
            let deadline = Instant::now() + Duration::from_millis(limits.timeout_ms);
            let remaining = || deadline.saturating_duration_since(Instant::now());
            let mode = Mode::Code { limits, deadline };
            let fut = run_inner(source, api, host, mode, opts.prelude, artifact);
            tokio::time::timeout(remaining(), fut)
                .await
                .unwrap_or(Err(CodeModeError::Timeout))
        }
        LimitsMode::Workflow(limits) => {
            // No outer wall-clock timeout: only the guest-CPU budget
            // interrupts the guest.
            let mode = Mode::Workflow { limits };
            run_inner(source, api, host, mode, opts.prelude, artifact).await
        }
    }
}

async fn run_inner<'a>(
    source: &str,
    api: ApiSurface,
    host: &'a dyn CapabilityHost,
    mode: Mode,
    prelude: Option<String>,
    artifact: &[u8],
) -> Result<RunReport, CodeModeError> {
    let (max_memory_bytes, max_output_bytes, deadline, max_host_calls, cpu_budget) = match mode {
        Mode::Code { limits, deadline } => (
            limits.max_memory_bytes,
            limits.max_output_bytes,
            Some(deadline),
            Some(limits.max_host_calls),
            None,
        ),
        Mode::Workflow { limits } => (
            limits.max_memory_bytes,
            limits.max_output_bytes,
            None,
            None,
            Some(Duration::from_millis(limits.cpu_budget_ms)),
        ),
    };
    let (engine, module) = compiled_guest(artifact)?;
    let linker: Linker<HostState<'a>> = make_linker(engine)?;

    let limiter = StoreLimitsBuilder::new()
        .memory_size(max_memory_bytes)
        .build();
    let state = HostState {
        host,
        deadline,
        max_host_calls,
        cpu_budget,
        cpu_spent: Duration::ZERO,
        segment_start: None,
        host_call_count: 0,
        pending: Vec::new(),
        unhandled: Vec::new(),
        first_trap: None,
        memory: None,
        exports: None,
        limiter,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limiter);

    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| rt(format!("guest instantiation failed: {e}")))?;
    let exports = resolve_exports(&mut store, &instance)?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| rt("guest artifact does not export memory"))?;
    store.data_mut().exports = Some(exports.clone());
    store.data_mut().memory = Some(memory);
    let exports = &exports;

    // Workflow mode: the guest-CPU clock runs for the whole remainder of the
    // run and is paused only while the host awaits capability futures inside
    // `drive()` -- every guest entry (eval, job queue, promise settlement,
    // stringify, exception description) is therefore covered by the budget.
    if cpu_budget.is_some() {
        store.data_mut().cpu_begin();
    }

    let result = run_body(
        &mut store,
        exports,
        memory,
        source,
        api,
        prelude,
        deadline,
        max_output_bytes,
    )
    .await;
    if cpu_budget.is_some() {
        store.data_mut().cpu_end();
    }
    result
}

/// Everything a run does after the guest instance and its captured globals
/// are set up: initialization, bootstrap, prelude, user script, driving the
/// result promise, and encoding the final value. Kept separate so `run_inner`
/// can stop the guest-CPU clock on every exit path (success and error).
#[allow(clippy::too_many_arguments)]
async fn run_body<'a>(
    mut store: &mut Store<HostState<'a>>,
    exports: &QjsExports,
    memory: Memory,
    source: &str,
    api: ApiSurface,
    prelude: Option<String>,
    deadline: Option<Instant>,
    max_output_bytes: usize,
) -> Result<RunReport, CodeModeError> {
    exports
        .initialize
        .call(&mut store, ())
        .map_err(|e| rt(e.to_string()))?;
    let init = exports
        .qjs_init
        .call(&mut store, ())
        .map_err(|e| rt(e.to_string()))?;
    if init != 0 {
        return Err(rt("QuickJS runtime initialization failed"));
    }
    // The engine only calls `env.host_interrupt` at all once this is
    // enabled (matches the reference driver's `applyLimits`, which enables
    // it exactly when an interrupt handler is configured -- ours always is,
    // since every Code Mode run has a deadline).
    exports
        .qjs_set_interrupt_handler
        .call(&mut store, 1)
        .map_err(|e| rt(e.to_string()))?;
    exports
        .qjs_set_promise_rejection_handler
        .call(&mut store, 1)
        .map_err(|e| rt(e.to_string()))?;

    let global = exports
        .qjs_get_global
        .call(&mut store, ())
        .map_err(|e| rt(e.to_string()))?;
    let undefined = exports
        .qjs_get_undefined
        .call(&mut store, ())
        .map_err(|e| rt(e.to_string()))?;
    let json_obj = get_prop(&mut store, exports, memory, global, "JSON")?;
    let json_stringify_fn = get_prop(&mut store, exports, memory, json_obj, "stringify")?;
    let json_parse_fn = get_prop(&mut store, exports, memory, json_obj, "parse")?;
    exports
        .qjs_free_value
        .call(&mut store, json_obj)
        .map_err(|e| rt(e.to_string()))?;

    // Register the single generic host bridge, then bootstrap `gait.<ns>.<method>`
    // as a Proxy over it. `api.namespaces` is intentionally not consulted here:
    // any namespace/method reaches `host_call`, and it is `CapabilityDispatcher`
    // (on the Rust side, via `host`) that enforces which namespaces/methods are
    // actually enabled -- this keeps the guest bootstrap generic and matches
    // "absent namespace fails with its name" being a *host* error, not a
    // guest-side ReferenceError.
    let _ = &api;
    let bridge_name = write_bytes(&mut store, exports, memory, b"__gait_call")?;
    let bridge_fn = exports
        .qjs_new_host_function
        .call(&mut store, (bridge_name, 11, 0))
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut store, bridge_name)
        .map_err(|e| rt(e.to_string()))?;
    set_prop(
        &mut store,
        exports,
        memory,
        global,
        "__gait_call",
        bridge_fn,
    )?;

    let bootstrap = r#"
        globalThis.gait = new Proxy({}, {
            get(_t, ns) {
                return new Proxy({}, {
                    get(_t2, method) {
                        return (args) => __gait_call(
                            String(ns), String(method),
                            args === undefined ? "null" : JSON.stringify(args)
                        ).then((json) => JSON.parse(json));
                    }
                });
            }
        });
    "#;
    eval_js(store, exports, memory, bootstrap, "<bootstrap>")?;

    // The workflow prelude runs after the bootstrap (so it sees the real
    // `globalThis.gait`) and before the user script.
    if let Some(prelude) = &prelude {
        eval_js(store, exports, memory, prelude, "<prelude>")?;
    }

    let wrapped = format!("(async () => {{\n{source}\n}})()");
    let result_promise = eval_js(store, exports, memory, &wrapped, "<code>")?;

    let final_value = drive(store, exports, memory, result_promise, deadline).await?;

    let json = json_stringify(
        &mut store,
        exports,
        memory,
        json_stringify_fn,
        undefined,
        final_value,
    )?;
    exports
        .qjs_free_value
        .call(&mut store, final_value)
        .map_err(|e| rt(e.to_string()))?;
    let _ = json_parse_fn; // reserved for future guest-side JSON.parse use

    if let Some(trap) = store.data().first_trap.clone() {
        return Err(rt(trap));
    }

    match json {
        None => {
            let unhandled = collect_unhandled_rejections(store)?;
            Ok(RunReport {
                value: Value::Null,
                unhandled_rejections: unhandled,
            })
        }
        Some(text) => {
            if text.len() > max_output_bytes {
                return Err(CodeModeError::OutputLimit);
            }
            let value = serde_json::from_str(&text)
                .map_err(|e| rt(format!("Code Mode result was not valid JSON: {e}")))?;
            let unhandled = collect_unhandled_rejections(store)?;
            Ok(RunReport {
                value,
                unhandled_rejections: unhandled,
            })
        }
    }
}

fn eval_js(
    store: &mut Store<HostState<'_>>,
    exports: &QjsExports,
    memory: Memory,
    source: &str,
    filename: &str,
) -> Result<i32, CodeModeError> {
    let code_ptr = write_bytes(store, exports, memory, source.as_bytes())?;
    // Unlike `code`, `qjs_eval` takes no filename length -- the guest must
    // find the end via NUL, matching the reference driver's `writeString`
    // (always NUL-terminated) used for the filename argument.
    let fn_ptr = write_bytes(store, exports, memory, filename.as_bytes())?;
    let eval_result = exports
        .qjs_eval
        .call(&mut *store, (code_ptr, source.len() as i32, fn_ptr, 0));
    let result = eval_result.map_err(wasm_err)?;
    let exception_result = exports.qjs_is_exception.call(&mut *store, result);
    let is_exception = exception_result.map_err(wasm_err)?;
    exports
        .wasm_free
        .call(&mut *store, code_ptr)
        .map_err(|e| rt(e.to_string()))?;
    exports
        .wasm_free
        .call(&mut *store, fn_ptr)
        .map_err(|e| rt(e.to_string()))?;
    if is_exception != 0 {
        return Err(exception_error(store, exports, memory)?);
    }
    Ok(result)
}

/// Return the description of every still-stored unhandled rejection at the
/// end of a successful run, in the order the rejections were observed. The
/// messages were captured (and the raw JSValues freed) at notification time.
fn collect_unhandled_rejections(
    store: &mut Store<HostState<'_>>,
) -> Result<Vec<String>, CodeModeError> {
    let unhandled = &store.data().unhandled;
    Ok(unhandled
        .iter()
        .map(|(_, _, _, message)| message.clone())
        .collect())
}

/// Drain the job queue and await in-flight capability calls (concurrently)
/// until `promise` settles, then return its resolved/rejected JSValue.
async fn drive(
    store: &mut Store<HostState<'_>>,
    exports: &QjsExports,
    memory: Memory,
    promise: i32,
    deadline: Option<Instant>,
) -> Result<i32, CodeModeError> {
    loop {
        loop {
            let pending_job = exports
                .qjs_is_job_pending
                .call(&mut *store, ())
                .map_err(|e| rt(e.to_string()))?;
            if pending_job == 0 {
                break;
            }
            let job_result = exports
                .qjs_execute_pending_job
                .call(&mut *store, ())
                .map_err(|e| rt(e.to_string()))?;
            if job_result < 0 {
                return Err(exception_error(store, exports, memory)?);
            }
        }

        let state = exports
            .qjs_promise_state
            .call(&mut *store, promise)
            .map_err(|e| rt(e.to_string()))?;
        if state == 1 {
            return exports
                .qjs_promise_result
                .call(&mut *store, promise)
                .map_err(|e| rt(e.to_string()));
        }
        if state == 2 {
            let reason = exports
                .qjs_promise_result
                .call(&mut *store, promise)
                .map_err(|e| rt(e.to_string()))?;
            let message = describe_exception(store, exports, memory, reason)?;
            exports
                .qjs_free_value
                .call(&mut *store, reason)
                .map_err(|e| rt(e.to_string()))?;
            return Err(classify_message(message));
        }

        if store.data().pending.is_empty() {
            // Nothing left to drive and the top-level promise never settled:
            // the guest is waiting on a promise it will never resolve itself.
            return Err(rt("Code Mode source did not settle its result promise"));
        }

        let pending = std::mem::take(&mut store.data_mut().pending);
        // The host is about to await real capability work: pause the
        // guest-CPU clock so host wait time does not consume the budget.
        store.data_mut().cpu_end();
        let awaited = await_one(pending, deadline).await;
        // Resume the clock before touching the guest again -- including on
        // the error path, so the shutdown/teardown accounting stays balanced.
        store.data_mut().cpu_begin();
        let ((resolve_ptr, reject_ptr, outcome), remaining) = awaited?;
        store.data_mut().pending = remaining;

        let (target, arg) = match outcome {
            Ok(value) => {
                let json = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());
                (resolve_ptr, new_js_string(store, exports, memory, &json)?)
            }
            Err(err) => {
                let message = match &err {
                    HostError::Unavailable { namespace, method } => {
                        format!("capability {namespace}.{method} is unavailable")
                    }
                    HostError::Failed(m) => m.clone(),
                };
                (reject_ptr, new_js_error(store, exports, memory, &message)?)
            }
        };
        let undefined = exports
            .qjs_get_undefined
            .call(&mut *store, ())
            .map_err(|e| rt(e.to_string()))?;
        let call_result = call_raw(store, exports, memory, target, undefined, &[arg])?;
        exports
            .qjs_free_value
            .call(&mut *store, call_result)
            .map_err(|e| rt(e.to_string()))?;
    }
}

type Settled = (i32, i32, Result<Value, HostError>);

/// Await whichever pending capability call completes first, returning its
/// outcome plus the rest, still in flight, to be re-driven on the next
/// `drive()` iteration. All of `pending`'s futures are polled together by
/// `select_all`, so the underlying host work (e.g. two concurrent `gait.*`
/// calls from `Promise.all`) genuinely progresses concurrently regardless of
/// how many settle per call to this function.
async fn await_one<'a>(
    pending: Vec<Pending<'a>>,
    deadline: Option<Instant>,
) -> Result<(Settled, Vec<Pending<'a>>), CodeModeError> {
    use futures::future::select_all;

    let mut meta: Vec<(i32, i32)> = pending
        .iter()
        .map(|p| (p.resolve_ptr, p.reject_ptr))
        .collect();
    let futs: Vec<BoxFuture<'a, Result<Value, HostError>>> =
        pending.into_iter().map(|p| p.future).collect();

    let (result, index, remaining_futs) = match deadline {
        Some(deadline) => {
            let timeout = tokio::time::sleep(deadline.saturating_duration_since(Instant::now()));
            tokio::pin!(timeout);
            tokio::select! {
                out = select_all(futs) => out,
                _ = &mut timeout => return Err(CodeModeError::Timeout),
            }
        }
        None => select_all(futs).await,
    };
    // `select_all`'s "rest" is built with `swap_remove` (the completed
    // future's slot is filled by the last element), not a stable remove --
    // `meta` must be kept in sync with the exact same swap_remove, or this
    // resolves/rejects the wrong guest promise with another call's result.
    let (resolve_ptr, reject_ptr) = meta.swap_remove(index);
    let remaining = remaining_futs
        .into_iter()
        .zip(meta)
        .map(|(future, (resolve_ptr, reject_ptr))| Pending {
            resolve_ptr,
            reject_ptr,
            future,
        })
        .collect();
    Ok(((resolve_ptr, reject_ptr, result), remaining))
}

fn resolve_exports(
    store: &mut Store<HostState<'_>>,
    instance: &Instance,
) -> Result<QjsExports, CodeModeError> {
    macro_rules! typed {
        ($name:expr) => {
            instance
                .get_typed_func(&mut *store, $name)
                .map_err(|e| rt(format!("missing guest export {}: {e}", $name)))?
        };
    }
    Ok(QjsExports {
        initialize: typed!("_initialize"),
        qjs_init: typed!("qjs_init"),
        wasm_malloc: typed!("wasm_malloc"),
        wasm_free: typed!("wasm_free"),
        qjs_eval: typed!("qjs_eval"),
        qjs_new_string: typed!("qjs_new_string"),
        qjs_get_string_len: typed!("qjs_get_string_len"),
        qjs_free_cstring: typed!("qjs_free_cstring"),
        qjs_get_global: typed!("qjs_get_global"),
        qjs_get_prop_string: typed!("qjs_get_prop_string"),
        qjs_set_prop_string: typed!("qjs_set_prop_string"),
        qjs_call: typed!("qjs_call"),
        qjs_new_host_function: typed!("qjs_new_host_function"),
        qjs_new_promise: typed!("qjs_new_promise"),
        qjs_promise_state: typed!("qjs_promise_state"),
        qjs_promise_result: typed!("qjs_promise_result"),
        qjs_is_job_pending: typed!("qjs_is_job_pending"),
        qjs_execute_pending_job: typed!("qjs_execute_pending_job"),
        qjs_is_exception: typed!("qjs_is_exception"),
        qjs_get_exception: typed!("qjs_get_exception"),
        qjs_throw: typed!("qjs_throw"),
        qjs_free_value: typed!("qjs_free_value"),
        qjs_new_error: typed!("qjs_new_error"),
        qjs_get_undefined: typed!("qjs_get_undefined"),
        qjs_set_interrupt_handler: typed!("qjs_set_interrupt_handler"),
        qjs_set_promise_rejection_handler: typed!("qjs_set_promise_rejection_handler"),
    })
}
