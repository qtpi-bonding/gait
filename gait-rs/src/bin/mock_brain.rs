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

    if let Ok(ms) = std::fs::read_to_string(dir.join("mock_delay_ms")) {
        if let Ok(ms) = ms.trim().parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }

    let mut stdout = std::io::stdout();
    if let Ok(bytes) = std::fs::read(dir.join("mock_frames.bin")) {
        let _ = stdout.write_all(&bytes);
    }
    let _ = stdout.flush();
}