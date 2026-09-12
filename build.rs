fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // The static FFmpeg package's pkg-config metadata does not propagate
        // the Windows SDK libraries used by its Media Foundation/QSV objects
        // to ffmpeg-sys-next. Link them explicitly so the final executable,
        // rather than just the FFmpeg archives, resolves those symbols.
        for library in ["bcrypt", "mfplat", "mfuuid", "ole32", "strmiids"] {
            println!("cargo:rustc-link-lib={library}");
        }
    }
}
