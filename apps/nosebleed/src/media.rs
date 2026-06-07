use std::env;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaBackend {
    Legacy,
    Gstreamer,
}

impl MediaBackend {
    pub const ENV_VAR: &str = "NOSEBLEED_MEDIA_BACKEND";

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "legacy" => Ok(Self::Legacy),
            "gstreamer" | "gst" => Ok(Self::Gstreamer),
            other => Err(anyhow!(
                "unsupported media backend '{other}' (expected legacy or gstreamer)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Gstreamer => "gstreamer",
        }
    }
}

impl Default for MediaBackend {
    fn default() -> Self {
        Self::Legacy
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaConfig {
    pub selected_backend: MediaBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRtcTransportMode {
    RawDataChannel,
    Vp8DataChannel,
    MediaTracks,
}

impl MediaConfig {
    pub fn from_sources(cli_backend: Option<&str>, file_backend: Option<&str>) -> Result<Self> {
        let selected_backend = if let Some(raw) = cli_backend {
            MediaBackend::parse(raw)?
        } else if let Some(raw) = env::var_os(MediaBackend::ENV_VAR) {
            MediaBackend::parse(raw.to_string_lossy().as_ref())?
        } else if let Some(raw) = file_backend {
            MediaBackend::parse(raw)?
        } else {
            MediaBackend::Legacy
        };

        Ok(Self { selected_backend })
    }

    pub fn select_webrtc_transport(&self, requested_video_mode: Option<&str>) -> WebRtcTransportMode {
        match self.selected_backend {
            MediaBackend::Gstreamer => WebRtcTransportMode::MediaTracks,
            MediaBackend::Legacy => match requested_video_mode {
                Some("track-vp8") => WebRtcTransportMode::MediaTracks,
                Some("vp8") => WebRtcTransportMode::Vp8DataChannel,
                _ => WebRtcTransportMode::RawDataChannel,
            },
        }
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
            backend: "legacy",
            transport: "data-channel",
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
            audio_codec: "opus",
            audio_encoder: "opusenc",
            video_pipeline: concat!(
                "appsrc name=video_src is-live=true format=time do-timestamp=false ",
                "! queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ",
                "! videoconvert ",
                "! vp8enc deadline=1 cpu-used=8 error-resilient=partitions keyframe-max-dist=60 threads=4 ",
                "! rtpvp8pay pt=96 picture-id-mode=15-bit ",
                "! appsink name=video_sink sync=false async=false drop=true max-buffers=8 emit-signals=true"
            )
            .to_string(),
            audio_pipeline: concat!(
                "appsrc name=audio_src is-live=true format=time do-timestamp=false ",
                "! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ",
                "! audioconvert ! audioresample ",
                "! opusenc audio-type=restricted-lowdelay frame-size=20 bitrate=64000 inband-fec=false ",
                "! rtpopuspay pt=111 ",
                "! appsink name=audio_sink sync=false async=false drop=true max-buffers=16 emit-signals=true"
            )
            .to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaCapabilities {
    pub selected_backend: MediaBackend,
    pub compiled_backends: Vec<&'static str>,
    pub available_backends: Vec<&'static str>,
    pub gstreamer: GstreamerCapabilities,
    pub runtime: MediaRuntimeStatus,
}

impl MediaCapabilities {
    pub fn detect(config: &MediaConfig) -> Self {
        let gstreamer = detect_gstreamer_capabilities();
        let mut compiled_backends = vec!["legacy"];
        if gstreamer.compiled_in {
            compiled_backends.push("gstreamer");
        }

        let mut available_backends = vec!["legacy"];
        if gstreamer.available_for_runtime {
            available_backends.push("gstreamer");
        }

        Self {
            selected_backend: config.selected_backend,
            compiled_backends,
            available_backends,
            gstreamer,
            runtime: MediaRuntimeStatus {
                backend: config.selected_backend.as_str(),
                ..Default::default()
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

#[cfg(feature = "media-gstreamer")]
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

#[cfg(not(feature = "media-gstreamer"))]
fn detect_gstreamer_capabilities() -> GstreamerCapabilities {
    GstreamerCapabilities {
        compiled_in: false,
        available_for_runtime: false,
        init_ok: false,
        version: None,
        missing_reason: Some(
            "binary not built with Cargo feature media-gstreamer; rebuild with --features media-gstreamer"
                .to_string(),
        ),
        elements: GstreamerElements::all_false(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GstreamerPipelineSpec, MediaBackend, MediaConfig, WebRtcTransportMode,
    };

    #[test]
    fn parses_media_backend_aliases() {
        assert_eq!(MediaBackend::parse("legacy").unwrap(), MediaBackend::Legacy);
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
            MediaConfig::from_sources(Some("gstreamer"), Some("legacy"))
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );

        unsafe { std::env::set_var(MediaBackend::ENV_VAR, "gstreamer") };
        assert_eq!(
            MediaConfig::from_sources(None, Some("legacy"))
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );

        unsafe { std::env::remove_var(MediaBackend::ENV_VAR) };
        assert_eq!(
            MediaConfig::from_sources(None, Some("gstreamer"))
                .unwrap()
                .selected_backend,
            MediaBackend::Gstreamer
        );
        assert_eq!(
            MediaConfig::from_sources(None, None)
                .unwrap()
                .selected_backend,
            MediaBackend::Legacy
        );
    }

    #[test]
    fn legacy_backend_honors_requested_webrtc_transport_mode() {
        let config = MediaConfig {
            selected_backend: MediaBackend::Legacy,
        };

        assert_eq!(
            config.select_webrtc_transport(Some("vp8")),
            WebRtcTransportMode::Vp8DataChannel
        );
        assert_eq!(
            config.select_webrtc_transport(Some("track-vp8")),
            WebRtcTransportMode::MediaTracks
        );
        assert_eq!(
            config.select_webrtc_transport(Some("raw")),
            WebRtcTransportMode::RawDataChannel
        );
    }

    #[test]
    fn gstreamer_backend_forces_media_tracks() {
        let config = MediaConfig {
            selected_backend: MediaBackend::Gstreamer,
        };

        assert_eq!(
            config.select_webrtc_transport(Some("raw")),
            WebRtcTransportMode::MediaTracks
        );
        assert_eq!(
            config.select_webrtc_transport(Some("vp8")),
            WebRtcTransportMode::MediaTracks
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
}
