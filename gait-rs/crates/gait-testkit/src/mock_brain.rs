// Test-only mock brain. Spawned by the spine in place of `gait __brain`.
// Ignores all argv except `--gait-dir <DIR>`; replays a prebaked frame blob.
use std::io::{Read, Write};

use gait_core::proto::gait::sandbox::v1::{signal, BrainInit, Signal};
use prost::Message;

fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn read_one_frame(stdin: &mut impl Read) -> Option<Signal> {
    // [version u8][len u32 BE][len bytes].
    let mut hdr = [0u8; 5];
    if stdin.read_exact(&mut hdr).is_err() {
        return None;
    }
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body).ok()?;
    Signal::decode(body.as_slice()).ok()
}

fn current_scope(init: Option<BrainInit>) -> (u32, u32) {
    // The spine puts the current turn's durable history in BrainInit.  In
    // particular, this is the authoritative run coordinate after a REPL
    // continues a session; invocation count is not a run number (slash
    // commands and other callers can advance those independently).
    match init {
        Some(init) => init
            .events
            .iter()
            .filter_map(|event| Some((event.run?, event.turn?)))
            .max()
            .unwrap_or((0, 0)),
        None => (0, 0),
    }
}

pub fn run() {
    let dir = arg_value("--gait-dir").expect("mock_brain: --gait-dir required");
    let dir = std::path::PathBuf::from(dir);

    let mut stdin = std::io::stdin();
    let init = read_one_frame(&mut stdin).and_then(|signal| match signal.body {
        Some(signal::Body::BrainInit(init)) => Some(init),
        _ => None,
    }); // consume BrainInit so the spine's write completes
    let (run, turn) = current_scope(init);

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
    let numbered_fixture = numbered.exists();
    let frames_path = if numbered_fixture {
        numbered
    } else {
        dir.join("mock_frames.bin")
    };
    let mut stdout = std::io::stdout();
    if let Ok(bytes) = std::fs::read(&frames_path) {
        // Unnumbered fixtures are legacy single-turn fixtures and already use
        // the protocol coordinates expected by their caller. Only the REPL's
        // numbered multi-turn fixtures need rebasing to BrainInit's scope.
        if !numbered_fixture {
            let _ = stdout.write_all(&bytes);
            let _ = stdout.flush();
        } else {
            // Frame fixtures contain envelopes stamped with the first run because
            // they are written before the brain is launched. Rewrite their run
            // coordinate to the one supplied in BrainInit so the normal lifecycle
            // validator sees the mock's events as part of this turn.
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            let mut rewritten = Vec::new();
            while let Some(frame) = read_one_frame(&mut cursor) {
                if let Some(signal::Body::Append(mut append)) = frame.body {
                    if let Some(event) = append.event.as_mut() {
                        event.run = Some(run);
                        event.turn = Some(turn);
                    }
                    let signal = Signal {
                        corr_id: frame.corr_id,
                        body: Some(signal::Body::Append(append)),
                    };
                    let encoded = signal.encode_to_vec();
                    rewritten.push(1);
                    rewritten.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                    rewritten.extend_from_slice(&encoded);
                } else {
                    let encoded = frame.encode_to_vec();
                    rewritten.push(1);
                    rewritten.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
                    rewritten.extend_from_slice(&encoded);
                }
            }
            let _ = stdout.write_all(&rewritten);
            let _ = stdout.flush();
        }
    }

    // mock_hang_ms: sleep AFTER sending frames (brain sends output then hangs).
    // Use this to arm the watchdog then go silent.
    if let Ok(ms) = std::fs::read_to_string(dir.join("mock_hang_ms")) {
        if let Ok(ms) = ms.trim().parse::<u64>() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}
