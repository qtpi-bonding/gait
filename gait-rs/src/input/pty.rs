use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub enum CaptureResult {
    Output(String, Option<i32>),
    Interactive,
}

/// Detect entry into the terminal alternate screen without depending on PTY
/// behavior. The PTY wrapper below uses this same parser for live capture.
fn contains_alt_screen_escape(bytes: &[u8]) -> bool {
    bytes
        .windows(b"\x1b[?1049h".len())
        .any(|window| window == b"\x1b[?1049h")
}

pub fn run_and_capture_pty_raw(cmd: &str, cwd: &std::path::Path) -> CaptureResult {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => {
            let (out, code) = run_and_capture_simple(cmd, cwd);
            return CaptureResult::Output(out, code);
        }
    };

    let mut builder = CommandBuilder::new("sh");
    builder.args(["-c", cmd]);
    builder.cwd(cwd);

    let mut child = match pair.slave.spawn_command(builder) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31m[!] error: {}\x1b[0m", e);
            return CaptureResult::Output(String::new(), None);
        }
    };

    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return CaptureResult::Output(String::new(), None),
    };

    const SCAN_LIMIT: usize = 4096;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    while buf.len() < SCAN_LIMIT {
        match reader.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if contains_alt_screen_escape(&buf) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CaptureResult::Interactive;
                }
            }
        }
    }

    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    let code = child.wait().ok().map(|s| s.exit_code() as i32);

    let all = [buf, rest].concat();
    let output = String::from_utf8_lossy(&all).into_owned();
    for line in output.lines() {
        println!("{}", line);
    }
    CaptureResult::Output(output, code)
}

/// Run `cmd` via `sh -c`, merging stderr into stdout. Used by expand_shell_marker.
fn run_and_capture_simple(cmd: &str, cwd: &std::path::Path) -> (String, Option<i32>) {
    let merged = format!("{} 2>&1", cmd);
    let mut child = match Command::new("sh")
        .args(["-c", &merged])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31m[!] error: {}\x1b[0m", e);
            return (String::new(), None);
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = BufReader::new(stdout);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) => {
                println!("{}", l);
                lines.push(l);
            }
            Err(_) => break,
        }
    }
    let code = child.wait().ok().and_then(|s| s.code());
    (lines.join("\n"), code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_capture_normal_command() {
        let result = run_and_capture_pty_raw("echo hello", std::path::Path::new("."));
        match result {
            CaptureResult::Output(out, code) => {
                assert_eq!(out.trim(), "hello");
                assert_eq!(code, Some(0));
            }
            CaptureResult::Interactive => panic!("echo should not be Interactive"),
        }
    }

    #[test]
    fn alt_screen_escape_detector_is_deterministic() {
        assert!(contains_alt_screen_escape(b"prefix\x1b[?1049hsuffix"));
        assert!(!contains_alt_screen_escape(b"\x1b[?1049l"));
        assert!(!contains_alt_screen_escape(b"ordinary output"));
    }

    #[test]
    fn alt_screen_escape_detector_handles_split_input() {
        let mut bytes = b"prefix\x1b[?1049".to_vec();
        bytes.extend_from_slice(b"hsuffix");
        assert!(contains_alt_screen_escape(&bytes));
    }

    #[test]
    #[ignore = "requires a functioning host PTY"]
    fn pty_capture_detects_alt_screen() {
        let result = run_and_capture_pty_raw(r#"printf "\033[?1049h""#, std::path::Path::new("."));
        assert!(matches!(result, CaptureResult::Interactive));
    }
}
