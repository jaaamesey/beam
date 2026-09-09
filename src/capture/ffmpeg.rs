use anyhow::{Context as _, Result, bail};
use ffmpeg_next as ffmpeg;
use ffmpeg::{Dictionary, Rational, codec, encoder, format, frame};
use std::{sync::{Arc, OnceLock}, time::Duration};
use tokio::sync::mpsc;

use super::{Codec, EncodedFrame, LatestFrame, RawFrame, FPS, next_frame, scale_bgra_to_i420};

static FFMPEG_INIT: OnceLock<bool> = OnceLock::new();

pub(crate) fn hardware_codecs() -> super::HardwareCodecAvailability {
    let _ = initialize();
    super::HardwareCodecAvailability {
        h264: hardware_encoder_available("h264"),
        h265: hardware_encoder_available("hevc"),
        av1: hardware_encoder_available("av1"),
    }
}

pub(super) fn encode(
    codec: Codec,
    frames: LatestFrame,
    sender: mpsc::Sender<EncodedFrame>,
    first: Arc<RawFrame>,
    width: usize,
    height: usize,
    bitrate: u32,
) -> Result<()> {
    initialize().context("initialize FFmpeg")?;

    let mut encoder = Encoder::open(codec, width, height, bitrate)
        .context("open FFmpeg video encoder")?;
    tracing::info!(codec = ?codec, encoder = encoder.name, hardware = encoder.hardware, "using FFmpeg video encoder");

    let mut previous_time = None;
    let mut source = Some(first);
    loop {
        let Some(source) = source.take().or_else(|| next_frame(&frames)) else { return Ok(()) };
        let video = yuv_frame(&source, width, height)?;
        encoder.context.send_frame(&video).context("queue FFmpeg video frame")?;
        let duration = previous_time
            .and_then(|time| source.captured_at.duration_since(time).ok())
            .unwrap_or(Duration::from_secs_f64(1.0 / FPS as f64))
            .clamp(Duration::from_millis(1), Duration::from_secs(1));
        previous_time = Some(source.captured_at);

        let mut packet = ffmpeg::Packet::empty();
        while encoder.context.receive_packet(&mut packet).is_ok() {
            let Some(data) = packet.data() else { continue };
            if sender.blocking_send(EncodedFrame { data: data.to_vec(), duration }).is_err() {
                return Ok(());
            }
            packet = ffmpeg::Packet::empty();
        }
    }
}

fn initialize() -> Result<()> {
    if *FFMPEG_INIT.get_or_init(|| ffmpeg::init().is_ok()) {
        Ok(())
    } else {
        bail!("FFmpeg initialization failed")
    }
}

fn hardware_encoder_available(codec: &str) -> bool {
    // FFmpeg exposes a hardware encoder only when the corresponding backend
    // was compiled in. Opening a tiny throwaway context is unreliable: some
    // drivers reject probe dimensions or require device-specific setup that
    // is only available on the capture thread. Runtime failures are still
    // handled by Encoder::open(), which logs and falls back to software.
    hardware_names(codec)
        .iter()
        .any(|name| encoder::find_by_name(name).is_some())
}

struct Encoder {
    context: encoder::Video,
    name: &'static str,
    hardware: bool,
}

impl Encoder {
    fn open(codec: Codec, width: usize, height: usize, bitrate: u32) -> Result<Self> {
        let (hardware, software): (&[&str], &[&str]) = match codec {
            Codec::H264 => (hardware_names("h264"), &["libx264"]),
            Codec::H265 => (hardware_names("hevc"), &["libx265"]),
            Codec::Av1 => (hardware_names("av1"), &["libsvtav1", "libaom-av1"]),
        };
        let mut last_error = None;
        for &name in hardware.iter().chain(software.iter()) {
            let Some(found) = encoder::find_by_name(name) else { continue };
            let mut video = codec::context::Context::new_with_codec(found)
                .encoder().video().context("create FFmpeg video context")?;
            video.set_width(width as u32);
            video.set_height(height as u32);
            video.set_format(format::Pixel::YUV420P);
            video.set_time_base(Rational::new(1, FPS as i32));
            video.set_frame_rate(Some(Rational::new(FPS as i32, 1)));
            video.set_bit_rate(bitrate as usize);
            video.set_gop(FPS * 2);
            video.set_max_b_frames(0);
            video.set_flags(codec::Flags::LOW_DELAY);
            let mut options = Dictionary::new();
            set_low_latency_options(name, &mut options);
            match video.open_with(options) {
                Ok(context) => {
                    if !hardware.contains(&name) {
                        tracing::warn!(codec = ?codec, encoder = name, "hardware FFmpeg encoder unavailable; falling back to software");
                    }
                    return Ok(Self { context, name, hardware: hardware.contains(&name) });
                }
                Err(error) => {
                    if hardware.contains(&name) {
                        tracing::warn!(codec = ?codec, encoder = name, error = ?error, "hardware FFmpeg encoder could not be opened");
                    }
                    last_error = Some((name, error));
                }
            }
        }
        if let Some((name, error)) = last_error {
            bail!("no usable FFmpeg encoder for {codec:?}; last attempt {name}: {error:?}");
        }
        bail!("no FFmpeg encoder was compiled for {codec:?}")
    }
}

fn set_low_latency_options(name: &str, options: &mut Dictionary) {
    match name {
        "libx264" => {
            options.set("preset", "ultrafast");
            options.set("tune", "zerolatency");
        }
        "libsvtav1" => {
            // SVT presets run from 0 (slowest) through 13 (fastest).
            options.set("preset", "13");
            options.set("svtav1-params", "la_depth=0:scd=0");
        }
        "libaom-av1" => {
            options.set("usage", "realtime");
            options.set("cpu-used", "8");
            options.set("lag-in-frames", "0");
            options.set("row-mt", "1");
            options.set("end-usage", "cbr");
        }
        name if name.ends_with("_nvenc") => {
            options.set("preset", "p1");
            options.set("tune", "ull");
            options.set("rc", "cbr");
            options.set("zerolatency", "1");
        }
        name if name.ends_with("_amf") => {
            options.set("usage", "ultralowlatency");
            options.set("quality", "speed");
        }
        _ => {}
    }
}

fn hardware_names(codec: &str) -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    { return match codec { "h264" => &["h264_videotoolbox"], "hevc" => &["hevc_videotoolbox"], _ => &["av1_videotoolbox"] }; }
    #[cfg(target_os = "windows")]
    { return match codec { "h264" => &["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf"], "hevc" => &["hevc_nvenc", "hevc_amf", "hevc_qsv", "hevc_mf"], _ => &["av1_nvenc", "av1_amf", "av1_qsv"] }; }
    #[cfg(target_os = "linux")]
    { return match codec { "h264" => &["h264_nvenc", "h264_vaapi", "h264_qsv"], "hevc" => &["hevc_nvenc", "hevc_vaapi", "hevc_qsv"], _ => &["av1_nvenc", "av1_vaapi", "av1_qsv"] }; }
    #[allow(unreachable_code)]
    &[]
}

fn yuv_frame(source: &RawFrame, width: usize, height: usize) -> Result<frame::Video> {
    let (y, u, v) = scale_bgra_to_i420(&source.bgra, source.width, source.height, source.stride, width, height)?;
    let mut frame = frame::Video::new(format::Pixel::YUV420P, width as u32, height as u32);
    copy_plane(&mut frame, 0, &y, width, height);
    copy_plane(&mut frame, 1, &u, width / 2, height / 2);
    copy_plane(&mut frame, 2, &v, width / 2, height / 2);
    Ok(frame)
}

fn copy_plane(frame: &mut frame::Video, plane: usize, source: &[u8], width: usize, height: usize) {
    let stride = frame.stride(plane);
    let target = frame.data_mut(plane);
    for row in 0..height {
        let source_start = row * width;
        let target_start = row * stride;
        target[target_start..target_start + width]
            .copy_from_slice(&source[source_start..source_start + width]);
    }
}
