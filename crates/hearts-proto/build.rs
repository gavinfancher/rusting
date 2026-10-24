// Runs before compiling this crate: turns proto/hearts.proto into Rust code
// (message structs + gRPC client/server traits) using `protoc`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/hearts.proto");
    tonic_prost_build::configure()
        .compile_protos(&["../../proto/hearts.proto"], &["../../proto"])?;
    Ok(())
}
