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

pub fn check_permissions() {
    #[cfg(target_os = "macos")]
    match pinray::enumerate_sources() {
        Ok(_) => tracing::info!("screen recording permission is available"),
        Err(error) => tracing::warn!(%error, "screen recording permission is unavailable"),
    }
}

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
pub const FPS: u32 = 60;
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
    pub encode_duration: Duration,
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
    capture_thread: Option<std::thread::JoinHandle<()>>,
    encoder_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.request_stop();
    }
}

impl Session {
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.frames.0.lock().unwrap().closed = true;
        self.frames.1.notify_all();
    }

    pub fn shutdown(mut self) {
        self.request_stop();
        if let Some(thread) = self.capture_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.encoder_thread.take() {
            let _ = thread.join();
        }
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
    let capture_thread = std::thread::Builder::new()
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
    let encoder_thread = std::thread::Builder::new()
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
            capture_thread: Some(capture_thread),
            encoder_thread: Some(encoder_thread),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn shutdown_joins_capture_and_encoder_workers() {
        let latest = Arc::new((
            Mutex::new(FrameSlot { frame: None, closed: false }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let joined = Arc::new(AtomicUsize::new(0));
        let capture_joined = joined.clone();
        let encoder_joined = joined.clone();
        let capture_stop = stop.clone();
        let encoder_stop = stop.clone();
        let capture_thread = std::thread::spawn(move || {
            while !capture_stop.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            capture_joined.fetch_add(1, Ordering::Relaxed);
        });
        let encoder_thread = std::thread::spawn(move || {
            while !encoder_stop.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            encoder_joined.fetch_add(1, Ordering::Relaxed);
        });

        Session {
            stop,
            frames: latest,
            capture_thread: Some(capture_thread),
            encoder_thread: Some(encoder_thread),
        }
        .shutdown();

        assert_eq!(joined.load(Ordering::Relaxed), 2);
    }
}
