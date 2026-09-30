fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // FFmpeg's Media Foundation encoder objects reference interface GUIDs
        // provided by the Windows SDK. Its pkg-config metadata omits these
        // system libraries when consumed through ffmpeg-sys-next.
        // IID_IMF* GUIDs come from mfuuid; IID_ICodecAPI comes from strmiids.
        println!("cargo:rustc-link-lib=mfuuid");
        println!("cargo:rustc-link-lib=strmiids");
    }
}
