use std::path::PathBuf;

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    let proto_dir = PathBuf::from("../../../proto");
    let proto = proto_dir.join("gait/events/v1/events.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    let descriptor = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("events_descriptor.bin");
    let includes = [proto_dir, concord_telemetry_proto::proto_include_dir()];
    let mut config = prost_build::Config::new();
    config
        .file_descriptor_set_path(&descriptor)
        .message_attribute(".gait.events", "#[derive(::concord_telemetry::Event)]")
        .message_attribute(
            ".gait.events",
            "#[concord_event(package = \"gait.events.v1\")]",
        )
        .extern_path(
            ".concord_telemetry.v1",
            "::concord_telemetry::proto::v1",
        )
        .compile_protos(&[proto], &includes)
        .expect("compile event proto");

    let descriptor_set = std::fs::read(&descriptor).expect("read event descriptor");
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_set)
        .expect("register event descriptors")
        .extern_path(
            ".concord_telemetry.v1",
            "::concord_telemetry::proto::v1",
        )
        .build(&[".gait.events"])
        .expect("build event serde");
}
