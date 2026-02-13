# nosebleed

Web-first runtime for libretro (RetroArch core) execution with low-latency streaming and realtime virtual gamepad input over WebSockets.

Commands in this file assume your working directory is `apps/nosebleed`.
For standard build/launch from repo root, use `pnpm` scripts in `../../package.json`.

Consumer integration guide: `docs/public-service.md`.
Express integration pattern: `docs/express-wrapper.md`.
TypeScript player SDK: `../../packages/player-sdk/`.
Virtual arcade blueprint: `../../docs/virtual-arcade-blueprint.md`.
Virtual arcade schematic: `../../docs/virtual-arcade-schematic.md`.

## What this provides

- Runs a libretro core (`.so`) and repeatedly calls `retro_run`.
- Captures frames from `retro_video_refresh_t` callback.
- Streams the newest frame over WebSocket (`/ws/video`) as a compact binary packet.
- Streams stereo PCM audio over WebSocket (`/ws/audio`) in low-latency chunks.
- Accepts gamepad input from one or more clients over WebSocket (`/ws/input`) as JSON.
- Merges multiple virtual controllers per emulated port with stale-input pruning.
- Includes a browser probe UI at `/` with keyboard + Gamepad API input.
- Supports optional WebRTC DataChannel transport via `/webrtc/session` (signaling over HTTP).

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

Run from a JSON config file:

```bash
cargo run -- --config ./nosebleed.config.json.example
```

Config precedence: CLI flags override values from `--config`.

Session workspace options:

- `--session-root /path/to/sessions`
- `--session-id match-123`
- `--session-copy-core`
- `--session-copy-content`

When session root is set, a per-session directory is created and a `session.json` manifest is written there. If copy flags are enabled, core/content files are copied into that directory and runtime uses the copied paths.


## Control API

Runtime session control endpoints are available over HTTP:

- `GET /session/status`
- `POST /session/start`
- `POST /session/stop`

Example start payload:

```json
{
  "core": "../../test-core.so",
  "content": "../../test-rom.nes",
  "force_restart": true,
  "workspace": {
    "root_dir": "./target/sessions",
    "id": "match-123",
    "copy_content": true
  }
}
```

If `core` is omitted, runtime starts in mock mode.

## Virtual Arcade API (MVP)

- `GET /api/arcade/overview`
- `GET /api/arcade/machines/:id`
- `POST /api/arcade/machines/:id/queue/join`
- `POST /api/arcade/machines/:id/queue/leave`
- `POST /api/arcade/machines/:id/claim`
- `POST /api/arcade/machines/:id/round/end`

## Browser gamepad quick start

1. Pair the controller in your OS (USB or Bluetooth).
2. Open `http://localhost:8080/` (or HTTPS in deployed environments).
3. Keep the tab focused and press any controller button once.
4. Select the desired input `Port` in the UI.
5. Check Input lights:
   - `PAD` means a controller is detected.
   - `MOVE` blinks when controller state changes.
   - `TX` blinks when input packets are sent.
   - `ACK` blinks when server acknowledgements arrive.
6. If buttons/axes are wrong on Linux or non-standard pads, open `Gamepad Debug` and run `Start Bind Wizard` once per device profile.
7. To test WebRTC transport in the probe, open `http://localhost:8080/?transport=webrtc`.

Virtual arcade MVP UI:

- `http://localhost:8080/arcade`

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

### WebRTC signaling: `POST /webrtc/session`

- Transport: HTTP JSON request/response (offer/answer exchange)
- Auth: same token query parameter model as WebSocket routes (`?token=...`)
- DataChannels expected by this service:
  - `video` (binary chunks of `NBV1` VP8 packets when `video_mode=vp8`, otherwise `NBF0`)
  - `audio` (binary chunks of `NBA0` packets)
  - `input` (JSON control + ack/error, same schema as `/ws/input`)

Request body:

```json
{
  "type": "offer",
  "sdp": "v=0...",
  "video_mode": "vp8"
}
```

`video_mode` is optional:

- `vp8`: server attempts VP8 encode (`NBV1` packets over `video` DataChannel), falls back to `NBF0` raw packets if encoder startup fails.
- `raw`: always send `NBF0` packets.

`NBV1` payload layout:

1. `magic` (`[u8; 4]`) = `NBV1`
2. `pts_us` (`u64`, little endian)
3. `duration_us` (`u32`, little endian)
4. `flags` (`u8`) bit0 = keyframe
5. `payload_len` (`u32`, little endian)
6. `payload` (VP8 frame bytes)

Encoder configuration (environment variables):

- `NOSEBLEED_FFMPEG_BIN` (default `ffmpeg`)
- `NOSEBLEED_WEBRTC_VIDEO_ENCODER` (default `libvpx`; set to a VP8-capable hardware ffmpeg encoder for your host)
- `NOSEBLEED_WEBRTC_VIDEO_ENCODER_ARGS` (optional extra ffmpeg args, split on spaces; useful for hardware encoder setup like device/filter flags)
- `NOSEBLEED_WEBRTC_VIDEO_BITRATE_KBPS` (default `2500`)

Response body:

```json
{
  "type": "answer",
  "sdp": "v=0..."
}
```

## Notes for low latency

- The pipeline intentionally favors dropping old frames over queueing.
- Keep `permessage-deflate` off for video channels.
- WebRTC DataChannel video can run in VP8 mode (`NBV1`) to cut bandwidth versus raw frames.
- For WAN scale, evolve media to codec RTP tracks (H.264/AV1 + Opus).

## Current limits

- WebSocket video is raw frame payload (`NBF0`) and therefore high-bandwidth.
- WebRTC video supports VP8 packet mode (`NBV1`) but does not yet use RTP media tracks.
- Environment callback support is minimal (pixel format negotiation only).
- Audio is uncompressed PCM over WebSocket (works, but bandwidth-heavy).

## WebRTC path

Current implementation is DataChannel-first so existing packet formats and decoders continue to work.
Next phase is codec-based media tracks + SFU compatibility.
