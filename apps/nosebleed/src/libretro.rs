use std::ffi::{CString, c_char, c_void};
use std::fs;
use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use libloading::{Library, Symbol};

use crate::audio::AudioBus;
use crate::frame::{LatestFrameStore, PixelFormat};
use crate::input::InputHub;

const RETRO_ENVIRONMENT_SET_PIXEL_FORMAT: u32 = 10;
const RETRO_DEVICE_JOYPAD: u32 = 1;
const RETRO_DEVICE_ANALOG: u32 = 5;

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
type RetroLoadGame = unsafe extern "C" fn(game: *const RetroGameInfo) -> bool;
type RetroUnloadGame = unsafe extern "C" fn();
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

#[derive(Debug, Clone)]
struct CoreLoadHints {
    library_name: String,
    library_version: String,
    valid_extensions: String,
    need_fullpath: bool,
    block_extract: bool,
}

fn callback_context() -> &'static Mutex<CallbackContext> {
    static CONTEXT: OnceLock<Mutex<CallbackContext>> = OnceLock::new();
    CONTEXT.get_or_init(|| Mutex::new(CallbackContext::default()))
}

pub fn run_libretro(
    config: LibretroRunConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    // SAFETY: Function pointers are loaded from a libretro core and invoked with the
    // signatures mandated by the libretro ABI.
    unsafe { run_libretro_unsafe(config, frame_store, audio_bus, input_hub, shutdown) }
}

unsafe fn run_libretro_unsafe(
    config: LibretroRunConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
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
    let retro_load_game = unsafe { load_symbol::<RetroLoadGame>(&library, b"retro_load_game\0") }?;
    let retro_unload_game =
        unsafe { load_symbol::<RetroUnloadGame>(&library, b"retro_unload_game\0") }?;

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
        let library_name = c_string_or_unknown(info.library_name);
        let library_version = c_string_or_unknown(info.library_version);
        let valid_extensions = c_string_or_unknown(info.valid_extensions);
        eprintln!("Loaded core: {} ({})", library_name, library_version);
        eprintln!(
            "Core content hints: valid_extensions={} need_fullpath={} block_extract={}",
            valid_extensions, info.need_fullpath, info.block_extract
        );
        core_hints = Some(CoreLoadHints {
            library_name,
            library_version,
            valid_extensions,
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

    while !shutdown.load(Ordering::Relaxed) {
        unsafe { retro_run() };

        next_frame += frame_interval;
        let now = Instant::now();
        if next_frame > now {
            thread::sleep(next_frame - now);
        } else {
            next_frame = now;
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

fn c_string_or_unknown(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return "unknown".to_string();
    }

    // SAFETY: pointer is owned by the loaded core and expected to be valid C-string.
    unsafe {
        std::ffi::CStr::from_ptr(ptr)
            .to_str()
            .map(|value| value.to_string())
            .unwrap_or_else(|_| "unknown".to_string())
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
        "load rejection context: core={} version={} valid_extensions={} need_fullpath={} block_extract={}",
        hints.library_name,
        hints.library_version,
        hints.valid_extensions,
        hints.need_fullpath,
        hints.block_extract
    );

    if let Some(extension) = content_path.extension().and_then(|value| value.to_str()) {
        let ext = extension.to_ascii_lowercase();
        if !hints.valid_extensions.eq_ignore_ascii_case("unknown") {
            let allowed = hints
                .valid_extensions
                .split('|')
                .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            if !allowed.is_empty() && !allowed.iter().any(|value| value == &ext) {
                eprintln!(
                    "content extension mismatch: got .{} but core declares [{}]",
                    ext, hints.valid_extensions
                );
            }
        }
    } else {
        eprintln!("content has no file extension; core may require a known extension");
    }
}
