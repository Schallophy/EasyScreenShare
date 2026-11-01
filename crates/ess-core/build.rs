use std::path::PathBuf;

fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);

    let proto = PathBuf::from("proto/ess.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    prost_build::Config::new()
        .compile_protos(&[proto], &["proto"])
        .expect("ess.proto must compile");
}
