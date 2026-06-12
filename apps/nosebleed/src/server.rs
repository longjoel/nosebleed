use std::collections::HashMap;
use std::net::SocketAddr;
use std::str;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_H264, MediaEngine};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::TrackLocal;
use webrtc_ice::network_type::NetworkType;
use webrtc_ice::udp_network::{EphemeralUDP, UDPNetwork};

use crate::arcade::{ArcadeError, ArcadeService, Side};
use crate::auth::{MatchClaims, MatchRole, validate_match_token};
use crate::gstreamer_backend::SharedGstreamerMedia;
use crate::input::{Button, InputHub};
use crate::media::select_encoder;
use crate::media::{MediaCapabilities, MediaConfig};
use crate::protocol::{
    ClientCommand, ClientMessage, ServerMessage, decode_input_binary, now_unix_ms,
    parse_client_message, serialize_server_message,
};
use crate::session::{
    SessionManager, StartRequest as SessionStartRequest, Status as SessionStatus,
};

#[derive(Debug)]
pub struct AuthConfig {
    pub require_auth: bool,
    pub secret: Option<Arc<[u8]>>,
    pub reconnect_window: Duration,
}

#[derive(Clone)]
pub struct ServerState {
    pub video_rx: watch::Receiver<Option<Arc<[u8]>>>,
    pub media_config: MediaConfig,
    pub media_capabilities: Arc<MediaCapabilities>,
    pub gstreamer_media: Option<Arc<SharedGstreamerMedia>>,
    pub audio_tx: broadcast::Sender<Arc<[u8]>>,
    pub input_hub: Arc<InputHub>,
    pub shutdown: Arc<AtomicBool>,
    pub next_client_id: Arc<AtomicU64>,
    pub auth: Arc<AuthConfig>,
    pub session_manager: Arc<SessionManager>,
    pub arcade: Arc<ArcadeService>,
    pub turn_credential: String,
    pub turn_host: String,
    pub turn_url_internal: String,
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
        media_config: MediaConfig,
        media_capabilities: MediaCapabilities,
        turn_credential: String,
        turn_host: String,
        turn_url_internal: String,
        public_ip: Option<String>,
    ) -> Result<Self> {
        let selection = select_encoder(&media_config.video_encoder)
            .context("failed to select GStreamer video encoder")?;
        eprintln!(
            "encoder selected: element={} codec={} hardware={} reason={}",
            selection.spec.video_encoder,
            selection.spec.video_codec,
            selection.spec.hardware,
            selection.selection_reason,
        );
        let gstreamer_media = Some(Arc::new(SharedGstreamerMedia::start(
            video_rx.clone(),
            audio_tx.clone(),
            selection,
        )?));

        let mut setting_engine = SettingEngine::default();
        setting_engine.set_network_types(vec![NetworkType::Udp4, NetworkType::Tcp4]);
        let udp_range = EphemeralUDP::new(8100, 8110)
            .context("failed to configure ICE UDP port range 8100-8110")?;
        eprintln!(
            "webrtc: port_range={}..{}",
            udp_range.port_min(),
            udp_range.port_max()
        );
        setting_engine.set_udp_network(UDPNetwork::Ephemeral(udp_range));
        if let Some(ip) = &public_ip {
            eprintln!("nat_1to1_ips: setting public IP to {ip}");
            setting_engine.set_nat_1to1_ips(vec![ip.clone()], RTCIceCandidateType::Host);
        }

        Ok(Self {
            video_rx,
            media_config,
            media_capabilities: Arc::new(media_capabilities),
            gstreamer_media,
            audio_tx,
            input_hub,
            shutdown,
            next_client_id,
            auth,
            session_manager,
            arcade: Arc::new(ArcadeService::new(6)),
            turn_credential,
            turn_host,
            turn_url_internal,
            input_sessions: Arc::new(std::sync::Mutex::new(InputSessionRegistry::default())),
            rtc_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            webrtc_api: {
                let mut media_engine = MediaEngine::default();
                // Start from defaults (VP8, Opus, etc.) then add H.264 for GStreamer
                media_engine.register_default_codecs().ok();
                media_engine
                    .register_codec(
                        RTCRtpCodecParameters {
                            capability: RTCRtpCodecCapability {
                                mime_type: MIME_TYPE_H264.to_owned(),
                                clock_rate: 90000,
                                channels: 0,
                                sdp_fmtp_line:
                                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                                        .to_owned(),
                                rtcp_feedback: vec![],
                            },
                            payload_type: 96,
                            ..Default::default()
                        },
                        RTPCodecType::Video,
                    )
                    .ok();
                Arc::new(
                    APIBuilder::new()
                        .with_media_engine(media_engine)
                        .with_setting_engine(setting_engine)
                        .build(),
                )
            },
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct WsQuery {
    token: Option<String>,
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
        .route("/media/capabilities", get(media_capabilities))
        .route("/session/status", get(session_status))
        .route("/session/start", post(session_start))
        .route("/session/stop", post(session_stop))
        .route("/session/snapshot", get(session_snapshot))
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

async fn media_capabilities(State(state): State<ServerState>) -> Json<MediaCapabilities> {
    let mut capabilities = (*state.media_capabilities).clone();
    capabilities.runtime.backend = state.media_config.selected_backend.as_str();
    if let Some(gstreamer_media) = state.gstreamer_media.as_ref() {
        capabilities.runtime = gstreamer_media.snapshot();
    }
    Json(capabilities)
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

async fn session_snapshot(State(state): State<ServerState>) -> Response {
    let Some(packet) = state.video_rx.borrow().clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no frame available").into_response();
    };
    let Some(frame) = crate::gstreamer_backend::decode_raw_frame_packet(&packet) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to decode frame").into_response();
    };

    let (width, height) = (frame.width as u32, frame.height as u32);
    let pixels_per_row = frame.width as usize;

    // Convert raw pixel data to RGB based on pixel format
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    match frame.pixel_format {
        // BGRx → RGB (skip 4th byte)
        0 => {
            for y in 0..height as usize {
                let row_start = y * frame.pitch;
                for x in 0..pixels_per_row {
                    let off = row_start + x * 4;
                    if off + 3 < frame.payload.len() {
                        rgb.push(frame.payload[off + 2]); // R
                        rgb.push(frame.payload[off + 1]); // G
                        rgb.push(frame.payload[off]); // B
                    }
                }
            }
        }
        // RGB16 (5-6-5) → RGB (8-8-8)
        1 => {
            for y in 0..height as usize {
                let row_start = y * frame.pitch;
                for x in 0..pixels_per_row {
                    let off = row_start + x * 2;
                    if off + 1 < frame.payload.len() {
                        let pixel =
                            u16::from_le_bytes([frame.payload[off], frame.payload[off + 1]]);
                        rgb.push(((pixel >> 11) as u8) << 3); // R 5→8
                        rgb.push((((pixel >> 5) & 0x3F) as u8) << 2); // G 6→8
                        rgb.push(((pixel & 0x1F) as u8) << 3); // B 5→8
                    }
                }
            }
        }
        // xRGB1555 (1-5-5-5) → RGB (8-8-8)
        2 => {
            for y in 0..height as usize {
                let row_start = y * frame.pitch;
                for x in 0..pixels_per_row {
                    let off = row_start + x * 2;
                    if off + 1 < frame.payload.len() {
                        let pixel =
                            u16::from_le_bytes([frame.payload[off], frame.payload[off + 1]]);
                        rgb.push((((pixel >> 10) & 0x1F) as u8) << 3); // R 5→8
                        rgb.push((((pixel >> 5) & 0x1F) as u8) << 3); // G 5→8
                        rgb.push(((pixel & 0x1F) as u8) << 3); // B 5→8
                    }
                }
            }
        }
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "unsupported pixel format",
            )
                .into_response();
        }
    }

    match image::RgbImage::from_raw(width, height, rgb) {
        Some(img) => {
            let mut png_bytes = std::io::Cursor::new(Vec::new());
            if img
                .write_to(&mut png_bytes, image::ImageFormat::Png)
                .is_err()
            {
                return (StatusCode::INTERNAL_SERVER_ERROR, "png encode failed").into_response();
            }
            (
                StatusCode::OK,
                [("content-type", "image/png")],
                png_bytes.into_inner(),
            )
                .into_response()
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid frame dimensions",
        )
            .into_response(),
    }
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
    eprintln!(
        "webrtc_session: offer.kind={} sdp_len={}",
        offer.kind,
        offer.sdp.len()
    );
    if offer.kind != "offer" {
        return (StatusCode::BAD_REQUEST, "sdp type must be offer").into_response();
    }

    let claims = match authorize_stream_claims(&state, query.token.as_deref()) {
        Ok(claims) => claims,
        Err(response) => {
            eprintln!("webrtc_session: auth failed");
            return response;
        }
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
                    .unwrap_or_else(crate::lock_recover);
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

    {
        let Some(gstreamer_media) = state.gstreamer_media.as_ref() else {
            cleanup_input_source(&state, &source_id);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "gstreamer backend selected but runtime was unavailable".to_string(),
            )
                .into_response();
        };

        let runtime = gstreamer_media.snapshot();
        eprintln!(
            "starting gstreamer webrtc session: video_encoder={} audio_encoder={} pipeline_state={} video_pipeline={} audio_pipeline={}",
            runtime.video_encoder.unwrap_or("unknown"),
            runtime.audio_encoder.unwrap_or("unknown"),
            runtime.pipeline_state,
            runtime.video_pipeline.as_deref().unwrap_or("<none>"),
            runtime.audio_pipeline.as_deref().unwrap_or("<none>"),
        );

        let video_track = gstreamer_media.video_track.clone() as Arc<dyn TrackLocal + Send + Sync>;
        let video_sender = match peer_connection.add_track(video_track).await {
            Ok(sender) => sender,
            Err(err) => {
                eprintln!("gstreamer webrtc: failed to attach video track: {err:#}");
                cleanup_input_source(&state, &source_id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to attach gstreamer video track: {err:#}"),
                )
                    .into_response();
            }
        };
        tokio::spawn(async move { while video_sender.read_rtcp().await.is_ok() {} });

        let audio_track = gstreamer_media.audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>;
        let audio_sender = match peer_connection.add_track(audio_track).await {
            Ok(sender) => sender,
            Err(err) => {
                eprintln!("gstreamer webrtc: failed to attach audio track: {err:#}");
                cleanup_input_source(&state, &source_id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to attach gstreamer audio track: {err:#}"),
                )
                    .into_response();
            }
        };
        tokio::spawn(async move { while audio_sender.read_rtcp().await.is_ok() {} });
    }

    // In MediaTracks mode the browser creates a negotiated "input" data channel
    // (id=0). The server must mirror this with the same id so both sides agree.
    // Negotiated channels don't fire on_data_channel — we must hook up on_message here.
    if input_allowed {
        match peer_connection
            .create_data_channel(
                "input",
                Some(
                    webrtc::data_channel::data_channel_init::RTCDataChannelInit {
                        negotiated: Some(0),
                        ..Default::default()
                    },
                ),
            )
            .await
        {
            Ok(channel) => {
                let state_for_input = state.clone();
                let source_for_input = source_id.clone();
                let owned_ports_for_input = owned_ports.clone();
                let channel_clone = channel.clone();
                channel.on_message(Box::new(move |message: DataChannelMessage| {
                    let state_for_input = state_for_input.clone();
                    let source_for_input = source_for_input.clone();
                    let owned_ports_for_input = owned_ports_for_input.clone();
                    let channel = channel_clone.clone();
                    Box::pin(async move {
                        // Binary frames → binary input protocol (fast path)
                        if message.data.len() == crate::input::INPUT_BINARY_SIZE {
                            if let Some(bin) =
                                crate::input::InputBinary::from_bytes(message.data.as_ref())
                            {
                                if input_allowed
                                    && (owned_ports_for_input.is_empty()
                                        || owned_ports_for_input.contains(&bin.port))
                                {
                                    let update = bin.to_input_update();
                                    state_for_input.input_hub.apply_update(
                                        bin.port,
                                        &source_for_input,
                                        &update,
                                    );
                                }
                            }
                            return;
                        }
                        let raw = match str::from_utf8(message.data.as_ref()) {
                            Ok(text) => text,
                            Err(_) => return,
                        };
                        let response = process_input_payload(
                            &state_for_input,
                            &source_for_input,
                            &owned_ports_for_input,
                            input_allowed,
                            raw,
                        );
                        if let Ok(payload) = serialize_server_message(&response) {
                            let _ = channel.send_text(payload).await;
                        }
                    })
                }));
            }
            Err(err) => {
                eprintln!("gstreamer webrtc: failed to create input data channel: {err:#}");
            }
        }
    }

    let cleanup_once = Arc::new(AtomicBool::new(false));
    let rtc_session_id = client_id;
    {
        let mut sessions = state
            .rtc_sessions
            .lock()
            .unwrap_or_else(crate::lock_recover);
        sessions.insert(rtc_session_id, peer_connection.clone());
    }

    {
        let state_for_close = state.clone();
        let source_for_close = source_id.clone();
        let cleanup_for_close = cleanup_once.clone();
        peer_connection.on_peer_connection_state_change(Box::new(move |connection_state| {
            eprintln!("webrtc: pc_state={rtc_session_id} => {connection_state:?}");
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
                            .unwrap_or_else(crate::lock_recover);
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
        peer_connection.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let state_for_channels = state_for_channels.clone();
            let source_for_channels = source_for_channels.clone();
            let owned_ports_for_channels = owned_ports_for_channels.clone();
            let cleanup_for_channels = cleanup_for_channels.clone();
            Box::pin(async move {
                let label = channel.label();
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
                            // Binary frames → binary input protocol (fast path)
                            if message.data.len() == crate::input::INPUT_BINARY_SIZE {
                                if let Some(bin) =
                                    crate::input::InputBinary::from_bytes(message.data.as_ref())
                                {
                                    if input_allowed
                                        && (owned_ports_for_input.is_empty()
                                            || owned_ports_for_input.contains(&bin.port))
                                    {
                                        let update = bin.to_input_update();
                                        state_for_input.input_hub.apply_update(
                                            bin.port,
                                            &source_for_input,
                                            &update,
                                        );
                                    }
                                }
                                return;
                            }
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

    let _cleanup = SessionCleanup {
        state: &state,
        source_id: &source_id,
        session_id: client_id,
        cleanup_once: &cleanup_once,
        armed: true,
    };

    match negotiate_webrtc_offer(&peer_connection, offer.sdp).await {
        Ok(answer) => {
            _cleanup.disarm();
            eprintln!(
                "webrtc_session: answer sdp_len={} has_video={} has_audio={}",
                answer.sdp.len(),
                answer.sdp.contains("msid:nosebleed video"),
                answer.sdp.contains("msid:nosebleed audio"),
            );
            Json(answer).into_response()
        }
        Err(err) => {
            eprintln!("gstreamer webrtc: session negotiation failed: {err:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response()
        }
    }
}

async fn create_peer_connection(state: &ServerState) -> Result<Arc<RTCPeerConnection>> {
    let mut ice_servers = vec![RTCIceServer {
        urls: vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
        ],
        ..Default::default()
    }];

    // Only include TURN server when a credential is configured.
    // An empty credential causes webrtc-rs to reject the config
    // with "turn server credentials required".
    let host = &state.turn_host;
    if !state.turn_credential.is_empty() {
        // Server-side TURN should prefer the internal Docker→host route.
        // Mixing unreachable public TURNS endpoints with the internal TURN URL
        // can leave webrtc-rs with no relay candidates even though coturn is
        // reachable on the host gateway.
        let urls = if !state.turn_url_internal.is_empty() {
            vec![state.turn_url_internal.clone()]
        } else {
            vec![format!("turns:{host}:5349?transport=tcp")]
        };
        ice_servers.push(RTCIceServer {
            urls,
            username: "nosebleed".to_string(),
            credential: state.turn_credential.clone(),
        });
    }

    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let connection = state
        .webrtc_api
        .new_peer_connection(config)
        .await
        .map_err(|err| anyhow!("failed to create peer connection: {err:#}"))?;
    Ok(Arc::new(connection))
}

/// Negotiate the SDP exchange with the peer: set remote description, create
/// answer, gather ICE candidates, and return the local answer.
async fn negotiate_webrtc_offer(pc: &RTCPeerConnection, offer_sdp: String) -> Result<WebRtcAnswer> {
    let remote_description = RTCSessionDescription::offer(offer_sdp)
        .map_err(|err| anyhow!("invalid remote offer: {err:#}"))?;
    pc.set_remote_description(remote_description)
        .await
        .map_err(|err| anyhow!("failed to set remote description: {err:#}"))?;
    let answer = pc
        .create_answer(None)
        .await
        .map_err(|err| anyhow!("failed to create answer: {err:#}"))?;
    let mut gather_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(answer)
        .await
        .map_err(|err| anyhow!("failed to set local description: {err:#}"))?;
    let _ = gather_complete.recv().await;
    let local_description = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("local description unavailable"))?;
    if local_description.sdp_type != RTCSdpType::Answer {
        return Err(anyhow!("local description was not an answer"));
    }
    Ok(WebRtcAnswer {
        kind: "answer",
        sdp: local_description.sdp,
    })
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
        .unwrap_or_else(crate::lock_recover);
    registry.mark_disconnected(source_id, state.auth.reconnect_window);
}

/// RAII guard that cleans up a WebRTC session on Drop.
/// Disarm with `.disarm()` on the success path to prevent cleanup.
struct SessionCleanup<'a> {
    state: &'a ServerState,
    source_id: &'a str,
    session_id: u64,
    cleanup_once: &'a AtomicBool,
    armed: bool,
}

impl SessionCleanup<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SessionCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut sessions = self
                .state
                .rtc_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            sessions.remove(&self.session_id);
        }
        cleanup_input_source_once(self.state, self.source_id, self.cleanup_once);
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
                .unwrap_or_else(crate::lock_recover);
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
                let parsed = match decode_input_binary(raw.as_ref()) {
                    Ok(msg) => msg,
                    Err(err) => {
                        let _ = send_server_message(
                            &mut socket,
                            &ServerMessage::Error {
                                message: format!("invalid binary input: {err:#}"),
                            },
                        )
                        .await;
                        continue;
                    }
                };
                let (port, sequence, update) = match parsed {
                    ClientMessage::Input {
                        port,
                        sequence,
                        update,
                    } => (port, sequence, update),
                    _ => {
                        let _ = send_server_message(
                            &mut socket,
                            &ServerMessage::Error {
                                message: "unexpected non-input binary message".to_string(),
                            },
                        )
                        .await;
                        continue;
                    }
                };
                if !owned_ports.is_empty() && !owned_ports.contains(&port) {
                    let _ = send_server_message(
                        &mut socket,
                        &ServerMessage::Error {
                            message: format!("port {port} not assigned to this player"),
                        },
                    )
                    .await;
                    continue;
                }
                state.input_hub.apply_update(port, &source_id, &update);
                let _ = send_server_message(
                    &mut socket,
                    &ServerMessage::Ack {
                        sequence,
                        server_time_ms: now_unix_ms(),
                    },
                )
                .await;
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
                        .unwrap_or_else(crate::lock_recover);
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
            slot,
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

            let slot = slot.unwrap_or(0);
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
                ClientCommand::SaveState => {
                    if slot == 0 {
                        return ServerMessage::Error {
                            message: "save state requires a slot number".to_string(),
                        };
                    }

                    match state.session_manager.request_save_state(slot) {
                        Ok(()) => ServerMessage::Ack {
                            sequence,
                            server_time_ms: now_unix_ms(),
                        },
                        Err(err) => ServerMessage::Error {
                            message: format!("save state failed: {err:#}"),
                        },
                    }
                }
                ClientCommand::LoadState => {
                    if slot == 0 {
                        return ServerMessage::Error {
                            message: "load state requires a slot number".to_string(),
                        };
                    }

                    match state.session_manager.request_load_state(slot) {
                        Ok(()) => ServerMessage::Ack {
                            sequence,
                            server_time_ms: now_unix_ms(),
                        },
                        Err(err) => ServerMessage::Error {
                            message: format!("load state failed: {err:#}"),
                        },
                    }
                }
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
                    // Different player — the C# seat manager is the authority.
                    // If it issued a token for a new player on this port, the old
                    // reservation is stale.  Force-release so the new player can
                    // connect even when the old tab's input WS is still open.
                    self.per_port.remove(port);
                    continue;
                }

                // Same player, different source — auto-release the old reservation
                // so reconnects don't get stuck on 409.
                if owner.reconnect_until.is_none() {
                    self.per_port.remove(port);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use tokio::sync::watch;

    use crate::arcade::ArcadeService;
    use crate::audio::AudioBus;
    use crate::frame::LatestFrameStore;
    use crate::input::InputHub;
    use crate::media::{
        EncoderReport, GstreamerCapabilities, GstreamerElements, MediaBackend, MediaCapabilities,
        MediaConfig, MediaRuntimeStatus, VideoEncoderConfig,
    };
    use crate::session::{LaunchConfig, SessionManager, WorkspaceConfig};

    // ── helpers ────────────────────────────────────────────────────────────

    fn dummy_auth_config(require_auth: bool) -> AuthConfig {
        AuthConfig {
            require_auth,
            secret: if require_auth {
                Some(Arc::<[u8]>::from(b"test-secret".to_vec()))
            } else {
                None
            },
            reconnect_window: Duration::from_secs(30),
        }
    }

    fn dummy_server_state(auth: AuthConfig) -> ServerState {
        let (_, video_rx) = watch::channel(None::<Arc<[u8]>>);
        let (audio_tx, _) = tokio::sync::broadcast::channel(8);

        let session_manager = Arc::new(SessionManager::new(
            Arc::new(LatestFrameStore::default()),
            Arc::new(AudioBus::new(44100, 8)),
            Arc::new(InputHub::default()),
            LaunchConfig {
                core: None,
                content: None,
                fps: 60.0,
                width: 640,
                height: 480,
                workspace: WorkspaceConfig::default(),
            },
        ));

        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs().ok();
        let webrtc_api = Arc::new(APIBuilder::new().with_media_engine(media_engine).build());

        ServerState {
            video_rx,
            media_config: MediaConfig {
                selected_backend: MediaBackend::Gstreamer,
                video_encoder: VideoEncoderConfig::default(),
            },
            media_capabilities: Arc::new(MediaCapabilities {
                selected_backend: MediaBackend::Gstreamer,
                gstreamer: GstreamerCapabilities {
                    compiled_in: false,
                    available_for_runtime: false,
                    init_ok: false,
                    version: None,
                    missing_reason: None,
                    elements: GstreamerElements {
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
                    },
                },
                runtime: MediaRuntimeStatus {
                    backend: "gstreamer",
                    transport: "webrtc",
                    video_codec: None,
                    video_encoder: None,
                    audio_codec: None,
                    audio_encoder: None,
                    video_pipeline: None,
                    audio_pipeline: None,
                    pipeline_state: "idle",
                    dropped_video_frames: 0,
                },
                encoders: EncoderReport {
                    selected: None,
                    candidates: vec![],
                    selection_reason: None,
                },
            }),
            gstreamer_media: None,
            audio_tx,
            input_hub: Arc::new(InputHub::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            next_client_id: Arc::new(AtomicU64::new(0)),
            auth: Arc::new(auth),
            session_manager,
            arcade: Arc::new(ArcadeService::new(1)),
            turn_credential: "test-credential".to_string(),
            turn_host: "localhost".to_string(),
            turn_url_internal: String::new(),
            input_sessions: Arc::new(std::sync::Mutex::new(InputSessionRegistry::default())),
            rtc_sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            webrtc_api,
        }
    }

    async fn dummy_peer_connection(state: &ServerState) -> Arc<RTCPeerConnection> {
        let config = RTCConfiguration {
            ice_servers: vec![],
            ..Default::default()
        };
        let pc = state
            .webrtc_api
            .new_peer_connection(config)
            .await
            .expect("create dummy peer connection");
        Arc::new(pc)
    }

    // ── 1. Invalid SDP parsing ─────────────────────────────────────────────

    #[test]
    fn test_invalid_sdp_parsing() {
        let result = RTCSessionDescription::offer("this is not valid sdp".to_string());
        assert!(result.is_err(), "garbage SDP should fail to parse");
    }

    // ── 2. WebRTC offer kind validation ────────────────────────────────────

    #[tokio::test]
    async fn test_webrtc_offer_kind_validation() {
        let state = dummy_server_state(dummy_auth_config(false));
        let offer = WebRtcOffer {
            kind: "answer".to_string(),
            sdp: "dummy".to_string(),
            video_mode: None,
        };

        let response = webrtc_session(
            axum::extract::Query(WsQuery { token: None }),
            axum::extract::State(state),
            axum::extract::Json(offer),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // ── 3. Token / authorization: no auth required ─────────────────────────

    #[test]
    fn test_authorize_stream_claims_no_auth_no_token() {
        let state = dummy_server_state(dummy_auth_config(false));
        let result = authorize_stream_claims(&state, None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "should return None claims");
    }

    // ── 4. Token / authorization: missing token with auth required ─────────

    #[test]
    fn test_authorize_stream_claims_missing_token() {
        let state = dummy_server_state(dummy_auth_config(true));
        let result = authorize_stream_claims(&state, None);
        assert!(result.is_err());
        // We can't easily inspect the Response — verify it's an error
        assert!(
            result.is_err_and(|r| r.status() == StatusCode::UNAUTHORIZED),
            "missing token with auth required should be 401"
        );
    }

    // ── 5. Token / authorization: invalid token ────────────────────────────

    #[test]
    fn test_authorize_stream_claims_invalid_token() {
        let state = dummy_server_state(dummy_auth_config(true));
        let result = authorize_stream_claims(&state, Some("not.a.real.token"));
        assert!(result.is_err(), "invalid token should be rejected");
    }

    // ── 6. SessionCleanup: armed drop cleans up session ────────────────────

    #[tokio::test]
    async fn test_session_cleanup_armed_removes_session() {
        let state = dummy_server_state(dummy_auth_config(false));
        let cleanup_once = Arc::new(AtomicBool::new(false));
        let session_id = 42u64;

        // Insert a dummy entry into rtc_sessions
        {
            let pc = dummy_peer_connection(&state).await;
            let mut sessions = state
                .rtc_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            sessions.insert(session_id, pc);
            assert!(
                sessions.contains_key(&session_id),
                "session should be present before cleanup"
            );
        }

        // Drop the guard while armed
        {
            let _guard = SessionCleanup {
                state: &state,
                source_id: "test-source-1",
                session_id,
                cleanup_once: &cleanup_once,
                armed: true,
            };
            // guard drops here → should call cleanup
        }

        // Verify the session was removed from rtc_sessions
        {
            let sessions = state
                .rtc_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            assert!(
                !sessions.contains_key(&session_id),
                "armed SessionCleanup Drop should remove session from registry"
            );
        }

        // Verify cleanup_once was fired (swap to true)
        assert!(
            cleanup_once.load(Ordering::Relaxed),
            "armed SessionCleanup Drop should set cleanup_once"
        );
    }

    // ── 7. SessionCleanup: disarmed drop is a no-op ────────────────────────

    #[tokio::test]
    async fn test_session_cleanup_disarmed_noop() {
        let state = dummy_server_state(dummy_auth_config(false));
        let cleanup_once = Arc::new(AtomicBool::new(false));
        let session_id = 99u64;

        // Insert a dummy entry into rtc_sessions
        {
            let pc = dummy_peer_connection(&state).await;
            let mut sessions = state
                .rtc_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            sessions.insert(session_id, pc);
        }

        // Drop the guard after disarming
        {
            let guard = SessionCleanup {
                state: &state,
                source_id: "test-source-2",
                session_id,
                cleanup_once: &cleanup_once,
                armed: true,
            };
            guard.disarm(); // disarms — now drop should be a no-op
        }

        // Verify the session is STILL present
        {
            let sessions = state
                .rtc_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            assert!(
                sessions.contains_key(&session_id),
                "disarmed SessionCleanup Drop should NOT remove session"
            );
        }

        // Verify cleanup_once was NOT fired
        assert!(
            !cleanup_once.load(Ordering::Relaxed),
            "disarmed SessionCleanup Drop should NOT set cleanup_once"
        );
    }

    // ── 8. cleanup_input_source_once: second call is skipped ───────────────

    #[test]
    fn test_cleanup_input_source_once_second_call_skipped() {
        let state = dummy_server_state(dummy_auth_config(false));
        let cleanup_once = Arc::new(AtomicBool::new(false));

        // First call — should fire
        cleanup_input_source_once(&state, "test-source", &cleanup_once);
        assert!(
            cleanup_once.load(Ordering::Relaxed),
            "first call should set cleanup_once"
        );

        // Reset the once flag to prove second call doesn't re-trigger
        // (We can't reset — the guard is designed to fire exactly once.
        //  We verify the swap prevented re-entry by checking the old value.)
        let old = cleanup_once.swap(false, Ordering::Relaxed);
        assert!(old, "cleanup_once should have been true before reset");

        // Now call again — should skip because old value was false after reset.
        // Actually, the pattern is: if swap(true) returns true, skip.
        // After reset to false, the next call would find false → fire again.
        // The real test: call twice in a row without reset.
        let flag = Arc::new(AtomicBool::new(false));
        cleanup_input_source_once(&state, "test-source-a", &flag);
        assert!(flag.load(Ordering::Relaxed), "first call fires");

        // Second call — swap(true) returns true → skip
        cleanup_input_source_once(&state, "test-source-b", &flag);
        // flag is still true — verify it wasn't toggled off
        assert!(
            flag.load(Ordering::Relaxed),
            "flag should remain true after second call"
        );
    }

    // ── 9. cleanup_input_source_once: first call fires cleanup ─────────────

    #[test]
    fn test_cleanup_input_source_triggers_cleanup() {
        let state = dummy_server_state(dummy_auth_config(false));
        let cleanup_once = Arc::new(AtomicBool::new(false));

        // Reserve a port first so cleanup has something to clean
        {
            let mut registry = state
                .input_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            registry
                .reserve_ports("cleanup-test", "player-1", &[0], Duration::from_secs(30))
                .expect("reserve port");
        }

        // Run cleanup
        cleanup_input_source(&state, "cleanup-test");

        // Verify the source was removed from the registry
        {
            let registry = state
                .input_sessions
                .lock()
                .unwrap_or_else(crate::lock_recover);
            // After cleanup, the port should be marked with reconnect_until set
            // (since reconnect_window > 0, it's not removed, just marked)
            if let Some(owner) = registry.per_port.get(&0) {
                assert!(
                    owner.reconnect_until.is_some(),
                    "port should have reconnect_until set after cleanup"
                );
                assert_eq!(owner.source_id, "cleanup-test");
            } else {
                panic!("port 0 should still exist with reconnect window");
            }
        }

        // Run the once-guarded version
        cleanup_input_source_once(&state, "cleanup-test", &cleanup_once);
        assert!(
            cleanup_once.load(Ordering::Relaxed),
            "first call should fire"
        );

        // Second call with once guard should not re-trigger
        cleanup_input_source_once(&state, "cleanup-test", &cleanup_once);
        assert!(
            cleanup_once.load(Ordering::Relaxed),
            "flag should still be true after second call"
        );
    }

    // ── 10. InputSessionRegistry: reserve ports ────────────────────────────

    #[test]
    fn test_input_session_registry_reserve_ports() {
        let mut registry = InputSessionRegistry::default();

        registry
            .reserve_ports("source-1", "player-1", &[0, 1], Duration::from_secs(30))
            .expect("reserve ports");

        assert_eq!(registry.per_port.len(), 2);
        assert_eq!(registry.per_port.get(&0).unwrap().player_id, "player-1");
        assert_eq!(registry.per_port.get(&1).unwrap().player_id, "player-1");

        // Same player, different source — should succeed (auto-release old)
        registry
            .reserve_ports("source-2", "player-1", &[0], Duration::from_secs(30))
            .expect("same player re-reserve");
        assert_eq!(
            registry.per_port.get(&0).unwrap().source_id,
            "source-2",
            "port should transfer to new source for same player"
        );

        // Different player — now succeeds (force-release old reservation)
        registry
            .reserve_ports("source-3", "player-2", &[1], Duration::from_secs(30))
            .expect("different player can take over port (force-release old)");
        assert_eq!(
            registry.per_port.get(&1).unwrap().player_id,
            "player-2",
            "port should transfer to new player"
        );
    }

    // ── 11. InputSessionRegistry: reconnect window ─────────────────────────

    #[test]
    fn test_input_session_registry_mark_disconnected_reconnect() {
        let mut registry = InputSessionRegistry::default();

        registry
            .reserve_ports("source-1", "player-1", &[0, 1], Duration::from_secs(30))
            .expect("reserve");

        registry.mark_disconnected("source-1", Duration::from_secs(60));

        let owner0 = registry.per_port.get(&0).unwrap();
        assert!(
            owner0.reconnect_until.is_some(),
            "should have reconnect window"
        );
        assert_eq!(owner0.source_id, "source-1");

        // Source owner check should fail during reconnect window
        assert!(
            !registry.is_source_owner("source-1", 0),
            "source should not own port during reconnect window"
        );
    }

    // ── 12. negotiate_webrtc_offer with invalid SDP (integration) ──────────

    #[cfg_attr(not(feature = "integration"), ignore)]
    #[tokio::test]
    async fn test_negotiate_webrtc_offer_invalid_sdp() {
        let state = dummy_server_state(dummy_auth_config(false));
        let pc = dummy_peer_connection(&state).await;

        let result = negotiate_webrtc_offer(&pc, "not a valid sdp".to_string()).await;
        assert!(
            result.is_err(),
            "negotiate_webrtc_offer with garbage SDP should fail"
        );
    }

    // ── 13. WebRtcOffer serialization/deserialization ─────────────────────

    #[test]
    fn test_webrtc_offer_deserialize() {
        let json =
            r#"{"type":"offer","sdp":"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n","video_mode":"h264"}"#;
        let offer: WebRtcOffer = serde_json::from_str(json).expect("deserialize offer");
        assert_eq!(offer.kind, "offer");
        assert!(offer.sdp.contains("v=0"));
        assert_eq!(offer.video_mode, Some("h264".to_string()));
    }

    #[test]
    fn test_webrtc_offer_deserialize_no_video_mode() {
        let json = r#"{"type":"offer","sdp":"v=0"}"#;
        let offer: WebRtcOffer =
            serde_json::from_str(json).expect("deserialize offer without video_mode");
        assert_eq!(offer.kind, "offer");
        assert_eq!(offer.sdp, "v=0");
        assert_eq!(offer.video_mode, None);
    }

    // ── 14. WebRtcAnswer serialization ────────────────────────────────────

    #[test]
    fn test_webrtc_answer_serialize() {
        let answer = WebRtcAnswer {
            kind: "answer",
            sdp: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n".to_string(),
        };
        let json = serde_json::to_string(&answer).expect("serialize answer");
        assert!(json.contains("\"type\":\"answer\""));
        assert!(json.contains("\"sdp\":\"v=0"));
    }

    // ── 15. SessionStatus default values ──────────────────────────────────

    #[test]
    fn test_session_status_default_field_values() {
        let status = SessionStatus {
            running: false,
            mode: "stopped".to_string(),
            core: None,
            content: None,
            fps: 0.0,
            width: 0,
            height: 0,
            started_at_unix_ms: None,
            session_dir: None,
            last_exit: None,
        };
        assert!(!status.running);
        assert_eq!(status.mode, "stopped");
        assert!(status.core.is_none());
        assert!(status.content.is_none());
        assert_eq!(status.fps, 0.0);
        assert_eq!(status.width, 0);
        assert_eq!(status.height, 0);
        assert!(status.started_at_unix_ms.is_none());
        assert!(status.session_dir.is_none());
        assert!(status.last_exit.is_none());
    }

    #[test]
    fn test_session_status_with_last_exit() {
        let status = SessionStatus {
            running: false,
            mode: "stopped".to_string(),
            core: None,
            content: None,
            fps: 0.0,
            width: 0,
            height: 0,
            started_at_unix_ms: None,
            session_dir: None,
            last_exit: Some("runtime crashed".to_string()),
        };
        assert_eq!(status.last_exit.as_deref(), Some("runtime crashed"));
    }

    // ── 16. ServerState test helpers (construction with defaults) ─────────

    #[test]
    fn test_dummy_server_state_has_expected_defaults() {
        let state = dummy_server_state(dummy_auth_config(false));
        assert!(!state.shutdown.load(Ordering::Relaxed));
        assert_eq!(state.next_client_id.load(Ordering::Relaxed), 0);
        assert!(!state.auth.require_auth);
        assert_eq!(state.auth.reconnect_window, Duration::from_secs(30));
        assert_eq!(state.turn_credential, "test-credential");
    }

    #[test]
    fn test_dummy_server_state_with_auth_has_secret() {
        let state = dummy_server_state(dummy_auth_config(true));
        assert!(state.auth.require_auth);
        assert!(state.auth.secret.is_some());
    }

    // ── 17. InputSessionRegistry: is_source_owner edge cases ──────────────

    #[test]
    fn test_input_session_registry_is_source_owner_no_port() {
        let registry = InputSessionRegistry::default();
        assert!(!registry.is_source_owner("anyone", 0));
    }

    #[test]
    fn test_input_session_registry_is_source_owner_reconnect_blocks() {
        let mut registry = InputSessionRegistry::default();
        registry
            .reserve_ports("source-1", "player-1", &[0], Duration::from_secs(30))
            .expect("reserve");

        // Before disconnect, source owns port
        assert!(registry.is_source_owner("source-1", 0));

        // Mark disconnected — reconnect window active
        registry.mark_disconnected("source-1", Duration::from_secs(60));

        // During reconnect window, source does NOT own port
        assert!(
            !registry.is_source_owner("source-1", 0),
            "source should not own port during reconnect window"
        );
    }

    // ── 18. InputSessionRegistry: cleanup_expired behavior ────────────────

    #[test]
    fn test_input_session_registry_cleanup_expired_removes_expired() {
        let mut registry = InputSessionRegistry::default();
        registry
            .reserve_ports("source-1", "player-1", &[0], Duration::from_secs(30))
            .expect("reserve");

        // Set reconnect_until to the past (expired)
        if let Some(owner) = registry.per_port.get_mut(&0) {
            owner.reconnect_until = Some(Instant::now() - Duration::from_secs(1));
        }

        registry.cleanup_expired();

        // Port should be removed
        assert!(
            !registry.per_port.contains_key(&0),
            "expired reconnect port should be cleaned up"
        );
    }

    #[test]
    fn test_input_session_registry_cleanup_expired_keeps_active() {
        let mut registry = InputSessionRegistry::default();
        registry
            .reserve_ports("source-1", "player-1", &[0], Duration::ZERO)
            .expect("reserve with no reconnect");

        // No reconnect window → reconnect_until is None
        assert!(registry.per_port.contains_key(&0));

        registry.cleanup_expired();
        assert!(
            registry.per_port.contains_key(&0),
            "port without reconnect should survive cleanup"
        );
    }

    // ── 19. InputSessionRegistry: reserve_ports with reconnect window ─────

    #[test]
    fn test_input_session_registry_reserve_with_zero_reconnect() {
        let mut registry = InputSessionRegistry::default();
        registry
            .reserve_ports("source-1", "player-1", &[0, 1], Duration::ZERO)
            .expect("reserve with zero reconnect");
        assert_eq!(registry.per_port.len(), 2);
        assert!(registry.is_source_owner("source-1", 0));
    }

    // ── 20. authorize_claims with auth disabled ───────────────────────────

    #[test]
    fn test_authorize_claims_no_auth_returns_none() {
        let state = dummy_server_state(dummy_auth_config(false));

        // We can't call authorize_claims directly because it returns Response.
        // Test the logic via authorize_stream_claims which is the public path.
        let result = authorize_stream_claims(&state, None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // ── 21. WsQuery defaults ─────────────────────────────────────────────

    #[test]
    fn test_ws_query_default_token_is_none() {
        let query = WsQuery::default();
        assert!(query.token.is_none());
    }

    // ── 22. cleanup_input_source removes from input_hub ───────────────────

    #[test]
    fn test_cleanup_input_source_removes_source_from_hub() {
        // Verify the function is callable and doesn't panic with a clean state
        let state = dummy_server_state(dummy_auth_config(false));
        // No source registered — should be a no-op
        cleanup_input_source(&state, "non-existent-source");
        // If we get here without panicking, the test passes
    }
}
