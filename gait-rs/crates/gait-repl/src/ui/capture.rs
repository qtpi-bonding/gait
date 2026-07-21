//! Process-wide stderr fd capture for the dev-harness. `ReplRenderer::paint`
//! writes real ANSI bytes straight to `eprint!`, unconditionally (it doesn't
//! check `is_terminal`) — this captures those bytes without needing a pty.
//!
//! `dup2`-ing fd 2 repoints stderr for the *entire process*, not per-thread.
//! Every caller MUST serialize (`#[serial]`, from `serial_test`) against any
//! other test using this or writing to stderr concurrently.
#![cfg(feature = "dev-harness")]

use std::io::Read;
use std::os::unix::io::{FromRawFd, RawFd};

pub(crate) struct Capture {
    saved_fd: Option<RawFd>,
    drain: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl Capture {
    /// Repoints fd 2 (stderr) to a pipe and starts draining it on a background
    /// thread. Everything written to stderr from here until `end()` is called
    /// is captured instead of reaching the real terminal.
    pub(crate) fn begin() -> Self {
        let mut fds = [0 as RawFd; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let saved_fd = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved_fd >= 0, "dup(stderr) failed");

        let rc = unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) };
        assert_eq!(rc, libc::STDERR_FILENO, "dup2(write_fd, stderr) failed");
        // fd 2 now aliases write_fd; close our extra copy so fd 2 is the
        // pipe's *only* writer — otherwise the drain thread below never sees
        // EOF and `end()` hangs joining it.
        unsafe {
            libc::close(write_fd);
        }

        let drain = std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let mut buf = Vec::new();
            let _ = file.read_to_end(&mut buf);
            buf
        });

        Capture {
            saved_fd: Some(saved_fd),
            drain: Some(drain),
        }
    }

    fn restore(&mut self) {
        if let Some(fd) = self.saved_fd.take() {
            unsafe {
                libc::dup2(fd, libc::STDERR_FILENO);
                libc::close(fd);
            }
        }
    }

    /// Restores stderr and returns everything captured while this `Capture`
    /// was active.
    pub(crate) fn end(mut self) -> Vec<u8> {
        self.restore();
        self.drain
            .take()
            .expect("end() called twice")
            .join()
            .expect("capture drain thread panicked")
    }
}

impl Drop for Capture {
    /// If a scripted turn panics before `end()` runs, stderr must still be
    /// restored — otherwise every later write in this process silently
    /// vanishes into an orphaned pipe.
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn captures_bytes_written_to_stderr() {
        let capture = Capture::begin();
        eprint!("hello capture");
        let bytes = capture.end();
        assert_eq!(String::from_utf8_lossy(&bytes), "hello capture");
    }

    #[test]
    #[serial]
    fn sequential_captures_dont_leak_into_each_other() {
        let c1 = Capture::begin();
        eprint!("first");
        let first = c1.end();

        let c2 = Capture::begin();
        eprint!("second");
        let second = c2.end();

        assert_eq!(String::from_utf8_lossy(&first), "first");
        assert_eq!(String::from_utf8_lossy(&second), "second");
    }
}
