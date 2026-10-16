fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = std::path::Path::new("../proto");
    let protos = [
        proto_dir.join("compilation_caching_cas.proto"),
        proto_dir.join("compilation_caching_kv.proto"),
    ];
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&protos, &[proto_dir])?;
    Ok(())
}
