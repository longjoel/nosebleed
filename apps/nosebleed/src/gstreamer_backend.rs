use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use gst_app::{AppSinkCallbacks, AppSrc};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use rtp::packet::Packet as RtpPacket;
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8};
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc_util::marshal::Unmarshal;

use crate::media::{MediaRuntimeStatus, SelectedEncoder};

const FRAME_MAGIC: &[u8; 4] = b"NBF0";
const FRAME_HEADER_LEN: usize = 4 + 8 + 8 + 4 + 4 + 4 + 1 + 4;
const AUDIO_PACKET_MAGIC: &[u8; 4] = b"NBA0";
const AUDIO_PACKET_HEADER_LEN: usize = 34;
const DEFAULT_VIDEO_FRAME_DURATION_US: u64 = 16_666;

#[derive(Clone)]
pub struct SharedGstreamerMedia {
    pub video_track: Arc<TrackLocalStaticRTP>,
    pub audio_track: Arc<TrackLocalStaticRTP>,
    runtime: Arc<Mutex<MediaRuntimeStatus>>,
}

impl SharedGstreamerMedia {
    pub fn start(
        raw_video_rx: watch::Receiver<Option<Arc<[u8]>>>,
        audio_tx: broadcast::Sender<Arc<[u8]>>,
        selection: SelectedEncoder,
    ) -> Result<Self> {
        gst::init().context("failed to initialize GStreamer runtime")?;

        let spec = selection.spec;
        let runtime = Arc::new(Mutex::new(MediaRuntimeStatus {
            backend: "gstreamer",
            transport: "media-tracks",
            video_codec: Some(spec.video_codec),
            video_encoder: Some(spec.video_encoder),
            audio_codec: Some(spec.audio_codec),
            audio_encoder: Some(spec.audio_encoder),
            video_pipeline: Some(spec.video_pipeline.clone()),
            audio_pipeline: Some(spec.audio_pipeline.clone()),
            pipeline_state: "starting",
            dropped_video_frames: 0,
        }));

        let video_mime = if spec.video_codec == "h264" {
            MIME_TYPE_H264
        } else {
            MIME_TYPE_VP8
        };
        let video_clock_rate = if spec.video_codec == "h264" {
            90000
        } else {
            90000
        };

        let video_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: video_mime.to_owned(),
                clock_rate: video_clock_rate,
                ..Default::default()
            },
            "video".to_owned(),
            "nosebleed".to_owned(),
        ));
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1;sprop-maxcapturerate=48000".to_string(),
                ..Default::default()
            },
            "audio".to_owned(),
            "nosebleed".to_owned(),
        ));

        let video_pipeline = build_pipeline(spec.video_pipeline.as_str(), "video")?;
        let audio_pipeline = build_pipeline(spec.audio_pipeline.as_str(), "audio")?;
        let video_src = required_appsrc(&video_pipeline, "video_src")?;
        let audio_src = required_appsrc(&audio_pipeline, "audio_src")?;
        let video_sink = required_appsink(&video_pipeline, "video_sink")?;
        let audio_sink = required_appsink(&audio_pipeline, "audio_sink")?;

        let (video_rtp_tx, video_rtp_rx) = mpsc::channel::<Vec<u8>>(64);
        let (audio_rtp_tx, audio_rtp_rx) = mpsc::channel::<Vec<u8>>(128);

        install_appsink_bridge(&video_sink, video_rtp_tx, runtime.clone(), true);
        install_appsink_bridge(&audio_sink, audio_rtp_tx, runtime.clone(), false);

        video_pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| anyhow!("failed to start video GStreamer pipeline: {err:?}"))?;
        audio_pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| anyhow!("failed to start audio GStreamer pipeline: {err:?}"))?;

        eprintln!(
            "gstreamer media backend selected: video_encoder={} audio_encoder={}",
            spec.video_encoder, spec.audio_encoder
        );
        eprintln!("gstreamer video pipeline: {}", spec.video_pipeline);
        eprintln!("gstreamer audio pipeline: {}", spec.audio_pipeline);

        set_pipeline_state(&runtime, "playing");

        tokio::spawn(drain_rtp_packets(
            video_rtp_rx,
            video_track.clone(),
            runtime.clone(),
        ));
        tokio::spawn(drain_rtp_packets(
            audio_rtp_rx,
            audio_track.clone(),
            runtime.clone(),
        ));
        tokio::spawn(feed_video_appsrc(raw_video_rx, video_src, runtime.clone()));
        tokio::spawn(feed_audio_appsrc(
            audio_tx.subscribe(),
            audio_src,
            runtime.clone(),
        ));

        Ok(Self {
            video_track,
            audio_track,
            runtime,
        })
    }

    pub fn snapshot(&self) -> MediaRuntimeStatus {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn build_pipeline(description: &str, label: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(description)
        .with_context(|| format!("failed to parse {label} GStreamer pipeline"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("{label} pipeline description did not produce a gst::Pipeline"))
}

fn required_appsrc(pipeline: &gst::Pipeline, name: &str) -> Result<AppSrc> {
    pipeline
        .by_name(name)
        .with_context(|| format!("missing GStreamer element '{name}'"))?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| anyhow!("element '{name}' was not an AppSrc"))
}

fn required_appsink(pipeline: &gst::Pipeline, name: &str) -> Result<gst_app::AppSink> {
    pipeline
        .by_name(name)
        .with_context(|| format!("missing GStreamer element '{name}'"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("element '{name}' was not an AppSink"))
}

fn install_appsink_bridge(
    sink: &gst_app::AppSink,
    packet_tx: mpsc::Sender<Vec<u8>>,
    runtime: Arc<Mutex<MediaRuntimeStatus>>,
    is_video: bool,
) {
    sink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                if packet_tx.try_send(map.as_slice().to_vec()).is_err() {
                    if is_video {
                        increment_dropped_video_frames(&runtime, 1);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

async fn drain_rtp_packets(
    mut packet_rx: mpsc::Receiver<Vec<u8>>,
    track: Arc<TrackLocalStaticRTP>,
    runtime: Arc<Mutex<MediaRuntimeStatus>>,
) {
    while let Some(raw_packet) = packet_rx.recv().await {
        let mut bytes = Bytes::from(raw_packet);
        match RtpPacket::unmarshal(&mut bytes) {
            Ok(packet) => {
                if let Err(err) = track.write_rtp_with_extensions(&packet, &[]).await {
                    set_pipeline_state(&runtime, "error");
                    eprintln!("failed to write RTP packet to webrtc track: {err:#}");
                    break;
                }
            }
            Err(err) => {
                set_pipeline_state(&runtime, "error");
                eprintln!("failed to decode RTP packet from GStreamer appsink: {err:#}");
                break;
            }
        }
    }
}

async fn feed_video_appsrc(
    mut raw_video_rx: watch::Receiver<Option<Arc<[u8]>>>,
    video_src: AppSrc,
    runtime: Arc<Mutex<MediaRuntimeStatus>>,
) {
    let mut last_sequence = None;
    let mut last_timestamp_us = None;
    let mut last_caps_key = None::<(u32, u32, usize, u8)>;

    while raw_video_rx.changed().await.is_ok() {
        let Some(packet) = raw_video_rx.borrow().clone() else {
            continue;
        };
        let Some(frame) = decode_raw_frame_packet(packet.as_ref()) else {
            continue;
        };

        if let Some(previous_sequence) = last_sequence {
            let skipped = frame.sequence.saturating_sub(previous_sequence + 1);
            if skipped > 0 {
                increment_dropped_video_frames(&runtime, skipped);
            }
        }
        last_sequence = Some(frame.sequence);

        let caps_key = (frame.width, frame.height, frame.pitch, frame.pixel_format);
        if last_caps_key != Some(caps_key) {
            match build_video_caps(&frame) {
                Ok(caps) => video_src.set_caps(Some(&caps)),
                Err(err) => {
                    set_pipeline_state(&runtime, "error");
                    eprintln!("failed to build GStreamer video caps: {err:#}");
                    break;
                }
            }
            last_caps_key = Some(caps_key);
        }

        let payload = match repack_video_payload(&frame) {
            Some(payload) => payload,
            None => continue,
        };
        let duration_us = last_timestamp_us
            .map(|previous| frame.timestamp_us.saturating_sub(previous).max(1))
            .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_US);
        last_timestamp_us = Some(frame.timestamp_us);

        let mut buffer = gst::Buffer::from_mut_slice(payload);
        if let Some(buffer_mut) = buffer.get_mut() {
            buffer_mut.set_duration(gst::ClockTime::from_useconds(duration_us));
        }

        if let Err(err) = video_src.push_buffer(buffer) {
            set_pipeline_state(&runtime, "error");
            eprintln!("failed to push video frame into GStreamer appsrc: {err:?}");
            break;
        }
    }

    let _ = video_src.end_of_stream();
}

async fn feed_audio_appsrc(
    mut audio_rx: broadcast::Receiver<Arc<[u8]>>,
    audio_src: AppSrc,
    runtime: Arc<Mutex<MediaRuntimeStatus>>,
) {
    let mut last_caps_key = None::<(u32, u8)>;

    while let Ok(packet) = audio_rx.recv().await {
        let Some(audio) = decode_audio_packet(packet.as_ref()) else {
            continue;
        };

        let caps_key = (audio.sample_rate_hz, audio.channels);
        if last_caps_key != Some(caps_key) {
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("layout", "interleaved")
                .field("channels", i32::from(audio.channels))
                .field("rate", audio.sample_rate_hz as i32)
                .build();
            audio_src.set_caps(Some(&caps));
            last_caps_key = Some(caps_key);
        }

        let mut payload = Vec::with_capacity(audio.samples.len() * std::mem::size_of::<i16>());
        for sample in &audio.samples {
            payload.extend_from_slice(&sample.to_le_bytes());
        }

        let mut buffer = gst::Buffer::from_mut_slice(payload);
        if let Some(buffer_mut) = buffer.get_mut() {
            let duration_us = ((audio.frame_count as u64) * 1_000_000)
                .saturating_div(audio.sample_rate_hz.max(1) as u64)
                .max(1);
            buffer_mut.set_duration(gst::ClockTime::from_useconds(duration_us));
        }

        if let Err(err) = audio_src.push_buffer(buffer) {
            set_pipeline_state(&runtime, "error");
            eprintln!("failed to push audio packet into GStreamer appsrc: {err:?}");
            break;
        }
    }

    let _ = audio_src.end_of_stream();
}

fn build_video_caps(frame: &RawFramePacket) -> Result<gst::Caps> {
    let format = match frame.pixel_format {
        0 => "BGRx",
        1 => "RGB16",
        2 => "xRGB1555",
        other => return Err(anyhow!("unsupported libretro pixel format {other}")),
    };

    Ok(gst::Caps::builder("video/x-raw")
        .field("format", format)
        .field("width", frame.width as i32)
        .field("height", frame.height as i32)
        .field("framerate", gst::Fraction::new(60, 1))
        .build())
}

fn repack_video_payload(frame: &RawFramePacket) -> Option<Vec<u8>> {
    let bytes_per_pixel = match frame.pixel_format {
        0 => 4,
        1 | 2 => 2,
        _ => return None,
    };
    let width = frame.width as usize;
    let height = frame.height as usize;
    let row_bytes = width.checked_mul(bytes_per_pixel)?;
    let packed_len = row_bytes.checked_mul(height)?;
    if frame.payload.len() < frame.pitch.checked_mul(height)? {
        return None;
    }

    if frame.pitch == row_bytes && frame.payload.len() >= packed_len {
        return Some(frame.payload[..packed_len].to_vec());
    }

    let mut packed = Vec::with_capacity(packed_len);
    for y in 0..height {
        let start = y.checked_mul(frame.pitch)?;
        let end = start.checked_add(row_bytes)?;
        packed.extend_from_slice(frame.payload.get(start..end)?);
    }
    Some(packed)
}

fn set_pipeline_state(runtime: &Arc<Mutex<MediaRuntimeStatus>>, pipeline_state: &'static str) {
    let mut guard = runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.pipeline_state = pipeline_state;
}

fn increment_dropped_video_frames(runtime: &Arc<Mutex<MediaRuntimeStatus>>, count: u64) {
    let mut guard = runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.dropped_video_frames = guard.dropped_video_frames.saturating_add(count);
}

#[derive(Debug, Clone)]
struct RawFramePacket {
    sequence: u64,
    timestamp_us: u64,
    width: u32,
    height: u32,
    pitch: usize,
    pixel_format: u8,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct AudioPacket {
    #[allow(dead_code)]
    timestamp_us: u64,
    sample_rate_hz: u32,
    channels: u8,
    frame_count: u32,
    samples: Vec<i16>,
}

fn decode_raw_frame_packet(packet: &[u8]) -> Option<RawFramePacket> {
    if packet.len() < FRAME_HEADER_LEN || &packet[..4] != FRAME_MAGIC {
        return None;
    }

    let sequence = le_u64(&packet[4..12]);
    let timestamp_us = le_u64(&packet[12..20]);
    let width = le_u32(&packet[20..24]);
    let height = le_u32(&packet[24..28]);
    let pitch = le_u32(&packet[28..32]) as usize;
    let pixel_format = packet[32];
    let payload_len = le_u32(&packet[33..37]) as usize;
    if FRAME_HEADER_LEN + payload_len > packet.len() {
        return None;
    }

    Some(RawFramePacket {
        sequence,
        timestamp_us,
        width,
        height,
        pitch,
        pixel_format,
        payload: packet[FRAME_HEADER_LEN..FRAME_HEADER_LEN + payload_len].to_vec(),
    })
}

fn decode_audio_packet(packet: &[u8]) -> Option<AudioPacket> {
    if packet.len() < AUDIO_PACKET_HEADER_LEN || &packet[..4] != AUDIO_PACKET_MAGIC {
        return None;
    }

    let timestamp_us = le_u64(&packet[12..20]);
    let sample_rate_hz = le_u32(&packet[20..24]);
    let channels = packet[24].max(1);
    let sample_format = packet[25];
    if sample_format != 0 {
        return None;
    }
    let frame_count = le_u32(&packet[26..30]);
    let payload_len = le_u32(&packet[30..34]) as usize;
    if AUDIO_PACKET_HEADER_LEN + payload_len > packet.len() || payload_len % 2 != 0 {
        return None;
    }

    let payload = &packet[AUDIO_PACKET_HEADER_LEN..AUDIO_PACKET_HEADER_LEN + payload_len];
    let mut samples = Vec::with_capacity(payload_len / 2);
    for chunk in payload.chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }

    let expected_samples = frame_count as usize * channels as usize;
    if samples.len() != expected_samples {
        return None;
    }

    Some(AudioPacket {
        timestamp_us,
        sample_rate_hz,
        channels,
        frame_count,
        samples,
    })
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(out)
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(out)
}
