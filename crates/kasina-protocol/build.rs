fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc is available");

    // SAFETY: Cargo executes this single-threaded build script in its own process. The
    // variable is set before invoking prost and no other threads access the environment.
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    tonic_prost_build::configure()
        .compile_protos(&["../../proto/kasina.proto"], &["../../proto"])
        .expect("kasina protobuf schema compiles");

    println!("cargo:rerun-if-changed=../../proto/kasina.proto");
}
