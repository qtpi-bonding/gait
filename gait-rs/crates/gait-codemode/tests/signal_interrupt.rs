#![cfg(target_os = "macos")]
//! On macOS, wasmtime's default Mach-port trap handler thread aborts the
//! whole process when any signal (for example SIGCHLD from an exiting shell
//! step) interrupts it. A signal landing on any thread must leave a host
//! that runs scripts alive.

use gait_codemode::{ApiSurface, CapabilityHost, Limits};
use serde_json::{json, Value};

#[async_trait::async_trait]
impl CapabilityHost for EchoHost {
    async fn call(
        &self,
        _namespace: &str,
        _method: &str,
        args: Value,
    ) -> Result<Value, gait_codemode::HostError> {
        Ok(args)
    }
}

struct EchoHost;

async fn trivial_script() -> Value {
    let host = EchoHost;
    gait_codemode::run(
        "return await gait.echo.call({ hello: 'world' });",
        ApiSurface::default(),
        &host,
        Limits::default(),
    )
    .await
    .expect("eval should succeed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signal_on_any_thread_does_not_abort_a_script_host() {
    // 1. Initialise the engine with one trivial script (this also starts
    //    wasmtime's helper threads, including the Mach-port trap handler).
    let first = trivial_script().await;
    assert_eq!(first, json!({ "hello": "world" }));

    // 2. Install a no-op handler for SIGUSR1.
    extern "C" fn noop(_: libc::c_int) {}
    unsafe {
        libc::signal(libc::SIGUSR1, noop as *const () as libc::sighandler_t);
    }

    // 3. Deliver SIGUSR1 to every thread of this process (except the
    //    current one) three times.
    extern "C" {
        static mach_task_self_: u32;
        fn task_threads(task: u32, list: *mut *mut u32, count: *mut u32) -> i32;
    }
    for _ in 0..3 {
        unsafe {
            let mut list: *mut u32 = std::ptr::null_mut();
            let mut count: u32 = 0;
            let kr = task_threads(mach_task_self_, &mut list, &mut count);
            assert_eq!(kr, 0, "task_threads failed with kern_return_t {kr}");
            let ports = std::slice::from_raw_parts(list, count as usize);
            for &port in ports {
                let t = libc::pthread_from_mach_thread_np(port);
                if t == 0 || t == libc::pthread_self() {
                    continue;
                }
                libc::pthread_kill(t, libc::SIGUSR1);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 4. The engine still works after all those signals.
    let second = trivial_script().await;
    assert_eq!(second, json!({ "hello": "world" }));
}
