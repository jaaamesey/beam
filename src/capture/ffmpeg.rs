use anyhow::{Context as _, Result, bail};
use ffmpeg_next as ffmpeg;
use ffmpeg::{
    Dictionary, Rational, codec, encoder, format, frame,
    software::scaling::{context::Context as ScalingContext, flag::Flags},
};
use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

use super::{Codec, EncodedFrame, LatestFrame, RawFrame, FPS, next_frame};

static FFMPEG_INIT: OnceLock<bool> = OnceLock::new();

#[derive(Default)]
struct TimingTotals {
    frames: u64,
    conversion: Duration,
    submit: Duration,
    drain: Duration,
    egress: Duration,
}

impl TimingTotals {
    fn report(&mut self) {
        if self.frames == 0 {
            return;
        }
        let frames = self.frames as f64;
        tracing::info!(
            frames = self.frames,
            conversion_ms = self.conversion.as_secs_f64() * 1000.0 / frames,
            submit_ms = self.submit.as_secs_f64() * 1000.0 / frames,
            drain_ms = self.drain.as_secs_f64() * 1000.0 / frames,
            egress_ms = self.egress.as_secs_f64() * 1000.0 / frames,
            "video pipeline timing"
        );
        *self = Self::default();
    }
}

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
    stop: Arc<AtomicBool>,
    sender: mpsc::Sender<EncodedFrame>,
    first: Arc<RawFrame>,
    width: usize,
    height: usize,
    bitrate: u32,
) -> Result<()> {
    initialize().context("initialize FFmpeg")?;

    let mut encoder =
        Encoder::open(codec, width, height, bitrate).context("open FFmpeg video encoder")?;
    tracing::info!(codec = ?codec, encoder = encoder.name, hardware = encoder.hardware, "using FFmpeg video encoder");

    let (scaled_width, scaled_height) = scaled_dimensions(first.width, first.height, width, height);
    let mut scaler = ScalingContext::get(
        format::Pixel::BGRA,
        first.width as u32,
        first.height as u32,
        encoder.pixel,
        scaled_width as u32,
        scaled_height as u32,
        Flags::FAST_BILINEAR,
    )
    .context("create FFmpeg video scaler")?;
    let mut previous_time = None;
    let mut source = Some(first);
    let mut submissions: VecDeque<(i64, Instant)> = VecDeque::new();
    let mut next_pts = 0i64;
    let mut timings = TimingTotals::default();
    // FFmpeg encoders are pipelined: keep feeding frames and drain whatever
    // is ready. Waiting for one packet before feeding the next can deadlock
    // encoders that need multiple input frames before producing output.
    let mut primed = false;
    while !stop.load(Ordering::Relaxed) {
        let Some(source) = source.take().or_else(|| next_frame(&frames, &stop)) else {
            return Ok(());
        };
        let conversion_start = Instant::now();
        let mut video = yuv_frame(
            &source,
            width,
            height,
            scaled_width,
            scaled_height,
            &mut scaler,
            encoder.pixel,
        )?;
        timings.conversion += conversion_start.elapsed();
        video.set_pts(Some(next_pts));
        let duration = previous_time
            .and_then(|time| source.captured_at.duration_since(time).ok())
            .unwrap_or(Duration::from_secs_f64(1.0 / FPS as f64))
            .clamp(Duration::from_millis(1), Duration::from_secs(1));
        previous_time = Some(source.captured_at);
        submissions.push_back((next_pts, Instant::now()));
        next_pts += 1;
        let submit_start = Instant::now();
        encoder
            .context
            .send_frame(&video)
            .context("queue FFmpeg video frame")?;
        timings.submit += submit_start.elapsed();
        timings.frames += 1;
        if !drain_packets(&mut encoder.context, &mut submissions, &sender, &mut primed, duration, &mut timings)? {
            return Ok(());
        }
        if timings.frames >= FPS as u64 {
            timings.report();
        }
    }
    Ok(())
}

fn drain_packets(
    encoder: &mut encoder::Video,
    submissions: &mut VecDeque<(i64, Instant)>,
    sender: &mpsc::Sender<EncodedFrame>,
    primed: &mut bool,
    duration: Duration,
    timings: &mut TimingTotals,
) -> Result<bool> {
    let drain_start = Instant::now();
    let mut egress = Duration::ZERO;
    let mut packet = ffmpeg::Packet::empty();
    let mut had_packets = false;
    while encoder.receive_packet(&mut packet).is_ok() {
        let Some(data) = packet.data() else {
            packet = ffmpeg::Packet::empty();
            continue;
        };
        let pts = packet.pts();
        let drained_at = Instant::now();
        let index = pts.and_then(|pts| submissions.iter().position(|&(submission_pts, _)| submission_pts == pts));
        let entry = match index {
            Some(index) => submissions.remove(index),
            None => submissions.pop_front(),
        };
        let encode_duration = if *primed {
            entry
                .as_ref()
                .map(|&(_, submitted_at)| drained_at.duration_since(submitted_at))
                .unwrap_or(Duration::ZERO)
        } else {
            Duration::ZERO
        };
        let egress_start = Instant::now();
        if sender
            .blocking_send(EncodedFrame {
                data: data.to_vec(),
                duration,
                encode_duration,
            })
            .is_err()
        {
            return Ok(false);
        }
        egress += egress_start.elapsed();
        had_packets = true;
        packet = ffmpeg::Packet::empty();
    }
    timings.drain += drain_start.elapsed().saturating_sub(egress);
    timings.egress += egress;
    *primed |= had_packets;
    Ok(true)
}

fn scaled_dimensions(
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
) -> (usize, usize) {
    if width * source_height <= height * source_width {
        (width, (source_height * width / source_width).max(2) & !1)
    } else {
        ((source_width * height / source_height).max(2) & !1, height)
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
    pixel: format::Pixel,
}

impl Encoder {
    fn open(codec: Codec, width: usize, height: usize, bitrate: u32) -> Result<Self> {
        let (hardware, software): (&[&str], &[&str]) = match codec {
            Codec::H264 => (hardware_names("h264"), &["libx264"]),
            Codec::H265 => (hardware_names("hevc"), &["libx265"]),
            Codec::Av1 => (hardware_names("av1"), &["libsvtav1"]),
        };
        let mut last_error = None;
        for &name in hardware.iter().chain(software.iter()) {
            let Some(found) = encoder::find_by_name(name) else { continue };
            let mut video = codec::context::Context::new_with_codec(found)
                .encoder().video().context("create FFmpeg video context")?;
            video.set_width(width as u32);
            video.set_height(height as u32);
            let pixel = if hardware.contains(&name) {
                format::Pixel::NV12
            } else {
                format::Pixel::YUV420P
            };
            video.set_format(pixel);
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
                    return Ok(Self { context, name, hardware: hardware.contains(&name), pixel });
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
        "libx265" => {
            options.set("preset", "ultrafast");
            options.set("tune", "zerolatency");
        }
        "libsvtav1" => {
            // SVT presets run from 0 (slowest) through 13 (fastest).
            options.set("preset", "13");
            options.set("svtav1-params", "la_depth=0:scd=0");
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
        name if name.ends_with("_videotoolbox") => {
            options.set("realtime", "1");
            options.set("prio_speed", "1");
            options.set("max_ref_frames", "1");
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

fn yuv_frame(
    source: &RawFrame,
    width: usize,
    height: usize,
    scaled_width: usize,
    scaled_height: usize,
    scaler: &mut ScalingContext,
    pixel: format::Pixel,
) -> Result<frame::Video> {
    let mut input =
        frame::Video::new(format::Pixel::BGRA, source.width as u32, source.height as u32);
    let input_stride = input.stride(0);
    let row_bytes = source.width * 4;
    for row in 0..source.height {
        let source_start = row * source.stride;
        let target_start = row * input_stride;
        input.data_mut(0)[target_start..target_start + row_bytes]
            .copy_from_slice(&source.bgra[source_start..source_start + row_bytes]);
    }

    let mut scaled = frame::Video::new(
        pixel,
        scaled_width as u32,
        scaled_height as u32,
    );
    scaler
        .run(&input, &mut scaled)
        .context("convert captured frame")?;
    if scaled_width == width && scaled_height == height {
        return Ok(scaled);
    }

    let mut output = frame::Video::new(pixel, width as u32, height as u32);
    output.data_mut(0).fill(16);
    output.data_mut(1).fill(128);
    let left = (width - scaled_width) / 2;
    let top = (height - scaled_height) / 2;
    copy_plane(
        &mut output,
        &scaled,
        0,
        left,
        top,
        scaled_width,
        scaled_height,
    );
    if pixel == format::Pixel::NV12 {
        copy_plane(&mut output, &scaled, 1, left, top / 2, scaled_width, scaled_height / 2);
    } else {
        output.data_mut(2).fill(128);
        copy_plane(&mut output, &scaled, 1, left / 2, top / 2, scaled_width / 2, scaled_height / 2);
        copy_plane(&mut output, &scaled, 2, left / 2, top / 2, scaled_width / 2, scaled_height / 2);
    }
    Ok(output)
}

fn copy_plane(
    target: &mut frame::Video,
    source: &frame::Video,
    plane: usize,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
) {
    let source_stride = source.stride(plane);
    let target_stride = target.stride(plane);
    let source_data = source.data(plane);
    let target_data = target.data_mut(plane);
    for row in 0..height {
        let source_start = row * source_stride;
        let target_start = (top + row) * target_stride + left;
        target_data[target_start..target_start + width]
            .copy_from_slice(&source_data[source_start..source_start + width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    #[test]
    fn timing_totals_report_resets_the_window() {
        let mut totals = TimingTotals {
            frames: FPS as u64,
            conversion: Duration::from_millis(60),
            submit: Duration::from_millis(30),
            drain: Duration::from_millis(90),
            egress: Duration::from_millis(15),
        };
        totals.report();
        assert_eq!(totals.frames, 0);
        assert_eq!(totals.conversion, Duration::ZERO);
        assert_eq!(totals.egress, Duration::ZERO);
    }

    #[test]
    #[ignore = "microbenchmark; run explicitly with --ignored --nocapture"]
    fn benchmark_hardware_input_formats() {
        initialize().unwrap();
        let width = 1920;
        let height = 1080;
        let source = RawFrame {
            bgra: vec![0; width * height * 4],
            width,
            height,
            stride: width * 4,
            captured_at: SystemTime::now(),
        };
        let mut yuv_scaler = ScalingContext::get(
            format::Pixel::BGRA,
            width as u32,
            height as u32,
            format::Pixel::YUV420P,
            width as u32,
            height as u32,
            Flags::FAST_BILINEAR,
        )
        .unwrap();
        let mut nv12_scaler = ScalingContext::get(
            format::Pixel::BGRA,
            width as u32,
            height as u32,
            format::Pixel::NV12,
            width as u32,
            height as u32,
            Flags::FAST_BILINEAR,
        )
        .unwrap();
        let iterations = 100;
        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(yuv_frame(&source, width, height, width, height, &mut yuv_scaler, format::Pixel::YUV420P).unwrap());
        }
        let yuv_elapsed = start.elapsed();
        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(yuv_frame(&source, width, height, width, height, &mut nv12_scaler, format::Pixel::NV12).unwrap());
        }
        let nv12_elapsed = start.elapsed();
        println!(
            "input format benchmark: yuv420p={:.2}ms/frame nv12={:.2}ms/frame speedup={:.1}%",
            yuv_elapsed.as_secs_f64() * 1000.0 / iterations as f64,
            nv12_elapsed.as_secs_f64() * 1000.0 / iterations as f64,
            (1.0 - nv12_elapsed.as_secs_f64() / yuv_elapsed.as_secs_f64()) * 100.0,
        );
    }

}
