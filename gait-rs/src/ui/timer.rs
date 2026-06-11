use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct Timer {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

fn format_elapsed(secs: u64) -> String {
    format!("  thinking… [{}s]", secs)
}

impl Timer {
    pub fn start() -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag = stop_flag.clone();
        let handle = thread::spawn(move || {
            let mut secs = 0u64;
            loop {
                eprint!("\r{}", format_elapsed(secs));
                let _ = std::io::stderr().flush();
                // Check flag every 50ms so stop() returns promptly.
                for _ in 0..20 {
                    if flag.load(Ordering::Relaxed) {
                        eprint!("\r\x1b[2K");
                        let _ = std::io::stderr().flush();
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                secs += 1;
            }
        });
        Self { stop_flag, handle: Some(handle) }
    }

    pub fn stop(&mut self) {
        if self.handle.is_none() { return; }
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_zero() {
        assert_eq!(format_elapsed(0), "  thinking… [0s]");
    }

    #[test]
    fn format_elapsed_nonzero() {
        assert_eq!(format_elapsed(42), "  thinking… [42s]");
    }

    #[test]
    fn timer_starts_and_stops_without_panic() {
        let mut t = Timer::start();
        t.stop();
    }

    #[test]
    fn timer_stop_is_idempotent() {
        let mut t = Timer::start();
        t.stop();
        t.stop();
    }

    #[test]
    fn timer_drops_without_hang() {
        let _t = Timer::start();
    }
}
