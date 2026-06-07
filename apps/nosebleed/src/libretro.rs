use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::fs;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use libloading::{Library, Symbol};

use crate::audio::AudioBus;
use crate::frame::{LatestFrameStore, PixelFormat};
use crate::input::InputHub;

#[derive(Debug, Clone)]
pub enum RuntimeCommand {
    Reset,
    SaveState { slot: u8 },
    LoadState { slot: u8 },
}

#[derive(Debug, Default)]
pub struct RuntimeControl {
    commands: std::sync::Mutex<Vec<RuntimeCommand>>,
}

impl RuntimeControl {
    pub fn request_reset(&self) {
        self.request_command(RuntimeCommand::Reset);
    }

    pub fn request_save_state(&self, slot: u8) {
        self.request_command(RuntimeCommand::SaveState { slot });
    }

    pub fn request_load_state(&self, slot: u8) {
        self.request_command(RuntimeCommand::LoadState { slot });
    }

    pub fn take_commands(&self) -> Vec<RuntimeCommand> {
        let mut commands = self
            .commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *commands)
    }

    fn request_command(&self, command: RuntimeCommand) {
        self.commands
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(command);
    }
}

const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: u32 = 10;
const RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY: u32 = 9;
const RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY: u32 = 31;
const RETRO_ENVIRONMENT_GET_VARIABLE: u32 = 15;
const RETRO_ENVIRONMENT_SET_VARIABLES: u32 = 16;
const RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE: u32 = 17;
const RETRO_ENVIRONMENT_GET_LANGUAGE: u32 = 39;
const RETRO_ENVIRONMENT_SET_CORE_OPTIONS: u32 = 53;
const RETRO_ENVIRONMENT_SET_CORE_OPTIONS_INTL: u32 = 54;
const RETRO_ENVIRONMENT_SET_CORE_OPTIONS_DISPLAY: u32 = 55;
const RETRO_ENVIRONMENT_SET_CORE_OPTIONS_V2: u32 = 67;
const RETRO_ENVIRONMENT_SET_CORE_OPTIONS_V2_INTL: u32 = 68;
const RETRO_DEVICE_JOYPAD: u32 = 1;
const RETRO_DEVICE_ANALOG: u32 = 5;
const RETRO_LANGUAGE_ENGLISH: u32 = 0;
const RETRO_MEMORY_SAVE_RAM: u32 = 0;
const RETRO_NUM_CORE_OPTION_VALUES_MAX: usize = 128;

#[derive(Debug, Clone)]
pub struct LibretroRunConfig {
    pub core_path: PathBuf,
    pub content_path: Option<PathBuf>,
    pub fallback_fps: f32,
}

type RetroEnvironmentFn = unsafe extern "C" fn(cmd: u32, data: *mut c_void) -> bool;
type RetroVideoRefreshFn =
    unsafe extern "C" fn(data: *const c_void, width: u32, height: u32, pitch: usize);
type RetroAudioSampleFn = unsafe extern "C" fn(left: i16, right: i16);
type RetroAudioSampleBatchFn = unsafe extern "C" fn(data: *const i16, frames: usize) -> usize;
type RetroInputPollFn = unsafe extern "C" fn();
type RetroInputStateFn = unsafe extern "C" fn(port: u32, device: u32, index: u32, id: u32) -> i16;

type RetroSetEnvironment = unsafe extern "C" fn(cb: RetroEnvironmentFn);
type RetroSetVideoRefresh = unsafe extern "C" fn(cb: RetroVideoRefreshFn);
type RetroSetAudioSample = unsafe extern "C" fn(cb: RetroAudioSampleFn);
type RetroSetAudioSampleBatch = unsafe extern "C" fn(cb: RetroAudioSampleBatchFn);
type RetroSetInputPoll = unsafe extern "C" fn(cb: RetroInputPollFn);
type RetroSetInputState = unsafe extern "C" fn(cb: RetroInputStateFn);
type RetroInit = unsafe extern "C" fn();
type RetroDeinit = unsafe extern "C" fn();
type RetroRun = unsafe extern "C" fn();
type RetroReset = unsafe extern "C" fn();
type RetroLoadGame = unsafe extern "C" fn(game: *const RetroGameInfo) -> bool;
type RetroUnloadGame = unsafe extern "C" fn();
type RetroGetMemoryData = unsafe extern "C" fn(id: u32) -> *mut c_void;
type RetroGetMemorySize = unsafe extern "C" fn(id: u32) -> usize;
type RetroSerializeSize = unsafe extern "C" fn() -> usize;
type RetroSerialize = unsafe extern "C" fn(data: *mut c_void, size: usize) -> bool;
type RetroUnserialize = unsafe extern "C" fn(data: *const c_void, size: usize) -> bool;
type RetroGetSystemInfo = unsafe extern "C" fn(info: *mut RetroSystemInfo);
type RetroGetSystemAvInfo = unsafe extern "C" fn(info: *mut RetroSystemAvInfo);

#[derive(Debug, Default)]
struct CallbackContext {
    frame_store: Option<Arc<LatestFrameStore>>,
    audio_bus: Option<Arc<AudioBus>>,
    input_hub: Option<Arc<InputHub>>,
    pixel_format: PixelFormat,
}

#[repr(C)]
#[derive(Debug)]
struct RetroGameInfo {
    path: *const c_char,
    data: *const c_void,
    size: usize,
    meta: *const c_char,
}

#[repr(C)]
#[derive(Debug)]
struct RetroGameGeometry {
    base_width: u32,
    base_height: u32,
    max_width: u32,
    max_height: u32,
    aspect_ratio: f32,
}

#[repr(C)]
#[derive(Debug)]
struct RetroSystemTiming {
    fps: f64,
    sample_rate: f64,
}

#[repr(C)]
#[derive(Debug)]
struct RetroSystemAvInfo {
    geometry: RetroGameGeometry,
    timing: RetroSystemTiming,
}

#[repr(C)]
#[derive(Debug)]
struct RetroSystemInfo {
    library_name: *const c_char,
    library_version: *const c_char,
    valid_extensions: *const c_char,
    need_fullpath: bool,
    block_extract: bool,
}

#[repr(C)]
struct RetroVariable {
    key: *const c_char,
    value: *const c_char,
}

#[repr(C)]
struct RetroCoreOptionValue {
    value: *const c_char,
    label: *const c_char,
}

#[repr(C)]
struct RetroCoreOptionDefinition {
    key: *const c_char,
    desc: *const c_char,
    info: *const c_char,
    values: [RetroCoreOptionValue; RETRO_NUM_CORE_OPTION_VALUES_MAX],
    default_value: *const c_char,
}

#[repr(C)]
struct RetroCoreOptionsIntl {
    us: *const RetroCoreOptionDefinition,
    local: *const RetroCoreOptionDefinition,
}

#[repr(C)]
struct RetroCoreOptionV2Category {
    key: *const c_char,
    desc: *const c_char,
    info: *const c_char,
}

#[repr(C)]
struct RetroCoreOptionV2Definition {
    key: *const c_char,
    desc: *const c_char,
    desc_categorized: *const c_char,
    info: *const c_char,
    info_categorized: *const c_char,
    category_key: *const c_char,
    values: [RetroCoreOptionValue; RETRO_NUM_CORE_OPTION_VALUES_MAX],
    default_value: *const c_char,
}

#[repr(C)]
struct RetroCoreOptionsV2 {
    categories: *const RetroCoreOptionV2Category,
    definitions: *const RetroCoreOptionV2Definition,
}

#[repr(C)]
struct RetroCoreOptionsV2Intl {
    us: *const RetroCoreOptionsV2,
    local: *const RetroCoreOptionsV2,
}

#[repr(C)]
struct RetroCoreOptionDisplay {
    key: *const c_char,
    visible: bool,
}

#[derive(Debug, Clone)]
struct CoreLoadHints {
    need_fullpath: bool,
    block_extract: bool,
}

fn callback_context() -> &'static Mutex<CallbackContext> {
    static CONTEXT: OnceLock<Mutex<CallbackContext>> = OnceLock::new();
    CONTEXT.get_or_init(|| Mutex::new(CallbackContext::default()))
}

fn system_directory_cstr() -> &'static CString {
    static SYSTEM_DIR: OnceLock<CString> = OnceLock::new();
    SYSTEM_DIR.get_or_init(|| {
        let path = std::env::var("NOSEBLEED_SYSTEM_DIR")
            .unwrap_or_else(|_| "/srv/storage/games/system".to_string());
        let path = PathBuf::from(path);
        let _ = fs::create_dir_all(&path);
        CString::new(path.to_string_lossy().as_bytes())
            .expect("system dir path should not contain nulls")
    })
}

fn save_directory_path() -> PathBuf {
    let path = std::env::var("NOSEBLEED_SAVE_DIR")
        .unwrap_or_else(|_| "/srv/storage/games/saves".to_string());
    PathBuf::from(path)
}

fn save_directory_cstr() -> &'static CString {
    static SAVE_DIR: OnceLock<CString> = OnceLock::new();
    SAVE_DIR.get_or_init(|| {
        let path = save_directory_path();
        let _ = fs::create_dir_all(&path);
        CString::new(path.to_string_lossy().as_bytes())
            .expect("save dir path should not contain nulls")
    })
}

fn log_save_directory_once(path: &Path) {
    static LOGGED: OnceLock<()> = OnceLock::new();
    LOGGED.get_or_init(|| {
        eprintln!("save directory configured: {}", path.display());
    });
}

fn summarize_save_directory(path: &Path) -> String {
    let mut entries = Vec::new();
    if let Ok(read_dir) = fs::read_dir(path) {
        for entry in read_dir.flatten() {
            let entry_path = entry.path();
            if !entry_path.is_file() {
                continue;
            }

            let Some(ext) = entry_path.extension().and_then(|value| value.to_str()) else {
                continue;
            };
            if !matches!(ext.to_ascii_lowercase().as_str(), "sav" | "srm" | "ram") {
                continue;
            }

            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            entries.push(format!(
                "{} ({} bytes)",
                entry.file_name().to_string_lossy(),
                size
            ));
        }
    }

    entries.sort();
    if entries.is_empty() {
        "<empty>".to_string()
    } else {
        entries.join(", ")
    }
}

fn start_save_directory_watch(path: PathBuf) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let watch_path = path.clone();
        let _ = thread::Builder::new()
            .name("nosebleed-save-watch".to_string())
            .spawn(move || {
                let mut last_snapshot = String::new();
                loop {
                    let snapshot = summarize_save_directory(&watch_path);
                    if snapshot != last_snapshot {
                        eprintln!(
                            "save directory snapshot: {} => {}",
                            watch_path.display(),
                            snapshot
                        );
                        last_snapshot = snapshot;
                    }
                    thread::sleep(Duration::from_secs(5));
                }
            });
    });
}

fn save_snapshot_path(save_dir: &Path, content_path: &Path) -> Option<PathBuf> {
    let stem = content_path.file_stem()?.to_string_lossy().trim().to_string();
    if stem.is_empty() {
        return None;
    }

    Some(save_dir.join(stem).with_extension("srm"))
}

fn sync_save_ram_to_disk(
    save_dir: &Path,
    content_path: &Path,
    get_memory_data: RetroGetMemoryData,
    get_memory_size: RetroGetMemorySize,
) -> Result<bool> {
    let Some(snapshot_path) = save_snapshot_path(save_dir, content_path) else {
        return Ok(false);
    };

    let size = unsafe { get_memory_size(RETRO_MEMORY_SAVE_RAM) };
    if size == 0 {
        return Ok(false);
    }

    let data_ptr = unsafe { get_memory_data(RETRO_MEMORY_SAVE_RAM) };
    if data_ptr.is_null() {
        return Ok(false);
    }

    let bytes = unsafe { slice::from_raw_parts(data_ptr as *const u8, size) };
    if bytes.is_empty() {
        return Ok(false);
    }

    if let Ok(existing) = fs::read(&snapshot_path) {
        if existing == bytes {
            return Ok(false);
        }
    }

    if let Some(parent) = snapshot_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create save directory {}", parent.display()))?;
    }

    fs::write(&snapshot_path, bytes)
        .with_context(|| format!("failed to write save RAM snapshot to {}", snapshot_path.display()))?;
    Ok(true)
}

fn state_snapshot_path(save_dir: &Path, content_path: &Path, slot: u8) -> Option<PathBuf> {
    let stem = content_path.file_stem()?.to_string_lossy().trim().to_string();
    if stem.is_empty() {
        return None;
    }

    let slot = slot.max(1);
    Some(save_dir.join("states").join(stem).join(format!("slot-{slot:02}.state")))
}

fn sync_state_to_disk(
    save_dir: &Path,
    content_path: &Path,
    slot: u8,
    serialize_size: RetroSerializeSize,
    serialize: RetroSerialize,
) -> Result<bool> {
    let Some(state_path) = state_snapshot_path(save_dir, content_path, slot) else {
        return Ok(false);
    };

    let size = unsafe { serialize_size() };
    if size == 0 {
        return Ok(false);
    }

    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create save state directory {}", parent.display()))?;
    }

    let mut buffer = vec![0u8; size];
    let ok = unsafe { serialize(buffer.as_mut_ptr() as *mut c_void, size) };
    if !ok {
        return Ok(false);
    }

    fs::write(&state_path, buffer)
        .with_context(|| format!("failed to write save state snapshot to {}", state_path.display()))?;
    Ok(true)
}

fn restore_state_from_disk(
    save_dir: &Path,
    content_path: &Path,
    slot: u8,
    unserialize: RetroUnserialize,
) -> Result<bool> {
    let Some(state_path) = state_snapshot_path(save_dir, content_path, slot) else {
        return Ok(false);
    };

    let Ok(bytes) = fs::read(&state_path) else {
        return Ok(false);
    };
    if bytes.is_empty() {
        return Ok(false);
    }

    let ok = unsafe { unserialize(bytes.as_ptr() as *const c_void, bytes.len()) };
    if !ok {
        return Ok(false);
    }

    Ok(true)
}

fn c_string_from_ptr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }

    unsafe {
        std::ffi::CStr::from_ptr(ptr)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

fn parse_core_option_default(definition: &str) -> Option<String> {
    let (_, values) = definition.split_once(';')?;
    values
        .split('|')
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

fn core_option_defaults() -> &'static Mutex<HashMap<String, CString>> {
    static CORE_OPTION_DEFAULTS: OnceLock<Mutex<HashMap<String, CString>>> = OnceLock::new();
    CORE_OPTION_DEFAULTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_core_option_default(key: String, default_value: String) {
    let override_value = match key.as_str() {
        "mupen64plus-rdp-plugin" => Some("angrylion"),
        "mupen64plus-rsp-plugin" => Some("cxd4"),
        _ => None,
    };
    let chosen_value = override_value.unwrap_or(default_value.as_str());

    if let Ok(value) = CString::new(chosen_value) {
        core_option_defaults()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, value);
    }
}

unsafe fn collect_legacy_core_options(mut current: *const RetroVariable) {
    let mut defaults = core_option_defaults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    defaults.clear();

    loop {
        let variable = &*current;
        if variable.key.is_null() {
            break;
        }

        if let (Some(key), Some(definition)) = (
            c_string_from_ptr(variable.key),
            c_string_from_ptr(variable.value),
        ) {
            if let Some(default_value) = parse_core_option_default(&definition) {
                if let Ok(value) = CString::new(default_value) {
                    defaults.insert(key, value);
                }
            }
        }

        current = current.add(1);
    }
}

unsafe fn collect_core_option_definitions(mut current: *const RetroCoreOptionDefinition) {
    core_option_defaults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();

    loop {
        let definition = &*current;
        if definition.key.is_null() {
            break;
        }

        if let (Some(key), Some(default_value)) = (
            c_string_from_ptr(definition.key),
            c_string_from_ptr(definition.default_value),
        ) {
            set_core_option_default(key, default_value);
        }

        current = current.add(1);
    }
}

unsafe fn collect_core_option_definitions_v2(mut current: *const RetroCoreOptionV2Definition) {
    core_option_defaults()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();

    loop {
        let definition = &*current;
        if definition.key.is_null() {
            break;
        }

        if let (Some(key), Some(default_value)) = (
            c_string_from_ptr(definition.key),
            c_string_from_ptr(definition.default_value),
        ) {
            set_core_option_default(key, default_value);
        }

        current = current.add(1);
    }
}

pub fn run_libretro(
    config: LibretroRunConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
    control: Arc<RuntimeControl>,
) -> Result<()> {
    // SAFETY: Function pointers are loaded from a libretro core and invoked with the
    // signatures mandated by the libretro ABI.
    unsafe { run_libretro_unsafe(config, frame_store, audio_bus, input_hub, shutdown, control) }
}

unsafe fn run_libretro_unsafe(
    config: LibretroRunConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
    control: Arc<RuntimeControl>,
) -> Result<()> {
    let library = unsafe { Library::new(&config.core_path) }
        .with_context(|| format!("failed to load core library {}", config.core_path.display()))?;

    let retro_set_environment =
        unsafe { load_symbol::<RetroSetEnvironment>(&library, b"retro_set_environment\0") }?;
    let retro_set_video_refresh =
        unsafe { load_symbol::<RetroSetVideoRefresh>(&library, b"retro_set_video_refresh\0") }?;
    let retro_set_audio_sample =
        unsafe { load_symbol::<RetroSetAudioSample>(&library, b"retro_set_audio_sample\0") }?;
    let retro_set_audio_sample_batch = unsafe {
        load_symbol::<RetroSetAudioSampleBatch>(&library, b"retro_set_audio_sample_batch\0")
    }?;
    let retro_set_input_poll =
        unsafe { load_symbol::<RetroSetInputPoll>(&library, b"retro_set_input_poll\0") }?;
    let retro_set_input_state =
        unsafe { load_symbol::<RetroSetInputState>(&library, b"retro_set_input_state\0") }?;
    let retro_init = unsafe { load_symbol::<RetroInit>(&library, b"retro_init\0") }?;
    let retro_deinit = unsafe { load_symbol::<RetroDeinit>(&library, b"retro_deinit\0") }?;
    let retro_run = unsafe { load_symbol::<RetroRun>(&library, b"retro_run\0") }?;
    let retro_reset = unsafe { load_optional_symbol::<RetroReset>(&library, b"retro_reset\0") };
    let retro_load_game = unsafe { load_symbol::<RetroLoadGame>(&library, b"retro_load_game\0") }?;
    let retro_unload_game =
        unsafe { load_symbol::<RetroUnloadGame>(&library, b"retro_unload_game\0") }?;
    let retro_get_memory_data =
        unsafe { load_optional_symbol::<RetroGetMemoryData>(&library, b"retro_get_memory_data\0") };
    let retro_get_memory_size =
        unsafe { load_optional_symbol::<RetroGetMemorySize>(&library, b"retro_get_memory_size\0") };
    let retro_serialize_size =
        unsafe { load_optional_symbol::<RetroSerializeSize>(&library, b"retro_serialize_size\0") };
    let retro_serialize =
        unsafe { load_optional_symbol::<RetroSerialize>(&library, b"retro_serialize\0") };
    let retro_unserialize =
        unsafe { load_optional_symbol::<RetroUnserialize>(&library, b"retro_unserialize\0") };

    let retro_get_system_info =
        unsafe { load_optional_symbol::<RetroGetSystemInfo>(&library, b"retro_get_system_info\0") };
    let retro_get_system_av_info = unsafe {
        load_optional_symbol::<RetroGetSystemAvInfo>(&library, b"retro_get_system_av_info\0")
    };
    {
        let mut context = callback_context()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        context.frame_store = Some(frame_store);
        context.audio_bus = Some(audio_bus.clone());
        context.input_hub = Some(input_hub);
        context.pixel_format = PixelFormat::Xrgb8888;
    }

    unsafe { retro_set_environment(environment_callback) };
    unsafe { retro_set_video_refresh(video_refresh_callback) };
    unsafe { retro_set_audio_sample(audio_sample_callback) };
    unsafe { retro_set_audio_sample_batch(audio_sample_batch_callback) };
    unsafe { retro_set_input_poll(input_poll_callback) };
    unsafe { retro_set_input_state(input_state_callback) };
    unsafe { retro_init() };

    // Some cores expose `retro_set_controller_port_device` but are unstable when frontends
    // call it directly. We rely on default core-side port mappings here.

    let mut core_hints: Option<CoreLoadHints> = None;
    if let Some(get_info) = retro_get_system_info {
        let mut info = MaybeUninit::<RetroSystemInfo>::zeroed();
        unsafe { get_info(info.as_mut_ptr()) };
        let info = unsafe { info.assume_init() };
        eprintln!(
            "Loaded core metadata: need_fullpath={} block_extract={}",
            info.need_fullpath, info.block_extract
        );
        core_hints = Some(CoreLoadHints {
            need_fullpath: info.need_fullpath,
            block_extract: info.block_extract,
        });
    } else {
        eprintln!("Loaded core: unknown (retro_get_system_info unavailable)");
    }

    // Keep C string storage alive for the core lifetime in case the core keeps a raw pointer.
    let content_cstr = if let Some(content_path) = &config.content_path {
        if let Err(err) = log_content_file_details(content_path) {
            eprintln!(
                "failed to read content metadata for {}: {err:#}",
                content_path.display()
            );
        }
        Some(
            CString::new(content_path.to_string_lossy().as_bytes()).with_context(|| {
                format!(
                    "content path contains null byte: {}",
                    content_path.display()
                )
            })?,
        )
    } else {
        None
    };

    let content_bytes = if let Some(content_path) = &config.content_path {
        let should_preload = core_hints
            .as_ref()
            .map(|hints| !hints.need_fullpath)
            .unwrap_or(true);
        if should_preload {
            match fs::read(content_path) {
                Ok(bytes) => {
                    eprintln!("content preload: {} bytes", bytes.len());
                    Some(bytes)
                }
                Err(err) => {
                    eprintln!(
                        "content preload failed for {}: {err:#}",
                        content_path.display()
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let loaded = if let Some(content_cstr) = &content_cstr {
        let (data_ptr, data_len) = if let Some(bytes) = &content_bytes {
            (bytes.as_ptr() as *const c_void, bytes.len())
        } else {
            (ptr::null(), 0)
        };
        let game_info = RetroGameInfo {
            path: content_cstr.as_ptr(),
            data: data_ptr,
            size: data_len,
            meta: ptr::null(),
        };

        unsafe { retro_load_game(&game_info) }
    } else {
        unsafe { retro_load_game(ptr::null()) }
    };

    if !loaded {
        if let Some(content_path) = &config.content_path {
            eprintln!("core rejected content path: {}", content_path.display());
            if let Some(hints) = &core_hints {
                log_core_rejection_hints(hints, content_path);
            }
        } else {
            eprintln!("core rejected load with null content (retro_load_game(NULL))");
        }
        unsafe { retro_deinit() };
        clear_callback_context();
        bail!("core failed to load game/content");
    }

    let (fps, sample_rate_hz) = if let Some(get_av_info) = retro_get_system_av_info {
        let mut av_info = MaybeUninit::<RetroSystemAvInfo>::zeroed();
        unsafe { get_av_info(av_info.as_mut_ptr()) };
        let av_info = unsafe { av_info.assume_init() };
        let fps = if av_info.timing.fps > 1.0 {
            av_info.timing.fps as f32
        } else {
            config.fallback_fps
        };
        let sample_rate = if av_info.timing.sample_rate > 1.0 {
            av_info.timing.sample_rate as u32
        } else {
            48_000
        };
        (fps, sample_rate)
    } else {
        (config.fallback_fps, 48_000)
    };
    let fps = fps.max(1.0);
    audio_bus.set_sample_rate_hz(sample_rate_hz);

    let frame_interval = Duration::from_secs_f64((1.0 / fps as f64).max(0.001));
    let mut next_frame = Instant::now();
    let save_sync_interval = Duration::from_secs(5);
    let mut next_save_sync = Instant::now() + save_sync_interval;

    while !shutdown.load(Ordering::Relaxed) {
        for command in control.take_commands() {
            match command {
                RuntimeCommand::Reset => {
                    if let Some(retro_reset) = retro_reset {
                        unsafe { retro_reset() };
                    } else {
                        eprintln!("reset requested, but core does not expose retro_reset");
                    }
                }
                RuntimeCommand::SaveState { slot } => {
                    let Some(content_path) = config.content_path.as_deref() else {
                        eprintln!("save state requested for slot {slot}, but no content path is configured");
                        continue;
                    };

                    let Some(serialize_size) = retro_serialize_size else {
                        eprintln!("save state requested for slot {slot}, but core does not expose retro_serialize_size");
                        continue;
                    };
                    let Some(serialize) = retro_serialize else {
                        eprintln!("save state requested for slot {slot}, but core does not expose retro_serialize");
                        continue;
                    };

                    let save_dir = save_directory_path();
                    match sync_state_to_disk(&save_dir, content_path, slot, serialize_size, serialize) {
                        Ok(true) => eprintln!("save state slot {slot} written to disk"),
                        Ok(false) => eprintln!("save state slot {slot} produced no snapshot"),
                        Err(err) => eprintln!("save state sync failed for slot {slot}: {err:#}"),
                    }
                }
                RuntimeCommand::LoadState { slot } => {
                    let Some(content_path) = config.content_path.as_deref() else {
                        eprintln!("load state requested for slot {slot}, but no content path is configured");
                        continue;
                    };

                    let Some(unserialize) = retro_unserialize else {
                        eprintln!("load state requested for slot {slot}, but core does not expose retro_unserialize");
                        continue;
                    };

                    let save_dir = save_directory_path();
                    match restore_state_from_disk(&save_dir, content_path, slot, unserialize) {
                        Ok(true) => eprintln!("save state slot {slot} restored from disk"),
                        Ok(false) => eprintln!("save state slot {slot} not restored"),
                        Err(err) => eprintln!("save state restore failed for slot {slot}: {err:#}"),
                    }
                }
            }
        }

        unsafe { retro_run() };

        next_frame += frame_interval;
        let now = Instant::now();
        if next_frame > now {
            thread::sleep(next_frame - now);
        } else {
            next_frame = now;
        }

        let now = Instant::now();
        if now >= next_save_sync {
            if let (Some(get_memory_data), Some(get_memory_size), Some(content_path)) = (
                retro_get_memory_data,
                retro_get_memory_size,
                config.content_path.as_deref(),
            ) {
                let save_dir = save_directory_path();
                if let Err(err) =
                    sync_save_ram_to_disk(&save_dir, content_path, get_memory_data, get_memory_size)
                {
                    eprintln!("save RAM sync failed: {err:#}");
                }
            }
            next_save_sync = now + save_sync_interval;
        }
    }

    unsafe { retro_unload_game() };
    unsafe { retro_deinit() };
    clear_callback_context();

    Ok(())
}

fn clear_callback_context() {
    let mut context = callback_context()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    context.frame_store = None;
    context.audio_bus = None;
    context.input_hub = None;
    context.pixel_format = PixelFormat::Xrgb8888;
}

unsafe fn load_symbol<T>(library: &Library, name: &[u8]) -> Result<T>
where
    T: Copy,
{
    let symbol: Symbol<'_, T> = unsafe { library.get(name) }
        .with_context(|| format!("missing symbol {}", String::from_utf8_lossy(name)))?;
    Ok(*symbol)
}

unsafe fn load_optional_symbol<T>(library: &Library, name: &[u8]) -> Option<T>
where
    T: Copy,
{
    let symbol: Symbol<'_, T> = unsafe { library.get(name).ok()? };
    Some(*symbol)
}

unsafe extern "C" fn environment_callback(cmd: u32, data: *mut c_void) -> bool {
    match cmd {
        RETRO_ENVIRONMENT_GET_SYSTEM_DIRECTORY => {
            if data.is_null() {
                return false;
            }

            unsafe {
                *(data as *mut *const c_char) = system_directory_cstr().as_ptr();
            }
            true
        }
        RETRO_ENVIRONMENT_GET_SAVE_DIRECTORY => {
            if data.is_null() {
                return false;
            }

            let save_dir = save_directory_path();
            let _ = fs::create_dir_all(&save_dir);
            log_save_directory_once(&save_dir);
            start_save_directory_watch(save_dir.clone());
            eprintln!("libretro GET_SAVE_DIRECTORY -> {}", save_dir.display());

            unsafe {
                *(data as *mut *const c_char) = save_directory_cstr().as_ptr();
            }
            true
        }
        RETRO_ENVIRONMENT_SET_VARIABLES => {
            if data.is_null() {
                return false;
            }

            unsafe { collect_legacy_core_options(data as *const RetroVariable) };
            true
        }
        RETRO_ENVIRONMENT_SET_CORE_OPTIONS => {
            if data.is_null() {
                return false;
            }

            unsafe { collect_core_option_definitions(data as *const RetroCoreOptionDefinition) };
            true
        }
        RETRO_ENVIRONMENT_SET_CORE_OPTIONS_INTL => {
            if data.is_null() {
                return false;
            }

            let options = unsafe { &*(data as *const RetroCoreOptionsIntl) };
            if options.us.is_null() {
                return false;
            }

            unsafe { collect_core_option_definitions(options.us) };
            true
        }
        RETRO_ENVIRONMENT_SET_CORE_OPTIONS_V2 => {
            if data.is_null() {
                return false;
            }

            let options = unsafe { &*(data as *const RetroCoreOptionsV2) };
            if options.definitions.is_null() {
                return false;
            }

            unsafe { collect_core_option_definitions_v2(options.definitions) };
            true
        }
        RETRO_ENVIRONMENT_SET_CORE_OPTIONS_V2_INTL => {
            if data.is_null() {
                return false;
            }

            let options = unsafe { &*(data as *const RetroCoreOptionsV2Intl) };
            if options.us.is_null() {
                return false;
            }

            let us = unsafe { &*options.us };
            if us.definitions.is_null() {
                return false;
            }

            unsafe { collect_core_option_definitions_v2(us.definitions) };
            true
        }
        RETRO_ENVIRONMENT_SET_CORE_OPTIONS_DISPLAY => {
            if data.is_null() {
                return false;
            }

            let display = unsafe { &*(data as *const RetroCoreOptionDisplay) };
            !display.key.is_null()
        }
        RETRO_ENVIRONMENT_GET_VARIABLE_UPDATE => {
            if data.is_null() {
                return false;
            }

            unsafe {
                *(data as *mut bool) = false;
            }
            true
        }
        RETRO_ENVIRONMENT_GET_LANGUAGE => {
            if data.is_null() {
                return false;
            }

            unsafe {
                *(data as *mut u32) = RETRO_LANGUAGE_ENGLISH;
            }
            true
        }
        RETRO_ENVIRONMENT_GET_VARIABLE => {
            if data.is_null() {
                return false;
            }

            let variable = unsafe { &mut *(data as *mut RetroVariable) };
            let Some(key) = c_string_from_ptr(variable.key) else {
                return false;
            };

            let defaults = core_option_defaults()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(value) = defaults.get(&key) else {
                return false;
            };

            variable.value = value.as_ptr();
            true
        }
        RETRO_ENVIRONMENT_SET_PIXEL_FORMAT => {
            if data.is_null() {
                return false;
            }

            let raw_format = unsafe { *(data as *const i32) };
            let Some(format) = PixelFormat::from_libretro(raw_format) else {
                return false;
            };

            let mut context = callback_context()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            context.pixel_format = format;
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn video_refresh_callback(
    data: *const c_void,
    width: u32,
    height: u32,
    pitch: usize,
) {
    if data.is_null() || width == 0 || height == 0 {
        return;
    }

    // libretro may pass this sentinel to indicate the frontend can duplicate the previous frame.
    if data as usize == usize::MAX {
        return;
    }

    let (frame_store, pixel_format) = {
        let context = callback_context()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (context.frame_store.clone(), context.pixel_format)
    };

    let Some(frame_store) = frame_store else {
        return;
    };

    let byte_len = pitch.saturating_mul(height as usize);
    if byte_len == 0 {
        return;
    }

    // SAFETY: `data` comes from the core callback and points to at least `pitch * height` bytes
    // for the current video frame.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, byte_len) };
    frame_store.publish(width, height, pitch, pixel_format, bytes);
}

unsafe extern "C" fn audio_sample_callback(left: i16, right: i16) {
    let audio_bus = {
        let context = callback_context()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        context.audio_bus.clone()
    };
    let Some(audio_bus) = audio_bus else {
        return;
    };

    audio_bus.push_interleaved_stereo_i16(&[left, right]);
}

unsafe extern "C" fn audio_sample_batch_callback(data: *const i16, frames: usize) -> usize {
    if data.is_null() || frames == 0 {
        return 0;
    }

    let audio_bus = {
        let context = callback_context()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        context.audio_bus.clone()
    };
    let Some(audio_bus) = audio_bus else {
        return frames;
    };

    let sample_count = frames.saturating_mul(2);
    let samples = unsafe { std::slice::from_raw_parts(data, sample_count) };
    audio_bus.push_interleaved_stereo_i16(samples);
    frames
}

unsafe extern "C" fn input_poll_callback() {}

unsafe extern "C" fn input_state_callback(port: u32, device: u32, index: u32, id: u32) -> i16 {
    let input_hub = {
        let context = callback_context()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        context.input_hub.clone()
    };

    let Some(input_hub) = input_hub else {
        return 0;
    };

    match device {
        RETRO_DEVICE_JOYPAD => input_hub.joypad_button_state(port, id),
        RETRO_DEVICE_ANALOG => input_hub.analog_state(port, index, id),
        _ => 0,
    }
}

fn log_content_file_details(content_path: &PathBuf) -> Result<()> {
    let canonical = fs::canonicalize(content_path).with_context(|| {
        format!(
            "unable to resolve canonical path for {}",
            content_path.display()
        )
    })?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("unable to stat content file {}", canonical.display()))?;
    let file_type = if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "special"
    };
    eprintln!(
        "content file: path={} canonical={} type={} size={} bytes readonly={}",
        content_path.display(),
        canonical.display(),
        file_type,
        metadata.len(),
        metadata.permissions().readonly()
    );
    Ok(())
}

fn log_core_rejection_hints(hints: &CoreLoadHints, content_path: &PathBuf) {
    eprintln!(
        "load rejection context: need_fullpath={} block_extract={}",
        hints.need_fullpath, hints.block_extract
    );

    if content_path
        .extension()
        .and_then(|value| value.to_str())
        .is_none()
    {
        eprintln!("content has no file extension; core may require a known extension");
    }
}
