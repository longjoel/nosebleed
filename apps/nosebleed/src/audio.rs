use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::protocol::encode_audio_packet;

const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const CHUNK_FRAMES: usize = 512;
const CHANNELS: u8 = 2;

#[derive(Debug, Default)]
struct AudioState {
    pending_samples: Vec<i16>,
    next_sequence: u64,
}

#[derive(Debug)]
pub struct AudioBus {
    tx: broadcast::Sender<Arc<[u8]>>,
    sample_rate_hz: AtomicU32,
    state: Mutex<AudioState>,
}

impl AudioBus {
    pub fn new(default_sample_rate_hz: u32, queue_capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(queue_capacity.max(8));
        Self {
            tx,
            sample_rate_hz: AtomicU32::new(sanitize_sample_rate(default_sample_rate_hz)),
            state: Mutex::new(AudioState::default()),
        }
    }

    pub fn sender(&self) -> broadcast::Sender<Arc<[u8]>> {
        self.tx.clone()
    }

    pub fn set_sample_rate_hz(&self, sample_rate_hz: u32) {
        self.sample_rate_hz
            .store(sanitize_sample_rate(sample_rate_hz), Ordering::Relaxed);
    }

    pub fn push_interleaved_stereo_i16(&self, samples: &[i16]) {
        let aligned_len = samples.len() - (samples.len() % CHANNELS as usize);
        if aligned_len == 0 {
            return;
        }

        let chunk_samples = CHUNK_FRAMES * CHANNELS as usize;
        let mut packets = Vec::new();

        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            guard
                .pending_samples
                .extend_from_slice(&samples[..aligned_len]);

            while guard.pending_samples.len() >= chunk_samples {
                let tail = guard.pending_samples.split_off(chunk_samples);
                let chunk = std::mem::replace(&mut guard.pending_samples, tail);

                let sequence = guard.next_sequence;
                guard.next_sequence = guard.next_sequence.wrapping_add(1);
                packets.push((sequence, chunk));
            }
        }

        if packets.is_empty() {
            return;
        }

        let sample_rate_hz = self.sample_rate_hz.load(Ordering::Relaxed);
        for (sequence, chunk) in packets {
            let packet = encode_audio_packet(sequence, sample_rate_hz, CHANNELS, &chunk);
            let _ = self.tx.send(packet);
        }
    }
}

impl Default for AudioBus {
    fn default() -> Self {
        Self::new(DEFAULT_SAMPLE_RATE, 256)
    }
}

fn sanitize_sample_rate(sample_rate_hz: u32) -> u32 {
    if (8_000..=384_000).contains(&sample_rate_hz) {
        sample_rate_hz
    } else {
        DEFAULT_SAMPLE_RATE
    }
}
