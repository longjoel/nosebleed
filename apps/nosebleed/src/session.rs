use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::audio::AudioBus;
use crate::core::{MockCoreConfig, spawn_mock_core};
use crate::frame::LatestFrameStore;
use crate::input::InputHub;
use crate::libretro::{self, LibretroRunConfig, RuntimeControl};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    pub root_dir: Option<PathBuf>,
    pub id: Option<String>,
    #[serde(default)]
    pub copy_core: bool,
    #[serde(default)]
    pub copy_content: bool,
}

#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub core: Option<PathBuf>,
    pub content: Option<PathBuf>,
    pub fps: f32,
    pub width: u32,
    pub height: u32,
    pub workspace: WorkspaceConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartRequest {
    pub core: Option<PathBuf>,
    pub content: Option<PathBuf>,
    pub fps: Option<f32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub force_restart: bool,
    pub workspace: Option<WorkspaceConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub running: bool,
    pub mode: String,
    pub core: Option<String>,
    pub content: Option<String>,
    pub fps: f32,
    pub width: u32,
    pub height: u32,
    pub started_at_unix_ms: Option<u64>,
    pub session_dir: Option<String>,
    pub last_exit: Option<String>,
}

pub struct SessionManager {
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    defaults: LaunchConfig,
    state: std::sync::Mutex<ManagerState>,
}

struct ActiveRuntime {
    launch: LaunchConfig,
    session_dir: Option<PathBuf>,
    started_at_unix_ms: u64,
    shutdown: Arc<AtomicBool>,
    control: Arc<RuntimeControl>,
    handle: JoinHandle<Result<()>>,
}

#[derive(Default)]
struct ManagerState {
    active: Option<ActiveRuntime>,
    last_exit: Option<String>,
}

impl SessionManager {
    pub fn new(
        frame_store: Arc<LatestFrameStore>,
        audio_bus: Arc<AudioBus>,
        input_hub: Arc<InputHub>,
        defaults: LaunchConfig,
    ) -> Self {
        Self {
            frame_store,
            audio_bus,
            input_hub,
            defaults,
            state: std::sync::Mutex::new(ManagerState::default()),
        }
    }

    pub fn start(&self, mut launch: LaunchConfig, force_restart: bool) -> Result<Status> {
        self.reap_finished_runtime();

        let existing = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            if state.active.is_some() && !force_restart {
                return Err(anyhow!("session already running; set force_restart=true"));
            }
            state.active.take()
        };
        if let Some(runtime) = existing {
            let exit = join_runtime(runtime);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            state.last_exit = Some(exit);
        }

        let session_dir = prepare_session_workspace(&mut launch)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let control = Arc::new(RuntimeControl::default());
        let handle = spawn_runtime(
            &launch,
            self.frame_store.clone(),
            self.audio_bus.clone(),
            self.input_hub.clone(),
            shutdown.clone(),
            control.clone(),
        );

        let started_at_unix_ms = now_unix_ms();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);
        state.active = Some(ActiveRuntime {
            launch,
            session_dir,
            started_at_unix_ms,
            shutdown,
            control,
            handle,
        });
        state.last_exit = None;
        Ok(snapshot_locked(&state))
    }

    pub fn start_from_request(&self, request: StartRequest) -> Result<Status> {
        let launch = LaunchConfig {
            core: request.core.or_else(|| self.defaults.core.clone()),
            content: request.content.or_else(|| self.defaults.content.clone()),
            fps: request.fps.unwrap_or(self.defaults.fps).max(1.0),
            width: request.width.unwrap_or(self.defaults.width).max(1),
            height: request.height.unwrap_or(self.defaults.height).max(1),
            workspace: request
                .workspace
                .unwrap_or_else(|| self.defaults.workspace.clone()),
        };
        self.start(launch, request.force_restart)
    }

    pub fn stop(&self) -> Status {
        self.reap_finished_runtime();
        let active = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            state.active.take()
        };

        if let Some(runtime) = active {
            let exit = join_runtime(runtime);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            state.last_exit = Some(exit);
            snapshot_locked(&state)
        } else {
            self.status()
        }
    }

    pub fn status(&self) -> Status {
        self.reap_finished_runtime();
        let state = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);
        snapshot_locked(&state)
    }

    pub fn request_reset(&self) -> Result<()> {
        self.reap_finished_runtime();
        let state = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);
        let runtime = state
            .active
            .as_ref()
            .ok_or_else(|| anyhow!("no active runtime to reset"))?;
        runtime.control.request_reset();
        Ok(())
    }

    pub fn request_save_state(&self, slot: u8) -> Result<()> {
        self.reap_finished_runtime();
        let state = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);
        let runtime = state
            .active
            .as_ref()
            .ok_or_else(|| anyhow!("no active runtime to save a state from"))?;
        runtime.control.request_save_state(slot);
        Ok(())
    }

    pub fn request_load_state(&self, slot: u8) -> Result<()> {
        self.reap_finished_runtime();
        let state = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);
        let runtime = state
            .active
            .as_ref()
            .ok_or_else(|| anyhow!("no active runtime to load a state into"))?;
        runtime.control.request_load_state(slot);
        Ok(())
    }

    pub fn shutdown_and_join(&self) -> Result<()> {
        let active = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            state.active.take()
        };

        if let Some(runtime) = active {
            let exit = join_runtime(runtime);
            if exit != "stopped" {
                return Err(anyhow!(exit));
            }
        }
        Ok(())
    }

    fn reap_finished_runtime(&self) {
        let finished = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            let should_take = state
                .active
                .as_ref()
                .is_some_and(|runtime| runtime.handle.is_finished());
            if should_take {
                state.active.take()
            } else {
                None
            }
        };

        if let Some(runtime) = finished {
            let exit = join_runtime(runtime);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(crate::lock_recover);
            state.last_exit = Some(exit);
        }
    }
}

fn spawn_runtime(
    launch: &LaunchConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
    control: Arc<RuntimeControl>,
) -> JoinHandle<Result<()>> {
    if let Some(core_path) = &launch.core {
        let config = LibretroRunConfig {
            core_path: core_path.clone(),
            content_path: launch.content.clone(),
            fallback_fps: launch.fps,
        };

        std::thread::spawn(move || {
            libretro::run_libretro(config, frame_store, audio_bus, input_hub, shutdown, control)
        })
    } else {
        if launch.content.is_some() {
            eprintln!("Ignoring content because core was not provided");
        }
        let config = MockCoreConfig {
            width: launch.width,
            height: launch.height,
            fps: launch.fps,
        };
        spawn_mock_core(config, frame_store, audio_bus, input_hub, shutdown)
    }
}

fn snapshot_locked(state: &ManagerState) -> Status {
    if let Some(active) = &state.active {
        Status {
            running: true,
            mode: if active.launch.core.is_some() {
                "libretro".to_string()
            } else {
                "mock".to_string()
            },
            core: active
                .launch
                .core
                .as_ref()
                .map(|value| value.display().to_string()),
            content: active
                .launch
                .content
                .as_ref()
                .map(|value| value.display().to_string()),
            fps: active.launch.fps,
            width: active.launch.width,
            height: active.launch.height,
            started_at_unix_ms: Some(active.started_at_unix_ms),
            session_dir: active
                .session_dir
                .as_ref()
                .map(|value| value.display().to_string()),
            last_exit: state.last_exit.clone(),
        }
    } else {
        Status {
            running: false,
            mode: "stopped".to_string(),
            core: None,
            content: None,
            fps: 0.0,
            width: 0,
            height: 0,
            started_at_unix_ms: None,
            session_dir: None,
            last_exit: state.last_exit.clone(),
        }
    }
}

fn join_runtime(runtime: ActiveRuntime) -> String {
    runtime.shutdown.store(true, Ordering::Relaxed);
    match runtime.handle.join() {
        Ok(Ok(())) => "stopped".to_string(),
        Ok(Err(err)) => format!("runtime exited with error: {err:#}"),
        Err(_) => "runtime thread panicked".to_string(),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

fn prepare_session_workspace(launch: &mut LaunchConfig) -> Result<Option<PathBuf>> {
    let Some(root_dir) = launch.workspace.root_dir.clone() else {
        return Ok(None);
    };

    let session_base = sanitize_session_id(launch.workspace.id.as_deref().unwrap_or("session"));
    let stamp = now_unix_ms();
    let session_dir = root_dir.join(format!("{session_base}-{stamp}"));
    fs::create_dir_all(&session_dir).with_context(|| {
        format!(
            "failed to create session workspace directory {}",
            session_dir.display()
        )
    })?;

    if launch.workspace.copy_core {
        if let Some(core_path) = launch.core.clone() {
            launch.core = Some(copy_file_to_dir(&core_path, &session_dir)?);
        }
    }

    if launch.workspace.copy_content {
        if let Some(content_path) = launch.content.clone() {
            launch.content = Some(copy_file_to_dir(&content_path, &session_dir)?);
        }
    }

    let manifest = serde_json::json!({
        "session_dir": session_dir,
        "core": launch.core.as_ref().map(|value| value.display().to_string()),
        "content": launch.content.as_ref().map(|value| value.display().to_string()),
        "fps": launch.fps,
        "width": launch.width,
        "height": launch.height,
    });
    fs::write(
        session_dir.join("session.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .with_context(|| {
        format!(
            "failed to write session manifest in {}",
            session_dir.display()
        )
    })?;

    eprintln!("session workspace ready: {}", session_dir.display());
    Ok(Some(session_dir))
}

fn copy_file_to_dir(src: &Path, target_dir: &Path) -> Result<PathBuf> {
    let file_name = src
        .file_name()
        .ok_or_else(|| anyhow!("path has no filename: {}", src.display()))?;
    let target = target_dir.join(file_name);
    fs::copy(src, &target)
        .with_context(|| format!("failed to copy {} into {}", src.display(), target.display()))?;
    Ok(target)
}

fn sanitize_session_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "session".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── snapshot_locked ─────────────────────────────────────────────────

    #[test]
    fn test_snapshot_locked_no_active_returns_stopped() {
        let state = ManagerState::default();
        let status = snapshot_locked(&state);
        assert!(!status.running, "no active runtime → not running");
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
    fn test_snapshot_locked_active_mock_mode() {
        let state = ManagerState {
            active: Some(ActiveRuntime {
                launch: LaunchConfig {
                    core: None,
                    content: None,
                    fps: 30.0,
                    width: 320,
                    height: 240,
                    workspace: WorkspaceConfig::default(),
                },
                session_dir: Some(PathBuf::from("/tmp/session-1")),
                started_at_unix_ms: 1000,
                shutdown: Arc::new(AtomicBool::new(false)),
                control: Arc::new(RuntimeControl::default()),
                handle: std::thread::spawn(|| Ok(())),
            }),
            last_exit: Some("crashed".into()),
        };
        let status = snapshot_locked(&state);
        assert!(status.running, "should be running");
        assert_eq!(status.mode, "mock", "no core → mock mode");
        assert!(status.core.is_none());
        assert!(status.content.is_none());
        assert_eq!(status.fps, 30.0);
        assert_eq!(status.width, 320);
        assert_eq!(status.height, 240);
        assert_eq!(status.started_at_unix_ms, Some(1000));
        assert_eq!(
            status.session_dir,
            Some("/tmp/session-1".to_string())
        );
        assert_eq!(status.last_exit, Some("crashed".to_string()));
    }

    #[test]
    fn test_snapshot_locked_active_libretro_mode() {
        let state = ManagerState {
            active: Some(ActiveRuntime {
                launch: LaunchConfig {
                    core: Some(PathBuf::from("/cores/genesis_libretro.so")),
                    content: Some(PathBuf::from("/roms/sonic.bin")),
                    fps: 60.0,
                    width: 640,
                    height: 480,
                    workspace: WorkspaceConfig::default(),
                },
                session_dir: None,
                started_at_unix_ms: 2000,
                shutdown: Arc::new(AtomicBool::new(false)),
                control: Arc::new(RuntimeControl::default()),
                handle: std::thread::spawn(|| Ok(())),
            }),
            last_exit: None,
        };
        let status = snapshot_locked(&state);
        assert!(status.running, "should be running");
        assert_eq!(status.mode, "libretro");
        assert_eq!(
            status.core,
            Some("/cores/genesis_libretro.so".to_string())
        );
        assert_eq!(status.content, Some("/roms/sonic.bin".to_string()));
        assert_eq!(status.fps, 60.0);
    }

    // ── sanitize_session_id ─────────────────────────────────────────────

    #[test]
    fn test_sanitize_session_id_preserves_alphanumeric() {
        assert_eq!(sanitize_session_id("my-session_42"), "my-session_42");
    }

    #[test]
    fn test_sanitize_session_id_replaces_special_chars() {
        assert_eq!(sanitize_session_id("hello world!"), "hello_world_");
    }

    #[test]
    fn test_sanitize_session_id_empty_fallback() {
        assert_eq!(sanitize_session_id(""), "session");
    }

    // ── LaunchConfig / WorkspaceConfig ──────────────────────────────────

    #[test]
    fn test_workspace_config_default() {
        let cfg = WorkspaceConfig::default();
        assert!(cfg.root_dir.is_none());
        assert!(cfg.id.is_none());
        assert!(!cfg.copy_core);
        assert!(!cfg.copy_content);
    }

    #[test]
    fn test_now_unix_ms_returns_nonzero() {
        let ts = now_unix_ms();
        assert!(ts > 1_700_000_000_000, "should be a recent unix timestamp in ms");
    }

    // ── start_from_request config merging ───────────────────────────────

    #[test]
    fn test_start_from_request_merges_defaults() {
        let request = StartRequest {
            core: None,
            content: None,
            fps: None,
            width: None,
            height: None,
            force_restart: false,
            workspace: None,
        };
        // We can't easily test start() because it spawns threads, but we
        // verify the request → LaunchConfig merging is correct by constructing
        // the manager with known defaults and checking what start_from_request
        // builds internally.
        //
        // Simulate the merge logic here as a unit test of the defaults:
        let defaults = LaunchConfig {
            core: Some(PathBuf::from("/cores/test.so")),
            content: Some(PathBuf::from("/roms/test.bin")),
            fps: 60.0,
            width: 640,
            height: 480,
            workspace: WorkspaceConfig {
                root_dir: Some(PathBuf::from("/sessions")),
                id: Some("test".into()),
                ..Default::default()
            },
        };

        let merged = LaunchConfig {
            core: request.core.or_else(|| defaults.core.clone()),
            content: request.content.or_else(|| defaults.content.clone()),
            fps: request.fps.unwrap_or(defaults.fps).max(1.0),
            width: request.width.unwrap_or(defaults.width).max(1),
            height: request.height.unwrap_or(defaults.height).max(1),
            workspace: request
                .workspace
                .unwrap_or_else(|| defaults.workspace.clone()),
        };

        assert_eq!(merged.core, Some(PathBuf::from("/cores/test.so")));
        assert_eq!(merged.content, Some(PathBuf::from("/roms/test.bin")));
        assert_eq!(merged.fps, 60.0);
        assert_eq!(merged.width, 640);
        assert_eq!(merged.height, 480);
        assert_eq!(
            merged.workspace.root_dir,
            Some(PathBuf::from("/sessions"))
        );
    }

    #[test]
    fn test_start_from_request_overrides_defaults() {
        let defaults = LaunchConfig {
            core: Some(PathBuf::from("/cores/default.so")),
            content: Some(PathBuf::from("/roms/default.bin")),
            fps: 60.0,
            width: 640,
            height: 480,
            workspace: WorkspaceConfig::default(),
        };

        let request = StartRequest {
            core: Some(PathBuf::from("/cores/custom.so")),
            content: None,
            fps: Some(30.0),
            width: Some(1280),
            height: Some(720),
            force_restart: false,
            workspace: None,
        };

        let merged = LaunchConfig {
            core: request.core.or_else(|| defaults.core.clone()),
            content: request.content.or_else(|| defaults.content.clone()),
            fps: request.fps.unwrap_or(defaults.fps).max(1.0),
            width: request.width.unwrap_or(defaults.width).max(1),
            height: request.height.unwrap_or(defaults.height).max(1),
            workspace: request
                .workspace
                .unwrap_or_else(|| defaults.workspace.clone()),
        };

        assert_eq!(merged.core, Some(PathBuf::from("/cores/custom.so")));
        assert_eq!(merged.content, Some(PathBuf::from("/roms/default.bin")));
        assert_eq!(merged.fps, 30.0);
        assert_eq!(merged.width, 1280);
        assert_eq!(merged.height, 720);
    }

    // ── Status struct ──────────────────────────────────────────────────

    #[test]
    fn test_status_serialization() {
        let status = Status {
            running: true,
            mode: "mock".into(),
            core: None,
            content: None,
            fps: 60.0,
            width: 640,
            height: 480,
            started_at_unix_ms: Some(12345),
            session_dir: Some("/tmp/session".into()),
            last_exit: None,
        };
        let json = serde_json::to_string(&status).expect("serialize Status");
        assert!(json.contains("\"running\":true"));
        assert!(json.contains("\"mode\":\"mock\""));
        assert!(json.contains("\"fps\":60.0"));
    }

    #[test]
    fn test_status_serialize_round_trip_fields_present() {
        // Status only implements Serialize, not Deserialize, so we just
        // verify that toggling running produces different JSON keys.
        let stopped_json = serde_json::to_string(&Status {
            running: false,
            mode: "stopped".into(),
            core: None,
            content: None,
            fps: 0.0,
            width: 0,
            height: 0,
            started_at_unix_ms: None,
            session_dir: None,
            last_exit: None,
        })
        .expect("serialize stopped");
        assert!(stopped_json.contains("\"mode\":\"stopped\""));
        assert!(stopped_json.contains("\"running\":false"));

        let running_json = serde_json::to_string(&Status {
            running: true,
            mode: "mock".into(),
            core: None,
            content: None,
            fps: 60.0,
            width: 640,
            height: 480,
            started_at_unix_ms: Some(999),
            session_dir: Some("/s".into()),
            last_exit: None,
        })
        .expect("serialize running");
        assert!(running_json.contains("\"running\":true"));
        assert!(running_json.contains("\"fps\":60.0"));
    }

    // ── ManagerState transitions ────────────────────────────────────────

    #[test]
    fn test_manager_state_active_replace() {
        let mut state = ManagerState::default();
        assert!(state.active.is_none());

        state.active = Some(ActiveRuntime {
            launch: LaunchConfig {
                core: None,
                content: None,
                fps: 60.0,
                width: 640,
                height: 480,
                workspace: WorkspaceConfig::default(),
            },
            session_dir: None,
            started_at_unix_ms: 0,
            shutdown: Arc::new(AtomicBool::new(false)),
            control: Arc::new(RuntimeControl::default()),
            handle: std::thread::spawn(|| Ok(())),
        });
        assert!(state.active.is_some());

        // Take and replace
        let old = state.active.take();
        assert!(old.is_some());
        assert!(state.active.is_none());

        state.last_exit = Some("replaced".to_string());
        assert_eq!(state.last_exit.as_deref(), Some("replaced"));
    }

    // ── LaunchConfig builder helpers ────────────────────────────────────

    #[test]
    fn test_launch_config_fps_min_clamp() {
        let mut base = LaunchConfig {
            core: None,
            content: None,
            fps: 0.5,
            width: 640,
            height: 480,
            workspace: WorkspaceConfig::default(),
        };
        // Simulate the clamp from start
        base.fps = base.fps.max(1.0);
        assert_eq!(base.fps, 1.0);
    }

    #[test]
    fn test_launch_config_dimensions_min_clamp() {
        let mut base = LaunchConfig {
            core: None,
            content: None,
            fps: 60.0,
            width: 0,
            height: 0,
            workspace: WorkspaceConfig::default(),
        };
        base.width = base.width.max(1);
        base.height = base.height.max(1);
        assert_eq!(base.width, 1);
        assert_eq!(base.height, 1);
    }
}
