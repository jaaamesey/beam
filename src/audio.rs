use anyhow::{Result, bail};
use opus::{Application, Channels, Encoder};
use pinray::{AudioData, AudioFrame, SampleFormat};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tokio::sync::mpsc;

const OPUS_FRAME_SAMPLES: usize = 240 * 2;
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(5);

pub struct Session {
    stop: Arc<AtomicBool>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn spawn(sender: mpsc::Sender<(Vec<u8>, Duration)>) -> (Session, mpsc::Sender<AudioFrame>) {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let (frames_sender, frames_receiver) = mpsc::channel(8);
    std::thread::Builder::new()
        .name("beam-audio".into())
        .spawn(move || encode_audio(frames_receiver, thread_stop, sender))
        .expect("audio encoder thread");
    (Session { stop }, frames_sender)
}

fn encode_audio(
    mut frames: mpsc::Receiver<AudioFrame>,
    stop: Arc<AtomicBool>,
    sender: mpsc::Sender<(Vec<u8>, Duration)>,
) {
    let mut encoder = match Encoder::new(48_000, Channels::Stereo, Application::LowDelay) {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::error!(%error, "could not initialise Opus encoder");
            return;
        }
    };
    let mut pcm = Vec::new();
    let mut previous_time = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = frames.blocking_recv() else { break };
        let mut samples = Vec::new();
        if let Err(error) = append_stereo(&mut samples, &frame.data, frame.sample_format, frame.channels) {
            tracing::warn!(%error, "unsupported system audio format");
            break;
        }
        let input_frames = samples.len() / 2;
        let input_rate = previous_time
            .and_then(|time| {
                let elapsed = frame.stream_time_ns.saturating_sub(time);
                (elapsed > 0 && input_frames > 0).then_some(
                    input_frames as f64 * 1_000_000_000.0 / elapsed as f64,
                )
            })
            .unwrap_or(frame.sample_rate as f64);
        previous_time = Some(frame.stream_time_ns);
        if (input_rate - 48_000.0).abs() < 1.0 {
            pcm.extend(samples);
        } else {
            tracing::debug!(input_rate, input_frames, "resampling system audio");
            pcm.extend(resample_stereo(&samples, input_rate));
        }
        while pcm.len() >= OPUS_FRAME_SAMPLES {
            let mut packet = vec![0; 4_000];
            let size = match encoder.encode_float(&pcm[..OPUS_FRAME_SAMPLES], &mut packet) {
                Ok(size) => size,
                Err(error) => {
                    tracing::warn!(%error, "could not encode system audio");
                    return;
                }
            };
            packet.truncate(size);
            pcm.drain(..OPUS_FRAME_SAMPLES);
            if sender.blocking_send((packet, OPUS_FRAME_DURATION)).is_err() {
                return;
            }
        }
    }
}

fn append_stereo(output: &mut Vec<f32>, data: &AudioData, format: SampleFormat, channels: u16) -> Result<()> {
    if channels == 0 {
        bail!("audio capture reported zero channels");
    }
    let channels = channels as usize;
    let mut samples = Vec::new();
    match data {
        AudioData::Interleaved(bytes) => {
            #[cfg(target_os = "macos")]
            if channels == 2 {
                decode_planar_stereo(&mut samples, bytes, format)?;
            } else {
                decode_samples(&mut samples, bytes, format)?;
            }
            #[cfg(not(target_os = "macos"))]
            decode_samples(&mut samples, bytes, format)?;
        }
        AudioData::Planar(planes) => {
            let size = sample_size(format);
            let frames = planes.iter().map(|plane| plane.len() / size).min().unwrap_or(0);
            for frame in 0..frames {
                for channel in 0..channels.min(planes.len()) {
                    samples.push(decode_one(&planes[channel], frame * size, format)?);
                }
            }
        }
    }
    if channels == 1 {
        for sample in samples {
            output.extend([sample, sample]);
        }
    } else {
        for frame in samples.chunks(channels) {
            output.extend([frame[0], frame.get(1).copied().unwrap_or(frame[0])]);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn decode_planar_stereo(output: &mut Vec<f32>, bytes: &[u8], format: SampleFormat) -> Result<()> {
    let size = sample_size(format);
    if !bytes.len().is_multiple_of(size * 2) {
        bail!("planar audio sample buffer has an invalid length");
    }
    let plane_len = bytes.len() / 2;
    for index in (0..plane_len).step_by(size) {
        output.push(decode_one(bytes, index, format)?);
        output.push(decode_one(bytes, plane_len + index, format)?);
    }
    Ok(())
}

fn decode_samples(output: &mut Vec<f32>, bytes: &[u8], format: SampleFormat) -> Result<()> {
    let size = sample_size(format);
    if !bytes.len().is_multiple_of(size) {
        bail!("audio sample buffer has an invalid length");
    }
    for index in (0..bytes.len()).step_by(size) {
        output.push(decode_one(bytes, index, format)?);
    }
    Ok(())
}

fn decode_one(bytes: &[u8], index: usize, format: SampleFormat) -> Result<f32> {
    Ok(match format {
        SampleFormat::I16 => i16::from_le_bytes(bytes[index..index + 2].try_into()?) as f32 / 32_768.0,
        SampleFormat::I32 => i32::from_le_bytes(bytes[index..index + 4].try_into()?) as f32 / 2_147_483_648.0,
        SampleFormat::F32 => f32::from_le_bytes(bytes[index..index + 4].try_into()?),
        SampleFormat::F64 => f64::from_le_bytes(bytes[index..index + 8].try_into()?) as f32,
    })
}

fn sample_size(format: SampleFormat) -> usize {
    match format {
        SampleFormat::I16 => 2,
        SampleFormat::I32 | SampleFormat::F32 => 4,
        SampleFormat::F64 => 8,
    }
}

fn resample_stereo(input: &[f32], sample_rate: f64) -> Vec<f32> {
    if input.is_empty() || sample_rate <= 0.0 {
        return Vec::new();
    }
    let input_frames = input.len() / 2;
    let output_frames = (input_frames as f64 * 48_000.0 / sample_rate).round() as usize;
    let mut output = Vec::with_capacity(output_frames * 2);
    for frame in 0..output_frames {
        let position = frame as f64 * sample_rate / 48_000.0;
        let lower = position.floor() as usize;
        let upper = (lower + 1).min(input_frames.saturating_sub(1));
        let fraction = (position - lower as f64) as f32;
        for channel in 0..2 {
            let first = input[lower * 2 + channel];
            let second = input[upper * 2 + channel];
            output.push(first + (second - first) * fraction);
        }
    }
    output
}
