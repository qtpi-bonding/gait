use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Timer {
    stop_flag: Arc<AtomicBool>,
    /// Current status text shown after the spinner frame (e.g. "thinking…" or "read src/main.rs").
    /// Updated from the main thread; read by the background thread.
    pub(crate) activity: Arc<Mutex<String>>,
    /// Stable context rendered on the row below the live spinner.
    pub(crate) context: Arc<Mutex<String>>,
    pub(crate) handle: Option<JoinHandle<()>>,
    /// Shared with `ReplRenderer` — `paint` holds the lock for the full drain
    /// loop; the timer's tick uses `try_lock` and skips if paint owns it.
    pub(crate) paint_lock: Arc<Mutex<()>>,
}

fn format_secs(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn try_paint_tick<F>(lock: &Mutex<()>, paint: F) -> bool
where
    F: FnOnce(),
{
    let Ok(_guard) = lock.try_lock() else {
        return false;
    };
    paint();
    true
}

impl Timer {
    /// Start the status indicator.  `turn_start` is the absolute start of the
    /// turn so the displayed time always reflects total turn elapsed.
    pub fn start(turn_start: Instant) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let activity = Arc::new(Mutex::new("thinking…".to_string()));
        let context = Arc::new(Mutex::new(String::new()));
        let paint_lock = Arc::new(Mutex::new(()));
        let flag = stop_flag.clone();
        let act = activity.clone();
        let ctx = context.clone();
        let lock = paint_lock.clone();
        let handle = thread::spawn(move || {
            let mut frame = 0usize;
            loop {
                if flag.load(Ordering::Relaxed) {
                    if !ctx.lock().unwrap().is_empty() {
                        // The cursor is below the two-line overlay.
                        eprint!("\x1b[1A\r\x1b[2K\x1b[1A\r\x1b[2K");
                    } else {
                        eprint!("\r\x1b[2K");
                    }
                    let _ = std::io::stderr().flush();
                    return;
                }
                // Skip the tick if `paint` is mid-drain — otherwise the
                // status-line erase (`\r\x1b[2K`) lands inside a block.
                if !try_paint_tick(&lock, || {
                    let current = act.lock().unwrap().clone();
                    let context = ctx.lock().unwrap().clone();
                    if context.is_empty() {
                        eprint!(
                            "\r\x1b[2K  {} {} ({})",
                            FRAMES[frame % FRAMES.len()],
                            current,
                            format_secs(turn_start.elapsed())
                        );
                    } else {
                        // Redraw both managed rows, leaving the cursor beneath them.
                        eprint!(
                            "\x1b[1A\r\x1b[2K  {} {} ({})\r\n\x1b[2K  {}\r\n",
                            FRAMES[frame % FRAMES.len()],
                            current,
                            format_secs(turn_start.elapsed()),
                            context
                        );
                    }
                    let _ = std::io::stderr().flush();
                }) {
                    thread::sleep(Duration::from_millis(100));
                    frame += 1;
                    continue;
                }
                frame += 1;
                thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            stop_flag,
            activity,
            context,
            handle: Some(handle),
            paint_lock,
        }
    }

    /// Update the displayed activity text (e.g. switch to tool name or back to "thinking…").
    pub fn set_activity(&self, text: &str) {
        *self.activity.lock().unwrap() = text.to_string();
    }

    pub fn set_context(&self, text: &str) {
        *self.context.lock().unwrap() = text.to_string();
    }

    /// Erase the overlay while the caller owns `paint_lock`. The cursor ends
    /// at the row where the spinner began, ready for permanent output.
    pub(crate) fn erase_overlay(&self) {
        if self.context.lock().unwrap().is_empty() {
            eprint!("\r\x1b[2K");
        } else {
            eprint!("\x1b[1A\r\x1b[2K\x1b[1A\r\x1b[2K");
        }
    }

    /// Draw the overlay from its starting row, leaving the cursor below it.
    pub(crate) fn draw_overlay(&self, turn_start: Instant) {
        let context = self.context.lock().unwrap().clone();
        let activity = self.activity.lock().unwrap().clone();
        eprint!(
            "  {} {} ({})",
            FRAMES[0],
            activity,
            format_secs(turn_start.elapsed())
        );
        if !context.is_empty() {
            eprint!("\r\n  {}", context);
        }
        eprint!("\r\n");
    }

    /// Access the lock shared with `ReplRenderer` so `paint` can hold it.
    pub fn paint_lock(&self) -> Arc<Mutex<()>> {
        self.paint_lock.clone()
    }

    #[cfg(test)]
    fn try_tick<F>(&self, paint: F) -> bool
    where
        F: FnOnce(),
    {
        try_paint_tick(&self.paint_lock, paint)
    }

    pub fn stop(&mut self) {
        if self.handle.is_none() {
            return;
        }
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
        let mut t = t;
        t.stop();
    }

    #[test]
    fn paint_lock_is_shared_across_clones() {
        let mut t = Timer::start(Instant::now());
        let lock = t.paint_lock();
        // Holding the lock here proves `paint_lock()` returns a usable handle.
        let _guard = lock.lock().unwrap();
        t.stop();
    }

    #[test]
    fn timer_holds_paint_lock_during_tick_write() {
        let mut t = Timer::start(Instant::now());
        let lock = t.paint_lock();
        let mut observed_locked = false;

        assert!(t.try_tick(|| {
            observed_locked = lock.try_lock().is_err();
        }));
        assert!(
            observed_locked,
            "the timer must hold paint_lock while emitting a tick"
        );
        t.stop();
    }
}
