use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const STALE_TIMEOUT: Duration = Duration::from_millis(300);
const TRANSIENT_PULSE_DURATION: Duration = Duration::from_millis(100);

pub const MAX_PORTS: u32 = 8;
pub const JOYPAD_BUTTON_COUNT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    B,
    Y,
    Select,
    Start,
    Up,
    Down,
    Left,
    Right,
    A,
    X,
    L,
    R,
    L2,
    R2,
    L3,
    R3,
}

impl Button {
    pub fn retro_id(self) -> u32 {
        match self {
            Self::B => 0,
            Self::Y => 1,
            Self::Select => 2,
            Self::Start => 3,
            Self::Up => 4,
            Self::Down => 5,
            Self::Left => 6,
            Self::Right => 7,
            Self::A => 8,
            Self::X => 9,
            Self::L => 10,
            Self::R => 11,
            Self::L2 => 12,
            Self::R2 => 13,
            Self::L3 => 14,
            Self::R3 => 15,
        }
    }

    pub fn from_retro_id(id: u32) -> Option<Self> {
        match id {
            0 => Some(Self::B),
            1 => Some(Self::Y),
            2 => Some(Self::Select),
            3 => Some(Self::Start),
            4 => Some(Self::Up),
            5 => Some(Self::Down),
            6 => Some(Self::Left),
            7 => Some(Self::Right),
            8 => Some(Self::A),
            9 => Some(Self::X),
            10 => Some(Self::L),
            11 => Some(Self::R),
            12 => Some(Self::L2),
            13 => Some(Self::R2),
            14 => Some(Self::L3),
            15 => Some(Self::R3),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Lx,
    Ly,
    Rx,
    Ry,
    L2,
    R2,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputUpdate {
    #[serde(default)]
    pub buttons: HashMap<Button, bool>,
    #[serde(default)]
    pub axes: HashMap<Axis, f32>,
}

/// Fixed-size binary encoding for the high-frequency input path.
///
/// 34 bytes total — no allocation, no serialization overhead.
///
/// Layout (little-endian):
/// ```text
/// [0..1]   sequence: u16     (0 = no sequence)
/// [2..5]   port: u32
/// [6..9]   buttons: u32      (bitmask, 16 buttons → 16 bits used)
/// [10..13] lx: f32
/// [14..17] ly: f32
/// [18..21] rx: f32
/// [22..25] ry: f32
/// [26..29] lt: f32
/// [30..33] rt: f32
/// ```
pub const INPUT_BINARY_SIZE: usize = 34;

#[derive(Debug, Clone, Copy)]
pub struct InputBinary {
    pub sequence: u16,
    pub port: u32,
    pub buttons: u32,
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    pub lt: f32,
    pub rt: f32,
}

impl InputBinary {
    /// Decode from the 34-byte wire format.
    /// Returns `None` if the slice is the wrong size.
    pub fn from_bytes(raw: &[u8]) -> Option<Self> {
        if raw.len() < INPUT_BINARY_SIZE {
            return None;
        }
        Some(Self {
            sequence: u16::from_le_bytes([raw[0], raw[1]]),
            port: u32::from_le_bytes([raw[2], raw[3], raw[4], raw[5]]),
            buttons: u32::from_le_bytes([raw[6], raw[7], raw[8], raw[9]]),
            lx: f32::from_le_bytes([raw[10], raw[11], raw[12], raw[13]]),
            ly: f32::from_le_bytes([raw[14], raw[15], raw[16], raw[17]]),
            rx: f32::from_le_bytes([raw[18], raw[19], raw[20], raw[21]]),
            ry: f32::from_le_bytes([raw[22], raw[23], raw[24], raw[25]]),
            lt: f32::from_le_bytes([raw[26], raw[27], raw[28], raw[29]]),
            rt: f32::from_le_bytes([raw[30], raw[31], raw[32], raw[33]]),
        })
    }

    /// Encode to a 34-byte `Vec` (useful for relay / future use).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(INPUT_BINARY_SIZE);
        buf.extend_from_slice(&self.sequence.to_le_bytes());
        buf.extend_from_slice(&self.port.to_le_bytes());
        buf.extend_from_slice(&self.buttons.to_le_bytes());
        buf.extend_from_slice(&self.lx.to_le_bytes());
        buf.extend_from_slice(&self.ly.to_le_bytes());
        buf.extend_from_slice(&self.rx.to_le_bytes());
        buf.extend_from_slice(&self.ry.to_le_bytes());
        buf.extend_from_slice(&self.lt.to_le_bytes());
        buf.extend_from_slice(&self.rt.to_le_bytes());
        buf
    }

    /// Convert to the canonical `InputUpdate` HashMap representation.
    pub fn to_input_update(&self) -> InputUpdate {
        let mut buttons = HashMap::new();
        for (retro_id, name) in [
            (0, Button::B),
            (1, Button::Y),
            (2, Button::Select),
            (3, Button::Start),
            (4, Button::Up),
            (5, Button::Down),
            (6, Button::Left),
            (7, Button::Right),
            (8, Button::A),
            (9, Button::X),
            (10, Button::L),
            (11, Button::R),
            (12, Button::L2),
            (13, Button::R2),
            (14, Button::L3),
            (15, Button::R3),
        ] {
            buttons.insert(name, (self.buttons >> retro_id) & 1 == 1);
        }

        let mut axes = HashMap::new();
        axes.insert(Axis::Lx, self.lx);
        axes.insert(Axis::Ly, self.ly);
        axes.insert(Axis::Rx, self.rx);
        axes.insert(Axis::Ry, self.ry);
        axes.insert(Axis::L2, self.lt);
        axes.insert(Axis::R2, self.rt);

        InputUpdate { buttons, axes }
    }
}

#[derive(Debug, Clone)]
struct SourceState {
    buttons: [bool; JOYPAD_BUTTON_COUNT],
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    l2: i16,
    r2: i16,
    updated_at: Instant,
}

impl Default for SourceState {
    fn default() -> Self {
        Self {
            buttons: [false; JOYPAD_BUTTON_COUNT],
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            l2: 0,
            r2: 0,
            updated_at: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MergedState {
    buttons: [bool; JOYPAD_BUTTON_COUNT],
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    l2: i16,
    r2: i16,
}

impl Default for MergedState {
    fn default() -> Self {
        Self {
            buttons: [false; JOYPAD_BUTTON_COUNT],
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            l2: 0,
            r2: 0,
        }
    }
}

#[derive(Debug, Default)]
struct InputState {
    per_port: HashMap<u32, HashMap<String, SourceState>>,
    transient_buttons: HashMap<u32, TransientPortState>,
}

#[derive(Debug, Clone)]
struct TransientPortState {
    buttons: [Option<Instant>; JOYPAD_BUTTON_COUNT],
}

impl Default for TransientPortState {
    fn default() -> Self {
        Self {
            buttons: [None; JOYPAD_BUTTON_COUNT],
        }
    }
}

#[derive(Debug, Default)]
pub struct InputHub {
    state: RwLock<InputState>,
}

impl InputHub {
    pub fn apply_update(&self, port: u32, source: &str, update: &InputUpdate) {
        if port >= MAX_PORTS {
            return;
        }

        let mut guard = self
            .state
            .write()
            .unwrap_or_else(crate::lock_recover);
        let source_map = guard.per_port.entry(port).or_default();
        let entry = source_map.entry(source.to_owned()).or_default();

        for (button, pressed) in &update.buttons {
            let idx = button.retro_id() as usize;
            entry.buttons[idx] = *pressed;
        }

        for (axis, value) in &update.axes {
            let clamped = clamp_axis(*value);
            match axis {
                Axis::Lx => entry.lx = clamped,
                Axis::Ly => entry.ly = clamped,
                Axis::Rx => entry.rx = clamped,
                Axis::Ry => entry.ry = clamped,
                Axis::L2 => entry.l2 = clamped,
                Axis::R2 => entry.r2 = clamped,
            }
        }

        entry.updated_at = Instant::now();
    }

    pub fn pulse_button(&self, port: u32, source: &str, button: Button) {
        if port >= MAX_PORTS {
            return;
        }

        let mut guard = self
            .state
            .write()
            .unwrap_or_else(crate::lock_recover);
        let source_map = guard.per_port.entry(port).or_default();
        let entry = source_map.entry(source.to_owned()).or_default();
        entry.updated_at = Instant::now();

        let transient = guard.transient_buttons.entry(port).or_default();
        transient.buttons[button.retro_id() as usize] =
            Some(Instant::now() + TRANSIENT_PULSE_DURATION);
    }

    pub fn remove_source(&self, source: &str) {
        let mut guard = self
            .state
            .write()
            .unwrap_or_else(crate::lock_recover);
        let mut empty_ports = Vec::new();

        for (port, map) in &mut guard.per_port {
            map.remove(source);
            if map.is_empty() {
                empty_ports.push(*port);
            }
        }

        for port in empty_ports {
            guard.per_port.remove(&port);
        }
    }

    pub fn joypad_button_state(&self, port: u32, button_id: u32) -> i16 {
        let Some(button) = Button::from_retro_id(button_id) else {
            return 0;
        };
        let merged = self.merged_for_port(port);
        if merged.buttons[button.retro_id() as usize] {
            1
        } else {
            0
        }
    }

    pub fn analog_state(&self, port: u32, index: u32, id: u32) -> i16 {
        let merged = self.merged_for_port(port);
        match (index, id) {
            (0, 0) => merged.lx,
            (0, 1) => merged.ly,
            (1, 0) => merged.rx,
            (1, 1) => merged.ry,
            _ => 0,
        }
    }

    fn merged_for_port(&self, port: u32) -> MergedState {
        if port >= MAX_PORTS {
            return MergedState::default();
        }

        let now = Instant::now();
        let mut merged = MergedState::default();
        let guard = self
            .state
            .read()
            .unwrap_or_else(crate::lock_recover);

        let Some(sources) = guard.per_port.get(&port) else {
            return merged;
        };

        for state in sources.values() {
            if now.duration_since(state.updated_at) > STALE_TIMEOUT {
                continue;
            }

            for (idx, pressed) in state.buttons.iter().enumerate() {
                merged.buttons[idx] |= *pressed;
            }

            merged.lx = dominant_axis(merged.lx, state.lx);
            merged.ly = dominant_axis(merged.ly, state.ly);
            merged.rx = dominant_axis(merged.rx, state.rx);
            merged.ry = dominant_axis(merged.ry, state.ry);
            merged.l2 = dominant_axis(merged.l2, state.l2);
            merged.r2 = dominant_axis(merged.r2, state.r2);
        }

        if let Some(transient) = guard.transient_buttons.get(&port) {
            for (idx, deadline) in transient.buttons.iter().enumerate() {
                if deadline.is_some_and(|deadline| deadline > now) {
                    merged.buttons[idx] = true;
                }
            }
        }

        // Expose analog trigger activity as digital shoulder presses.
        if merged.l2 > i16::MAX / 4 {
            merged.buttons[Button::L2.retro_id() as usize] = true;
        }
        if merged.r2 > i16::MAX / 4 {
            merged.buttons[Button::R2.retro_id() as usize] = true;
        }

        merged
    }
}

fn dominant_axis(existing: i16, candidate: i16) -> i16 {
    if candidate.abs() > existing.abs() {
        candidate
    } else {
        existing
    }
}

fn clamp_axis(value: f32) -> i16 {
    let clamped = value.clamp(-1.0, 1.0);
    if clamped >= 0.0 {
        (clamped * i16::MAX as f32).round() as i16
    } else {
        (clamped * -(i16::MIN as f32)).round() as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_button_survives_followup_input_update_from_same_source() {
        let hub = InputHub::default();
        let port = 0;
        let source = "player-1";

        let mut released = InputUpdate::default();
        released.buttons.insert(Button::Select, false);

        hub.apply_update(port, source, &released);
        hub.pulse_button(port, source, Button::Select);
        hub.apply_update(port, source, &released);

        assert_eq!(hub.joypad_button_state(port, Button::Select.retro_id()), 1);
    }

    #[test]
    fn pulse_button_expires_after_short_duration() {
        let hub = InputHub::default();
        let port = 0;

        hub.pulse_button(port, "player-1", Button::Select);
        assert_eq!(hub.joypad_button_state(port, Button::Select.retro_id()), 1);

        std::thread::sleep(TRANSIENT_PULSE_DURATION + Duration::from_millis(30));

        assert_eq!(hub.joypad_button_state(port, Button::Select.retro_id()), 0);
    }
}
