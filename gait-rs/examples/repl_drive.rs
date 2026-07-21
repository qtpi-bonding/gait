//! Dev-only tool: drive the real REPL turn-engine path through a scripted,
//! multi-turn YAML conversation and dump the annotated transcript to a file.
//! Not part of any shipped binary — requires `--features gait-repl/dev-harness`.
//!
//! Usage:
//!   cargo run -p gait-core --features gait-repl/dev-harness --example repl_drive -- <script.yaml> [output.txt]
use gait_repl::dev_harness::{run_script_fresh, Script};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let script_path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: repl_drive <script.yaml> [output.txt]"))?;
    let out_path = args
        .next()
        .unwrap_or_else(|| format!("{script_path}.transcript.txt"));

    let yaml = std::fs::read_to_string(&script_path)?;
    let script: Script = serde_yaml::from_str(&yaml)?;

    // `CARGO_BIN_EXE_<name>` is only set by Cargo for integration tests and
    // benchmarks, not for examples (see Cargo's env-var docs) — so unlike
    // `tests/repl_drive_e2e.rs`, this example can't use `env!(...)` for it.
    // `mock_brain` is a `[[bin]]` of this same package, so its executable is
    // a sibling of this example's own binary: `target/debug/examples/repl_drive`
    // -> `target/debug/mock_brain`.
    let mut mock_brain = std::env::current_exe()?;
    mock_brain.pop(); // examples/
    mock_brain.pop(); // debug/ (or release/)
    mock_brain.push("mock_brain");
    anyhow::ensure!(
        mock_brain.exists(),
        "expected mock_brain binary at {mock_brain:?} (built alongside this example); \
         run `cargo build -p gait-core --bin mock_brain` first if it's missing"
    );

    let (_tempdir, transcript) = run_script_fresh(&script, &mock_brain).await?;

    std::fs::write(&out_path, &transcript)?;
    eprintln!("wrote {out_path}");
    Ok(())
}
