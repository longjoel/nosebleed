use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Xrgb8888,
    Rgb565,
    Xrgb1555,
}

impl Default for PixelFormat {
    fn default() -> Self {
        Self::Xrgb8888
    }
}

impl PixelFormat {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Xrgb8888 => 0,
            Self::Rgb565 => 1,
            Self::Xrgb1555 => 2,
        }
    }

    pub fn from_libretro(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Xrgb1555),
            1 => Some(Self::Xrgb8888),
            2 => Some(Self::Rgb565),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: usize,
    pub pixel_format: PixelFormat,
    pub pixel_aspect_ratio: f32,
    pub data: Arc<[u8]>,
}

#[derive(Debug)]
struct LatestFrameState {
    next_sequence: u64,
    latest: Option<VideoFrame>,
}

#[derive(Debug)]
pub struct LatestFrameStore {
    state: Mutex<LatestFrameState>,
    available: Condvar,
}

impl Default for LatestFrameStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(LatestFrameState {
                next_sequence: 0,
                latest: None,
            }),
            available: Condvar::new(),
        }
    }
}

impl LatestFrameStore {
    pub fn publish(
        &self,
        width: u32,
        height: u32,
        pitch: usize,
        pixel_format: PixelFormat,
        pixel_aspect_ratio: f32,
        bytes: &[u8],
    ) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);
        let sequence = guard.next_sequence;
        guard.next_sequence = guard.next_sequence.wrapping_add(1);

        guard.latest = Some(VideoFrame {
            sequence,
            width,
            height,
            pitch,
            pixel_format,
            pixel_aspect_ratio,
            data: Arc::<[u8]>::from(bytes),
        });

        self.available.notify_all();
    }

    pub fn wait_for_next(
        &self,
        last_sequence: Option<u64>,
        timeout: Duration,
    ) -> Option<VideoFrame> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(crate::lock_recover);

        if let Some(frame) = guard.latest.as_ref() {
            if last_sequence.is_none_or(|last| frame.sequence > last) {
                return Some(frame.clone());
            }
        }

        let (guard_after_wait, _) = self
            .available
            .wait_timeout(guard, timeout)
            .unwrap_or_else(crate::lock_recover);
        guard = guard_after_wait;

        guard.latest.as_ref().and_then(|frame| {
            if last_sequence.is_none_or(|last| frame.sequence > last) {
                Some(frame.clone())
            } else {
                None
            }
        })
    }
}
