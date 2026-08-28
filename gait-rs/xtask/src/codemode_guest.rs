use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    // CARGO_MANIFEST_DIR is gait-rs/xtask; the workspace root is its parent.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn run(check: bool) -> Result<(), Box<dyn std::error::Error>> {
    let root = root();
    // This is a checked-in upstream artifact, not a build output. Do not use
    // an environment override: reproducibility requires one pinned input.
    // gait-codemode-js cannot produce it: rquickjs's C build cannot target
    // wasm32-unknown-unknown (no libc headers on that target). See that
    // crate's Cargo.toml and vendor/quickjs-wasi/*/README.md.
    let source = root.join("vendor/quickjs-wasi/3.6.0/quickjs.wasm");
    if !source.is_file() {
        return Err(format!(
            "QuickJS-WASI vendor artifact not found at {}",
            source.display()
        )
        .into());
    }
    let destination = root.join("crates/gait-codemode/guest/gait_codemode_js.wasm");
    let manifest = destination.with_extension("wasm.sha256");

    let source_bytes = fs::read(&source)?;
    if source_bytes.len() < 8 || &source_bytes[..4] != b"\0asm" {
        return Err("QuickJS-WASI vendor input is not a WebAssembly module".into());
    }
    let source_hash = hex::encode(Sha256::digest(&source_bytes));

    if !check {
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::copy(&source, &destination)?;
        fs::write(&manifest, format!("source {source_hash}\n"))?;
        return Ok(());
    }

    if !destination.is_file() {
        return Err("checked-in guest artifact is missing".into());
    }
    let destination_bytes = fs::read(&destination)?;
    if destination_bytes != source_bytes {
        return Err(
            "checked-in guest artifact does not match the vendored QuickJS-WASI source".into(),
        );
    }
    let expected = format!("source {source_hash}\n");
    if fs::read_to_string(&manifest).unwrap_or_default() != expected {
        return Err("checked-in guest artifact manifest is stale".into());
    }
    Ok(())
}
