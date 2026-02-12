# nosebleed

Web-first runtime for libretro (RetroArch core) execution with low-latency streaming and realtime virtual gamepad input over WebSockets.

Consumer integration guide: `docs/public-service.md`.

## What this provides

- Runs a libretro core (`.so`) and repeatedly calls `retro_run`.
- Captures frames from `retro_video_refresh_t` callback.
- Streams the newest frame over WebSocket (`/ws/video`) as a compact binary packet.
- Streams stereo PCM audio over WebSocket (`/ws/audio`) in low-latency chunks.
- Accepts gamepad input from one or more clients over WebSocket (`/ws/input`) as JSON.
- Merges multiple virtual controllers per emulated port with stale-input pruning.
- Includes a browser probe UI at `/` for quick testing.

When `--core` is omitted, a mock core generates synthetic video so transport/input can be tested in isolation.

## Run

```bash
cargo run -- --listen 0.0.0.0:8080
```

Run with a libretro core:

```bash
cargo run -- --listen 0.0.0.0:8080 --core /path/to/core.so --content /path/to/rom.bin
```

Useful optional args:

- `--fps 60`
- `--width 320` (mock mode only)
- `--height 240` (mock mode only)

## WebSocket API

### Video stream: `/ws/video`

- Transport: binary WebSocket messages
- Semantics: latest-frame only (old frames are dropped under load)

Packet layout (`NBF0`):

1. `magic` (`[u8; 4]`) = `NBF0`
2. `sequence` (`u64`, little endian)
3. `server_timestamp_us` (`u64`, little endian)
4. `width` (`u32`, little endian)
5. `height` (`u32`, little endian)
6. `pitch` (`u32`, little endian)
7. `pixel_format` (`u8`) where:
   - `0` = XRGB8888
   - `1` = RGB565
   - `2` = XRGB1555
8. `payload_len` (`u32`, little endian)
9. `payload` (`[u8; payload_len]`)

### Input stream: `/ws/input`

- Transport: text JSON (binary UTF-8 JSON is also accepted)
- Direction: client -> server for control, server -> client for ack/errors

Input message:

```json
{
  "type": "input",
  "port": 0,
  "sequence": 42,
  "buttons": {
    "a": true,
    "b": false,
    "start": true,
    "up": false
  },
  "axes": {
    "lx": 0.2,
    "ly": -0.4,
    "rx": 0.0,
    "ry": 0.0,
    "l2": 0.0,
    "r2": 0.0
  }
}
```

Ping message:

```json
{ "type": "ping", "client_time_ms": 1730000000000 }
```

Server ack:

```json
{
  "type": "ack",
  "sequence": 42,
  "server_time_ms": 1730000000012
}
```

### Audio stream: `/ws/audio`

- Transport: binary WebSocket messages
- Encoding: interleaved stereo PCM S16LE (`i16`, little endian)

Packet layout (`NBA0`):

1. `magic` (`[u8; 4]`) = `NBA0`
2. `sequence` (`u64`, little endian)
3. `server_timestamp_us` (`u64`, little endian)
4. `sample_rate_hz` (`u32`, little endian)
5. `channels` (`u8`) currently `2`
6. `sample_format` (`u8`) currently `0` for S16LE
7. `frame_count` (`u32`, little endian)
8. `payload_len` (`u32`, little endian)
9. `payload` (`[u8; payload_len]`) interleaved PCM frames

## Notes for low latency

- The pipeline intentionally favors dropping old frames over queueing.
- Keep `permessage-deflate` off for video channels.
- For WAN use, packetize compressed video (H.264/AV1) instead of raw frames.
- If you want browser-native hardware decode with lower bandwidth, evolve this to WebRTC and keep WebSocket for control.

## Current limits

- Video is raw frame payload over WebSocket (high bandwidth).
- Environment callback support is minimal (pixel format negotiation only).
- Audio is uncompressed PCM over WebSocket (works, but bandwidth-heavy).

## WebRTC path

Yes, WebRTC is a good next step. The current split (`/ws/input` control + media packetization) maps cleanly to:

- WebRTC media tracks for audio/video (browser jitter buffer + hardware decode paths)
- DataChannel or existing WebSocket for input/control
- Optional SFU integration for multi-viewer scaling

These are straightforward extension points in `src/libretro.rs` and `src/server.rs`.
