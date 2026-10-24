use std::path::PathBuf;

fn main() {
    // proto/ lives three levels up from crates/gait-proto/ (up to gait repo root).
    let proto_dir = PathBuf::from("../../../proto");
    let protos = [
        proto_dir.join("log.proto"),
        proto_dir.join("state.proto"),
        proto_dir.join("workflow.proto"),
        proto_dir.join("sandbox.proto"),
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", &protoc);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let descriptor_path = out_dir.join("proto_descriptor.bin");

    let mut config = prost_build::Config::new();
    config
        .file_descriptor_set_path(&descriptor_path)
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .extern_path(".identitycrypto.v1", "::identity_crypto::pb");

    config
        .compile_protos(
            &protos,
            &[&proto_dir, &PathBuf::from(identity_crypto::proto_include_dir())],
        )
        .expect("compile protos");

    let descriptor_set = std::fs::read(&descriptor_path).expect("read descriptor set");
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)
        .expect("register descriptors")
        .extern_path(".identitycrypto.v1", "::identity_crypto::pb")
        .build(&[".gait"])
        .expect("build pbjson serde");
}
