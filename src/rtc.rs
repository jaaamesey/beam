use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MediaEngine},
    },
    interceptor::registry::Registry,
    data_channel::{RTCDataChannel, data_channel_message::DataChannelMessage},
    media::Sample,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
};

use crate::capture::{self, Codec};

pub struct Media {
    api: webrtc::api::API,
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    settings: Arc<tokio::sync::RwLock<crate::capture::StreamSettings>>,
    hardware_codecs: capture::HardwareCodecAvailability,
    encode_duration_us: Arc<AtomicU64>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ControlMessage {
    #[serde(rename = "setStreamSettings")]
    SetStreamSettings {
        codec: Option<Codec>,
        resolution: Option<f32>,
        bitrate: Option<u32>,
        host_cursor_visible: Option<bool>,
    },
}

#[derive(Serialize)]
struct StreamSettingsMessage {
    r#type: &'static str,
    codec: Codec,
    resolution: f32,
    bitrate: u32,
    host_cursor_visible: bool,
    hardware_codecs: capture::HardwareCodecAvailability,
}

#[derive(Serialize)]
struct StreamStatsMessage {
    r#type: &'static str,
    encode_ms: f64,
}

impl Media {
    pub fn new(input_tx: std::sync::mpsc::Sender<Vec<u8>>) -> Result<Arc<Self>> {
        let mut engine = MediaEngine::default();
        let h264 = RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            ..Default::default()
        };
        let av1 = RTCRtpCodecCapability { mime_type: MIME_TYPE_AV1.to_owned(), clock_rate: 90_000, ..Default::default() };
        let h265 = RTCRtpCodecCapability { mime_type: MIME_TYPE_HEVC.to_owned(), clock_rate: 90_000, ..Default::default() };
        engine.register_codec(
            RTCRtpCodecParameters { capability: h264, payload_type: 102, ..Default::default() },
            RTPCodecType::Video,
        )?;
        engine.register_codec(
            RTCRtpCodecParameters { capability: av1, payload_type: 45, ..Default::default() },
            RTPCodecType::Video,
        )?;
        engine.register_codec(
            RTCRtpCodecParameters { capability: h265, payload_type: 127, ..Default::default() },
            RTPCodecType::Video,
        )?;
        engine.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    ..Default::default()
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;
        let registry = register_default_interceptors(Registry::new(), &mut engine)?;
        let media = Arc::new(Self {
            api: APIBuilder::new()
                .with_media_engine(engine)
                .with_interceptor_registry(registry)
                .build(),
            input_tx,
            settings: Arc::new(tokio::sync::RwLock::new(Default::default())),
            hardware_codecs: capture::ffmpeg::hardware_codecs(),
            encode_duration_us: Arc::new(AtomicU64::new(0)),
        });
        Ok(media)
    }

    pub async fn answer(&self, sdp: String) -> Result<RTCSessionDescription> {
        let settings_snapshot = *self.settings.read().await;
        let capability = match settings_snapshot.codec {
            Codec::H264 => RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
                ..Default::default()
            },
            Codec::Av1 => RTCRtpCodecCapability { mime_type: MIME_TYPE_AV1.to_owned(), clock_rate: 90_000, ..Default::default() },
            Codec::H265 => RTCRtpCodecCapability { mime_type: MIME_TYPE_HEVC.to_owned(), clock_rate: 90_000, ..Default::default() },
        };
        let peer = Arc::new(
            self.api
                .new_peer_connection(RTCConfiguration::default())
                .await?,
        );
        let input_tx = self.input_tx.clone();
        let settings = self.settings.clone();
        let hardware_codecs = self.hardware_codecs;
        // NOTE: This measurement is fucking wrong and absolutely fucking braindead. This needs to be rethought.
        let encode_duration_us = self.encode_duration_us.clone();
        peer.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let input_tx = input_tx.clone();
            let settings = settings.clone();
            let encode_duration_us = encode_duration_us.clone();
            Box::pin(async move {
                if channel.label() != "input" {
                    return;
                }
                tracing::info!("input data channel connected");
                let open_channel = channel.clone();
                let open_settings = settings.clone();
                channel.on_open(Box::new(move || {
                    let channel = open_channel.clone();
                    let settings = open_settings.clone();
                    let encode_duration_us = encode_duration_us.clone();
                    Box::pin(async move {
                        let settings = *settings.read().await;
                        let message = serde_json::to_string(&StreamSettingsMessage {
                            r#type: "streamSettings",
                            codec: settings.codec,
                            resolution: settings.resolution,
                            bitrate: settings.bitrate,
                            host_cursor_visible: settings.host_cursor_visible,
                            hardware_codecs,
                        }).unwrap();
                        let _ = channel.send_text(message).await;
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                if channel.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
                                    break;
                                }
                                let stats = serde_json::to_string(&StreamStatsMessage {
                                    r#type: "streamStats",
                                    encode_ms: encode_duration_us.load(Ordering::Relaxed) as f64 / 1000.0,
                                }).unwrap();
                                if channel.send_text(stats).await.is_err() {
                                    break;
                                }
                            }
                        });
                    })
                }));
                let message_channel = channel.clone();
                channel.on_message(Box::new(move |message: DataChannelMessage| {
                    let input_tx = input_tx.clone();
                    let settings = settings.clone();
                    let channel = message_channel.clone();
                    Box::pin(async move {
                        if let Ok(ControlMessage::SetStreamSettings {
                            codec,
                            resolution,
                            bitrate,
                            host_cursor_visible,
                        }) = serde_json::from_slice::<ControlMessage>(&message.data)
                        {
                            let current = *settings.read().await;
                            let next = crate::capture::StreamSettings {
                                codec: codec.unwrap_or(current.codec),
                                resolution: resolution.unwrap_or(current.resolution).clamp(0.25, 1.0),
                                bitrate: bitrate.unwrap_or(current.bitrate).clamp(1_000_000, 200_000_000),
                                host_cursor_visible: host_cursor_visible.unwrap_or(current.host_cursor_visible),
                            };
                            *settings.write().await = next;
                            let response = serde_json::to_string(&StreamSettingsMessage {
                                r#type: "streamSettings",
                                codec: next.codec,
                                resolution: next.resolution,
                                bitrate: next.bitrate,
                                host_cursor_visible: next.host_cursor_visible,
                                hardware_codecs,
                            }).unwrap();
                            let _ = channel.send_text(response).await;
                        } else {
                            let _ = input_tx.send(message.data.to_vec());
                        }
                    })
                }));
            })
        }));
        let track = Arc::new(TrackLocalStaticSample::new(
            capability,
            "desktop".into(),
            "beam".into(),
        ));
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                ..Default::default()
            },
            "audio".into(),
            "beam".into(),
        ));
        let sender = peer
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let audio_sender = peer
            .add_track(audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        tokio::spawn(async move {
            let mut buffer = vec![0; 1500];
            while sender.read(&mut buffer).await.is_ok() {}
        });
        tokio::spawn(async move {
            let mut buffer = vec![0; 1500];
            while audio_sender.read(&mut buffer).await.is_ok() {}
        });
        peer.set_remote_description(RTCSessionDescription::offer(sdp)?)
            .await?;
        let mut gathered = peer.gathering_complete_promise().await;
        peer.set_local_description(peer.create_answer(None).await?)
            .await?;
        let _ = gathered.recv().await;
        let answer = peer
            .local_description()
            .await
            .context("missing local description")?;
        tokio::spawn(run_session(
            peer,
            track,
            audio_track,
            settings_snapshot,
            self.encode_duration_us.clone(),
        ));
        Ok(answer)
    }
}

async fn run_session(
    peer: Arc<RTCPeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    settings: crate::capture::StreamSettings,
    encode_duration_us: Arc<AtomicU64>,
) {
    let connected = tokio::time::timeout(Duration::from_secs(15), async {
        while matches!(
            peer.connection_state(),
            RTCPeerConnectionState::New | RTCPeerConnectionState::Connecting
        ) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok()
        && peer.connection_state() == RTCPeerConnectionState::Connected;
    if !connected {
        let _ = peer.close().await;
        return;
    }

    let (frames_tx, mut frames_rx) = mpsc::channel(1);
    let (audio_tx, mut audio_rx) = mpsc::channel(8);
    let (audio, audio_frames_tx) = crate::audio::spawn(audio_tx);
    let (capture, mut capture_errors) = capture::spawn(frames_tx, audio_frames_tx, settings);
    loop {
        tokio::select! {
            error = capture_errors.recv() => {
                if let Some(error) = error {
                    tracing::warn!(%error, "closing peer after capture failure");
                }
                break;
            }
            frame = frames_rx.recv() => {
                let Some(frame) = frame else { break };
                // Zero means "warmup batch, excluded from telemetry"; skip it.
                if !frame.encode_duration.is_zero() {
                    encode_duration_us.store(frame.encode_duration.as_micros() as u64, Ordering::Relaxed);
                }
                if let Err(error) = track.write_sample(&Sample {
                    data: frame.data.into(),
                    duration: frame.duration,
                    ..Default::default()
                }).await {
                    tracing::warn!(%error, "failed to send video frame");
                    break;
                }
            }
            frame = audio_rx.recv() => {
                let Some((data, duration)) = frame else { break };
                if let Err(error) = audio_track.write_sample(&Sample {
                    data: data.into(),
                    duration,
                    ..Default::default()
                }).await {
                    tracing::warn!(%error, "failed to send audio frame");
                    break;
                }
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                if peer.connection_state() != RTCPeerConnectionState::Connected {
                    break;
                }
            }
        }
    }
    drop(capture);
    drop(audio);
    let _ = peer.close().await;
}
