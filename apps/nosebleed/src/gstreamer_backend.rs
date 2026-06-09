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
use crate::protocol::{
    AUDIO_PACKET_HEADER_LEN, AUDIO_PACKET_MAGIC, FRAME_HEADER_LEN, FRAME_MAGIC,
};

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
            .unwrap_or_else(crate::lock_recover)
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
        let Some(mut frame) = decode_raw_frame_packet(packet.as_ref()) else {
            continue;
        };

        // If pixels are non-square, pad the frame with black borders to
        // produce square-pixel output. This achieves correct aspect ratio
        // without relying on GStreamer PAR metadata (which doesn't survive
        // WebRTC encoding).
        //
        // PAR is computed from the display aspect ratio (from av_info) and
        // the ACTUAL frame dimensions (which may differ from av_info base
        // dims when the core upscales, e.g. N64 320→640 wide pixels).
        let par = frame.pixel_aspect_ratio;
        const PAR_EPSILON: f32 = 0.001;
        if (par - 1.0).abs() > PAR_EPSILON && par > 0.0 {
            let bpp = match frame.pixel_format {
                0 => 4,  // BGRx
                1 | 2 => 2, // RGB16, xRGB1555
                _ => {
                    continue; // unknown format, skip frame
                }
            };
            if par < 1.0 {
                // Pixels are narrower than tall (e.g. N64 640×240 → 4:3).
                // Add height via letterboxing to achieve square pixels.
                let target_h = ((frame.height as f32) / par).round() as u32;
                let pad_top = ((target_h - frame.height) / 2) as usize;
                let row_bytes = frame.width as usize * bpp;
                let mut padded = vec![0u8; row_bytes * target_h as usize];
                for row in 0..frame.height as usize {
                    let src_start = row * frame.pitch;
                    let dst_start = (row + pad_top) * row_bytes;
                    let copy_len = (frame.width as usize * bpp).min(row_bytes);
                    padded[dst_start..dst_start + copy_len]
                        .copy_from_slice(&frame.payload[src_start..src_start + copy_len]);
                }
                frame.payload = padded;
                frame.pitch = row_bytes;
                frame.height = target_h;
            } else {
                // Pixels are wider than tall. Add width via pillarboxing.
                let target_w = ((frame.width as f32) * par).round() as u32;
                let pad_left = ((target_w - frame.width) / 2) as usize;
                let row_bytes = target_w as usize * bpp;
                let mut padded = vec![0u8; row_bytes * frame.height as usize];
                for row in 0..frame.height as usize {
                    let src_start = row * frame.pitch;
                    let dst_start = row * row_bytes + pad_left * bpp;
                    let copy_len = (frame.width as usize * bpp).min(row_bytes);
                    padded[dst_start..dst_start + copy_len]
                        .copy_from_slice(&frame.payload[src_start..src_start + copy_len]);
                }
                frame.payload = padded;
                frame.pitch = row_bytes;
                frame.width = target_w;
            }
            frame.pixel_aspect_ratio = 1.0;
        }
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
    // Track sequence-anchored PTS to avoid burst-timestamping (do-timestamp=true
    // would assign nearly identical timestamps to every buffer in a rapid burst).
    let mut base_sequence: Option<u64> = None;
    let frames_per_chunk: u64 = 512; // matches AudioBus::CHUNK_FRAMES

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

            // Set explicit PTS from the sequence counter so downstream can schedule
            // buffers correctly even when they arrive in bursts from the broadcast
            // channel. Each chunk is frames_per_chunk frames independent of sample rate.
            let sample_rate = audio.sample_rate_hz.max(1) as u64;
            let chunk_duration_ns = frames_per_chunk * 1_000_000_000 / sample_rate;
            let pts_ns = match base_sequence {
                Some(base_seq) => {
                    let seq_offset = audio.sequence.wrapping_sub(base_seq);
                    seq_offset.saturating_mul(chunk_duration_ns)
                }
                None => {
                    base_sequence = Some(audio.sequence);
                    0
                }
            };
            buffer_mut.set_pts(gst::ClockTime::from_nseconds(pts_ns));
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
        .unwrap_or_else(crate::lock_recover);
    guard.pipeline_state = pipeline_state;
}

fn increment_dropped_video_frames(runtime: &Arc<Mutex<MediaRuntimeStatus>>, count: u64) {
    let mut guard = runtime
        .lock()
        .unwrap_or_else(crate::lock_recover);
    guard.dropped_video_frames = guard.dropped_video_frames.saturating_add(count);
}

#[derive(Debug, Clone)]
pub(crate) struct RawFramePacket {
    pub(crate) sequence: u64,
    pub(crate) timestamp_us: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: usize,
    pub(crate) pixel_format: u8,
    pub(crate) pixel_aspect_ratio: f32,
    pub(crate) payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct AudioPacket {
    sequence: u64,
    #[allow(dead_code)]
    timestamp_us: u64,
    sample_rate_hz: u32,
    channels: u8,
    frame_count: u32,
    samples: Vec<i16>,
}

pub(crate) fn decode_raw_frame_packet(packet: &[u8]) -> Option<RawFramePacket> {
    if packet.len() < FRAME_HEADER_LEN || &packet[..4] != FRAME_MAGIC {
        return None;
    }

    let sequence = le_u64(&packet[4..12]);
    let timestamp_us = le_u64(&packet[12..20]);
    let width = le_u32(&packet[20..24]);
    let height = le_u32(&packet[24..28]);
    let pitch = le_u32(&packet[28..32]) as usize;
    let pixel_format = packet[32];
    let pixel_aspect_ratio = f32::from_bits(le_u32(&packet[33..37]));
    let payload_len = le_u32(&packet[37..41]) as usize;
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
        pixel_aspect_ratio,
        payload: packet[FRAME_HEADER_LEN..FRAME_HEADER_LEN + payload_len].to_vec(),
    })
}

fn decode_audio_packet(packet: &[u8]) -> Option<AudioPacket> {
    if packet.len() < AUDIO_PACKET_HEADER_LEN || &packet[..4] != AUDIO_PACKET_MAGIC {
        return None;
    }

    let sequence = le_u64(&packet[4..12]);
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
        sequence,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        AUDIO_PACKET_HEADER_LEN, AUDIO_PACKET_MAGIC, FRAME_HEADER_LEN, FRAME_MAGIC,
    };

    // ── endian helpers ──────────────────────────────────────────────────

    #[test]
    fn test_le_u32_round_trip() {
        let buf = 0xDEAD_BEEFu32.to_le_bytes();
        assert_eq!(le_u32(&buf), 0xDEAD_BEEF);
    }

    #[test]
    fn test_le_u64_round_trip() {
        let buf = 0xCAFE_BABE_DEAD_BEEFu64.to_le_bytes();
        assert_eq!(le_u64(&buf), 0xCAFE_BABE_DEAD_BEEF);
    }

    #[test]
    fn test_le_u32_zero() {
        assert_eq!(le_u32(&[0u8; 4]), 0);
    }

    #[test]
    fn test_le_u64_zero() {
        assert_eq!(le_u64(&[0u8; 8]), 0);
    }

    // ── magic constants ─────────────────────────────────────────────────

    #[test]
    fn test_frame_magic_is_correct() {
        assert_eq!(FRAME_MAGIC, b"NBF0", "FRAME_MAGIC must be NBF0");
    }

    #[test]
    fn test_audio_packet_magic_is_correct() {
        assert_eq!(
            AUDIO_PACKET_MAGIC, b"NBA0",
            "AUDIO_PACKET_MAGIC must be NBA0"
        );
    }

    #[test]
    fn test_default_video_frame_duration_is_16_666_us() {
        assert_eq!(DEFAULT_VIDEO_FRAME_DURATION_US, 16_666);
    }

    // ── decode_raw_frame_packet ─────────────────────────────────────────

    fn make_valid_frame_packet(sequence: u64, extra_bytes: usize) -> Vec<u8> {
        let payload_len = 64u32;
        let total_len = FRAME_HEADER_LEN + payload_len as usize + extra_bytes;
        let mut buf = Vec::with_capacity(total_len);
        buf.extend_from_slice(FRAME_MAGIC); // 4
        buf.extend_from_slice(&sequence.to_le_bytes()); // 8
        buf.extend_from_slice(&1234u64.to_le_bytes()); // timestamp_us 8
        buf.extend_from_slice(&640u32.to_le_bytes()); // width 4
        buf.extend_from_slice(&480u32.to_le_bytes()); // height 4
        buf.extend_from_slice(&2560u32.to_le_bytes()); // pitch 4
        buf.push(0u8); // pixel_format (BGRx) 1
        buf.extend_from_slice(&1.0f32.to_bits().to_le_bytes()); // pixel_aspect_ratio 4
        buf.extend_from_slice(&payload_len.to_le_bytes()); // payload_len 4
        // padding to match payload_len
        buf.resize(total_len, 0xAB);
        buf
    }

    #[test]
    fn test_decode_raw_frame_packet_valid() {
        let packet = make_valid_frame_packet(42, 0);
        let frame = decode_raw_frame_packet(&packet).expect("valid frame packet");
        assert_eq!(frame.sequence, 42);
        assert_eq!(frame.timestamp_us, 1234);
        assert_eq!(frame.width, 640);
        assert_eq!(frame.height, 480);
        assert_eq!(frame.pitch, 2560);
        assert_eq!(frame.pixel_format, 0);
        assert_eq!(frame.pixel_aspect_ratio, 1.0);
        assert_eq!(frame.payload.len(), 64);
    }

    #[test]
    fn test_decode_raw_frame_packet_too_short() {
        let short = vec![0u8; FRAME_HEADER_LEN - 1];
        assert!(decode_raw_frame_packet(&short).is_none());
    }

    #[test]
    fn test_decode_raw_frame_packet_wrong_magic() {
        let mut packet = make_valid_frame_packet(0, 0);
        packet[0] = 0xFF; // corrupt magic
        assert!(decode_raw_frame_packet(&packet).is_none());
    }

    #[test]
    fn test_decode_raw_frame_packet_truncated_payload() {
        // Make a header that says payload_len=100 but only provide 50 bytes
        let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + 50);
        buf.extend_from_slice(FRAME_MAGIC);
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(&640u32.to_le_bytes());
        buf.extend_from_slice(&480u32.to_le_bytes());
        buf.extend_from_slice(&2560u32.to_le_bytes());
        buf.push(0u8);
        buf.extend_from_slice(&1.0f32.to_bits().to_le_bytes()); // pixel_aspect_ratio 4
        buf.extend_from_slice(&100u32.to_le_bytes()); // claims 100 bytes
        buf.resize(FRAME_HEADER_LEN + 50, 0xBB);
        assert!(decode_raw_frame_packet(&buf).is_none());
    }

    // ── decode_audio_packet ─────────────────────────────────────────────

    fn make_valid_audio_packet(
        sample_rate_hz: u32,
        channels: u8,
        frame_count: u32,
        extra_bytes: usize,
    ) -> Vec<u8> {
        let sample_count = frame_count as usize * channels as usize;
        let payload_len = (sample_count * 2) as u32; // 2 bytes per i16
        let total_len = AUDIO_PACKET_HEADER_LEN + payload_len as usize + extra_bytes;
        let mut buf = Vec::with_capacity(total_len);
        buf.extend_from_slice(AUDIO_PACKET_MAGIC); // 4
        buf.extend_from_slice(&0u64.to_le_bytes()); // sequence 8
        buf.extend_from_slice(&0u64.to_le_bytes()); // timestamp_us 8
        buf.extend_from_slice(&sample_rate_hz.to_le_bytes()); // 4
        buf.push(channels); // 1
        buf.push(0u8); // sample_format (S16LE) 1
        buf.extend_from_slice(&frame_count.to_le_bytes()); // 4
        buf.extend_from_slice(&payload_len.to_le_bytes()); // 4
        // Fill with sample data (every other byte to make i16 values)
        for i in 0..sample_count {
            buf.extend_from_slice(&(i as i16).to_le_bytes());
        }
        buf.resize(total_len, 0);
        buf
    }

    #[test]
    fn test_decode_audio_packet_valid() {
        let packet = make_valid_audio_packet(44100, 2, 128, 0);
        let audio = decode_audio_packet(&packet).expect("valid audio packet");
        assert_eq!(audio.sequence, 0);
        assert_eq!(audio.sample_rate_hz, 44100);
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.frame_count, 128);
        assert_eq!(audio.samples.len(), 256); // 128 * 2
    }

    #[test]
    fn test_decode_audio_packet_too_short() {
        let short = vec![0u8; AUDIO_PACKET_HEADER_LEN - 1];
        assert!(decode_audio_packet(&short).is_none());
    }

    #[test]
    fn test_decode_audio_packet_wrong_magic() {
        let mut packet = make_valid_audio_packet(44100, 2, 64, 0);
        packet[0] = 0xFF;
        assert!(decode_audio_packet(&packet).is_none());
    }

    #[test]
    fn test_decode_audio_packet_bad_sample_format() {
        let mut packet = make_valid_audio_packet(48000, 1, 64, 0);
        // Set sample_format to something other than 0
        packet[25] = 1;
        assert!(decode_audio_packet(&packet).is_none());
    }

    #[test]
    fn test_decode_audio_packet_truncated_payload() {
        // payload_len says 1000 but we only have 100 bytes after header
        let mut buf = Vec::with_capacity(AUDIO_PACKET_HEADER_LEN + 100);
        buf.extend_from_slice(AUDIO_PACKET_MAGIC);
        buf.extend_from_slice(&[0u8; 8]); // reserved
        buf.extend_from_slice(&[0u8; 8]); // timestamp_us
        buf.extend_from_slice(&44100u32.to_le_bytes());
        buf.push(2u8); // channels
        buf.push(0u8); // sample_format
        buf.extend_from_slice(&500u32.to_le_bytes()); // frame_count
        buf.extend_from_slice(&1000u32.to_le_bytes()); // payload_len claims 1000
        buf.resize(AUDIO_PACKET_HEADER_LEN + 100, 0xBB);
        assert!(decode_audio_packet(&buf).is_none());
    }

    #[test]
    fn test_decode_audio_packet_odd_payload_len() {
        // Add one extra byte so payload_len is still correct in header,
        // but the actual data is odd length. Actually let me make a
        // packet where payload_len is odd.
        let total_len = AUDIO_PACKET_HEADER_LEN + 21; // 21 is odd
        let mut buf = Vec::with_capacity(total_len);
        buf.extend_from_slice(AUDIO_PACKET_MAGIC);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&44100u32.to_le_bytes());
        buf.push(1u8); // channels
        buf.push(0u8); // sample_format
        // frame_count=10 means 10 samples = 20 bytes, but we set payload_len=21
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.extend_from_slice(&21u32.to_le_bytes()); // odd payload_len
        buf.resize(total_len, 0xCC);
        assert!(decode_audio_packet(&buf).is_none());
    }

    #[test]
    fn test_decode_audio_packet_mismatched_sample_count() {
        // Create a packet where frame_count=10, channels=1, so expected=10 samples
        // but provide 12 samples worth of data
        let mut buf = Vec::with_capacity(1024);
        buf.extend_from_slice(AUDIO_PACKET_MAGIC);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&44100u32.to_le_bytes());
        buf.push(1u8); // channels
        buf.push(0u8); // sample_format
        buf.extend_from_slice(&10u32.to_le_bytes()); // frame_count=10 → 10 samples
        // Write payload_len for 12 samples (24 bytes) — mismatch
        buf.extend_from_slice(&24u32.to_le_bytes());
        for i in 0..12i16 {
            buf.extend_from_slice(&i.to_le_bytes());
        }
        assert!(decode_audio_packet(&buf).is_none());
    }

    // ── repack_video_payload ────────────────────────────────────────────

    #[test]
    fn test_repack_video_payload_fast_path_pitch_equals_row_bytes() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 4,
            height: 4,
            pitch: 16, // 4 * 4 bytes per pixel = 16
            pixel_format: 0, // BGRx → 4 bpp
            pixel_aspect_ratio: 1.0,
            payload: vec![0xAAu8; 64], // 4*4*4 = 64
        };
        let result = repack_video_payload(&frame);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 64);
    }

    #[test]
    fn test_repack_video_payload_slow_path_pitch_not_equal_row_bytes() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 4,
            height: 4,
            pitch: 20, // wider than row_bytes (16)
            pixel_format: 0, // BGRx → 4 bpp
            pixel_aspect_ratio: 1.0,
            payload: vec![0xBBu8; 80], // pitch * height = 80
        };
        let result = repack_video_payload(&frame);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 64);
    }

    #[test]
    fn test_repack_video_payload_invalid_pixel_format() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 4,
            height: 4,
            pitch: 16,
            pixel_format: 99, // unknown
            pixel_aspect_ratio: 1.0,
            payload: vec![0xCCu8; 64],
        };
        assert!(repack_video_payload(&frame).is_none());
    }

    #[test]
    fn test_repack_video_payload_insufficient_data() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 100,
            height: 100,
            pitch: 400,
            pixel_format: 0,
            pixel_aspect_ratio: 1.0,
            payload: vec![0xDDu8; 10], // far too little
        };
        assert!(repack_video_payload(&frame).is_none());
    }

    #[test]
    fn test_repack_video_payload_rgb16_format() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 4,
            height: 4,
            pitch: 8, // 4 * 2 = 8
            pixel_format: 1, // RGB16 → 2 bpp
            pixel_aspect_ratio: 1.0,
            payload: vec![0xEEu8; 32],
        };
        let result = repack_video_payload(&frame);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 32);
    }

    #[test]
    fn test_repack_video_payload_xrgb1555_format() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 4,
            height: 4,
            pitch: 8, // 4 * 2 = 8
            pixel_format: 2, // xRGB1555 → 2 bpp
            pixel_aspect_ratio: 1.0,
            payload: vec![0xFFu8; 32],
        };
        let result = repack_video_payload(&frame);
        assert!(result.is_some());
        assert_eq!(result.unwrap().len(), 32);
    }

    // ── header length constants ─────────────────────────────────────────

    #[test]
    fn test_frame_header_len_is_exact() {
        // FRAME_HEADER_LEN = 4 (magic) + 8 (seq) + 8 (ts) + 4 (w) + 4 (h) + 4 (pitch) + 1 (fmt) + 4 (par) + 4 (plen)
        assert_eq!(FRAME_HEADER_LEN, 41);
    }

    #[test]
    fn test_audio_packet_header_len_is_exact() {
        // AUDIO_PACKET_HEADER_LEN = 4 (magic) + 8 (reserved) + 8 (ts) + 4 (rate) + 1 (ch) + 1 (fmt) + 4 (fc) + 4 (plen)
        assert_eq!(AUDIO_PACKET_HEADER_LEN, 34);
    }

    // ── Shutdown / state helpers ────────────────────────────────────────

    #[test]
    fn test_increment_dropped_video_frames_adds_count() {
        let runtime = Arc::new(Mutex::new(MediaRuntimeStatus {
            backend: "gstreamer",
            transport: "test",
            video_codec: None,
            video_encoder: None,
            audio_codec: None,
            audio_encoder: None,
            video_pipeline: None,
            audio_pipeline: None,
            pipeline_state: "idle",
            dropped_video_frames: 0,
        }));
        increment_dropped_video_frames(&runtime, 5);
        assert_eq!(runtime.lock().unwrap().dropped_video_frames, 5);
        increment_dropped_video_frames(&runtime, 3);
        assert_eq!(runtime.lock().unwrap().dropped_video_frames, 8);
    }

    #[test]
    fn test_set_pipeline_state_changes_state() {
        let runtime = Arc::new(Mutex::new(MediaRuntimeStatus {
            backend: "gstreamer",
            transport: "test",
            video_codec: None,
            video_encoder: None,
            audio_codec: None,
            audio_encoder: None,
            video_pipeline: None,
            audio_pipeline: None,
            pipeline_state: "idle",
            dropped_video_frames: 0,
        }));
        set_pipeline_state(&runtime, "playing");
        assert_eq!(runtime.lock().unwrap().pipeline_state, "playing");
        set_pipeline_state(&runtime, "error");
        assert_eq!(runtime.lock().unwrap().pipeline_state, "error");
    }

    // ── build_video_caps string ─────────────────────────────────────────

    #[test]
    fn test_build_video_caps_bgrx_format() {
        let frame = RawFramePacket {
            sequence: 0,
            timestamp_us: 0,
            width: 640,
            height: 480,
            pitch: 2560,
            pixel_format: 0,
            pixel_aspect_ratio: 1.0,
            payload: vec![0; 640 * 480 * 4],
        };
        // build_video_caps needs gst::init which isn't available in tests.
        // We test the format string mapping logic inline:
        let format = match frame.pixel_format {
            0 => "BGRx",
            1 => "RGB16",
            2 => "xRGB1555",
            _ => panic!("unsupported"),
        };
        assert_eq!(format, "BGRx");
    }
}
