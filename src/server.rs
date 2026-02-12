use std::collections::HashMap;
use std::net::SocketAddr;
use std::str;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Router, body::Bytes};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::auth::{MatchClaims, MatchRole, validate_match_token};
use crate::input::InputHub;
use crate::protocol::{
    ClientMessage, ServerMessage, now_unix_ms, parse_client_message, serialize_server_message,
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
    pub audio_tx: broadcast::Sender<Arc<[u8]>>,
    pub input_hub: Arc<InputHub>,
    pub shutdown: Arc<AtomicBool>,
    pub next_client_id: Arc<AtomicU64>,
    pub auth: Arc<AuthConfig>,
    input_sessions: Arc<std::sync::Mutex<InputSessionRegistry>>,
}

impl ServerState {
    pub fn new(
        video_rx: watch::Receiver<Option<Arc<[u8]>>>,
        audio_tx: broadcast::Sender<Arc<[u8]>>,
        input_hub: Arc<InputHub>,
        shutdown: Arc<AtomicBool>,
        next_client_id: Arc<AtomicU64>,
        auth: Arc<AuthConfig>,
    ) -> Self {
        Self {
            video_rx,
            audio_tx,
            input_hub,
            shutdown,
            next_client_id,
            auth,
            input_sessions: Arc::new(std::sync::Mutex::new(InputSessionRegistry::default())),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct WsQuery {
    token: Option<String>,
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
        .route("/healthz", get(healthz))
        .route("/ws/video", get(video_ws))
        .route("/ws/audio", get(audio_ws))
        .route("/ws/input", get(input_ws))
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

async fn healthz() -> &'static str {
    "ok"
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
    ws.on_upgrade(move |socket| video_session(socket, rx))
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

async fn video_session(mut socket: WebSocket, mut video_rx: watch::Receiver<Option<Arc<[u8]>>>) {
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
                if !handle_input_payload(
                    &mut socket,
                    &state,
                    &source_id,
                    &owned_ports,
                    text.as_str(),
                )
                .await
                {
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

                if !handle_input_payload(&mut socket, &state, &source_id, &owned_ports, text).await
                {
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

    state.input_hub.remove_source(&source_id);
    let mut registry = state
        .input_sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.mark_disconnected(&source_id, state.auth.reconnect_window);
}

async fn handle_input_payload(
    socket: &mut WebSocket,
    state: &ServerState,
    source_id: &str,
    owned_ports: &[u32],
    raw: &str,
) -> bool {
    let parsed = match parse_client_message(raw) {
        Ok(message) => message,
        Err(err) => {
            return send_server_message(
                socket,
                &ServerMessage::Error {
                    message: format!("invalid message: {err:#}"),
                },
            )
            .await;
        }
    };

    match parsed {
        ClientMessage::Input {
            port,
            sequence,
            update,
        } => {
            if !owned_ports.is_empty() {
                if !owned_ports.contains(&port) {
                    return send_server_message(
                        socket,
                        &ServerMessage::Error {
                            message: format!("port {port} not assigned to this player"),
                        },
                    )
                    .await;
                }

                let is_owner = {
                    let registry = state
                        .input_sessions
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    registry.is_source_owner(source_id, port)
                };

                if !is_owner {
                    return send_server_message(
                        socket,
                        &ServerMessage::Error {
                            message: format!("source no longer owns port {port}"),
                        },
                    )
                    .await;
                }
            }

            state.input_hub.apply_update(port, source_id, &update);
            send_server_message(
                socket,
                &ServerMessage::Ack {
                    sequence,
                    server_time_ms: now_unix_ms(),
                },
            )
            .await
        }
        ClientMessage::Ping { client_time_ms } => {
            let _ = client_time_ms;
            send_server_message(
                socket,
                &ServerMessage::Ack {
                    sequence: None,
                    server_time_ms: now_unix_ms(),
                },
            )
            .await
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
