use anyhow::{Result, bail};
use pinray::{AudioCapture, AudioFrame, CaptureEvent, CaptureSession, CursorMode, FrameData, SourceId, VideoCaptureTarget};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::mpsc;

pub(crate) mod ffmpeg;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Av1,
    H264,
    H265,
}

#[derive(Clone, Copy, Serialize)]
pub struct HardwareCodecAvailability {
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
}

pub const CODEC: Codec = Codec::H264;
pub const FPS: u32 = 30;
// AV1 4:2:0 requires both stream dimensions to be even.
#[allow(dead_code)]
pub const STREAM_WIDTH: usize = 1920;
#[allow(dead_code)]
pub const STREAM_HEIGHT: usize = 1080;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct StreamSettings {
    pub codec: Codec,
    pub resolution: f32,
    pub bitrate: u32,
    pub host_cursor_visible: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            codec: CODEC,
            resolution: 1.0,
            bitrate: 40_000_000,
            host_cursor_visible: true,
        }
    }
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub duration: Duration,
}

pub(crate) struct RawFrame {
    pub(crate) bgra: Vec<u8>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride: usize,
    pub(crate) captured_at: SystemTime,
}

struct FrameSlot {
    frame: Option<Arc<RawFrame>>,
    closed: bool,
}

type LatestFrame = Arc<(Mutex<FrameSlot>, Condvar)>;

pub struct Session {
    stop: Arc<AtomicBool>,
    frames: LatestFrame,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.frames.0.lock().unwrap().closed = true;
        self.frames.1.notify_all();
    }
}

pub fn spawn(
    sender: mpsc::Sender<EncodedFrame>,
    audio_sender: mpsc::Sender<AudioFrame>,
    settings: StreamSettings,
) -> (Session, mpsc::Receiver<String>) {
    let latest = Arc::new((
        Mutex::new(FrameSlot {
            frame: None,
            closed: false,
        }),
        Condvar::new(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let (errors, receiver) = mpsc::channel(1);
    let frames = latest.clone();
    let capture_frames = latest.clone();
    let capture_stop = stop.clone();
    let capture_errors = errors.clone();
    std::thread::Builder::new()
        .name("beam-capture".into())
        .spawn(move || {
            if let Err(error) = capture_inner(capture_frames.clone(), capture_stop, audio_sender, settings) {
                tracing::error!(error = ?error, "capture stopped");
                let _ = capture_errors.blocking_send(error.to_string());
            }
            capture_frames.0.lock().unwrap().closed = true;
            capture_frames.1.notify_all();
        })
        .expect("capture thread");
    std::thread::Builder::new()
        .name("beam-ffmpeg".into())
        .spawn(move || {
            if let Err(error) = encode(frames, sender, settings) {
                tracing::error!(error = ?error, "video encoder stopped");
                let _ = errors.blocking_send(error.to_string());
            }
        })
        .expect("encoder thread");
    (
        Session {
            stop,
            frames: latest,
        },
        receiver,
    )
}

fn capture_inner(
    latest: LatestFrame,
    stop: Arc<AtomicBool>,
    audio_sender: mpsc::Sender<AudioFrame>,
    settings: StreamSettings,
) -> Result<()> {
    let mut capturer = CaptureSession::builder()
        .video_target(VideoCaptureTarget::Display(SourceId::new("auto")))
        .audio(AudioCapture::SystemMix)
        .cursor_mode(if settings.host_cursor_visible { CursorMode::Embedded } else { CursorMode::Hidden })
        .frame_rate(Some(FPS))
        .build()
        .map_err(|error| anyhow::anyhow!(error))?;
    capturer.start().map_err(|error| anyhow::anyhow!(error))?;

    let mut last_frame = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let event = capturer
            .next_event(Some(CAPTURE_TIMEOUT))
            .map_err(|error| anyhow::anyhow!(error))?;
        match event {
            CaptureEvent::Video(frame) => {
                let FrameData::Host(data) = frame.data else { continue };
                if frame.width < 2 || frame.height < 2 {
                    continue;
                }
                last_frame = Instant::now();
                latest.0.lock().unwrap().frame = Some(Arc::new(RawFrame {
                    bgra: data,
                    width: frame.width as usize,
                    height: frame.height as usize,
                    stride: frame.stride as usize,
                    captured_at: SystemTime::now(),
                }));
                latest.1.notify_one();
            }
            CaptureEvent::Audio(frame) => {
                if audio_sender.blocking_send(frame).is_err() {
                    break;
                }
            }
            _ if last_frame.elapsed() >= CAPTURE_TIMEOUT => {
                bail!("screen capture produced no frames for two seconds");
            }
            _ => {}
        }
    }
    capturer.stop().map_err(|error| anyhow::anyhow!(error))?;
    Ok(())
}

fn encode(frames: LatestFrame, sender: mpsc::Sender<EncodedFrame>, settings: StreamSettings) -> Result<()> {
    let Some(first) = next_frame(&frames) else { return Ok(()) };
    let (width, height) = stream_dimensions(first.width, first.height, settings.resolution);
    ffmpeg::encode(settings.codec, frames, sender, first, width, height, settings.bitrate)
}

pub fn stream_dimensions(source_width: usize, source_height: usize, scale: f32) -> (usize, usize) {
    let width = ((source_width as f32 * scale).round() as usize).max(2) & !1;
    let height = ((source_height as f32 * scale).round() as usize).max(2) & !1;
    (width, height)
}

fn next_frame(frames: &LatestFrame) -> Option<Arc<RawFrame>> {
    let mut slot = frames.0.lock().unwrap();
    while slot.frame.is_none() && !slot.closed {
        slot = frames.1.wait(slot).unwrap();
    }
    slot.frame.take()
}

pub(crate) fn scale_bgra_to_i420(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    stride: usize,
    width: usize,
    height: usize,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let Some(row_bytes) = source_width.checked_mul(4) else {
        bail!("capture returned an invalid BGRA frame");
    };
    let Some(required_len) = source_height
        .checked_sub(1)
        .and_then(|last_row| last_row.checked_mul(stride))
        .and_then(|last_row| last_row.checked_add(row_bytes))
    else {
        bail!("capture returned an invalid BGRA frame");
    };
    if source_width == 0
        || source_height == 0
        || width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || stride < row_bytes
        || source.len() < required_len
    {
        bail!("capture returned an invalid BGRA frame");
    }
    let (scaled_width, scaled_height) = if width * source_height <= height * source_width {
        (width, (source_height * width / source_width).max(2) & !1)
    } else {
        ((source_width * height / source_height).max(2) & !1, height)
    };
    let left = (width - scaled_width) / 2;
    let top = (height - scaled_height) / 2;
    let x = (0..width)
        .map(|column| {
            (column >= left && column < left + scaled_width)
                .then(|| (column - left) * source_width / scaled_width)
        })
        .collect::<Vec<_>>();
    let y_map = (0..height)
        .map(|row| {
            (row >= top && row < top + scaled_height)
                .then(|| (row - top) * source_height / scaled_height)
        })
        .collect::<Vec<_>>();

    let mut y = vec![16; width * height];
    let mut u = vec![128; width * height / 4];
    let mut v = vec![128; width * height / 4];

    for row in (0..height).step_by(2) {
        for column in (0..width).step_by(2) {
            let mut red = 0;
            let mut green = 0;
            let mut blue = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (r, g, b) = match (x[column + dx], y_map[row + dy]) {
                        (Some(source_x), Some(source_y)) => {
                            let source_x = source_x.min(source_width - 1);
                            let source_y = source_y.min(source_height - 1);
                            let pixel = source_y * stride + source_x * 4;
                            (
                                source[pixel + 2] as i32,
                                source[pixel + 1] as i32,
                                source[pixel] as i32,
                            )
                        }
                        _ => (0, 0, 0),
                    };
                    y[(row + dy) * width + column + dx] =
                        clamp((47 * r + 157 * g + 16 * b + 128) / 256 + 16);
                    red += r;
                    green += g;
                    blue += b;
                }
            }
            let r = red / 4;
            let g = green / 4;
            let b = blue / 4;
            let chroma = row / 2 * (width / 2) + column / 2;
            u[chroma] = clamp((-26 * r - 87 * g + 112 * b + 128) / 256 + 128);
            v[chroma] = clamp((112 * r - 102 * g - 10 * b + 128) / 256 + 128);
        }
    }
    Ok((y, u, v))
}

fn clamp(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_black_and_white_bgra() {
        let black = scale_bgra_to_i420(
            &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
            2,
            2,
            8,
            2,
            2,
        )
        .unwrap();
        assert_eq!(black, (vec![16; 4], vec![128], vec![128]));

        let white = scale_bgra_to_i420(&[255; 16], 2, 2, 8, 2, 2).unwrap();
        assert_eq!(white, (vec![235; 4], vec![128], vec![128]));
    }

    #[test]
    fn crops_an_odd_source_width_without_losing_stride() {
        let mut source = Vec::new();
        for _ in 0..2 {
            source.extend_from_slice(&[255; 12]);
        }
        let converted = scale_bgra_to_i420(&source, 3, 2, 12, 2, 2).unwrap();
        assert_eq!(converted, (vec![235; 4], vec![128], vec![128]));
    }

    #[test]
    fn rejects_truncated_bgra_rows() {
        assert!(scale_bgra_to_i420(&[0; 16], 2, 2, 12, 2, 2).is_err());
    }
}
