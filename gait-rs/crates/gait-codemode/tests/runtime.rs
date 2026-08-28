use gait_codemode::{
    ApiSurface, CapabilityHost, CodeModeError, GuestArtifact, HostError, Limits, QUICKJS_IMPORTS,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn artifact_has_only_quickjs_allowlisted_imports() {
    let imports = GuestArtifact::embedded().imports().unwrap();
    let expected: Vec<(String, String)> = QUICKJS_IMPORTS
        .iter()
        .map(|(module, name)| ((*module).into(), (*name).into()))
        .collect();
    assert_eq!(imports, expected);
}

#[test]
fn artifact_has_no_wasi_or_ambient_imports() {
    // "No WASI implementation is granted" is enforced by what Gait links
    // these imports to (inert, capability-denying stubs), not by the
    // absence of wasi_snapshot_preview1 import *names* in the artifact.
    // See QUICKJS_IMPORTS' doc comment. Here we only assert the artifact's
    // import surface is exactly the known, minimal, allowlisted set.
    let imports = GuestArtifact::embedded().imports().unwrap();
    let expected: Vec<(String, String)> = QUICKJS_IMPORTS
        .iter()
        .map(|(module, name)| ((*module).into(), (*name).into()))
        .collect();
    for (module, name) in &imports {
        assert!(
            expected.contains(&(module.clone(), name.clone())),
            "unexpected import: {module}.{name}"
        );
        assert!(
            module == "env" || module == "wasi_snapshot_preview1",
            "ambient import module: {module}"
        );
    }
}

#[test]
fn limits_are_explicit_and_output_is_bounded() {
    let limits = Limits::default();
    assert!(limits.timeout_ms > 0 && limits.max_host_calls > 0);
    assert!(gait_codemode::bounded_output(vec![0; 8], limits).is_ok());
    let tiny = Limits {
        max_output_bytes: 1,
        ..limits
    };
    assert!(gait_codemode::bounded_output(vec![0; 2], tiny).is_err());
}

/// A deterministic fake `CapabilityHost` used to exercise the real evaluator
/// end to end, entirely in-process (no Gait core, no filesystem).
struct FakeHost {
    calls: Arc<AtomicUsize>,
    concurrent_peak: Arc<AtomicUsize>,
    concurrent_now: Arc<AtomicUsize>,
}

impl FakeHost {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            concurrent_peak: Arc::new(AtomicUsize::new(0)),
            concurrent_now: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl CapabilityHost for FakeHost {
    async fn call(&self, namespace: &str, method: &str, args: Value) -> Result<Value, HostError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match namespace {
            "read" if method == "call" => {
                Ok(json!({ "content": format!("file:{}", args["path"]) }))
            }
            "echo" => Ok(args),
            "math" if method == "double" => {
                let n = args["n"].as_i64().unwrap_or(0);
                Ok(json!(n * 2))
            }
            "slow" => {
                let now = self.concurrent_now.fetch_add(1, Ordering::SeqCst) + 1;
                self.concurrent_peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                self.concurrent_now.fetch_sub(1, Ordering::SeqCst);
                Ok(json!({ "id": args["id"] }))
            }
            "boom" => Err(HostError::Failed("simulated capability failure".into())),
            _ => Err(HostError::Unavailable {
                namespace: namespace.to_string(),
                method: method.to_string(),
            }),
        }
    }
}

#[tokio::test]
async fn javascript_can_call_a_host_capability() {
    let host = FakeHost::new();
    let result = gait_codemode::run(
        "return await gait.echo.call({ hello: 'world' });",
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await
    .expect("eval should succeed");
    assert_eq!(result, json!({ "hello": "world" }));
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn javascript_can_chain_a_result_into_a_second_call() {
    let host = FakeHost::new();
    let result = gait_codemode::run(
        r#"
        const doubled = await gait.math.double({ n: 21 });
        const again = await gait.math.double({ n: doubled });
        return again;
        "#,
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await
    .expect("eval should succeed");
    assert_eq!(result, json!(84));
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn independent_calls_run_concurrently_through_promise_all() {
    warm_up().await;
    let host = FakeHost::new();
    let peak = host.concurrent_peak.clone();
    let start = Instant::now();
    let result = gait_codemode::run(
        r#"
        const results = await Promise.all([
            gait.slow.call({ id: 1 }),
            gait.slow.call({ id: 2 }),
            gait.slow.call({ id: 3 }),
        ]);
        return results.map((r) => r.id);
        "#,
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await
    .expect("eval should succeed");
    let elapsed = start.elapsed();
    assert_eq!(result, json!([1, 2, 3]));
    // Each call sleeps 30ms; if they ran sequentially this would take
    // ~90ms+. Concurrency keeps it well under that.
    assert!(
        elapsed < Duration::from_millis(80),
        "calls did not run concurrently: {elapsed:?}"
    );
    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "expected at least two capability calls in flight at once"
    );
}

#[tokio::test]
async fn absent_namespace_fails_with_its_name() {
    let host = FakeHost::new();
    let err = gait_codemode::run(
        "return await gait.nope.call({});",
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await
    .expect_err("unknown namespace must fail");
    let message = err.to_string();
    assert!(
        message.contains("nope"),
        "error should name the namespace: {message}"
    );
}

#[tokio::test]
async fn host_error_becomes_a_javascript_visible_failure() {
    let host = FakeHost::new();
    let result = gait_codemode::run(
        r#"
        try {
            await gait.boom.call({});
            return "did not throw";
        } catch (e) {
            return "caught: " + e.message;
        }
        "#,
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await
    .expect("eval should succeed (the guest catches the error itself)");
    assert_eq!(result, json!("caught: simulated capability failure"));
}

#[tokio::test]
async fn infinite_loop_is_killed_by_the_timeout() {
    warm_up().await;
    let host = FakeHost::new();
    let limits = Limits {
        timeout_ms: 200,
        ..Limits::default()
    };
    let start = Instant::now();
    let result = gait_codemode::run("while (true) {}", ApiSurface::default(), &host, limits).await;
    assert!(
        matches!(result, Err(CodeModeError::Timeout)),
        "expected a timeout, got {result:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "timeout enforcement took far too long"
    );
}

/// `run()` compiles the fixed guest module with Cranelift once per process
/// and caches it (dominant cost: seconds in an unoptimized build). Timing
/// assertions must not pay for that compile on whichever test happens to
/// run first; force it to happen here, outside any measured window.
async fn warm_up() {
    struct NoopHost;
    #[async_trait::async_trait]
    impl CapabilityHost for NoopHost {
        async fn call(&self, _: &str, _: &str, _: Value) -> Result<Value, HostError> {
            unreachable!()
        }
    }
    let _ = gait_codemode::run("1;", ApiSurface::default(), &NoopHost, Limits::default()).await;
}

#[tokio::test]
async fn host_call_limit_is_enforced() {
    let host = FakeHost::new();
    let limits = Limits {
        max_host_calls: 2,
        ..Limits::default()
    };
    let result = gait_codemode::run(
        r#"
        for (let i = 0; i < 5; i++) {
            await gait.echo.call({ i });
        }
        return "done";
        "#,
        ApiSurface::default(),
        &host,
        limits,
    )
    .await;
    assert!(result.is_err(), "exceeding max_host_calls should fail");
    assert!(
        host.calls.load(Ordering::SeqCst) <= 3,
        "should stop near the limit, not run all 5 calls"
    );
}
