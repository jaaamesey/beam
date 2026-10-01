fn main() {
    // Release builds embed the web UI (src/assets.rs), so don't produce one without it.
    println!("cargo:rerun-if-changed=web/dist/index.html");
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        assert!(
            std::path::Path::new("web/dist/index.html").exists(),
            "web/dist is missing; run `pnpm --dir web build` first"
        );
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // FFmpeg's Media Foundation encoder objects reference interface GUIDs
        // provided by the Windows SDK. Its pkg-config metadata omits these
        // system libraries when consumed through ffmpeg-sys-next.
        // IID_IMF* GUIDs come from mfuuid; IID_ICodecAPI comes from strmiids.
        println!("cargo:rustc-link-lib=mfuuid");
        println!("cargo:rustc-link-lib=strmiids");

        // CI puts the ViGEmBus installer here so it can be embedded in beam.exe.
        // Without it, embed nothing and the settings page links to the download.
        println!("cargo:rerun-if-changed=drivers/vigembus.exe");
        println!("cargo:rerun-if-env-changed=BEAM_REQUIRE_INSTALLER");
        let embedded = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("vigembus.exe");
        if std::fs::copy("drivers/vigembus.exe", &embedded).is_err() {
            assert!(
                std::env::var_os("BEAM_REQUIRE_INSTALLER").is_none(),
                "drivers/vigembus.exe is missing"
            );
            std::fs::write(&embedded, []).unwrap();
        }
    }
}
