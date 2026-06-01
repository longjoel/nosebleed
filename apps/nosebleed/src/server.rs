use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::str;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes as MediaBytes;
use media::Sample;

use anyhow::{Context, Result, anyhow};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, body::Bytes};
use image::ColorType;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_PCMU, MIME_TYPE_VP8};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::arcade::{ArcadeError, ArcadeService, Side};
use crate::auth::{MatchClaims, MatchRole, validate_match_token};
use crate::input::{Button, InputHub};
use crate::protocol::{
    ClientCommand, ClientMessage, ServerMessage, now_unix_ms, parse_client_message,
    serialize_server_message,
};
use crate::session::{
    SessionManager, StartRequest as SessionStartRequest, Status as SessionStatus,
};

const RTC_CHUNK_MAGIC: &[u8; 4] = b"NBC1";
const RTC_CHUNK_HEADER_LEN: usize = 4 + 4 + 2 + 2;
const RTC_CHUNK_PAYLOAD_MAX: usize = 14 * 1024;
const RTC_STUN_SERVER: &str = "stun:stun.l.google.com:19302";
const FRAME_MAGIC: &[u8; 4] = b"NBF0";
const FRAME_HEADER_LEN: usize = 4 + 8 + 8 + 4 + 4 + 4 + 1 + 4;
const VP8_VIDEO_MAGIC: &[u8; 4] = b"NBV1";
const VP8_VIDEO_HEADER_LEN: usize = 4 + 8 + 4 + 1 + 4;
const JPEG_VIDEO_MAGIC: &[u8; 4] = b"NBJ0";
const JPEG_VIDEO_HEADER_LEN: usize = 4 + 4 + 4 + 4;
const IVF_FILE_HEADER_LEN: usize = 32;
const IVF_FRAME_HEADER_LEN: usize = 12;
const DEFAULT_VP8_FRAME_DURATION_US: u32 = 16_666;
const PCMU_SAMPLE_RATE: u32 = 8_000;
const PCMU_FRAME_SAMPLES: usize = 160;
const AUDIO_PACKET_MAGIC: &[u8; 4] = b"NBA0";
const AUDIO_PACKET_HEADER_LEN: usize = 34;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoChannelMode {
    Raw,
    Vp8DataChannel,
    Vp8Track,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsVideoMode {
    Raw,
    Jpeg { quality: u8 },
}

#[derive(Debug)]
pub struct AuthConfig {
    pub require_auth: bool,
    pub secret: Option<Arc<[u8]>>,
    pub reconnect_window: Duration,
}

#[derive(Clone)]
pub struct ServerState {
    pub video_rx: watch::Receiver<Option<Arc<[u8]>>>,
    pub webrtc_vp8_rx: watch::Receiver<Option<Arc<[u8]>>>,
    pub webrtc_vp8_track: Arc<TrackLocalStaticSample>,
    pub webrtc_pcmu_track: Arc<TrackLocalStaticSample>,
    pub audio_tx: broadcast::Sender<Arc<[u8]>>,
    pub input_hub: Arc<InputHub>,
    pub shutdown: Arc<AtomicBool>,
    pub next_client_id: Arc<AtomicU64>,
    pub auth: Arc<AuthConfig>,
    pub session_manager: Arc<SessionManager>,
    pub arcade: Arc<ArcadeService>,
    input_sessions: Arc<std::sync::Mutex<InputSessionRegistry>>,
    rtc_sessions: Arc<std::sync::Mutex<HashMap<u64, Arc<RTCPeerConnection>>>>,
    webrtc_api: Arc<webrtc::api::API>,
}

impl ServerState {
    pub fn new(
        video_rx: watch::Receiver<Option<Arc<[u8]>>>,
        audio_tx: broadcast::Sender<Arc<[u8]>>,
        input_hub: Arc<InputHub>,
        shutdown: Arc<AtomicBool>,
        next_client_id: Arc<AtomicU64>,
        auth: Arc<AuthConfig>,
        session_manager: Arc<SessionManager>,
    ) -> Self {
        let webrtc_vp8_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.to_owned(),
                clock_rate: 90_000,
                ..Default::default()
            },
            "video".to_owned(),
            "nosebleed".to_owned(),
        ));
        let webrtc_vp8_rx =
            spawn_shared_webrtc_vp8_channel(video_rx.clone(), webrtc_vp8_track.clone());
        let webrtc_pcmu_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_PCMU.to_owned(),
                clock_rate: PCMU_SAMPLE_RATE,
                channels: 1,
                ..Default::default()
            },
            "audio".to_owned(),
            "nosebleed".to_owned(),
        ));
        spawn_shared_webrtc_pcmu_track(audio_tx.subscribe(), webrtc_pcmu_track.clone());
        Self {
            video_rx,
            webrtc_vp8_rx,
            webrtc_vp8_track,
            webrtc_pcmu_track,
            audio_tx,
            input_hub,
            shutdown,
            next_client_id,
            auth,
            session_manager,
            arcade: Arc::new(ArcadeService::new(6)),
            input_sessions: Arc::new(std::sync::Mutex::new(InputSessionRegistry::default())),
            rtc_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            webrtc_api: Arc::new(APIBuilder::new().build()),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct WsQuery {
    token: Option<String>,
    video_mode: Option<String>,
    jpeg_quality: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct WebRtcOffer {
    #[serde(rename = "type")]
    kind: String,
    sdp: String,
    #[serde(default)]
    video_mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct WebRtcAnswer {
    #[serde(rename = "type")]
    kind: &'static str,
    sdp: String,
}

#[derive(Debug, Deserialize)]
struct QueueJoinRequest {
    player_name: String,
    side: String,
}

#[derive(Debug, Deserialize)]
struct QueueLeaveRequest {
    ticket_id: u64,
}

#[derive(Debug, Deserialize)]
struct ClaimSeatRequest {
    ticket_id: u64,
}

#[derive(Debug, Deserialize)]
struct RoundEndRequest {
    winner_side: String,
    #[serde(default)]
    left_score: u32,
    #[serde(default)]
    right_score: u32,
}

#[derive(Debug, Clone)]
struct RawFramePacket {
    width: u32,
    height: u32,
    pitch: usize,
    pixel_format: u8,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct EncodedVp8Frame {
    pts_us: u64,
    duration_us: u32,
    keyframe: bool,
    payload: Vec<u8>,
}

struct Vp8IvfEncoder {
    width: u32,
    height: u32,
    pixel_format: u8,
    stdin: ChildStdin,
    frames_rx: mpsc::Receiver<EncodedVp8Frame>,
    _child: Child,
}

#[derive(Debug, Default)]
struct InputSessionRegistry {
    per_port: HashMap<u32, PortOwner>,
}

#[derive(Debug, Clone)]
struct PortOwner {
    player_id: String,
    source_id: String,
    reconnect_until: Option<Instant>,
}

pub async fn run(state: ServerState, listen_addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/arcade", get(arcade_index))
        .route("/healthz", get(healthz))
        .route("/session/status", get(session_status))
        .route("/session/start", post(session_start))
        .route("/session/stop", post(session_stop))
        .route("/api/arcade/overview", get(arcade_overview))
        .route("/api/arcade/machines/{id}", get(arcade_machine))
        .route(
            "/api/arcade/machines/{id}/queue/join",
            post(arcade_join_queue),
        )
        .route(
            "/api/arcade/machines/{id}/queue/leave",
            post(arcade_leave_queue),
        )
        .route("/api/arcade/machines/{id}/claim", post(arcade_claim_seat))
        .route(
            "/api/arcade/machines/{id}/round/end",
            post(arcade_round_end),
        )
        .route("/ws/video", get(video_ws))
        .route("/ws/audio", get(audio_ws))
        .route("/ws/input", get(input_ws))
        .route("/webrtc/session", post(webrtc_session))
        .with_state(state.clone());

    let listener = TcpListener::bind(listen_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.shutdown.clone()))
        .await?;
    Ok(())
}

async fn shutdown_signal(shutdown: Arc<AtomicBool>) {
    let _ = tokio::signal::ctrl_c().await;
    shutdown.store(true, Ordering::Relaxed);
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn arcade_index() -> Html<&'static str> {
    Html(include_str!("../static/arcade.html"))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn session_status(State(state): State<ServerState>) -> Json<SessionStatus> {
    Json(state.session_manager.status())
}

async fn session_start(
    State(state): State<ServerState>,
    Json(request): Json<SessionStartRequest>,
) -> Response {
    match state.session_manager.start_from_request(request) {
        Ok(status) => Json(status).into_response(),
        Err(err) => {
            let message = err.to_string();
            if message.contains("already running") {
                (StatusCode::CONFLICT, message).into_response()
            } else {
                (StatusCode::BAD_REQUEST, message).into_response()
            }
        }
    }
}

async fn session_stop(State(state): State<ServerState>) -> Json<SessionStatus> {
    Json(state.session_manager.stop())
}

async fn arcade_overview(State(state): State<ServerState>) -> Response {
    Json(state.arcade.overview()).into_response()
}

async fn arcade_machine(Path(machine_id): Path<u32>, State(state): State<ServerState>) -> Response {
    match state.arcade.machine(machine_id) {
        Ok(machine) => Json(machine).into_response(),
        Err(err) => arcade_error_response(err),
    }
}

async fn arcade_join_queue(
    Path(machine_id): Path<u32>,
    State(state): State<ServerState>,
    Json(request): Json<QueueJoinRequest>,
) -> Response {
    let side = match Side::parse(&request.side) {
        Some(side) => side,
        None => {
            return (StatusCode::BAD_REQUEST, "side must be one of: left, right").into_response();
        }
    };

    match state
        .arcade
        .join_queue(machine_id, request.player_name, side)
    {
        Ok(result) => Json(result).into_response(),
        Err(err) => arcade_error_response(err),
    }
}

async fn arcade_leave_queue(
    Path(machine_id): Path<u32>,
    State(state): State<ServerState>,
    Json(request): Json<QueueLeaveRequest>,
) -> Response {
    match state.arcade.leave_queue(machine_id, request.ticket_id) {
        Ok(machine) => Json(machine).into_response(),
        Err(err) => arcade_error_response(err),
    }
}

async fn arcade_claim_seat(
    Path(machine_id): Path<u32>,
    State(state): State<ServerState>,
    Json(request): Json<ClaimSeatRequest>,
) -> Response {
    match state.arcade.claim_seat(machine_id, request.ticket_id) {
        Ok(machine) => Json(machine).into_response(),
        Err(err) => arcade_error_response(err),
    }
}

async fn arcade_round_end(
    Path(machine_id): Path<u32>,
    State(state): State<ServerState>,
    Json(request): Json<RoundEndRequest>,
) -> Response {
    let winner_side = match Side::parse(&request.winner_side) {
        Some(side) => side,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "winner_side must be one of: left, right",
            )
                .into_response();
        }
    };

    match state.arcade.end_round(
        machine_id,
        winner_side,
        request.left_score,
        request.right_score,
    ) {
        Ok(machine) => Json(machine).into_response(),
        Err(err) => arcade_error_response(err),
    }
}

fn arcade_error_response(err: ArcadeError) -> Response {
    (err.status_code(), err.message().to_string()).into_response()
}

async fn video_ws(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<ServerState>,
) -> Response {
    if let Err(response) = authorize_stream_claims(&state, query.token.as_deref()) {
        return response;
    }

    let rx = state.video_rx.clone();
    let mode = if query.video_mode.as_deref() == Some("raw") {
        WsVideoMode::Raw
    } else {
        WsVideoMode::Jpeg {
            quality: sanitize_jpeg_quality(query.jpeg_quality.unwrap_or(70))
        }
    };
    ws.on_upgrade(move |socket| video_session(socket, rx, mode))
        .into_response()
}

async fn audio_ws(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<ServerState>,
) -> Response {
    if let Err(response) = authorize_stream_claims(&state, query.token.as_deref()) {
        return response;
    }

    let rx = state.audio_tx.subscribe();
    ws.on_upgrade(move |socket| audio_session(socket, rx))
        .into_response()
}

async fn input_ws(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<ServerState>,
) -> Response {
    let claims = match authorize_input_claims(&state, query.token.as_deref()) {
        Ok(claims) => claims,
        Err(response) => return response,
    };

    let client_id = state.next_client_id.fetch_add(1, Ordering::Relaxed);
    let source_id = if let Some(claims) = &claims {
        format!("{}-{}", claims.player_id, client_id)
    } else {
        format!("ws-{client_id}")
    };

    ws.on_upgrade(move |socket| input_session(socket, state, source_id, claims))
        .into_response()
}

async fn webrtc_session(
    Query(query): Query<WsQuery>,
    State(state): State<ServerState>,
    Json(offer): Json<WebRtcOffer>,
) -> Response {
    if offer.kind != "offer" {
        return (StatusCode::BAD_REQUEST, "sdp type must be offer").into_response();
    }

    let claims = match authorize_stream_claims(&state, query.token.as_deref()) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let requested_video_mode = match offer.video_mode.as_deref() {
        Some("track-vp8") => VideoChannelMode::Vp8Track,
        Some("vp8") => VideoChannelMode::Vp8DataChannel,
        _ => VideoChannelMode::Raw,
    };

    let input_allowed = claims
        .as_ref()
        .is_none_or(|claims| claims.role == MatchRole::Player);
    let owned_ports = claims
        .as_ref()
        .filter(|claims| claims.role == MatchRole::Player)
        .map(|claims| claims.allowed_ports.clone())
        .unwrap_or_default();

    let client_id = state.next_client_id.fetch_add(1, Ordering::Relaxed);
    let source_id = if let Some(claims) = &claims {
        format!("{}-{}", claims.player_id, client_id)
    } else {
        format!("rtc-{client_id}")
    };

    if let Some(claims) = &claims {
        if claims.role == MatchRole::Player {
            let reserve_result = {
                let mut registry = state
                    .input_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry.reserve_ports(
                    &source_id,
                    &claims.player_id,
                    &owned_ports,
                    state.auth.reconnect_window,
                )
            };

            if let Err(message) = reserve_result {
                return (StatusCode::CONFLICT, message).into_response();
            }
        }
    }

    let peer_connection = match create_peer_connection(&state).await {
        Ok(connection) => connection,
        Err(err) => {
            cleanup_input_source(&state, &source_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create peer connection: {err:#}"),
            )
                .into_response();
        }
    };

    if requested_video_mode == VideoChannelMode::Vp8Track {
        let video_track = state.webrtc_vp8_track.clone() as Arc<dyn TrackLocal + Send + Sync>;
        let video_sender = match peer_connection.add_track(video_track).await {
            Ok(sender) => sender,
            Err(err) => {
                cleanup_input_source(&state, &source_id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to attach webrtc video track: {err:#}"),
                )
                    .into_response();
            }
        };

        tokio::spawn(async move {
            while video_sender.read_rtcp().await.is_ok() {}
        });

        let audio_track = state.webrtc_pcmu_track.clone() as Arc<dyn TrackLocal + Send + Sync>;
        let audio_sender = match peer_connection.add_track(audio_track).await {
            Ok(sender) => sender,
            Err(err) => {
                cleanup_input_source(&state, &source_id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to attach webrtc audio track: {err:#}"),
                )
                    .into_response();
            }
        };

        tokio::spawn(async move {
            while audio_sender.read_rtcp().await.is_ok() {}
        });
    }

    let cleanup_once = Arc::new(AtomicBool::new(false));
    let rtc_session_id = client_id;
    {
        let mut sessions = state
            .rtc_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.insert(rtc_session_id, peer_connection.clone());
    }

    {
        let state_for_close = state.clone();
        let source_for_close = source_id.clone();
        let cleanup_for_close = cleanup_once.clone();
        peer_connection.on_peer_connection_state_change(Box::new(move |connection_state| {
            let state_for_close = state_for_close.clone();
            let source_for_close = source_for_close.clone();
            let cleanup_for_close = cleanup_for_close.clone();
            Box::pin(async move {
                if matches!(
                    connection_state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    {
                        let mut sessions = state_for_close
                            .rtc_sessions
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        sessions.remove(&rtc_session_id);
                    }
                    cleanup_input_source_once(
                        &state_for_close,
                        &source_for_close,
                        &cleanup_for_close,
                    );
                }
            })
        }));
    }

    {
        let state_for_channels = state.clone();
        let source_for_channels = source_id.clone();
        let owned_ports_for_channels = owned_ports.clone();
        let cleanup_for_channels = cleanup_once.clone();
        let video_mode_for_channels = requested_video_mode;
        peer_connection.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let state_for_channels = state_for_channels.clone();
            let source_for_channels = source_for_channels.clone();
            let owned_ports_for_channels = owned_ports_for_channels.clone();
            let cleanup_for_channels = cleanup_for_channels.clone();
            let video_mode_for_channels = video_mode_for_channels;
            Box::pin(async move {
                let label = channel.label();
                if label == "video" {
                    if video_mode_for_channels == VideoChannelMode::Vp8Track {
                        return;
                    }

                    let video_rx = match video_mode_for_channels {
                        VideoChannelMode::Raw => state_for_channels.video_rx.clone(),
                        VideoChannelMode::Vp8DataChannel => state_for_channels.webrtc_vp8_rx.clone(),
                        VideoChannelMode::Vp8Track => unreachable!(),
                    };
                    let channel_for_video = channel.clone();
                    channel.on_open(Box::new(move || {
                        let channel_for_video = channel_for_video.clone();
                        let video_rx = video_rx.clone();
                        Box::pin(async move {
                            tokio::spawn(async move {
                                rtc_video_channel_session(channel_for_video, video_rx).await;
                            });
                        })
                    }));
                    return;
                }

                if label == "audio" {
                    let audio_rx = state_for_channels.audio_tx.subscribe();
                    let channel_for_audio = channel.clone();
                    channel.on_open(Box::new(move || {
                        let channel_for_audio = channel_for_audio.clone();
                        let audio_rx = audio_rx.resubscribe();
                        Box::pin(async move {
                            tokio::spawn(async move {
                                rtc_audio_channel_session(channel_for_audio, audio_rx).await;
                            });
                        })
                    }));
                    return;
                }

                if label == "input" {
                    let state_for_input = state_for_channels.clone();
                    let source_for_input = source_for_channels.clone();
                    let owned_ports_for_input = owned_ports_for_channels.clone();
                    let channel_for_input = channel.clone();
                    channel.on_message(Box::new(move |message: DataChannelMessage| {
                        let state_for_input = state_for_input.clone();
                        let source_for_input = source_for_input.clone();
                        let owned_ports_for_input = owned_ports_for_input.clone();
                        let channel_for_input = channel_for_input.clone();
                        Box::pin(async move {
                            let raw = match str::from_utf8(message.data.as_ref()) {
                                Ok(text) => text,
                                Err(_) => {
                                    let response = ServerMessage::Error {
                                        message: "binary messages must be UTF-8 JSON".to_string(),
                                    };
                                    if let Ok(payload) = serialize_server_message(&response) {
                                        let _ = channel_for_input.send_text(payload).await;
                                    }
                                    return;
                                }
                            };

                            let response = process_input_payload(
                                &state_for_input,
                                &source_for_input,
                                &owned_ports_for_input,
                                input_allowed,
                                raw,
                            );

                            if let Ok(payload) = serialize_server_message(&response) {
                                let _ = channel_for_input.send_text(payload).await;
                            }
                        })
                    }));

                    let state_for_close = state_for_channels.clone();
                    let source_for_close = source_for_channels.clone();
                    let cleanup_for_close = cleanup_for_channels.clone();
                    channel.on_close(Box::new(move || {
                        let state_for_close = state_for_close.clone();
                        let source_for_close = source_for_close.clone();
                        let cleanup_for_close = cleanup_for_close.clone();
                        Box::pin(async move {
                            cleanup_input_source_once(
                                &state_for_close,
                                &source_for_close,
                                &cleanup_for_close,
                            );
                        })
                    }));
                }
            })
        }));
    }

    let remote_description = match RTCSessionDescription::offer(offer.sdp) {
        Ok(description) => description,
        Err(err) => {
            {
                let mut sessions = state
                    .rtc_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sessions.remove(&rtc_session_id);
            }
            cleanup_input_source_once(&state, &source_id, &cleanup_once);
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid remote offer: {err:#}"),
            )
                .into_response();
        }
    };

    if let Err(err) = peer_connection
        .set_remote_description(remote_description)
        .await
    {
        {
            let mut sessions = state
                .rtc_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.remove(&rtc_session_id);
        }
        cleanup_input_source_once(&state, &source_id, &cleanup_once);
        return (
            StatusCode::BAD_REQUEST,
            format!("failed to set remote description: {err:#}"),
        )
            .into_response();
    }

    let answer = match peer_connection.create_answer(None).await {
        Ok(answer) => answer,
        Err(err) => {
            {
                let mut sessions = state
                    .rtc_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sessions.remove(&rtc_session_id);
            }
            cleanup_input_source_once(&state, &source_id, &cleanup_once);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create answer: {err:#}"),
            )
                .into_response();
        }
    };

    let mut gather_complete = peer_connection.gathering_complete_promise().await;
    if let Err(err) = peer_connection.set_local_description(answer).await {
        {
            let mut sessions = state
                .rtc_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.remove(&rtc_session_id);
        }
        cleanup_input_source_once(&state, &source_id, &cleanup_once);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to set local description: {err:#}"),
        )
            .into_response();
    }

    let _ = gather_complete.recv().await;
    let local_description = match peer_connection.local_description().await {
        Some(description) if description.sdp_type == RTCSdpType::Answer => description,
        _ => {
            {
                let mut sessions = state
                    .rtc_sessions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                sessions.remove(&rtc_session_id);
            }
            cleanup_input_source_once(&state, &source_id, &cleanup_once);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "local description unavailable".to_string(),
            )
                .into_response();
        }
    };

    Json(WebRtcAnswer {
        kind: "answer",
        sdp: local_description.sdp,
    })
    .into_response()
}

async fn create_peer_connection(state: &ServerState) -> Result<Arc<RTCPeerConnection>> {
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec![RTC_STUN_SERVER.to_string()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let connection = state
        .webrtc_api
        .new_peer_connection(config)
        .await
        .map_err(|err| anyhow!("failed to create peer connection: {err:#}"))?;
    Ok(Arc::new(connection))
}

async fn rtc_video_channel_session(
    channel: Arc<RTCDataChannel>,
    mut video_rx: watch::Receiver<Option<Arc<[u8]>>>,
) {
    let mut next_message_id = 1u32;
    while video_rx.changed().await.is_ok() {
        let packet = video_rx.borrow().clone();
        let Some(packet) = packet else {
            continue;
        };

        if send_packet_over_data_channel(&channel, &packet, &mut next_message_id)
            .await
            .is_err()
        {
            break;
        }
    }
}

fn spawn_shared_webrtc_vp8_channel(
    raw_video_rx: watch::Receiver<Option<Arc<[u8]>>>,
    track: Arc<TrackLocalStaticSample>,
) -> watch::Receiver<Option<Arc<[u8]>>> {
    let (tx, rx) = watch::channel(None::<Arc<[u8]>>);
    tokio::spawn(async move {
        shared_webrtc_vp8_encoder(raw_video_rx, tx, track).await;
    });
    rx
}

async fn shared_webrtc_vp8_encoder(
    mut raw_video_rx: watch::Receiver<Option<Arc<[u8]>>>,
    encoded_tx: watch::Sender<Option<Arc<[u8]>>>,
    track: Arc<TrackLocalStaticSample>,
) {
    let mut encoder: Option<Vp8IvfEncoder> = None;
    let mut flush_interval = tokio::time::interval(Duration::from_millis(5));
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = raw_video_rx.changed() => {
                if changed.is_err() {
                    break;
                }

                let packet = raw_video_rx.borrow().clone();
                let Some(packet) = packet else {
                    continue;
                };

                let Some(raw) = decode_raw_frame_packet(packet.as_ref()) else {
                    let _ = encoded_tx.send_replace(Some(packet));
                    continue;
                };

                if encoder
                    .as_ref()
                    .is_none_or(|encoder| !encoder.matches_frame_format(&raw))
                {
                    encoder = Vp8IvfEncoder::start(&raw).await.ok();
                }

                if let Some(current_encoder) = encoder.as_mut() {
                    let send_failed = match prepare_ffmpeg_raw_frame(&raw) {
                        Some(payload) => current_encoder.send_frame(payload.as_ref()).await.is_err(),
                        None => true,
                    };
                    if send_failed {
                        encoder = None;
                        let _ = encoded_tx.send_replace(Some(packet));
                    }
                } else {
                    let _ = encoded_tx.send_replace(Some(packet));
                }
            }
            _ = flush_interval.tick() => {}
        }

        if let Some(current_encoder) = encoder.as_mut() {
            let mut encoder_failed = false;
            loop {
                let next_frame = match current_encoder.try_recv_frame() {
                    Ok(Some(frame)) => Some(frame),
                    Ok(None) => None,
                    Err(_) => {
                        encoder_failed = true;
                        None
                    }
                };
                let Some(frame) = next_frame else {
                    break;
                };
                let packet: Arc<[u8]> = Arc::from(encode_vp8_video_packet(&frame));
                let _ = encoded_tx.send_replace(Some(packet));
                let _ = track
                    .write_sample(&Sample {
                        data: MediaBytes::from(frame.payload.clone()),
                        timestamp: SystemTime::now(),
                        duration: Duration::from_micros(u64::from(frame.duration_us)),
                        packet_timestamp: (frame.pts_us.saturating_mul(90) / 1_000) as u32,
                        ..Default::default()
                    })
                    .await;
            }
            if encoder_failed {
                encoder = None;
            }
        }
    }
}

fn spawn_shared_webrtc_pcmu_track(
    audio_rx: broadcast::Receiver<Arc<[u8]>>,
    track: Arc<TrackLocalStaticSample>,
) {
    tokio::spawn(async move {
        shared_webrtc_pcmu_encoder(audio_rx, track).await;
    });
}

#[derive(Debug)]
struct DecodedAudioPacket {
    sample_rate: u32,
    channels: u8,
    frame_count: usize,
    payload_offset: usize,
}

fn decode_audio_packet(packet: &[u8]) -> Option<DecodedAudioPacket> {
    if packet.len() < AUDIO_PACKET_HEADER_LEN || &packet[..4] != AUDIO_PACKET_MAGIC {
        return None;
    }

    let sample_rate = u32::from_le_bytes(packet[20..24].try_into().ok()?);
    let channels = packet[24];
    let sample_format = packet[25];
    let frame_count = u32::from_le_bytes(packet[26..30].try_into().ok()?) as usize;
    let payload_len = u32::from_le_bytes(packet[30..34].try_into().ok()?) as usize;
    if sample_format != 0 || !(1..=2).contains(&channels) || sample_rate < 8_000 {
        return None;
    }

    let bytes_per_frame = channels as usize * 2;
    if payload_len < frame_count.checked_mul(bytes_per_frame)? {
        return None;
    }
    if packet.len() < AUDIO_PACKET_HEADER_LEN + payload_len {
        return None;
    }

    Some(DecodedAudioPacket {
        sample_rate,
        channels,
        frame_count,
        payload_offset: AUDIO_PACKET_HEADER_LEN,
    })
}

fn linear_to_mulaw(sample: i16) -> u8 {
    const MULAW_BIAS: i32 = 0x84;
    const MULAW_CLIP: i32 = 32_635;

    let mut pcm = sample as i32;
    let mask = if pcm < 0 {
        pcm = -pcm;
        0x7f
    } else {
        0xff
    };
    pcm = (pcm.min(MULAW_CLIP)) + MULAW_BIAS;

    let mut segment = 0u8;
    let mut value = pcm >> 7;
    while value > 1 {
        segment = segment.saturating_add(1);
        value >>= 1;
    }
    let mantissa = ((pcm >> (segment as i32 + 3)) & 0x0f) as u8;
    !(mask & ((segment << 4) | mantissa))
}

async fn shared_webrtc_pcmu_encoder(
    mut audio_rx: broadcast::Receiver<Arc<[u8]>>,
    track: Arc<TrackLocalStaticSample>,
) {
    let mut source_rate = 48_000u32;
    let mut phase = 0.0f64;
    let mut pending_mono = Vec::<i16>::new();
    let mut pending_pcmu = Vec::<u8>::new();
    let mut next_packet_timestamp = 0u32;

    loop {
        let packet = match audio_rx.recv().await {
            Ok(packet) => packet,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };

        let Some(decoded) = decode_audio_packet(packet.as_ref()) else {
            continue;
        };

        if decoded.sample_rate != source_rate {
            source_rate = decoded.sample_rate;
            phase = 0.0;
            pending_mono.clear();
            pending_pcmu.clear();
            next_packet_timestamp = 0;
        }

        let bytes_per_frame = decoded.channels as usize * 2;
        for frame_index in 0..decoded.frame_count {
            let sample_offset = decoded.payload_offset + frame_index * bytes_per_frame;
            let left = i16::from_le_bytes(packet[sample_offset..sample_offset + 2].try_into().unwrap());
            let mono = if decoded.channels == 1 {
                left
            } else {
                let right = i16::from_le_bytes(packet[sample_offset + 2..sample_offset + 4].try_into().unwrap());
                ((left as i32 + right as i32) / 2) as i16
            };
            pending_mono.push(mono);
        }

        let step = source_rate as f64 / PCMU_SAMPLE_RATE as f64;
        while phase + 1.0 < pending_mono.len() as f64 {
            let base_index = phase.floor() as usize;
            let next_index = (base_index + 1).min(pending_mono.len() - 1);
            let frac = phase - base_index as f64;
            let current = pending_mono[base_index] as f64;
            let next = pending_mono[next_index] as f64;
            let interpolated = current + (next - current) * frac;
            pending_pcmu.push(linear_to_mulaw(interpolated.round() as i16));
            phase += step;

            if pending_pcmu.len() >= PCMU_FRAME_SAMPLES {
                let frame = pending_pcmu.drain(..PCMU_FRAME_SAMPLES).collect::<Vec<_>>();
                let _ = track
                    .write_sample(&Sample {
                        data: MediaBytes::from(frame),
                        duration: Duration::from_millis(20),
                        packet_timestamp: next_packet_timestamp,
                        timestamp: SystemTime::now(),
                        ..Default::default()
                    })
                    .await;
                next_packet_timestamp = next_packet_timestamp.wrapping_add(PCMU_FRAME_SAMPLES as u32);
            }
        }

        let consumed = phase.floor() as usize;
        if consumed > 0 {
            pending_mono.drain(..consumed);
            phase -= consumed as f64;
        }
    }
}

async fn rtc_audio_channel_session(
    channel: Arc<RTCDataChannel>,
    mut audio_rx: broadcast::Receiver<Arc<[u8]>>,
) {
    let mut next_message_id = 1u32;
    loop {
        let packet = match audio_rx.recv().await {
            Ok(packet) => packet,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };

        if send_packet_over_data_channel(&channel, &packet, &mut next_message_id)
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn send_packet_over_data_channel(
    channel: &Arc<RTCDataChannel>,
    packet: &[u8],
    next_message_id: &mut u32,
) -> Result<()> {
    let message_id = *next_message_id;
    *next_message_id = next_message_id.wrapping_add(1);

    for chunk in encode_rtc_chunks(message_id, packet)? {
        channel
            .send(&chunk)
            .await
            .context("failed to send data channel chunk")?;
    }

    Ok(())
}

fn encode_rtc_chunks(message_id: u32, payload: &[u8]) -> Result<Vec<Bytes>> {
    if payload.is_empty() {
        let mut out = Vec::with_capacity(RTC_CHUNK_HEADER_LEN);
        out.extend_from_slice(RTC_CHUNK_MAGIC);
        out.extend_from_slice(&message_id.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        return Ok(vec![Bytes::from(out)]);
    }

    let chunk_count = payload.len().div_ceil(RTC_CHUNK_PAYLOAD_MAX);
    let total_chunks =
        u16::try_from(chunk_count).context("payload too large for data channel chunking")?;

    let mut chunks = Vec::with_capacity(chunk_count);
    for chunk_index in 0..chunk_count {
        let start = chunk_index * RTC_CHUNK_PAYLOAD_MAX;
        let end = ((chunk_index + 1) * RTC_CHUNK_PAYLOAD_MAX).min(payload.len());

        let mut out = Vec::with_capacity(RTC_CHUNK_HEADER_LEN + (end - start));
        out.extend_from_slice(RTC_CHUNK_MAGIC);
        out.extend_from_slice(&message_id.to_le_bytes());
        out.extend_from_slice(&(chunk_index as u16).to_le_bytes());
        out.extend_from_slice(&total_chunks.to_le_bytes());
        out.extend_from_slice(&payload[start..end]);
        chunks.push(Bytes::from(out));
    }

    Ok(chunks)
}

impl Vp8IvfEncoder {
    async fn start(raw: &RawFramePacket) -> Result<Self> {
        let pix_fmt = ffmpeg_raw_pixel_format(raw.pixel_format)
            .ok_or_else(|| anyhow!("unsupported pixel format {}", raw.pixel_format))?;
        let ffmpeg_binary =
            env::var("NOSEBLEED_FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".to_string());
        let encoder_name =
            env::var("NOSEBLEED_WEBRTC_VIDEO_ENCODER").unwrap_or_else(|_| "libvpx".to_string());
        let encoder_args = env::var("NOSEBLEED_WEBRTC_VIDEO_ENCODER_ARGS").ok();
        let bitrate_kbps: u32 = env::var("NOSEBLEED_WEBRTC_VIDEO_BITRATE_KBPS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 100)
            .unwrap_or(2_500);
        let is_libvpx = encoder_name.contains("libvpx");

        let mut child = Command::new(ffmpeg_binary);
        child
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-fflags")
            .arg("nobuffer")
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg(pix_fmt)
            .arg("-video_size")
            .arg(format!("{}x{}", raw.width, raw.height))
            .arg("-framerate")
            .arg("60")
            .arg("-i")
            .arg("pipe:0")
            .arg("-an")
            .arg("-c:v")
            .arg(encoder_name.as_str())
            .arg("-b:v")
            .arg(format!("{bitrate_kbps}k"))
            .arg("-maxrate")
            .arg(format!("{bitrate_kbps}k"))
            .arg("-bufsize")
            .arg(format!("{}k", bitrate_kbps * 2))
            .arg("-g")
            .arg("60");

        if is_libvpx {
            child
                .arg("-keyint_min")
                .arg("60")
                .arg("-deadline")
                .arg("realtime")
                .arg("-cpu-used")
                .arg("5")
                .arg("-lag-in-frames")
                .arg("0")
                .arg("-error-resilient")
                .arg("1");
        }

        if let Some(args) = encoder_args.as_deref() {
            for arg in args.split_whitespace() {
                child.arg(arg);
            }
        }

        child
            .arg("-f")
            .arg("ivf")
            .arg("pipe:1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = child
            .spawn()
            .context("failed to spawn ffmpeg video encoder")?;
        let stdin = child
            .stdin
            .take()
            .context("ffmpeg encoder stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("ffmpeg encoder stdout unavailable")?;

        let (frames_tx, frames_rx) = mpsc::channel(128);
        tokio::spawn(async move {
            let _ = read_ivf_frames(stdout, frames_tx).await;
        });

        Ok(Self {
            width: raw.width,
            height: raw.height,
            pixel_format: raw.pixel_format,
            stdin,
            frames_rx,
            _child: child,
        })
    }

    fn matches_frame_format(&self, raw: &RawFramePacket) -> bool {
        self.width == raw.width
            && self.height == raw.height
            && self.pixel_format == raw.pixel_format
    }

    async fn send_frame(&mut self, raw_bytes: &[u8]) -> Result<()> {
        self.stdin
            .write_all(raw_bytes)
            .await
            .context("failed to write frame to ffmpeg encoder")?;
        Ok(())
    }

    fn try_recv_frame(&mut self) -> Result<Option<EncodedVp8Frame>> {
        match self.frames_rx.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                Err(anyhow!("ffmpeg encoder output channel closed"))
            }
        }
    }
}

fn ffmpeg_raw_pixel_format(pixel_format: u8) -> Option<&'static str> {
    match pixel_format {
        0 => Some("bgr0"),
        1 => Some("rgb565le"),
        2 => Some("rgb555le"),
        _ => None,
    }
}

fn raw_frame_bytes_per_pixel(pixel_format: u8) -> Option<usize> {
    match pixel_format {
        0 => Some(4),
        1 | 2 => Some(2),
        _ => None,
    }
}

fn prepare_ffmpeg_raw_frame(raw: &RawFramePacket) -> Option<Cow<'_, [u8]>> {
    let bytes_per_pixel = raw_frame_bytes_per_pixel(raw.pixel_format)?;
    let width = raw.width as usize;
    let height = raw.height as usize;
    let row_bytes = width.checked_mul(bytes_per_pixel)?;
    let required = raw.pitch.checked_mul(height)?;
    if raw.payload.len() < required {
        return None;
    }

    if raw.pitch == row_bytes {
        let end = row_bytes.checked_mul(height)?;
        return Some(Cow::Borrowed(&raw.payload[..end]));
    }

    let mut packed = Vec::with_capacity(row_bytes.checked_mul(height)?);
    for row in 0..height {
        let start = row.checked_mul(raw.pitch)?;
        let end = start.checked_add(row_bytes)?;
        packed.extend_from_slice(&raw.payload[start..end]);
    }
    Some(Cow::Owned(packed))
}

fn decode_raw_frame_packet(packet: &[u8]) -> Option<RawFramePacket> {
    if packet.len() < FRAME_HEADER_LEN {
        return None;
    }

    if &packet[0..4] != FRAME_MAGIC {
        return None;
    }

    let width = le_u32(&packet[20..24]);
    let height = le_u32(&packet[24..28]);
    let pitch = le_u32(&packet[28..32]) as usize;
    let pixel_format = packet[32];
    let payload_len = le_u32(&packet[33..37]) as usize;
    if FRAME_HEADER_LEN + payload_len > packet.len() {
        return None;
    }

    let payload = packet[FRAME_HEADER_LEN..FRAME_HEADER_LEN + payload_len].to_vec();
    let expected_len = pitch.checked_mul(height as usize)?;
    if payload.len() < expected_len {
        return None;
    }

    Some(RawFramePacket {
        width,
        height,
        pitch,
        pixel_format,
        payload,
    })
}

fn sanitize_jpeg_quality(quality: u8) -> u8 {
    quality.clamp(25, 95)
}

fn encode_jpeg_video_packet(raw: &RawFramePacket, quality: u8) -> Option<Vec<u8>> {
    let width = raw.width as usize;
    let height = raw.height as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = raw.pitch;
    let mut rgb = vec![0u8; width.checked_mul(height)?.checked_mul(3)?];

    for y in 0..height {
        for x in 0..width {
            let di = (y * width + x) * 3;
            match raw.pixel_format {
                0 => {
                    let si = y * row_bytes + x * 4;
                    let b = *raw.payload.get(si)?;
                    let g = *raw.payload.get(si + 1)?;
                    let r = *raw.payload.get(si + 2)?;
                    rgb[di] = r;
                    rgb[di + 1] = g;
                    rgb[di + 2] = b;
                }
                1 => {
                    let si = y * row_bytes + x * 2;
                    let lo = *raw.payload.get(si)? as u16;
                    let hi = *raw.payload.get(si + 1)? as u16;
                    let v = lo | (hi << 8);
                    let r = ((v >> 11) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x3f) as u8;
                    let b = (v & 0x1f) as u8;
                    rgb[di] = (r << 3) | (r >> 2);
                    rgb[di + 1] = (g << 2) | (g >> 4);
                    rgb[di + 2] = (b << 3) | (b >> 2);
                }
                2 => {
                    let si = y * row_bytes + x * 2;
                    let lo = *raw.payload.get(si)? as u16;
                    let hi = *raw.payload.get(si + 1)? as u16;
                    let v = lo | (hi << 8);
                    let r = ((v >> 10) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x1f) as u8;
                    let b = (v & 0x1f) as u8;
                    rgb[di] = (r << 3) | (r >> 2);
                    rgb[di + 1] = (g << 3) | (g >> 2);
                    rgb[di + 2] = (b << 3) | (b >> 2);
                }
                _ => return None,
            }
        }
    }

    let mut jpeg_bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_bytes, quality);
    encoder
        .encode(&rgb, raw.width, raw.height, ColorType::Rgb8.into())
        .ok()?;

    let payload_len = jpeg_bytes.len() as u32;
    let mut out = Vec::with_capacity(JPEG_VIDEO_HEADER_LEN + jpeg_bytes.len());
    out.extend_from_slice(JPEG_VIDEO_MAGIC);
    out.extend_from_slice(&raw.width.to_le_bytes());
    out.extend_from_slice(&raw.height.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&jpeg_bytes);
    Some(out)
}

fn encode_vp8_video_packet(frame: &EncodedVp8Frame) -> Vec<u8> {
    let payload_len = frame.payload.len() as u32;
    let mut out = Vec::with_capacity(VP8_VIDEO_HEADER_LEN + frame.payload.len());
    out.extend_from_slice(VP8_VIDEO_MAGIC);
    out.extend_from_slice(&frame.pts_us.to_le_bytes());
    out.extend_from_slice(&frame.duration_us.to_le_bytes());
    out.push(u8::from(frame.keyframe));
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(&frame.payload);
    out
}

async fn read_ivf_frames(
    mut stdout: tokio::process::ChildStdout,
    frames_tx: mpsc::Sender<EncodedVp8Frame>,
) -> Result<()> {
    let mut header = [0u8; IVF_FILE_HEADER_LEN];
    stdout
        .read_exact(&mut header)
        .await
        .context("failed to read IVF file header")?;

    if &header[0..4] != b"DKIF" {
        return Err(anyhow!("unexpected IVF signature"));
    }
    if &header[8..12] != b"VP80" {
        return Err(anyhow!("unexpected IVF codec (expected VP80)"));
    }

    let timebase_denominator = le_u32(&header[16..20]).max(1);
    let timebase_numerator = le_u32(&header[20..24]).max(1);

    let computed_duration_us = ((1_000_000u128 * u128::from(timebase_numerator))
        / u128::from(timebase_denominator))
    .max(1)
    .min(u128::from(u32::MAX)) as u32;
    let frame_duration_us = if computed_duration_us > 0 {
        computed_duration_us
    } else {
        DEFAULT_VP8_FRAME_DURATION_US
    };

    loop {
        let mut frame_header = [0u8; IVF_FRAME_HEADER_LEN];
        stdout
            .read_exact(&mut frame_header)
            .await
            .context("failed to read IVF frame header")?;

        let frame_size = le_u32(&frame_header[0..4]) as usize;
        let timestamp = le_u64(&frame_header[4..12]);
        let pts_us = ((u128::from(timestamp) * u128::from(timebase_numerator) * 1_000_000u128)
            / u128::from(timebase_denominator))
        .min(u128::from(u64::MAX)) as u64;

        let mut payload = vec![0u8; frame_size];
        stdout
            .read_exact(payload.as_mut_slice())
            .await
            .context("failed to read IVF frame payload")?;

        let keyframe = payload.first().is_some_and(|byte| (byte & 0x01) == 0);
        if frames_tx
            .send(EncodedVp8Frame {
                pts_us,
                duration_us: frame_duration_us,
                keyframe,
                payload,
            })
            .await
            .is_err()
        {
            break;
        }
    }

    Ok(())
}

fn le_u32(raw: &[u8]) -> u32 {
    u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
}

fn le_u64(raw: &[u8]) -> u64 {
    u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ])
}

fn cleanup_input_source_once(state: &ServerState, source_id: &str, cleanup_once: &AtomicBool) {
    if cleanup_once.swap(true, Ordering::Relaxed) {
        return;
    }
    cleanup_input_source(state, source_id);
}

fn cleanup_input_source(state: &ServerState, source_id: &str) {
    state.input_hub.remove_source(source_id);
    let mut registry = state
        .input_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.mark_disconnected(source_id, state.auth.reconnect_window);
}

async fn video_session(
    mut socket: WebSocket,
    mut video_rx: watch::Receiver<Option<Arc<[u8]>>>,
    mode: WsVideoMode,
) {
    loop {
        tokio::select! {
            changed = video_rx.changed() => {
                if changed.is_err() {
                    break;
                }

                let packet = video_rx.borrow().clone();
                let Some(packet) = packet else {
                    continue;
                };

                let outgoing = match mode {
                    WsVideoMode::Raw => Bytes::copy_from_slice(packet.as_ref()),
                    WsVideoMode::Jpeg { quality } => {
                        let Some(raw) = decode_raw_frame_packet(packet.as_ref()) else {
                            continue;
                        };
                        let Some(jpeg) = encode_jpeg_video_packet(&raw, quality) else {
                            continue;
                        };
                        Bytes::from(jpeg)
                    }
                };

                if socket
                    .send(Message::Binary(outgoing))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn audio_session(mut socket: WebSocket, mut audio_rx: broadcast::Receiver<Arc<[u8]>>) {
    loop {
        tokio::select! {
            received = audio_rx.recv() => {
                let packet = match received {
                    Ok(packet) => packet,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };

                if socket
                    .send(Message::Binary(Bytes::copy_from_slice(packet.as_ref())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn input_session(
    mut socket: WebSocket,
    state: ServerState,
    source_id: String,
    claims: Option<MatchClaims>,
) {
    let owned_ports = claims
        .as_ref()
        .map(|claims| claims.allowed_ports.clone())
        .unwrap_or_default();

    if let Some(claims) = &claims {
        let reserve_result = {
            let mut registry = state
                .input_sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.reserve_ports(
                &source_id,
                &claims.player_id,
                &owned_ports,
                state.auth.reconnect_window,
            )
        };

        if let Err(message) = reserve_result {
            let _ = send_server_message(&mut socket, &ServerMessage::Error { message }).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    }

    while let Some(message) = socket.recv().await {
        let message = match message {
            Ok(message) => message,
            Err(_) => break,
        };

        match message {
            Message::Close(_) => break,
            Message::Text(text) => {
                let response =
                    process_input_payload(&state, &source_id, &owned_ports, true, text.as_str());
                if !send_server_message(&mut socket, &response).await {
                    break;
                }
            }
            Message::Binary(raw) => {
                let Ok(text) = str::from_utf8(raw.as_ref()) else {
                    if !send_server_message(
                        &mut socket,
                        &ServerMessage::Error {
                            message: "binary messages must be UTF-8 JSON".to_string(),
                        },
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                };

                let response = process_input_payload(&state, &source_id, &owned_ports, true, text);
                if !send_server_message(&mut socket, &response).await {
                    break;
                }
            }
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Pong(_) => {}
        }
    }

    cleanup_input_source(&state, &source_id);
}

fn process_input_payload(
    state: &ServerState,
    source_id: &str,
    owned_ports: &[u32],
    allow_input: bool,
    raw: &str,
) -> ServerMessage {
    let parsed = match parse_client_message(raw) {
        Ok(message) => message,
        Err(err) => {
            return ServerMessage::Error {
                message: format!("invalid message: {err:#}"),
            };
        }
    };

    match parsed {
        ClientMessage::Input {
            port,
            sequence,
            update,
        } => {
            if !allow_input {
                return ServerMessage::Error {
                    message: "input requires player role".to_string(),
                };
            }

            if !owned_ports.is_empty() {
                if !owned_ports.contains(&port) {
                    return ServerMessage::Error {
                        message: format!("port {port} not assigned to this player"),
                    };
                }

                let is_owner = {
                    let registry = state
                        .input_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    registry.is_source_owner(source_id, port)
                };

                if !is_owner {
                    return ServerMessage::Error {
                        message: format!("source no longer owns port {port}"),
                    };
                }
            }

            state.input_hub.apply_update(port, source_id, &update);
            ServerMessage::Ack {
                sequence,
                server_time_ms: now_unix_ms(),
            }
        }
        ClientMessage::Command {
            command,
            port,
            sequence,
        } => {
            if !allow_input {
                return ServerMessage::Error {
                    message: "commands require player role".to_string(),
                };
            }

            if !owned_ports.is_empty() && !owned_ports.contains(&port) {
                return ServerMessage::Error {
                    message: format!("port {port} not assigned to this player"),
                };
            }

            match command {
                ClientCommand::InsertCoin => {
                    state
                        .input_hub
                        .pulse_button(port, source_id, Button::Select);
                    ServerMessage::Ack {
                        sequence,
                        server_time_ms: now_unix_ms(),
                    }
                }
                ClientCommand::Reset => match state.session_manager.request_reset() {
                    Ok(()) => ServerMessage::Ack {
                        sequence,
                        server_time_ms: now_unix_ms(),
                    },
                    Err(err) => ServerMessage::Error {
                        message: format!("reset failed: {err:#}"),
                    },
                },
            }
        }
        ClientMessage::Ping { client_time_ms } => {
            let _ = client_time_ms;
            ServerMessage::Ack {
                sequence: None,
                server_time_ms: now_unix_ms(),
            }
        }
    }
}

async fn send_server_message(socket: &mut WebSocket, message: &ServerMessage) -> bool {
    let payload = match serialize_server_message(message) {
        Ok(payload) => payload,
        Err(_) => return false,
    };

    socket.send(Message::Text(payload.into())).await.is_ok()
}

fn authorize_stream_claims(
    state: &ServerState,
    token: Option<&str>,
) -> Result<Option<MatchClaims>, Response> {
    authorize_claims(state, token, false)
}

fn authorize_input_claims(
    state: &ServerState,
    token: Option<&str>,
) -> Result<Option<MatchClaims>, Response> {
    let claims = authorize_claims(state, token, true)?;
    if let Some(claims) = &claims {
        if claims.role != MatchRole::Player {
            return Err((StatusCode::FORBIDDEN, "input requires player role").into_response());
        }
    }
    Ok(claims)
}

fn authorize_claims(
    state: &ServerState,
    token: Option<&str>,
    require_player_ports: bool,
) -> Result<Option<MatchClaims>, Response> {
    if token.is_none() && !state.auth.require_auth {
        return Ok(None);
    }

    let token = token.ok_or_else(|| (StatusCode::UNAUTHORIZED, "missing token").into_response())?;
    let secret = state.auth.secret.as_deref().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth secret not configured",
        )
            .into_response()
    })?;

    let claims = validate_match_token(token, secret, now_unix_ms()).map_err(|err| {
        (StatusCode::UNAUTHORIZED, format!("invalid token: {err:#}")).into_response()
    })?;

    if require_player_ports && claims.allowed_ports.is_empty() {
        return Err((StatusCode::FORBIDDEN, "player token has no assigned ports").into_response());
    }

    Ok(Some(claims))
}

impl InputSessionRegistry {
    fn reserve_ports(
        &mut self,
        source_id: &str,
        player_id: &str,
        ports: &[u32],
        reconnect_window: Duration,
    ) -> std::result::Result<(), String> {
        self.cleanup_expired();

        for port in ports {
            if let Some(owner) = self.per_port.get(port) {
                if owner.source_id == source_id {
                    continue;
                }

                if owner.player_id != player_id {
                    return Err(format!("port {port} already assigned to another player"));
                }

                if owner.reconnect_until.is_none() {
                    return Err(format!("port {port} already active for this player"));
                }
            }
        }

        for port in ports {
            self.per_port.insert(
                *port,
                PortOwner {
                    player_id: player_id.to_owned(),
                    source_id: source_id.to_owned(),
                    reconnect_until: None,
                },
            );
        }

        if reconnect_window.is_zero() {
            self.cleanup_expired();
        }

        Ok(())
    }

    fn is_source_owner(&self, source_id: &str, port: u32) -> bool {
        self.per_port
            .get(&port)
            .is_some_and(|owner| owner.source_id == source_id && owner.reconnect_until.is_none())
    }

    fn mark_disconnected(&mut self, source_id: &str, reconnect_window: Duration) {
        self.cleanup_expired();
        let until = Instant::now() + reconnect_window;

        for owner in self.per_port.values_mut() {
            if owner.source_id == source_id {
                owner.reconnect_until = Some(until);
            }
        }
    }

    fn cleanup_expired(&mut self) {
        let now = Instant::now();
        self.per_port
            .retain(|_, owner| owner.reconnect_until.is_none_or(|until| until > now));
    }
}
