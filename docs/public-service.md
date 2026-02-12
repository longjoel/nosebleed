# Public Service API

This document describes how external services and clients should consume `nosebleed` when used behind matchmaking for multiplayer.

## Service model

- One `nosebleed` process is expected per active match.
- Matchmaking allocates/starts the process, then mints signed connection tickets for players/spectators.
- Clients connect directly to this process over WebSocket or WebRTC.

## Startup (recommended)

```bash
cargo run -- \
  --listen 0.0.0.0:8080 \
  --core /path/to/core.so \
  --content /path/to/rom.nes \
  --require-auth \
  --auth-secret "<shared-secret>" \
  --reconnect-window-ms 15000
```

Flags:

- `--require-auth`: reject websocket connections without a valid token.
- `--auth-secret`: HMAC signing secret shared with matchmaking (can also come from `NOSEBLEED_AUTH_SECRET`).
- `--reconnect-window-ms`: lease hold window for disconnected player ports.

## Connection ticket

Token is supplied as query string: `?token=<ticket>`.

Supported token shape:

- `base64url(payload_json).base64url(hmac_sha256(payload_json, secret))`
- `v1.base64url(payload_json).base64url(hmac_sha256(payload_json, secret))`

Payload JSON schema:

```json
{
  "match_id": "match-123",
  "player_id": "player-a",
  "role": "player",
  "allowed_ports": [0],
  "exp_unix_ms": 1739999999000,
  "iat_unix_ms": 1739999900000
}
```

Fields:

- `match_id`: non-empty string.
- `player_id`: non-empty string.
- `role`: `player`, `spectator`, or `observer`.
- `allowed_ports`: required/non-empty for `player` role, each in `[0..7]`.
- `exp_unix_ms`: hard expiration time (milliseconds since Unix epoch).

## Endpoints

- `GET /healthz`
- `WS /ws/video?token=...`
- `WS /ws/audio?token=...`
- `WS /ws/input?token=...`
- `POST /webrtc/session?token=...`

When `--require-auth` is enabled, all WS endpoints and WebRTC signaling require a valid token.

## Input ownership and reconnect

- Player input is accepted only for ports listed in `allowed_ports`.
- A port can be owned by only one player at a time.
- If a player disconnects, their port lease is held for `reconnect_window_ms`.
- During the lease window, only the same `player_id` can reclaim that port.
- Other players are rejected from taking that port until the lease expires.

## Input protocol (`/ws/input` and WebRTC `input` DataChannel)

Client -> server:

```json
{
  "type": "input",
  "port": 0,
  "sequence": 42,
  "buttons": { "a": true, "start": false },
  "axes": { "lx": 0.1, "ly": -0.2 }
}
```

Server -> client ack:

```json
{
  "type": "ack",
  "sequence": 42,
  "server_time_ms": 1730000000012
}
```

Server -> client error example:

```json
{
  "type": "error",
  "message": "port 1 not assigned to this player"
}
```

## Media protocols

### Video (`/ws/video`)

Binary packet magic: `NBF0`.

Layout:

1. `magic[4]`
2. `sequence u64`
3. `server_timestamp_us u64`
4. `width u32`
5. `height u32`
6. `pitch u32`
7. `pixel_format u8` (`0` XRGB8888, `1` RGB565, `2` XRGB1555)
8. `payload_len u32`
9. `payload bytes`

### Audio (`/ws/audio`)

Binary packet magic: `NBA0`.

Layout:

1. `magic[4]`
2. `sequence u64`
3. `server_timestamp_us u64`
4. `sample_rate_hz u32`
5. `channels u8` (currently `2`)
6. `sample_format u8` (`0` = S16LE)
7. `frame_count u32`
8. `payload_len u32`
9. `payload bytes` (interleaved stereo PCM i16 LE)

## WebRTC signaling and channels

Signaling endpoint:

- `POST /webrtc/session?token=...`

Request:

```json
{
  "type": "offer",
  "sdp": "v=0...",
  "video_mode": "vp8"
}
```

Response:

```json
{
  "type": "answer",
  "sdp": "v=0..."
}
```

Expected DataChannels:

- `video`: binary chunk stream carrying `NBV1` VP8 packets when `video_mode=vp8`; otherwise `NBF0`.
- `audio`: binary chunk stream carrying `NBA0` packets.
- `input`: UTF-8 JSON input messages + `ack/error` responses.

`video_mode` values:

- `vp8`: attempt VP8 encoding via ffmpeg and emit `NBV1` packets.
- `raw`: emit raw `NBF0` packets.

If VP8 encoder startup fails, server falls back to raw `NBF0`.

`NBV1` packet layout:

1. `magic[4]` (`NBV1`)
2. `pts_us u64`
3. `duration_us u32`
4. `flags u8` (bit0 = keyframe)
5. `payload_len u32`
6. `payload bytes` (VP8 frame)

Chunk wire format (`NBC1`) used on `video`/`audio` channels:

1. `magic[4]` (`NBC1`)
2. `message_id u32` (little endian)
3. `chunk_index u16` (little endian)
4. `total_chunks u16` (little endian)
5. `payload bytes`

## Client flow

1. Obtain ticket from matchmaking.
2. Connect using either:
   - WebSocket: `/ws/video`, `/ws/audio`, `/ws/input`, or
   - WebRTC: `POST /webrtc/session`, then open `video/audio/input` channels.
3. Start sending input frames to assigned `port`.
4. If disconnected, reconnect with a new ticket for the same `player_id` before lease expiry.

## Browser Gamepad API

When using a browser client (`navigator.getGamepads()`):

- Poll gamepad state continuously (for example every animation frame).
- Send `input` updates immediately on state change.
- Also send heartbeat input updates even when unchanged (at least every 250 ms).
- Use standard mapping indices:
  - Buttons `0..15` for face/D-pad/shoulders/start/select/stick-click.
  - Axes `0..3` for LX/LY/RX/RY.
  - Trigger analog values from button `6` and `7` `value`.

Browser connection checklist:

1. Pair the controller in the OS first (USB or Bluetooth).
2. Open the client over `https://` (or `http://localhost`) and keep the tab focused.
3. Press any gamepad button once after the page is open.
4. Send input to a valid `port`.
5. Verify UI status lights:
   - `PAD`: controller detected by browser
   - `MOVE`: controller state changes are being read
   - `TX`: input packets are being sent
   - `ACK`: server acknowledgements are arriving

Non-standard mapping workflow (recommended on Linux when mappings are wrong):

1. Open `Gamepad Debug`.
2. Start `Bind Wizard` and follow each prompt.
3. Use `Skip Step` for controls your device does not expose.
4. Mapping is saved per browser device profile in `localStorage`.
5. Use `Clear Saved Map` to reset to default mapping.

## Operational notes

- Keep the auth secret out of clients; only matchmaking signs tickets.
- Keep ticket TTL short (for example 30-120 seconds).
- Current WebRTC mode is DataChannel packet transport; codec RTP tracks are still the next optimization step.
- WebRTC VP8 encoding knobs:
  - `NOSEBLEED_FFMPEG_BIN` (default `ffmpeg`)
  - `NOSEBLEED_WEBRTC_VIDEO_ENCODER` (default `libvpx`; set VP8-capable hardware encoder per host)
  - `NOSEBLEED_WEBRTC_VIDEO_ENCODER_ARGS` (optional extra ffmpeg args for hardware device/filter setup)
  - `NOSEBLEED_WEBRTC_VIDEO_BITRATE_KBPS` (default `2500`)
