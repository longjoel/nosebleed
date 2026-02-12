# Public Service API

This document describes how external services and clients should consume `nosebleed` when used behind matchmaking for multiplayer.

## Service model

- One `nosebleed` process is expected per active match.
- Matchmaking allocates/starts the process, then mints signed connection tickets for players/spectators.
- Clients connect directly to this process over WebSocket.

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

When `--require-auth` is enabled, all WS endpoints require a valid token.

## Input ownership and reconnect

- Player input is accepted only for ports listed in `allowed_ports`.
- A port can be owned by only one player at a time.
- If a player disconnects, their port lease is held for `reconnect_window_ms`.
- During the lease window, only the same `player_id` can reclaim that port.
- Other players are rejected from taking that port until the lease expires.

## Input protocol (`/ws/input`)

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

## Client flow

1. Obtain ticket from matchmaking.
2. Connect to `/ws/video`, `/ws/audio`, `/ws/input` with `?token=...`.
3. Start sending input frames to assigned `port`.
4. If disconnected, reconnect with a new ticket for the same `player_id` before lease expiry.

## Operational notes

- Keep the auth secret out of clients; only matchmaking signs tickets.
- Keep ticket TTL short (for example 30-120 seconds).
- For internet play, plan migration to WebRTC for media; keep `/ws/input` for control.
