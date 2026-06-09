use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::frame::VideoFrame;
use crate::input::{InputBinary, InputUpdate};

pub const FRAME_MAGIC: &[u8; 4] = b"NBF0";
pub const FRAME_HEADER_LEN: usize = 4 + 8 + 8 + 4 + 4 + 4 + 1 + 4 + 4;
pub const AUDIO_PACKET_MAGIC: &[u8; 4] = b"NBA0";
pub const AUDIO_SAMPLE_FORMAT_S16LE: u8 = 0;
pub const AUDIO_PACKET_HEADER_LEN: usize = 4 + 8 + 8 + 4 + 1 + 1 + 4 + 4;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientCommand {
    Reset,
    InsertCoin,
    SaveState,
    LoadState,
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
        slot: Option<u8>,
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

/// Decode a 34-byte binary input payload into a `ClientMessage::Input`.
///
/// This path avoids JSON parsing entirely for the latency-critical
/// input loop. Commands (save, load, reset, ping) still use the JSON
/// path via `parse_client_message`.
pub fn decode_input_binary(raw: &[u8]) -> Result<ClientMessage> {
    let bin = InputBinary::from_bytes(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "binary input must be exactly {} bytes, got {}",
            crate::input::INPUT_BINARY_SIZE,
            raw.len()
        )
    })?;
    let update = bin.to_input_update();
    Ok(ClientMessage::Input {
        port: bin.port,
        sequence: Some(bin.sequence as u64),
        update,
    })
}

pub fn serialize_server_message(message: &ServerMessage) -> Result<String> {
    serde_json::to_string(message).context("failed to serialize server message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_client_command_supports_save_and_load_state_slots() {
        let save_message = parse_client_message(
            r#"{"type":"command","command":"save_state","port":0,"slot":3,"sequence":12}"#,
        )
        .expect("save state command should parse");
        match save_message {
            ClientMessage::Command {
                command,
                port,
                slot,
                sequence,
            } => {
                assert_eq!(command, ClientCommand::SaveState);
                assert_eq!(port, 0);
                assert_eq!(slot, Some(3));
                assert_eq!(sequence, Some(12));
            }
            _ => panic!("expected command message"),
        }

        let load_message = parse_client_message(
            r#"{"type":"command","command":"load_state","port":0,"slot":5,"sequence":13}"#,
        )
        .expect("load state command should parse");
        match load_message {
            ClientMessage::Command {
                command,
                port,
                slot,
                sequence,
            } => {
                assert_eq!(command, ClientCommand::LoadState);
                assert_eq!(port, 0);
                assert_eq!(slot, Some(5));
                assert_eq!(sequence, Some(13));
            }
            _ => panic!("expected command message"),
        }
    }
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
    out.extend_from_slice(&frame.pixel_aspect_ratio.to_bits().to_le_bytes());
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

    let mut out = Vec::with_capacity(AUDIO_PACKET_HEADER_LEN + payload_len as usize);
    out.extend_from_slice(AUDIO_PACKET_MAGIC);
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
