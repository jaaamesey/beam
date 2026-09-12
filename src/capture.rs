use anyhow::{Result, bail};
use pinray::{AudioCapture, AudioFrame, CaptureEvent, CaptureSession, CursorMode, FrameData, PinrayError, SourceId, VideoCaptureTarget};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{broadcast, mpsc};

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
pub const FPS: u32 = 60;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);
const CAPTURE_EVENT_TIMEOUT: Duration = Duration::from_millis(100);

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

pub(crate) struct FrameSlot {
    frame: Option<Arc<RawFrame>>,
    closed: bool,
}

pub(crate) type LatestFrame = Arc<(Mutex<FrameSlot>, Condvar)>;

pub struct SourceSession {
    stop: Arc<AtomicBool>,
    frames: LatestFrame,
    audio: broadcast::Sender<AudioFrame>,
    errors: broadcast::Sender<String>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SourceSession {
    fn drop(&mut self) {
        self.request_stop();
    }
}

impl SourceSession {
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.frames.0.lock().unwrap().closed = true;
        self.frames.1.notify_all();
    }

    pub fn stop(&self) {
        self.request_stop();
    }

    pub(crate) fn frames(&self) -> LatestFrame {
        self.frames.clone()
    }

    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioFrame> {
        self.audio.subscribe()
    }

    pub fn subscribe_errors(&self) -> broadcast::Receiver<String> {
        self.errors.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.frames.0.lock().unwrap().closed
    }

    pub fn shutdown(mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn_source(settings: StreamSettings) -> Arc<SourceSession> {
    let latest = Arc::new((
        Mutex::new(FrameSlot {
            frame: None,
            closed: false,
        }),
        Condvar::new(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let (audio, _) = broadcast::channel(32);
    let (errors, _) = broadcast::channel(4);
    let capture_frames = latest.clone();
    let capture_stop = stop.clone();
    let capture_audio = audio.clone();
    let capture_errors = errors.clone();
    let thread = std::thread::Builder::new()
        .name("beam-capture".into())
        .spawn(move || {
            if let Err(error) = capture_inner(capture_frames.clone(), capture_stop, capture_audio, settings) {
                tracing::error!(error = ?error, "capture stopped");
                let _ = capture_errors.send(error.to_string());
            }
            capture_frames.0.lock().unwrap().closed = true;
            capture_frames.1.notify_all();
        })
        .expect("capture thread");
    Arc::new(SourceSession {
        stop,
        frames: latest,
        audio,
        errors,
        thread: Some(thread),
    })
}

pub struct EncoderSession {
    stop: Arc<AtomicBool>,
    frames: LatestFrame,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EncoderSession {
    fn drop(&mut self) {
        self.request_stop();
    }
}

impl EncoderSession {
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.frames.1.notify_all();
    }

    pub fn shutdown(mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn_encoder(
    source: &Arc<SourceSession>,
    sender: mpsc::Sender<EncodedFrame>,
    settings: StreamSettings,
) -> (EncoderSession, mpsc::Receiver<String>) {
    let stop = Arc::new(AtomicBool::new(false));
    let (errors, receiver) = mpsc::channel(1);
    let frames = source.frames();
    let encoder_frames = frames.clone();
    let encoder_stop = stop.clone();
    let encoder_errors = errors.clone();
    let thread = std::thread::Builder::new()
        .name("beam-ffmpeg".into())
        .spawn(move || {
            if let Err(error) = encode(encoder_frames, encoder_stop, sender, settings) {
                tracing::error!(error = ?error, "video encoder stopped");
                let _ = encoder_errors.blocking_send(error.to_string());
            }
        })
        .expect("encoder thread");
    (EncoderSession { stop, frames, thread: Some(thread) }, receiver)
}

fn capture_inner(
    latest: LatestFrame,
    stop: Arc<AtomicBool>,
    audio_sender: broadcast::Sender<AudioFrame>,
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
        let event = match capturer.next_event(Some(CAPTURE_EVENT_TIMEOUT)) {
            Ok(event) => event,
            Err(PinrayError::Timeout(_)) if last_frame.elapsed() >= CAPTURE_TIMEOUT => {
                bail!("screen capture produced no frames for two seconds");
            }
            Err(PinrayError::Timeout(_)) => continue,
            Err(error) => return Err(anyhow::anyhow!(error)),
        };
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
                let _ = audio_sender.send(frame);
            }
            _ => {}
        }
    }
    capturer.stop().map_err(|error| anyhow::anyhow!(error))?;
    Ok(())
}

fn encode(
    frames: LatestFrame,
    stop: Arc<AtomicBool>,
    sender: mpsc::Sender<EncodedFrame>,
    settings: StreamSettings,
) -> Result<()> {
    let Some(first) = next_frame(&frames, &stop) else { return Ok(()) };
    let (width, height) = stream_dimensions(first.width, first.height, settings.resolution);
    ffmpeg::encode(settings.codec, frames, stop, sender, first, width, height, settings.bitrate)
}

pub fn stream_dimensions(source_width: usize, source_height: usize, scale: f32) -> (usize, usize) {
    let width = ((source_width as f32 * scale).round() as usize).max(2) & !1;
    let height = ((source_height as f32 * scale).round() as usize).max(2) & !1;
    (width, height)
}

fn next_frame(frames: &LatestFrame, stop: &AtomicBool) -> Option<Arc<RawFrame>> {
    let mut slot = frames.0.lock().unwrap();
    while slot.frame.is_none() && !slot.closed && !stop.load(Ordering::Relaxed) {
        slot = frames.1.wait(slot).unwrap();
    }
    slot.frame.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_encoder_does_not_wait_for_a_frame() {
        let latest = Arc::new((
            Mutex::new(FrameSlot { frame: None, closed: false }),
            Condvar::new(),
        ));
        let stop = AtomicBool::new(true);
        assert!(next_frame(&latest, &stop).is_none());
    }
}
