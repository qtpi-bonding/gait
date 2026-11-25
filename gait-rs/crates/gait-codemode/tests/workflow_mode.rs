use gait_codemode::{
    ApiSurface, CapabilityHost, CodeModeError, HostError, Limits, RunOptions, WorkflowLimits,
};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// Run a script on a dedicated thread with its own current-thread runtime so
/// that a JS busy loop that is never interrupted cannot block the test's
/// tokio runtime. The test thread waits on an mpsc channel with a hard
/// deadline; a leaked stuck thread is acceptable.
fn run_blocked(script: &'static str) -> Result<gait_codemode::RunReport, CodeModeError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let host = FakeHost::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime should build");
        let result = rt.block_on(async {
            gait_codemode::run_with(script, ApiSurface::default(), &host, wf_opts(200)).await
        });
        if tx.send(result).is_err() {
            // Test thread already gave up; nothing to do here.
        }
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(result) => result,
        Err(_) => panic!("cpu budget not enforced"),
    }
}

/// Workflow-mode limits: a tight CPU budget for tests, generous memory and
/// output bounds so those never interfere.
fn wf_limits(cpu_budget_ms: u64) -> WorkflowLimits {
    WorkflowLimits {
        cpu_budget_ms,
        max_memory_bytes: 64 * 1024 * 1024,
        max_output_bytes: 4 * 1024 * 1024,
    }
}

/// Workflow-mode run options with no prelude.
fn wf_opts(cpu_budget_ms: u64) -> RunOptions {
    RunOptions {
        prelude: None,
        limits: gait_codemode::LimitsMode::Workflow(wf_limits(cpu_budget_ms)),
    }
}

/// A fake `CapabilityHost` exposing only the namespaces these tests need.
struct FakeHost;

impl FakeHost {
    fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl CapabilityHost for FakeHost {
    async fn call(&self, namespace: &str, method: &str, args: Value) -> Result<Value, HostError> {
        match (namespace, method) {
            ("echo", "call") => Ok(args),
            ("sleep", "call") => {
                let ms = args["ms"].as_u64().unwrap_or(400);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(json!(1))
            }
            _ => Err(HostError::Unavailable {
                namespace: namespace.to_string(),
                method: method.to_string(),
            }),
        }
    }
}

/// The prelude replaces `globalThis.gait` with a wrapper that captures the
/// original host object; the new `gait.wf` object is real and the wrapped
/// capability call still reaches the host's echo namespace.
#[tokio::test]
async fn prelude_wraps_gait_and_exposes_real_object() {
    let host = FakeHost::new();
    let prelude = r#"
        const host = globalThis.gait;
        globalThis.gait = {
            wf: {
                run: { id: "r1" },
                call: (a) => host.echo.call(a),
            },
        };
    "#;
    let opts = RunOptions {
        prelude: Some(prelude.to_string()),
        limits: wf_opts(1000).limits,
    };
    let report = gait_codemode::run_with(
        "return [typeof gait.wf.run, gait.wf.run.id, await gait.wf.call({x: 1})];",
        ApiSurface::default(),
        &host,
        opts,
    )
    .await
    .expect("workflow eval should succeed");
    assert_eq!(report.value, json!(["object", "r1", { "x": 1 }]));
}

/// Time spent awaiting a host capability must not consume the CPU budget.
#[tokio::test]
async fn workflow_mode_does_not_count_host_wait_time() {
    let host = FakeHost::new();
    let report = gait_codemode::run_with(
        "return await gait.sleep.call({});",
        ApiSurface::default(),
        &host,
        wf_opts(100),
    )
    .await
    .expect("host wait must not trip the cpu budget");
    assert_eq!(report.value, json!(1));
}

/// Pure JavaScript spin must trip the workflow CPU budget and be reported
/// as a timeout, promptly.
#[tokio::test]
async fn workflow_mode_cpu_budget_trips_on_busy_loop() {
    let host = FakeHost::new();
    let start = Instant::now();
    let result = gait_codemode::run_with(
        "while (true) {}",
        ApiSurface::default(),
        &host,
        wf_opts(200),
    )
    .await;
    assert!(
        matches!(result, Err(CodeModeError::Timeout)),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "cpu budget enforcement took far too long: {:?}",
        start.elapsed()
    );
}

/// Workflow mode must not cap the number of host capability calls (the
/// code-mode cap is 128).
#[tokio::test]
async fn workflow_mode_has_no_host_call_cap() {
    let host = FakeHost::new();
    let report = gait_codemode::run_with(
        r#"
        let n = 0;
        for (let i = 0; i < 200; i++) {
            await gait.echo.call({ i });
            n += 1;
        }
        return n;
        "#,
        ApiSurface::default(),
        &host,
        wf_opts(5000),
    )
    .await
    .expect("workflow mode must allow more than 128 host calls");
    assert_eq!(report.value, json!(200));
}

/// A rejected promise that is never handled is reported exactly once, at the
/// end; one handled later is not reported at all; clean scripts report none.
#[tokio::test]
async fn unhandled_rejection_is_reported_at_end_only_if_never_handled() {
    let host = FakeHost::new();
    let script = r#"
        const never = Promise.reject(new Error("boom-never"));
        const late = Promise.reject(new Error("boom-late"));
        await gait.echo.call({});
        late.catch(() => {});
        return 7;
    "#;
    let report = gait_codemode::run_with(script, ApiSurface::default(), &host, wf_opts(1000))
        .await
        .expect("workflow eval should succeed despite the pending rejection");
    assert_eq!(report.value, json!(7));
    let never_count = report
        .unhandled_rejections
        .iter()
        .filter(|m| m.contains("boom-never"))
        .count();
    assert_eq!(never_count, 1, "exactly one never-handled rejection");
    assert!(
        !report
            .unhandled_rejections
            .iter()
            .any(|m| m.contains("boom-late")),
        "a handled rejection must not be reported"
    );

    // A script with no rejections reports none.
    let clean = gait_codemode::run_with(
        "return await gait.echo.call({});",
        ApiSurface::default(),
        &host,
        wf_opts(1000),
    )
    .await
    .expect("clean workflow eval should succeed");
    assert!(clean.unhandled_rejections.is_empty());
}

/// Plain `run()` keeps its code-mode behaviour: the host call cap still
/// applies, and the wall-clock timeout still counts host wait time.
#[tokio::test]
async fn code_mode_defaults_unchanged() {
    let host = FakeHost::new();
    let many_calls = gait_codemode::run(
        r#"
        for (let i = 0; i < 129; i++) {
            await gait.echo.call({ i });
        }
        return "done";
        "#,
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await;
    assert!(
        matches!(many_calls, Err(CodeModeError::Runtime(ref m)) if m.contains("host-call limit")),
        "129 calls must surface as a runtime error mentioning the host-call limit, got {many_calls:?}"
    );

    let limits = Limits {
        timeout_ms: 300,
        ..Limits::default()
    };
    let slow_host = gait_codemode::run(
        "return await gait.sleep.call({ ms: 1000 });",
        ApiSurface::default(),
        &host,
        limits,
    )
    .await;
    assert!(
        matches!(slow_host, Err(CodeModeError::Timeout)),
        "wall-clock timeout must count host wait, got {slow_host:?}"
    );
}

/// Time spent awaiting a host capability must resume budget accounting:
/// after an await completes, an unbounded JS spin must still trip the CPU
/// budget and be reported as a timeout.
#[test]
fn workflow_mode_cpu_budget_trips_after_an_await() {
    let start = Instant::now();
    let result = run_blocked("await gait.echo.call({}); while (true) {}");
    assert!(
        matches!(result, Err(CodeModeError::Timeout)),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "cpu budget enforcement took far too long: {:?}",
        start.elapsed()
    );

    // Variant: await a plain resolved value, then spin.
    let start = Instant::now();
    let result = run_blocked("await 0; while (true) {}");
    assert!(
        matches!(result, Err(CodeModeError::Timeout)),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "cpu budget enforcement took far too long: {:?}",
        start.elapsed()
    );
}

/// A JS spin running inside a `.then()` handler attached to a host call must
/// also trip the CPU budget and be reported as a timeout.
#[test]
fn workflow_mode_cpu_budget_trips_in_a_then_handler() {
    let start = Instant::now();
    let result = run_blocked(
        "gait.echo.call({}).then(() => { while (true) {} }); await gait.sleep.call({ ms: 50 }); return 1;",
    );
    assert!(
        matches!(result, Err(CodeModeError::Timeout)),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "cpu budget enforcement took far too long: {:?}",
        start.elapsed()
    );
}
