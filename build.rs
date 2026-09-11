fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // FFmpeg's Media Foundation encoder objects reference interface GUIDs
        // provided by the Windows SDK. Its pkg-config metadata omits this
        // system library when consumed through ffmpeg-sys-next.
        println!("cargo:rustc-link-lib=mfuuid");
    }
}
