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

// Overlay cursor protocol. With a context row the overlay is two rows (spinner
// at row R, context at R+1) and the cursor rests at R+2, below it; without one
// it is a single row and the cursor rests at the end of row R. Draw, tick and
// erase must all agree on that, or the overlay drifts down a row per tick.

/// Erase the overlay from its resting cursor position, leaving the cursor on
/// the overlay's first row.
fn overlay_erase_seq(has_context: bool) -> &'static str {
    if has_context {
        "\x1b[1A\r\x1b[2K\x1b[1A\r\x1b[2K"
    } else {
        "\r\x1b[2K"
    }
}

/// First paint of the overlay, starting on its first row.
fn overlay_draw_seq(frame: &str, activity: &str, elapsed: &str, context: &str) -> String {
    let mut seq = format!("  {frame} {activity} ({elapsed})");
    if !context.is_empty() {
        seq.push_str(&format!("\r\n  {context}\r\n"));
    }
    seq
}

/// Repaint a two-row overlay in place: back up over both rows, rewrite them,
/// and leave the cursor below them again.
fn overlay_tick_seq(frame: &str, activity: &str, elapsed: &str, context: &str) -> String {
    format!("\x1b[2A\r\x1b[2K  {frame} {activity} ({elapsed})\r\n\x1b[2K  {context}\r\n")
}

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
                    let has_context = !ctx.lock().unwrap().is_empty();
                    eprint!("{}", overlay_erase_seq(has_context));
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
                        eprint!(
                            "{}",
                            overlay_tick_seq(
                                FRAMES[frame % FRAMES.len()],
                                &current,
                                &format_secs(turn_start.elapsed()),
                                &context,
                            )
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
        let has_context = !self.context.lock().unwrap().is_empty();
        eprint!("{}", overlay_erase_seq(has_context));
    }

    /// Draw the overlay from its starting row, leaving the cursor at its resting
    /// position (see the cursor protocol above).
    pub(crate) fn draw_overlay(&self, turn_start: Instant) {
        let context = self.context.lock().unwrap().clone();
        let activity = self.activity.lock().unwrap().clone();
        eprint!(
            "{}",
            overlay_draw_seq(
                FRAMES[0],
                &activity,
                &format_secs(turn_start.elapsed()),
                &context,
            )
        );
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

    /// Net rows the cursor moves down while `seq` is printed: +1 per `\r\n`,
    /// -n per `ESC [ n A`. Other escapes (erase-line) don't move it.
    fn cursor_row_delta(seq: &str) -> i32 {
        let mut delta = 0;
        let mut rest = seq;
        while let Some(i) = rest.find(['\r', '\x1b']) {
            rest = &rest[i..];
            if let Some(after) = rest.strip_prefix("\r\n") {
                delta += 1;
                rest = after;
            } else if let Some(after) = rest.strip_prefix("\x1b[") {
                let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                let tail = &after[digits.len()..];
                if let Some(t) = tail.strip_prefix('A') {
                    delta -= digits.parse::<i32>().unwrap_or(1);
                    rest = t;
                } else {
                    rest = tail;
                }
            } else {
                rest = &rest[1..];
            }
        }
        delta
    }

    #[test]
    fn two_row_overlay_does_not_drift_across_ticks() {
        let mut total = cursor_row_delta(&overlay_draw_seq("f", "thinking…", "0s", "ctx"));
        assert_eq!(total, 2, "draw leaves the cursor two rows below the overlay start");
        for _ in 0..50 {
            total += cursor_row_delta(&overlay_tick_seq("f", "thinking…", "1s", "ctx"));
        }
        assert_eq!(total, 2, "ticks must repaint in place, not creep down");
        total += cursor_row_delta(overlay_erase_seq(true));
        assert_eq!(total, 0, "erase returns the cursor to the overlay's first row");
    }

    #[test]
    fn tick_backs_up_over_both_rows() {
        let seq = overlay_tick_seq("f", "thinking…", "1s", "ctx");
        assert!(seq.starts_with("\x1b[2A\r\x1b[2K"));
        assert_eq!(seq.matches("\x1b[2K").count(), 2);
    }

    #[test]
    fn one_row_overlay_rests_on_its_row() {
        assert_eq!(cursor_row_delta(&overlay_draw_seq("f", "thinking…", "0s", "")), 0);
        assert_eq!(cursor_row_delta(overlay_erase_seq(false)), 0);
    }
}
