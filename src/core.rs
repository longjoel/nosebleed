use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::watch;

use crate::audio::AudioBus;
use crate::frame::{LatestFrameStore, PixelFormat};
use crate::input::InputHub;
use crate::protocol::encode_frame_packet;

#[derive(Debug, Clone)]
pub struct MockCoreConfig {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
}

impl Default for MockCoreConfig {
    fn default() -> Self {
        Self {
            width: 320,
            height: 240,
            fps: 60.0,
        }
    }
}

pub fn spawn_mock_core(
    config: MockCoreConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<Result<()>> {
    thread::spawn(move || run_mock_core(config, frame_store, audio_bus, input_hub, shutdown))
}

pub fn spawn_frame_dispatcher(
    frame_store: Arc<LatestFrameStore>,
    shutdown: Arc<AtomicBool>,
) -> (watch::Receiver<Option<Arc<[u8]>>>, JoinHandle<()>) {
    let (tx, rx) = watch::channel(None::<Arc<[u8]>>);

    let handle = thread::spawn(move || {
        let mut last_sequence = None;

        while !shutdown.load(Ordering::Relaxed) {
            let Some(frame) = frame_store.wait_for_next(last_sequence, Duration::from_millis(50))
            else {
                continue;
            };

            last_sequence = Some(frame.sequence);
            let packet = encode_frame_packet(&frame);
            let _ = tx.send(Some(packet));
        }
    });

    (rx, handle)
}

fn run_mock_core(
    config: MockCoreConfig,
    frame_store: Arc<LatestFrameStore>,
    audio_bus: Arc<AudioBus>,
    input_hub: Arc<InputHub>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let width = config.width.max(1);
    let height = config.height.max(1);
    let fps = config.fps.max(1.0);
    let frame_interval = Duration::from_secs_f64((1.0f64 / fps as f64).max(0.001));
    let pitch = width as usize * 4;
    let sample_rate_hz = 48_000u32;
    audio_bus.set_sample_rate_hz(sample_rate_hz);
    let samples_per_frame = sample_rate_hz as f64 / fps as f64;

    let mut frame_counter: u64 = 0;
    let mut sample_fraction = 0.0f64;
    let mut tone_phase = 0.0f32;
    let mut next_frame_at = Instant::now();

    while !shutdown.load(Ordering::Relaxed) {
        let mut buffer = vec![0u8; pitch * height as usize];

        let a_held = input_hub.joypad_button_state(0, 8) != 0;
        let b_held = input_hub.joypad_button_state(0, 0) != 0;
        let lx = input_hub.analog_state(0, 0, 0) as f32 / i16::MAX as f32;
        let ly = input_hub.analog_state(0, 0, 1) as f32 / i16::MAX as f32;

        let x_bias = ((lx * 127.0) as i32).clamp(-127, 127);
        let y_bias = ((ly * 127.0) as i32).clamp(-127, 127);

        for y in 0..height {
            for x in 0..width {
                let offset = y as usize * pitch + x as usize * 4;

                let base_r =
                    (((x as u64 + frame_counter) & 0xff) as i32 + x_bias).clamp(0, 255) as u8;
                let base_g =
                    (((y as u64 + frame_counter / 2) & 0xff) as i32 + y_bias).clamp(0, 255) as u8;
                let base_b = ((frame_counter & 0xff) as u8).saturating_add((x ^ y) as u8 / 2);

                let r = if a_held { 255 } else { base_r };
                let g = if b_held { 255 } else { base_g };
                let b = base_b;

                // libretro XRGB8888 is little-endian BB GG RR XX
                buffer[offset] = b;
                buffer[offset + 1] = g;
                buffer[offset + 2] = r;
                buffer[offset + 3] = 0;
            }
        }

        frame_store.publish(width, height, pitch, PixelFormat::Xrgb8888, &buffer);
        frame_counter = frame_counter.wrapping_add(1);

        sample_fraction += samples_per_frame;
        let frame_samples = sample_fraction.floor() as usize;
        sample_fraction -= frame_samples as f64;

        if frame_samples > 0 {
            let mut pcm = Vec::with_capacity(frame_samples * 2);
            let amplitude = if a_held || b_held { 0.24 } else { 0.16 };
            let frequency_hz = 220.0 + (lx.abs() * 330.0) + (ly.abs() * 110.0);
            let phase_step = std::f32::consts::TAU * frequency_hz / sample_rate_hz as f32;

            for _ in 0..frame_samples {
                let value = (tone_phase.sin() * amplitude * i16::MAX as f32) as i16;
                pcm.push(value);
                pcm.push(value);
                tone_phase += phase_step;
                if tone_phase > std::f32::consts::TAU {
                    tone_phase -= std::f32::consts::TAU;
                }
            }

            audio_bus.push_interleaved_stereo_i16(&pcm);
        }

        next_frame_at += frame_interval;
        let now = Instant::now();
        if next_frame_at > now {
            thread::sleep(next_frame_at - now);
        } else {
            next_frame_at = now;
        }
    }

    Ok(())
}
