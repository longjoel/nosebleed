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
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::TrackLocal;

use crate::arcade::{ArcadeError, ArcadeService, Side};
use crate::auth::{MatchClaims, MatchRole, validate_match_token};
use crate::gstreamer_backend::SharedGstreamerMedia;
use crate::input::{Button, InputHub};
use crate::media::select_encoder;
use crate::media::{MediaCapabilities, MediaConfig, WebRtcTransportMode};
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
                Arc::new(APIBuilder::new().with_media_engine(media_engine).build())
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
    if offer.kind != "offer" {
        return (StatusCode::BAD_REQUEST, "sdp type must be offer").into_response();
    }

    let claims = match authorize_stream_claims(&state, query.token.as_deref()) {
        Ok(claims) => claims,
        Err(response) => return response,
    };
    let requested_transport = state
        .media_config
        .select_webrtc_transport(offer.video_mode.as_deref());

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

    if requested_transport == WebRtcTransportMode::MediaTracks {
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

            let video_track =
                gstreamer_media.video_track.clone() as Arc<dyn TrackLocal + Send + Sync>;
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

            let audio_track =
                gstreamer_media.audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>;
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
    }

    // In MediaTracks mode the browser creates a negotiated "input" data channel
    // (id=0). The server must mirror this with the same id so both sides agree.
    // Negotiated channels don't fire on_data_channel — we must hook up on_message here.
    if requested_transport == WebRtcTransportMode::MediaTracks && input_allowed {
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
        let _requested_transport_for_channels = requested_transport;
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
        eprintln!("gstreamer webrtc: failed to set remote description: {err:#}");
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
            eprintln!("gstreamer webrtc: failed to create answer: {err:#}");
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
        eprintln!("gstreamer webrtc: failed to set local description: {err:#}");
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
        ice_servers: vec![], // host-only — no STUN dependency for LAN
        ..Default::default()
    };

    let connection = state
        .webrtc_api
        .new_peer_connection(config)
        .await
        .map_err(|err| anyhow!("failed to create peer connection: {err:#}"))?;
    Ok(Arc::new(connection))
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
                    return Err(format!("port {port} already assigned to another player"));
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
