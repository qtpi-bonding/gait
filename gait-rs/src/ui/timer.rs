use std::io::Write;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Timer {
    stop_flag: Arc<AtomicBool>,
    /// Current status text shown after the spinner frame (e.g. "thinking…" or "read src/main.rs").
    /// Updated from the main thread; read by the background thread.
    pub(crate) activity: Arc<Mutex<String>>,
    handle: Option<JoinHandle<()>>,
}

fn format_secs(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{}s", s) } else { format!("{}m {}s", s / 60, s % 60) }
}

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl Timer {
    /// Start the status indicator.  `turn_start` is the absolute start of the
    /// turn so the displayed time always reflects total turn elapsed.
    pub fn start(turn_start: Instant) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let activity = Arc::new(Mutex::new("thinking…".to_string()));
        let flag = stop_flag.clone();
        let act = activity.clone();
        let handle = thread::spawn(move || {
            let mut frame = 0usize;
            loop {
                if flag.load(Ordering::Relaxed) {
                    eprint!("\r\x1b[2K");
                    let _ = std::io::stderr().flush();
                    return;
                }
                let current = act.lock().unwrap().clone();
                eprint!("\r  {} {} ({})", FRAMES[frame % FRAMES.len()], current, format_secs(turn_start.elapsed()));
                let _ = std::io::stderr().flush();
                frame += 1;
                thread::sleep(Duration::from_millis(100));
            }
        });
        Self { stop_flag, activity, handle: Some(handle) }
    }

    /// Update the displayed activity text (e.g. switch to tool name or back to "thinking…").
    pub fn set_activity(&self, text: &str) {
        *self.activity.lock().unwrap() = text.to_string();
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
    fn format_secs_zero() {
        assert_eq!(format_secs(Duration::from_secs(0)), "0s");
    }

    #[test]
    fn format_secs_nonzero() {
        assert_eq!(format_secs(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn format_secs_minutes() {
        assert_eq!(format_secs(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_secs(Duration::from_secs(288)), "4m 48s");
    }

    #[test]
    fn timer_starts_and_stops_without_panic() {
        let mut t = Timer::start(Instant::now());
        t.stop();
    }

    #[test]
    fn timer_stop_is_idempotent() {
        let mut t = Timer::start(Instant::now());
        t.stop();
        t.stop();
    }

    #[test]
    fn timer_drops_without_hang() {
        let _t = Timer::start(Instant::now());
    }

    #[test]
    fn set_activity_updates_label() {
        let t = Timer::start(Instant::now());
        t.set_activity("read src/main.rs");
        assert_eq!(*t.activity.lock().unwrap(), "read src/main.rs");
        t.set_activity("thinking…");
        assert_eq!(*t.activity.lock().unwrap(), "thinking…");
    }
}
