use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::{
    api::{
        APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine},
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

use crate::capture::{self, CODEC, Codec};

pub struct Media {
    api: webrtc::api::API,
    capability: RTCRtpCodecCapability,
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl Media {
    pub fn new(input_tx: std::sync::mpsc::Sender<Vec<u8>>) -> Result<Arc<Self>> {
        let mut engine = MediaEngine::default();
        let (capability, payload_type) = match CODEC {
            Codec::Av1 => (
                RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_AV1.to_owned(),
                    clock_rate: 90_000,
                    ..Default::default()
                },
                45,
            ),
            Codec::H264 => (
                RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                    ..Default::default()
                },
                102,
            ),
        };
        engine.register_codec(
            RTCRtpCodecParameters {
                capability: capability.clone(),
                payload_type,
                ..Default::default()
            },
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
            capability,
            input_tx,
        });
        Ok(media)
    }

    pub async fn answer(&self, sdp: String) -> Result<RTCSessionDescription> {
        let peer = Arc::new(
            self.api
                .new_peer_connection(RTCConfiguration::default())
                .await?,
        );
        let input_tx = self.input_tx.clone();
        peer.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let input_tx = input_tx.clone();
            Box::pin(async move {
                if channel.label() != "input" {
                    return;
                }
                tracing::info!("input data channel connected");
                channel.on_message(Box::new(move |message: DataChannelMessage| {
                    let input_tx = input_tx.clone();
                    Box::pin(async move {
                        let _ = input_tx.send(message.data.to_vec());
                    })
                }));
            })
        }));
        let track = Arc::new(TrackLocalStaticSample::new(
            self.capability.clone(),
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
        tokio::spawn(run_session(peer, track, audio_track));
        Ok(answer)
    }
}

async fn run_session(
    peer: Arc<RTCPeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
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
    let (capture, mut capture_errors) = capture::spawn(frames_tx, audio_frames_tx);
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
