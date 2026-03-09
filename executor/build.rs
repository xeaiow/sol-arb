fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Prefer homebrew protoc on macOS if ~/.local/bin has a broken binary
    if std::env::var("PROTOC").is_err() {
        let homebrew_protoc = "/opt/homebrew/bin/protoc";
        if std::path::Path::new(homebrew_protoc).exists() {
            std::env::set_var("PROTOC", homebrew_protoc);
        }
    }

    tonic_build::configure()
        .build_server(false)
        .compile_protos(
            &[
                "proto/packet.proto",
                "proto/shared.proto",
                "proto/bundle.proto",
                "proto/auth.proto",
                "proto/searcher.proto",
            ],
            &["proto/"],
        )?;
    Ok(())
}
