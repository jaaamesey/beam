use super::{EncodedFrame, FPS, LatestFrame, RawFrame, STREAM_HEIGHT, STREAM_WIDTH, next_frame};
use anyhow::{Context as _, Result, bail};
use cidre::{arc, cf, cm, cv, os, vt};
use std::{ffi::c_void, slice, sync::mpsc as std_mpsc, time::Duration};
use tokio::sync::mpsc;

const BITRATE: i32 = 8_000_000;
const START_CODE: &[u8] = &[0, 0, 0, 1];

struct Callback(std_mpsc::Sender<Result<Vec<u8>>>);

pub fn encode(frames: LatestFrame, sender: mpsc::Sender<EncodedFrame>) -> Result<()> {
    let (encoded_tx, encoded_rx) = std_mpsc::channel();
    let mut callback = Box::new(Callback(encoded_tx));
    let encoder = create_encoder(&mut callback)?;
    let mut pixel_buffer =
        cv::PixelBuf::new(STREAM_WIDTH, STREAM_HEIGHT, cv::PixelFormat::_32_BGRA, None)
            .context("create H.264 input buffer")?;

    let mut frame_number = 0;
    let mut previous_time = None;
    while let Some(source) = next_frame(&frames) {
        copy_bgra(&source, &mut pixel_buffer)?;
        encoder
            .encode_frame(
                &pixel_buffer,
                cm::Time::new(frame_number, FPS as i32),
                cm::Time::new(1, FPS as i32),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
            .context("submit H.264 frame")?;
        frame_number += 1;
        let data = encoded_rx
            .recv()
            .context("VideoToolbox callback stopped")??;
        let duration = previous_time
            .and_then(|time| source.captured_at.duration_since(time).ok())
            .unwrap_or(Duration::from_secs_f64(1.0 / FPS as f64))
            .clamp(Duration::from_millis(1), Duration::from_secs(1));
        previous_time = Some(source.captured_at);
        if sender
            .blocking_send(EncodedFrame { data, duration })
            .is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

fn create_encoder(callback: &mut Callback) -> Result<arc::R<vt::CompressionSession>> {
    let mut encoder = vt::CompressionSession::new(
        STREAM_WIDTH as u32,
        STREAM_HEIGHT as u32,
        cm::VideoCodec::H264,
        None,
        None,
        None,
        Some(encoded),
        callback,
    )
    .context("create VideoToolbox H.264 encoder")?;
    configure(&mut encoder)?;
    Ok(encoder)
}

fn configure(encoder: &mut vt::CompressionSession) -> Result<()> {
    let mut properties = cf::DictionaryMut::with_capacity(8);
    properties.insert(
        vt::compression::keys::real_time(),
        cf::Boolean::value_true(),
    );
    properties.insert(
        vt::compression::keys::allow_frame_reordering(),
        cf::Boolean::value_false(),
    );
    properties.insert(
        vt::compression::keys::prioritize_encoding_speed_over_quality(),
        cf::Boolean::value_true(),
    );
    properties.insert(
        vt::compression::keys::avarage_bit_rate(),
        &cf::Number::from_i32(BITRATE),
    );
    properties.insert(
        vt::compression::keys::expected_frame_rate(),
        &cf::Number::from_i32(FPS as i32),
    );
    properties.insert(
        vt::compression::keys::max_frame_delay_count(),
        &cf::Number::from_i32(0),
    );
    properties.insert(
        vt::compression::keys::max_key_frame_interval(),
        &cf::Number::from_i32((FPS * 2) as i32),
    );
    properties.insert(
        vt::compression::keys::profile_lvl(),
        vt::compression::profile_level::h264::constrained_baseline_auto_lvl(),
    );
    encoder
        .set_props(&properties)
        .context("configure VideoToolbox H.264 encoder")?;
    encoder
        .prepare()
        .context("prepare VideoToolbox H.264 encoder")?;
    let hardware = encoder
        .prop(using_hardware_encoder())?
        .and_then(|value| value.try_as_boolean().map(cf::Boolean::value))
        .unwrap_or(false);
    if !hardware {
        bail!("VideoToolbox selected a software H.264 encoder");
    }
    tracing::info!(codec = "H.264", hardware, "video encoder ready");
    Ok(())
}

fn copy_bgra(source: &RawFrame, target: &mut cv::PixelBuf) -> Result<()> {
    let target_pointer = std::ptr::from_mut(target);
    let _lock = target.base_address_lock(cv::pixel_buffer::LockFlags::DEFAULT)?;
    let stride = unsafe { CVPixelBufferGetBytesPerRow(&*target_pointer) };
    let pointer = unsafe { CVPixelBufferGetBaseAddress(&*target_pointer) }.cast::<u8>();
    if pointer.is_null() {
        bail!("H.264 input buffer has no address");
    }
    let data = unsafe { slice::from_raw_parts_mut(pointer, stride * STREAM_HEIGHT) };
    data.fill(0);

    let (width, height) = if STREAM_WIDTH * source.height <= STREAM_HEIGHT * source.width {
        (STREAM_WIDTH, source.height * STREAM_WIDTH / source.width)
    } else {
        (source.width * STREAM_HEIGHT / source.height, STREAM_HEIGHT)
    };
    let left = (STREAM_WIDTH - width) / 2;
    let top = (STREAM_HEIGHT - height) / 2;
    for y in 0..height {
        let source_y = y * source.height / height;
        for x in 0..width {
            let source_x = x * source.width / width;
            let from = (source_y * source.stride + source_x) * 4;
            let to = (top + y) * stride + (left + x) * 4;
            data[to..to + 4].copy_from_slice(&source.bgra[from..from + 4]);
        }
    }
    Ok(())
}

extern "C" fn encoded(
    callback: *mut Callback,
    _: *mut c_void,
    status: os::Status,
    _: vt::EncodeInfoFlags,
    sample: Option<&cm::SampleBuf>,
) {
    let result = (|| {
        if status.is_err() {
            bail!("VideoToolbox callback failed: {status:?}");
        }
        let sample = sample.context("VideoToolbox dropped a frame")?;
        let format = sample
            .format_desc()
            .context("H.264 frame has no format description")?;
        let (parameter_count, length_size) = format.h264_params_count_and_header_len()?;
        let block = sample.data_buf().context("H.264 frame has no data")?;
        let mut avcc = vec![0; block.data_len()];
        let copy_status =
            unsafe { CMBlockBufferCopyDataBytes(block, 0, avcc.len(), avcc.as_mut_ptr().cast()) };
        if copy_status.is_err() {
            bail!("copy H.264 frame: {copy_status:?}");
        }
        let nalus = avcc_nalus(&avcc, length_size as usize)?;
        let mut output = Vec::with_capacity(avcc.len() + 128);
        if nalus.iter().any(|nalu| nalu[0] & 0x1f == 5) {
            for index in 0..parameter_count {
                output.extend_from_slice(START_CODE);
                output.extend_from_slice(format.h264_param_set_at(index)?);
            }
        }
        for nalu in nalus {
            output.extend_from_slice(START_CODE);
            output.extend_from_slice(nalu);
        }
        Ok(output)
    })();
    if let Some(callback) = unsafe { callback.as_ref() } {
        let _ = callback.0.send(result);
    }
}

fn avcc_nalus(mut data: &[u8], length_size: usize) -> Result<Vec<&[u8]>> {
    if !(1..=4).contains(&length_size) {
        bail!("invalid H.264 NAL length size: {length_size}");
    }
    let mut nalus = Vec::new();
    while !data.is_empty() {
        if data.len() < length_size {
            bail!("truncated H.264 NAL length");
        }
        let mut length = [0; 4];
        length[4 - length_size..].copy_from_slice(&data[..length_size]);
        let length = u32::from_be_bytes(length) as usize;
        data = &data[length_size..];
        if length == 0 || data.len() < length {
            bail!("invalid H.264 NAL length");
        }
        nalus.push(&data[..length]);
        data = &data[length..];
    }
    Ok(nalus)
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: &'static cf::String;
}

fn using_hardware_encoder() -> &'static cf::String {
    unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder }
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetBaseAddress(buffer: &cv::PixelBuf) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRow(buffer: &cv::PixelBuf) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMBlockBufferCopyDataBytes(
        buffer: &cm::BlockBuf,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> os::Status;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires macOS video encoder hardware"]
    fn encodes_hardware_frame() {
        let (sender, receiver) = std_mpsc::channel();
        let mut callback = Callback(sender);
        let encoder = create_encoder(&mut callback).unwrap();
        let pixel_buffer =
            cv::PixelBuf::new(STREAM_WIDTH, STREAM_HEIGHT, cv::PixelFormat::_32_BGRA, None)
                .unwrap();
        encoder
            .encode_frame(
                &pixel_buffer,
                cm::Time::new(0, FPS as i32),
                cm::Time::new(1, FPS as i32),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
            .unwrap();
        assert!(receiver.recv().unwrap().unwrap().starts_with(START_CODE));
    }

    #[test]
    fn parses_avcc_nalus() {
        let data = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41];
        assert_eq!(
            avcc_nalus(&data, 4).unwrap(),
            vec![&[0x65, 1][..], &[0x41][..]]
        );
        assert!(avcc_nalus(&data[..5], 4).is_err());
    }
}
