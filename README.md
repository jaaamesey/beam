## Run

Requirements: Rust, pnpm, macOS 12.3+, and Screen Recording permission for the
terminal (or packaged app) running Beam.

```sh
pnpm --dir web install
pnpm --dir web build
cargo run --release
```

The bundled FFmpeg build requires the x264 and x265 development libraries to
be discoverable through `pkg-config`; release builds should also provide
SVT-AV1 to enable the AV1 software fallback.

Beam opens `http://127.0.0.1:9470/settings` on launch. The settings API accepts
loopback connections only. Other devices on the LAN can open
`http://HOST_IP:9470/` and connect with the configured password.

The default codec is H.264. All video encoding goes through the bundled GPL
FFmpeg build. Beam probes hardware encoders first (VideoToolbox, NVENC, AMF,
QSV, or VAAPI, depending on the platform) and falls back to software x264,
x265, or SVT-AV1 when available. H.265 and AV1 can be selected from the
stream settings UI.

This iteration intentionally uses HTTP and is suitable only for a trusted LAN:
WebRTC media is encrypted, but the login request is not protected until HTTPS
is added.
