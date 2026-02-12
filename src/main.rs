mod audio;
mod auth;
mod core;
mod frame;
mod input;
mod libretro;
mod protocol;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::Parser;

use crate::audio::AudioBus;
use crate::core::{MockCoreConfig, spawn_frame_dispatcher, spawn_mock_core};
use crate::frame::LatestFrameStore;
use crate::input::InputHub;
use crate::libretro::LibretroRunConfig;
use crate::server::ServerState;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "WebSocket-native low-latency frontend for libretro/RetroArch cores"
)]
struct Cli {
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,

    #[arg(long, value_name = "CORE_PATH")]
    core: Option<PathBuf>,

    #[arg(long, value_name = "ROM_PATH")]
    content: Option<PathBuf>,

    #[arg(long, default_value_t = 60.0)]
    fps: f32,

    #[arg(long, default_value_t = 320)]
    width: u32,

    #[arg(long, default_value_t = 240)]
    height: u32,

    #[arg(long, env = "NOSEBLEED_AUTH_SECRET")]
    auth_secret: Option<String>,

    #[arg(long, default_value_t = false)]
    require_auth: bool,

    #[arg(long, default_value_t = 15_000)]
    reconnect_window_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let frame_store = Arc::new(LatestFrameStore::default());
    let audio_bus = Arc::new(AudioBus::default());
    let input_hub = Arc::new(InputHub::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let auth_config = build_auth_config(&cli)?;

    let (video_rx, dispatcher_handle) =
        spawn_frame_dispatcher(frame_store.clone(), shutdown.clone());

    let core_handle = spawn_core(
        &cli,
        frame_store.clone(),
        audio_bus.clone(),
        input_hub.clone(),
        shutdown.clone(),
    );

    let server_state = ServerState::new(
        video_rx,
        audio_bus.sender(),
        input_hub,
        shutdown.clone(),
        Arc::new(AtomicU64::new(1)),
        Arc::new(auth_config),
    );

    let server_result = server::run(server_state, cli.listen).await;

    shutdown.store(true, Ordering::Relaxed);

    let _ = dispatcher_handle.join();

    let core_result = match core_handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow!("core thread panicked")),
    };

    server_result.context("websocket server exited with an error")?;
    core_result.context("core runtime exited with an error")?;

    Ok(())
}

fn build_auth_config(cli: &Cli) -> Result<server::AuthConfig> {
    if cli.require_auth && cli.auth_secret.is_none() {
        return Err(anyhow!(
            "--require-auth needs --auth-secret (or NOSEBLEED_AUTH_SECRET)"
        ));
    }

    let reconnect_window = Duration::from_millis(cli.reconnect_window_ms.max(1_000));
    let secret = cli
        .auth_secret
        .as_ref()
        .map(|value| Arc::<[u8]>::from(value.as_bytes().to_vec()));

    Ok(server::AuthConfig {
        require_auth: cli.require_auth,
        secret,
        reconnect_window,
    })
}

fn spawn_core(
    cli: &Cli,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<Result<()>> {
    if let Some(core_path) = &cli.core {
        let config = LibretroRunConfig {
            core_path: core_path.clone(),
            content_path: cli.content.clone(),
            fallback_fps: cli.fps,
        };

        std::thread::spawn(move || {
            libretro::run_libretro(config, frame_store, audio_bus, input_hub, shutdown)
        })
    } else {
        if cli.content.is_some() {
            eprintln!("Ignoring --content because --core was not provided");
        }

        let config = MockCoreConfig {
            width: cli.width,
            height: cli.height,
            fps: cli.fps,
        };
        spawn_mock_core(config, frame_store, audio_bus, input_hub, shutdown)
    }
}
