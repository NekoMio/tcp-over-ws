fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Check if we're cross-compiling on Linux
    let target = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let is_cross_compiling = target != host;
    let is_linux_host = host.contains("linux");

    // Only use embedded protoc for Linux cross-compilation
    let use_embedded_protoc = is_cross_compiling && is_linux_host && cfg!(feature = "protobuf-src");

    if use_embedded_protoc {
        #[cfg(feature = "protobuf-src")]
        {
            std::env::set_var("PROTOC", protobuf_src::protoc());
            println!("cargo:warning=Using embedded protoc for Linux cross-compilation");
        }
    } else {
        println!("cargo:warning=Using system protoc");
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile(&["proto/tunnel.proto"], &["proto"])?;
    Ok(())
}
