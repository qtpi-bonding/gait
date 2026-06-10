use std::path::PathBuf;

fn main() {
    capture_git_hash();
    compile_protos();
}

/// Capture git hash at compile time for attribution in the system prompt.
fn capture_git_hash() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GAIT_BUILD_GIT_HASH={}", hash);
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/");
}

/// Compile the proto log format to Rust (prost) + JSON serde impls (pbjson).
///
/// proto is the source of truth for the log format; the wire is JSON (proto3
/// canonical / camelCase). The well-known types (Struct/Value/Duration) are
/// mapped to `pbjson-types` so they carry the same canonical-JSON serde.
/// A vendored `protoc` keeps the build zero-config.
fn compile_protos() {
    // proto/ lives one level up from gait-rs/ (the crate root).
    let proto_dir = PathBuf::from("../proto");
    let protos = [proto_dir.join("log.proto"), proto_dir.join("state.proto")];
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    // Vendored protoc — no system install required.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", &protoc);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let descriptor_path = out_dir.join("proto_descriptor.bin");

    let mut config = prost_build::Config::new();
    config
        .file_descriptor_set_path(&descriptor_path)
        // Generate the WKTs via pbjson-types (canonical JSON), not prost-types.
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types");

    config
        .compile_protos(&protos, &[&proto_dir])
        .expect("compile protos");

    let descriptor_set = std::fs::read(&descriptor_path).expect("read descriptor set");
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)
        .expect("register descriptors")
        .build(&[".gait"])
        .expect("build pbjson serde");
}
