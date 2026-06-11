use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use serde::Deserialize;

use nosebleed::audio::AudioBus;
use nosebleed::core as runtime_core;
use nosebleed::frame::LatestFrameStore;
use nosebleed::input::InputHub;
use nosebleed::media::MediaConfig;
use nosebleed::server::{self, ServerState};
use nosebleed::session::{LaunchConfig, SessionManager, WorkspaceConfig};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "WebSocket-native low-latency frontend for libretro/RetroArch cores"
)]
struct Cli {
    #[arg(long, value_name = "CONFIG_PATH")]
    config: Option<PathBuf>,

    #[arg(long)]
    listen: Option<SocketAddr>,

    #[arg(long, value_name = "CORE_PATH")]
    core: Option<PathBuf>,

    #[arg(long, value_name = "ROM_PATH")]
    content: Option<PathBuf>,

    #[arg(long)]
    fps: Option<f32>,

    #[arg(long)]
    width: Option<u32>,

    #[arg(long)]
    height: Option<u32>,

    #[arg(long, env = "NOSEBLEED_AUTH_SECRET")]
    auth_secret: Option<String>,

    #[arg(long, default_value_t = false)]
    require_auth: bool,

    #[arg(long)]
    reconnect_window_ms: Option<u64>,

    #[arg(long)]
    session_root: Option<PathBuf>,

    #[arg(long)]
    session_id: Option<String>,

    #[arg(long, default_value_t = false)]
    session_copy_core: bool,

    #[arg(long, default_value_t = false)]
    session_copy_content: bool,

    #[arg(long, env = "NOSEBLEED_MEDIA_BACKEND")]
    media_backend: Option<String>,

    #[arg(long)]
    video_codec: Option<String>,

    #[arg(long)]
    video_encoder: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    listen: Option<SocketAddr>,
    core: Option<PathBuf>,
    content: Option<PathBuf>,
    fps: Option<f32>,
    width: Option<u32>,
    height: Option<u32>,
    media_backend: Option<String>,
    auth_secret: Option<String>,
    require_auth: Option<bool>,
    reconnect_window_ms: Option<u64>,
    session: Option<SessionConfig>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct SessionConfig {
    root_dir: Option<PathBuf>,
    id: Option<String>,
    copy_core: Option<bool>,
    copy_content: Option<bool>,
}

#[derive(Debug, Clone)]
struct AppConfig {
    listen: SocketAddr,
    launch: LaunchConfig,
    media: MediaConfig,
    auth_secret: Option<String>,
    require_auth: bool,
    reconnect_window_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_app_config(&cli)?;

    let frame_store = Arc::new(LatestFrameStore::default());
    let audio_bus = Arc::new(AudioBus::default());
    let input_hub = Arc::new(InputHub::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let auth_config = build_auth_config(&config)?;
    let media_capabilities = nosebleed::media::MediaCapabilities::detect(&config.media);

    let (video_rx, dispatcher_handle) =
        runtime_core::spawn_frame_dispatcher(frame_store.clone(), shutdown.clone());

    let session_manager = Arc::new(SessionManager::new(
        frame_store,
        audio_bus.clone(),
        input_hub.clone(),
        config.launch.clone(),
    ));
    session_manager
        .start(config.launch.clone(), true)
        .context("failed to start initial runtime session")?;

    let turn_credential = std::env::var("NOSEBLEED_TURN_SECRET")
        .unwrap_or_else(|_| "".to_string());
    let turn_host = std::env::var("NOSEBLEED_TURN_HOST")
        .unwrap_or_else(|_| "lngnckr.tech".to_string());
    let turn_url_internal = std::env::var("NOSEBLEED_TURN_URL_INTERNAL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let public_ip = std::env::var("NOSEBLEED_PUBLIC_IP").ok();

    let server_state = ServerState::new(
        video_rx,
        audio_bus.sender(),
        input_hub,
        shutdown.clone(),
        Arc::new(AtomicU64::new(1)),
        Arc::new(auth_config),
        session_manager.clone(),
        config.media.clone(),
        media_capabilities.clone(),
        turn_credential,
        turn_host,
        turn_url_internal,
        public_ip,

    )?;
    eprintln!("starting server: listen={}", config.listen);
    eprintln!(
        "media backend selected={}",
        media_capabilities.selected_backend.as_str(),
    );
    let server_result = server::run(server_state, config.listen).await;

    shutdown.store(true, Ordering::Relaxed);
    let _ = dispatcher_handle.join();
    let runtime_result = session_manager.shutdown_and_join();

    server_result.context("websocket server exited with an error")?;
    runtime_result.context("core runtime exited with an error")?;

    Ok(())
}

fn load_app_config(cli: &Cli) -> Result<AppConfig> {
    let (file_config, config_dir) = if let Some(path) = &cli.config {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let parsed: FileConfig = serde_json::from_str(&raw)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        let dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        (parsed, Some(dir))
    } else {
        (FileConfig::default(), None)
    };

    let mut session = file_config.session.unwrap_or_default();
    if cli.session_root.is_some() {
        session.root_dir = cli.session_root.clone();
    }
    if cli.session_id.is_some() {
        session.id = cli.session_id.clone();
    }
    if cli.session_copy_core {
        session.copy_core = Some(true);
    }
    if cli.session_copy_content {
        session.copy_content = Some(true);
    }

    let mut core = cli.core.clone().or(file_config.core);
    let mut content = cli.content.clone().or(file_config.content);
    if let Some(base) = config_dir.as_ref() {
        core = core.map(|path| resolve_relative_path(path, base));
        content = content.map(|path| resolve_relative_path(path, base));
        session.root_dir = session
            .root_dir
            .map(|path| resolve_relative_path(path, base));
    }

    let launch = LaunchConfig {
        core,
        content,
        fps: cli.fps.or(file_config.fps).unwrap_or(60.0),
        width: cli.width.or(file_config.width).unwrap_or(320),
        height: cli.height.or(file_config.height).unwrap_or(240),
        workspace: WorkspaceConfig {
            root_dir: session.root_dir,
            id: session.id,
            copy_core: session.copy_core.unwrap_or(false),
            copy_content: session.copy_content.unwrap_or(false),
        },
    };
    let media = MediaConfig::from_sources(
        cli.media_backend.as_deref(),
        file_config.media_backend.as_deref(),
        cli.video_codec.as_deref(),
        cli.video_encoder.as_deref(),
    )?;

    Ok(AppConfig {
        listen: cli.listen.or(file_config.listen).unwrap_or_else(|| {
            "0.0.0.0:8080"
                .parse()
                .expect("hard-coded listen address should parse")
        }),
        launch,
        media,
        auth_secret: cli.auth_secret.clone().or(file_config.auth_secret),
        require_auth: cli.require_auth || file_config.require_auth.unwrap_or(false),
        reconnect_window_ms: cli
            .reconnect_window_ms
            .or(file_config.reconnect_window_ms)
            .unwrap_or(15_000),
    })
}

fn resolve_relative_path(path: PathBuf, base: &Path) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    base.join(path)
}

fn build_auth_config(config: &AppConfig) -> Result<server::AuthConfig> {
    if config.require_auth && config.auth_secret.is_none() {
        return Err(anyhow!(
            "--require-auth needs --auth-secret (or NOSEBLEED_AUTH_SECRET)"
        ));
    }

    let reconnect_window = Duration::from_millis(config.reconnect_window_ms.max(1_000));
    let secret = config
        .auth_secret
        .as_ref()
        .map(|value| Arc::<[u8]>::from(value.as_bytes().to_vec()));

    Ok(server::AuthConfig {
        require_auth: config.require_auth,
        secret,
        reconnect_window,
    })
}
