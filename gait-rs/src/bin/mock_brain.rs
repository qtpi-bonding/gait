//! Test-only mock brain. Spawned by the spine in place of `gait __brain`.
//! Ignores all argv except `--gait-dir <DIR>`; replays a prebaked frame blob.
use std::io::{Read, Write};

fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn drain_one_frame(stdin: &mut impl Read) {
    // [version u8][len u32 BE][len bytes] — discard exactly one frame.
    let mut hdr = [0u8; 5];
    if stdin.read_exact(&mut hdr).is_err() {
        return;
    }
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut body = vec![0u8; len];
    let _ = stdin.read_exact(&mut body);
}

fn main() {
    let dir = arg_value("--gait-dir").expect("mock_brain: --gait-dir required");
    let dir = std::path::PathBuf::from(dir);

    let mut stdin = std::io::stdin();
    drain_one_frame(&mut stdin); // consume BrainInit so the spine's write completes

    // mock_delay_ms: sleep BEFORE sending frames (brain appears silent during delay).
    if let Ok(ms) = std::fs::read_to_string(dir.join("mock_delay_ms")) {
        if let Ok(ms) = ms.trim().parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }

    // Step-aware frame selection: invocation N (0-based) replays
    // `mock_frames.<N>.bin` if present, else falls back to `mock_frames.bin`.
    // The step index lives in `mock_step`, which this brain reads then increments.
    // Single-invocation tests never create numbered files, so every step falls
    // back to `mock_frames.bin` — behavior is unchanged for them.
    let step: u32 = std::fs::read_to_string(dir.join("mock_step"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let _ = std::fs::write(dir.join("mock_step"), (step + 1).to_string());
    let numbered = dir.join(format!("mock_frames.{step}.bin"));
    let frames_path = if numbered.exists() {
        numbered
    } else {
        dir.join("mock_frames.bin")
    };

    let mut stdout = std::io::stdout();
    if let Ok(bytes) = std::fs::read(&frames_path) {
        let _ = stdout.write_all(&bytes);
        let _ = stdout.flush();
    }

    // mock_hang_ms: sleep AFTER sending frames (brain sends output then hangs).
    // Use this to arm the watchdog then go silent.
    if let Ok(ms) = std::fs::read_to_string(dir.join("mock_hang_ms")) {
        if let Ok(ms) = ms.trim().parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}
