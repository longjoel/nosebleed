use std::env;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaBackend {
    Gstreamer,
}

impl MediaBackend {
    pub const ENV_VAR: &str = "NOSEBLEED_MEDIA_BACKEND";

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "gstreamer" | "gst" => Ok(Self::Gstreamer),
            other => Err(anyhow!(
                "unsupported media backend '{other}' (expected gstreamer)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gstreamer => "gstreamer",
        }
    }
}

impl Default for MediaBackend {
    fn default() -> Self {
        Self::Gstreamer
    }
}

// ── Video codec / encoder configuration ────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Vp8,
    Vp9,
    Av1,
}

impl VideoCodec {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoEncoderSelection {
    Auto,
    Software,
    Nvenc,
    Qsv,
    Vaapi,
    V4l2,
    X264,
    Vp8,
}

impl VideoEncoderSelection {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "software" => Ok(Self::Software),
            "nvenc" => Ok(Self::Nvenc),
            "qsv" => Ok(Self::Qsv),
            "vaapi" => Ok(Self::Vaapi),
            "v4l2" => Ok(Self::V4l2),
            "x264" => Ok(Self::X264),
            "vp8" => Ok(Self::Vp8),
            other => Err(anyhow!("unsupported video encoder '{other}'")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Software => "software",
            Self::Nvenc => "nvenc",
            Self::Qsv => "qsv",
            Self::Vaapi => "vaapi",
            Self::V4l2 => "v4l2",
            Self::X264 => "x264",
            Self::Vp8 => "vp8",
        }
    }

    pub fn is_hardware(self) -> bool {
        matches!(self, Self::Nvenc | Self::Qsv | Self::Vaapi | Self::V4l2)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoEncoderConfig {
    pub codec: VideoCodec,
    pub encoder: VideoEncoderSelection,
    pub bitrate_kbps: u32,
    pub keyframe_interval: u32,
    pub low_latency: bool,
}

impl Default for VideoEncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H264,
            encoder: VideoEncoderSelection::Auto,
            bitrate_kbps: 2500,
            keyframe_interval: 60,
            low_latency: true,
        }
    }
}

impl VideoEncoderConfig {
    const CODEC_ENV: &str = "NOSEBLEED_VIDEO_CODEC";
    const ENCODER_ENV: &str = "NOSEBLEED_VIDEO_ENCODER";
    const BITRATE_ENV: &str = "NOSEBLEED_VIDEO_BITRATE_KBPS";
    const KEYFRAME_ENV: &str = "NOSEBLEED_VIDEO_KEYFRAME_INTERVAL";
    const LOW_LATENCY_ENV: &str = "NOSEBLEED_VIDEO_LOW_LATENCY";

    pub fn from_env(codec_override: Option<&str>, encoder_override: Option<&str>) -> Result<Self> {
        let mut config = Self::default();

        let codec_raw = codec_override
            .map(|s| s.to_owned())
            .or_else(|| env::var(Self::CODEC_ENV).ok());
        if let Some(ref raw) = codec_raw {
            config.codec = match raw.trim().to_ascii_lowercase().as_str() {
                "h264" => VideoCodec::H264,
                "vp8" => VideoCodec::Vp8,
                "vp9" => VideoCodec::Vp9,
                "av1" => VideoCodec::Av1,
                "auto" => config.codec,
                other => return Err(anyhow!("unsupported video codec '{other}'")),
            };
        }

        let encoder_raw = encoder_override
            .map(|s| s.to_owned())
            .or_else(|| env::var(Self::ENCODER_ENV).ok());
        if let Some(ref raw) = encoder_raw {
            config.encoder = VideoEncoderSelection::parse(raw)?;
        }

        if let Ok(raw) = env::var(Self::BITRATE_ENV) {
            config.bitrate_kbps = raw.parse::<u32>().unwrap_or(config.bitrate_kbps).max(100);
        }
        if let Ok(raw) = env::var(Self::KEYFRAME_ENV) {
            config.keyframe_interval = raw
                .parse::<u32>()
                .unwrap_or(config.keyframe_interval)
                .max(1);
        }
        if let Ok(raw) = env::var(Self::LOW_LATENCY_ENV) {
            config.low_latency = !matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            );
        }

        Ok(config)
    }
}

// ── Encoder capability probe result ────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EncoderCandidate {
    pub element: &'static str,
    pub codec: &'static str,
    pub hardware: bool,
    pub usable: bool,
    pub skip_reason: Option<String>,
}

// ── GStreamer pipeline specs ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MediaConfig {
    pub selected_backend: MediaBackend,
    pub video_encoder: VideoEncoderConfig,
}

impl MediaConfig {
    pub fn from_sources(
        cli_backend: Option<&str>,
        file_backend: Option<&str>,
        codec_override: Option<&str>,
        encoder_override: Option<&str>,
    ) -> Result<Self> {
        let selected_backend = if let Some(raw) = cli_backend {
            MediaBackend::parse(raw)?
        } else if let Some(raw) = env::var_os(MediaBackend::ENV_VAR) {
            MediaBackend::parse(raw.to_string_lossy().as_ref())?
        } else if let Some(raw) = file_backend {
            MediaBackend::parse(raw)?
        } else {
            MediaBackend::Gstreamer
        };

        let video_encoder = VideoEncoderConfig::from_env(codec_override, encoder_override)?;

        Ok(Self {
            selected_backend,
            video_encoder,
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MediaRuntimeStatus {
    pub backend: &'static str,
    pub transport: &'static str,
    pub video_codec: Option<&'static str>,
    pub video_encoder: Option<&'static str>,
    pub audio_codec: Option<&'static str>,
    pub audio_encoder: Option<&'static str>,
    pub video_pipeline: Option<String>,
    pub audio_pipeline: Option<String>,
    pub pipeline_state: &'static str,
    pub dropped_video_frames: u64,
}

impl Default for MediaRuntimeStatus {
    fn default() -> Self {
        Self {
            backend: "gstreamer",
            transport: "media-tracks",
            video_codec: None,
            video_encoder: None,
            audio_codec: None,
            audio_encoder: None,
            video_pipeline: None,
            audio_pipeline: None,
            pipeline_state: "inactive",
            dropped_video_frames: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GstreamerPipelineSpec {
    pub video_codec: &'static str,
    pub video_encoder: &'static str,
    pub hardware: bool,
    pub audio_codec: &'static str,
    pub audio_encoder: &'static str,
    pub video_pipeline: String,
    pub audio_pipeline: String,
}

impl GstreamerPipelineSpec {
    pub fn vp8_opus_software() -> Self {
        Self {
            video_codec: "vp8",
            video_encoder: "vp8enc",
            hardware: false,
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline: concat!(
                "appsrc name=video_src is-live=true format=time do-timestamp=true ",
                "! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ",
                "! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 ",
                "! vp8enc deadline=1 cpu-used=8 error-resilient=partitions keyframe-max-dist=60 threads=4 ",
                "! rtpvp8pay pt=96 picture-id-mode=15-bit ",
                "! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true"
            )
            .to_string(),
            audio_pipeline: Self::opus_audio_pipeline(),
        }
    }

    pub fn h264_nvenc(bitrate_kbps: u32, keyframe_interval: u32) -> Self {
        let video_pipeline = format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=true \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
             ! nvh264enc bframes=0 rc-lookahead=0 gop-size={kf} bitrate={br} preset=low-latency-hq \
             ! h264parse config-interval=-1 \
             ! rtph264pay config-interval=-1 pt=96 \
             ! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true",
            kf = keyframe_interval,
            br = bitrate_kbps,
        );
        Self {
            video_codec: "h264",
            video_encoder: "nvh264enc",
            hardware: true,
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline,
            audio_pipeline: Self::opus_audio_pipeline(),
        }
    }

    pub fn h264_qsv(bitrate_kbps: u32, keyframe_interval: u32) -> Self {
        let video_pipeline = format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=true \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
             ! qsvh264enc b-frames=0 gop-size={kf} bitrate={br} \
             ! h264parse config-interval=-1 \
             ! rtph264pay config-interval=-1 pt=96 \
             ! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true",
            kf = keyframe_interval,
            br = bitrate_kbps,
        );
        Self {
            video_codec: "h264",
            video_encoder: "qsvh264enc",
            hardware: true,
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline,
            audio_pipeline: Self::opus_audio_pipeline(),
        }
    }

    pub fn h264_vaapi(bitrate_kbps: u32, keyframe_interval: u32) -> Self {
        let video_pipeline = format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=true \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
             ! vaapih264enc bitrate={br} keyframe-period={kf} rate-control=cbr \
             ! h264parse config-interval=-1 \
             ! rtph264pay config-interval=-1 pt=96 \
             ! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true",
            kf = keyframe_interval,
            br = bitrate_kbps,
        );
        Self {
            video_codec: "h264",
            video_encoder: "vaapih264enc",
            hardware: true,
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline,
            audio_pipeline: Self::opus_audio_pipeline(),
        }
    }

    pub fn h264_v4l2(bitrate_kbps: u32, _keyframe_interval: u32) -> Self {
        let video_pipeline = format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=true \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
             ! v4l2h264enc extra-controls=\"encode,bitrate={br},h26x_minimum_qp_value=10\" \
             ! h264parse config-interval=-1 \
             ! rtph264pay config-interval=-1 pt=96 \
             ! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true",
            br = bitrate_kbps * 1000,
        );
        Self {
            video_codec: "h264",
            video_encoder: "v4l2h264enc",
            hardware: true,
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline,
            audio_pipeline: Self::opus_audio_pipeline(),
        }
    }

    pub fn h264_x264_software(bitrate_kbps: u32, keyframe_interval: u32) -> Self {
        let video_pipeline = format!(
            "appsrc name=video_src is-live=true format=time do-timestamp=true \
             ! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 \
             ! videoconvert ! videoscale ! video/x-raw,pixel-aspect-ratio=1/1 \
             ! x264enc tune=zerolatency speed-preset=ultrafast bframes=0 key-int-max={kf} bitrate={br} \
             ! video/x-h264,profile=baseline \
             ! h264parse config-interval=-1 \
             ! rtph264pay config-interval=-1 pt=96 \
             ! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true",
            kf = keyframe_interval,
            br = bitrate_kbps,
        );
        Self {
            video_codec: "h264",
            video_encoder: "x264enc",
            hardware: false,
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline,
            audio_pipeline: Self::opus_audio_pipeline(),
        }
    }

    fn opus_audio_pipeline() -> String {
        concat!(
            "appsrc name=audio_src is-live=true format=time do-timestamp=true ",
            "! queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=500000000 ",
            "! audioconvert ! audioresample ",
            "! opusenc audio-type=restricted-lowdelay frame-size=20 bitrate=64000 inband-fec=true packet-loss-percentage=10 ",
            "! rtpopuspay pt=111 ",
            "! queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=400000000 ",
            "! appsink name=audio_sink sync=false async=false drop=true max-buffers=64 emit-signals=true"
        )
        .to_string()
    }
}

// ── Encoder selection ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SelectedEncoder {
    pub spec: GstreamerPipelineSpec,
    pub candidates: Vec<EncoderCandidate>,
    pub selection_reason: String,
}

pub fn select_encoder(config: &VideoEncoderConfig) -> Result<SelectedEncoder> {
    use gstreamer as gst;

    gst::init().map_err(|err| anyhow!("GStreamer init failed: {err}"))?;

    let has = |name: &str| gst::ElementFactory::find(name).is_some();

    let candidates = build_candidates(has);

    // Explicit encoder override — just try that one
    if config.encoder != VideoEncoderSelection::Auto {
        let (element, codec) = match config.encoder {
            VideoEncoderSelection::Nvenc => ("nvh264enc", "h264"),
            VideoEncoderSelection::Qsv => ("qsvh264enc", "h264"),
            VideoEncoderSelection::Vaapi => ("vaapih264enc", "h264"),
            VideoEncoderSelection::V4l2 => ("v4l2h264enc", "h264"),
            VideoEncoderSelection::X264 => ("x264enc", "h264"),
            VideoEncoderSelection::Vp8 => ("vp8enc", "vp8"),
            VideoEncoderSelection::Software => {
                // "software" means no hardware encoder, but try x264 first then vp8
                if has("x264enc") {
                    ("x264enc", "h264")
                } else {
                    ("vp8enc", "vp8")
                }
            }
            VideoEncoderSelection::Auto => unreachable!(),
        };

        if !has(element) {
            return Err(anyhow!(
                "encoder '{element}' requested but GStreamer element not found on this host"
            ));
        }

        let spec = build_hardcoded_pipeline(element, codec, config, has)?;
        return Ok(SelectedEncoder {
            spec,
            candidates,
            selection_reason: format!("explicit encoder override: {element}"),
        });
    }

    // Auto-detect: H.264 first, then VP8 fallback
    // Preference: nvh264enc > qsvh264enc > vaapih264enc > v4l2h264enc > x264enc > vp8enc
    for (element, codec) in &[
        ("nvh264enc", "h264"),
        ("qsvh264enc", "h264"),
        ("vaapih264enc", "h264"),
        ("v4l2h264enc", "h264"),
        ("x264enc", "h264"),
        ("vp8enc", "vp8"),
    ] {
        if *codec == "vp8" && config.codec != VideoCodec::H264 {
            // If user asked for H264 specifically and we couldn't find any H.264,
            // fall through to VP8 anyway as last resort
        } else if *codec == "h264" && config.codec == VideoCodec::Vp8 {
            continue; // user explicitly wants VP8
        }

        if !has(element) {
            continue;
        }

        let spec = build_hardcoded_pipeline(element, codec, config, has)?;
        let label = if spec.hardware {
            "hardware"
        } else {
            "software"
        };
        return Ok(SelectedEncoder {
            spec,
            candidates,
            selection_reason: format!("auto-detected {label} encoder: {element}"),
        });
    }

    Err(anyhow!(
        "no usable video encoder found; available elements: {:?}",
        candidates
            .iter()
            .filter(|c| c.usable)
            .map(|c| c.element)
            .collect::<Vec<_>>()
    ))
}

fn build_candidates(has: impl Fn(&str) -> bool) -> Vec<EncoderCandidate> {
    vec![
        EncoderCandidate {
            element: "nvh264enc",
            codec: "h264",
            hardware: true,
            usable: has("nvh264enc"),
            skip_reason: if has("nvh264enc") {
                None
            } else {
                Some("nvh264enc (NVIDIA NVENC) not installed".into())
            },
        },
        EncoderCandidate {
            element: "qsvh264enc",
            codec: "h264",
            hardware: true,
            usable: has("qsvh264enc"),
            skip_reason: if has("qsvh264enc") {
                None
            } else {
                Some("qsvh264enc (Intel Quick Sync) not installed".into())
            },
        },
        EncoderCandidate {
            element: "vaapih264enc",
            codec: "h264",
            hardware: true,
            usable: has("vaapih264enc"),
            skip_reason: if has("vaapih264enc") {
                None
            } else {
                Some("vaapih264enc (VA-API) not installed".into())
            },
        },
        EncoderCandidate {
            element: "v4l2h264enc",
            codec: "h264",
            hardware: true,
            usable: has("v4l2h264enc"),
            skip_reason: if has("v4l2h264enc") {
                None
            } else {
                Some("v4l2h264enc (V4L2) not installed".into())
            },
        },
        EncoderCandidate {
            element: "x264enc",
            codec: "h264",
            hardware: false,
            usable: has("x264enc"),
            skip_reason: if has("x264enc") {
                None
            } else {
                Some("x264enc (software H.264) not installed".into())
            },
        },
        EncoderCandidate {
            element: "vp8enc",
            codec: "vp8",
            hardware: false,
            usable: has("vp8enc"),
            skip_reason: if has("vp8enc") {
                None
            } else {
                Some("vp8enc (software VP8) not installed".into())
            },
        },
    ]
}

fn build_hardcoded_pipeline(
    element: &str,
    codec: &str,
    config: &VideoEncoderConfig,
    _has: impl Fn(&str) -> bool,
) -> Result<GstreamerPipelineSpec> {
    match (element, codec) {
        ("nvh264enc", "h264") => Ok(GstreamerPipelineSpec::h264_nvenc(
            config.bitrate_kbps,
            config.keyframe_interval,
        )),
        ("qsvh264enc", "h264") => Ok(GstreamerPipelineSpec::h264_qsv(
            config.bitrate_kbps,
            config.keyframe_interval,
        )),
        ("vaapih264enc", "h264") => Ok(GstreamerPipelineSpec::h264_vaapi(
            config.bitrate_kbps,
            config.keyframe_interval,
        )),
        ("v4l2h264enc", "h264") => Ok(GstreamerPipelineSpec::h264_v4l2(
            config.bitrate_kbps,
            config.keyframe_interval,
        )),
        ("x264enc", "h264") => Ok(GstreamerPipelineSpec::h264_x264_software(
            config.bitrate_kbps,
            config.keyframe_interval,
        )),
        ("vp8enc", "vp8") => Ok(GstreamerPipelineSpec::vp8_opus_software()),
        _ => Err(anyhow!("no pipeline builder for encoder {element}/{codec}")),
    }
}

// ── Capabilities summary ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MediaCapabilities {
    pub selected_backend: MediaBackend,
    pub gstreamer: GstreamerCapabilities,
    pub runtime: MediaRuntimeStatus,
    pub encoders: EncoderReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct EncoderReport {
    pub selected: Option<EncoderCandidate>,
    pub candidates: Vec<EncoderCandidate>,
    pub selection_reason: Option<String>,
}

impl MediaCapabilities {
    pub fn detect(config: &MediaConfig) -> Self {
        let gstreamer = detect_gstreamer_capabilities();

        // Probe encoders if GStreamer is available
        let (candidates, selected, reason) = if gstreamer.available_for_runtime {
            let candidates =
                build_candidates(|name| gstreamer::ElementFactory::find(name).is_some());
            // Try selection to get the exact selected encoder, but don't fail on auto
            let (selected, reason) = match select_encoder(&config.video_encoder) {
                Ok(sel) => {
                    let candidate = EncoderCandidate {
                        element: sel.spec.video_encoder,
                        codec: sel.spec.video_codec,
                        hardware: sel.spec.hardware,
                        usable: true,
                        skip_reason: None,
                    };
                    (Some(candidate), Some(sel.selection_reason))
                }
                Err(err) => (None, Some(err.to_string())),
            };
            (candidates, selected, reason)
        } else {
            (Vec::new(), None, None)
        };

        Self {
            selected_backend: config.selected_backend,
            gstreamer,
            runtime: MediaRuntimeStatus {
                backend: config.selected_backend.as_str(),
                ..Default::default()
            },
            encoders: EncoderReport {
                selected,
                candidates,
                selection_reason: reason,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GstreamerCapabilities {
    pub compiled_in: bool,
    pub available_for_runtime: bool,
    pub init_ok: bool,
    pub version: Option<String>,
    pub missing_reason: Option<String>,
    pub elements: GstreamerElements,
}

#[derive(Debug, Clone, Serialize)]
pub struct GstreamerElements {
    pub appsrc: bool,
    pub webrtcbin: bool,
    pub opusenc: bool,
    pub rtpopuspay: bool,
    pub vp8enc: bool,
    pub x264enc: bool,
    pub nvh264enc: bool,
    pub qsvh264enc: bool,
    pub vaapih264enc: bool,
    pub v4l2h264enc: bool,
}

impl GstreamerElements {
    fn all_false() -> Self {
        Self {
            appsrc: false,
            webrtcbin: false,
            opusenc: false,
            rtpopuspay: false,
            vp8enc: false,
            x264enc: false,
            nvh264enc: false,
            qsvh264enc: false,
            vaapih264enc: false,
            v4l2h264enc: false,
        }
    }
}

fn detect_gstreamer_capabilities() -> GstreamerCapabilities {
    use gstreamer as gst;

    match gst::init() {
        Ok(()) => {
            let has = |name: &str| gst::ElementFactory::find(name).is_some();
            GstreamerCapabilities {
                compiled_in: true,
                available_for_runtime: true,
                init_ok: true,
                version: Some(gst::version_string().to_string()),
                missing_reason: None,
                elements: GstreamerElements {
                    appsrc: has("appsrc"),
                    webrtcbin: has("webrtcbin"),
                    opusenc: has("opusenc"),
                    rtpopuspay: has("rtpopuspay"),
                    vp8enc: has("vp8enc"),
                    x264enc: has("x264enc"),
                    nvh264enc: has("nvh264enc"),
                    qsvh264enc: has("qsvh264enc"),
                    vaapih264enc: has("vaapih264enc"),
                    v4l2h264enc: has("v4l2h264enc"),
                },
            }
        }
        Err(err) => GstreamerCapabilities {
            compiled_in: true,
            available_for_runtime: false,
            init_ok: false,
            version: None,
            missing_reason: Some(err.to_string()),
            elements: GstreamerElements::all_false(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{GstreamerPipelineSpec, MediaBackend, MediaConfig, VideoEncoderSelection};

    #[test]
    fn parses_media_backend_aliases() {
        assert_eq!(
            MediaBackend::parse("gstreamer").unwrap(),
            MediaBackend::Gstreamer
        );
        assert_eq!(MediaBackend::parse("gst").unwrap(), MediaBackend::Gstreamer);
    }

    #[test]
    fn invalid_backend_returns_error() {
        let err = MediaBackend::parse("banana").unwrap_err().to_string();
        assert!(err.contains("unsupported media backend"));
    }

    #[test]
    fn source_priority_is_cli_then_env_then_file_then_default() {
        unsafe { std::env::remove_var(MediaBackend::ENV_VAR) };
        assert_eq!(
            MediaConfig::from_sources(Some("gstreamer"), Some("legacy"), None, None)
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );

        unsafe { std::env::set_var(MediaBackend::ENV_VAR, "gstreamer") };
        assert_eq!(
            MediaConfig::from_sources(None, Some("legacy"), None, None)
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );

        unsafe { std::env::remove_var(MediaBackend::ENV_VAR) };
        assert_eq!(
            MediaConfig::from_sources(None, Some("gstreamer"), None, None)
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );
        assert_eq!(
            MediaConfig::from_sources(None, None, None, None)
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );
    }

    #[test]
    fn vp8_opus_pipeline_spec_uses_low_latency_gstreamer_elements() {
        let spec = GstreamerPipelineSpec::vp8_opus_software();

        assert_eq!(spec.video_codec, "vp8");
        assert_eq!(spec.video_encoder, "vp8enc");
        assert_eq!(spec.audio_codec, "opus");
        assert_eq!(spec.audio_encoder, "opusenc");
        assert!(spec.video_pipeline.contains("queue leaky=downstream"));
        assert!(spec.video_pipeline.contains("videoconvert"));
        assert!(spec.video_pipeline.contains("rtpvp8pay"));
        assert!(spec.video_pipeline.contains("appsink name=video_sink"));
        assert!(spec.audio_pipeline.contains("audioconvert ! audioresample"));
        assert!(spec.audio_pipeline.contains("rtpopuspay"));
        assert!(spec.audio_pipeline.contains("appsink name=audio_sink"));
    }

    #[test]
    fn h264_pipelines_have_correct_mime_type() {
        for spec in &[
            GstreamerPipelineSpec::h264_nvenc(2500, 60),
            GstreamerPipelineSpec::h264_qsv(2500, 60),
            GstreamerPipelineSpec::h264_vaapi(2500, 60),
            GstreamerPipelineSpec::h264_x264_software(2500, 60),
        ] {
            assert_eq!(spec.video_codec, "h264");
            assert!(spec.video_pipeline.contains("rtph264pay"));
            assert!(spec.video_pipeline.contains("appsink name=video_sink"));
        }
    }

    #[test]
    fn encoder_selection_parse_aliases() {
        assert_eq!(
            VideoEncoderSelection::parse("auto").unwrap(),
            VideoEncoderSelection::Auto
        );
        assert_eq!(
            VideoEncoderSelection::parse("nvenc").unwrap(),
            VideoEncoderSelection::Nvenc
        );
        assert_eq!(
            VideoEncoderSelection::parse("software").unwrap(),
            VideoEncoderSelection::Software
        );
        assert!(VideoEncoderSelection::parse("banana").is_err());
    }
}
