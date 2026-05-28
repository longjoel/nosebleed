use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::frame::VideoFrame;
use crate::input::InputUpdate;

const FRAME_MAGIC: &[u8; 4] = b"NBF0";
const FRAME_HEADER_LEN: usize = 4 + 8 + 8 + 4 + 4 + 4 + 1 + 4;
const AUDIO_MAGIC: &[u8; 4] = b"NBA0";
const AUDIO_SAMPLE_FORMAT_S16LE: u8 = 0;
const AUDIO_HEADER_LEN: usize = 4 + 8 + 8 + 4 + 1 + 1 + 4 + 4;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientCommand {
    Reset,
    InsertCoin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Input {
        #[serde(default)]
        port: u32,
        #[serde(default)]
        sequence: Option<u64>,
        #[serde(flatten)]
        update: InputUpdate,
    },
    Command {
        command: ClientCommand,
        #[serde(default)]
        port: u32,
        #[serde(default)]
        sequence: Option<u64>,
    },
    Ping {
        #[serde(default)]
        client_time_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Ack {
        sequence: Option<u64>,
        server_time_ms: u64,
    },
    Error {
        message: String,
    },
}

pub fn parse_client_message(raw: &str) -> Result<ClientMessage> {
    serde_json::from_str(raw).context("invalid JSON message")
}

pub fn serialize_server_message(message: &ServerMessage) -> Result<String> {
    serde_json::to_string(message).context("failed to serialize server message")
}

pub fn encode_frame_packet(frame: &VideoFrame) -> Arc<[u8]> {
    let timestamp_us = now_unix_micros();
    let payload_len = frame.data.len() as u32;
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + frame.data.len());

    out.extend_from_slice(FRAME_MAGIC);
    out.extend_from_slice(&frame.sequence.to_le_bytes());
    out.extend_from_slice(&timestamp_us.to_le_bytes());
    out.extend_from_slice(&frame.width.to_le_bytes());
    out.extend_from_slice(&frame.height.to_le_bytes());
    out.extend_from_slice(&(frame.pitch as u32).to_le_bytes());
    out.push(frame.pixel_format.as_u8());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(frame.data.as_ref());

    Arc::<[u8]>::from(out)
}

pub fn encode_audio_packet(
    sequence: u64,
    sample_rate_hz: u32,
    channels: u8,
    interleaved_i16: &[i16],
) -> Arc<[u8]> {
    let safe_channels = channels.max(1);
    let sample_count = interleaved_i16.len() - (interleaved_i16.len() % safe_channels as usize);
    let frame_count = (sample_count / safe_channels as usize) as u32;
    let payload_len = (sample_count * std::mem::size_of::<i16>()) as u32;

    let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + payload_len as usize);
    out.extend_from_slice(AUDIO_MAGIC);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&now_unix_micros().to_le_bytes());
    out.extend_from_slice(&sample_rate_hz.to_le_bytes());
    out.push(safe_channels);
    out.push(AUDIO_SAMPLE_FORMAT_S16LE);
    out.extend_from_slice(&frame_count.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());

    for sample in interleaved_i16.iter().take(sample_count) {
        out.extend_from_slice(&sample.to_le_bytes());
    }

    Arc::<[u8]>::from(out)
}

pub fn now_unix_ms() -> u64 {
    let now = SystemTime::now();
    let Ok(since_epoch) = now.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    since_epoch.as_millis() as u64
}

fn now_unix_micros() -> u64 {
    let now = SystemTime::now();
    let Ok(since_epoch) = now.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    since_epoch.as_micros() as u64
}
