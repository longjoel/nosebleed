# nosebleed

`nosebleed` is a web-first libretro runtime for running emulator cores server-side and streaming low-latency video, audio, and realtime controller input to browsers.

It is designed for projects that want to host emulation sessions as a service: one runtime process per match/session, signed player tickets, browser clients, and direct WebSocket/WebRTC transport.

## Why it exists

Most emulator frontends assume the player owns the machine running the emulator. `nosebleed` flips that around:

- native libretro core runs on the host/server
- browser receives frames/audio
- browser sends virtual gamepad input
- orchestration layer assigns players to controller ports
- signed tickets enforce which client may control which port

That makes it useful for:

- web arcades
- LAN game libraries
- couch co-op over browser clients
- emulator kiosks
- preservation/demo environments
- applications that need an embeddable libretro streaming backend

## Repository layout

- `apps/nosebleed` — Rust runtime crate and CLI binary.
- `packages/player-sdk` — browser TypeScript SDK for consuming streams.
- `packages/joel-nosebleed-arcade` — example/experimental arcade frontend.
- `docs` — architecture and product notes.
- `scripts` — repo-level helper scripts.

## Install prerequisites

- Rust stable
- Node.js with Corepack / pnpm
- A native libretro core (`.so` on Linux) for real games

```bash
corepack enable
corepack prepare pnpm@10.4.1 --activate
pnpm install
```

## Run the runtime

Mock mode, no emulator core required:

```bash
cargo run -p nosebleed -- --listen 127.0.0.1:8080
```

With a libretro core and ROM/content file:

```bash
cargo run -p nosebleed -- \
  --listen 127.0.0.1:8080 \
  --core /path/to/core.so \
  --content /path/to/game.rom
```

Open:

```text
http://127.0.0.1:8080/
```

## Build

```bash
pnpm build
```

Or build pieces independently:

```bash
pnpm build:app      # Rust runtime binary
pnpm build:sdk      # TypeScript player SDK
pnpm build:arcade   # example arcade frontend
```

## Browser SDK

```ts
import { NosebleedPlayer } from "@nosebleed/player-sdk";

const player = new NosebleedPlayer({
  baseUrl: "http://127.0.0.1:8080",
  canvas: document.querySelector("#screen") as HTMLCanvasElement,
  token: "<optional-signed-ticket>",
  transport: "auto",
  enableAudio: true,
  defaultPort: 0
});

await player.enableAudio();
await player.connect();

player.sendInput({
  buttons: { a: true, start: false },
  axes: { lx: 0, ly: 0 }
});
```

See `packages/player-sdk/README.md` for more SDK examples.

## Public service model

For multiplayer/orchestrated sessions, run one `nosebleed` process per match and mint signed tickets for clients:

```bash
NOSEBLEED_AUTH_SECRET="shared-secret" cargo run -p nosebleed -- \
  --listen 0.0.0.0:8080 \
  --core /path/to/core.so \
  --content /path/to/game.rom \
  --require-auth
```

Client tokens specify:

- `match_id`
- `player_id`
- `role`: `player`, `spectator`, or `observer`
- `allowed_ports`: controller ports the player may control
- `exp_unix_ms`: hard expiry

See `apps/nosebleed/docs/public-service.md` for the full protocol and auth model.

## Runtime crate

The Rust crate exposes a library target as well as the CLI binary. Hosts can import server/session/input/protocol primitives from the crate, or simply spawn the CLI process.

```toml
[dependencies]
nosebleed = "0.1"
```

## Status

Early public release. The runtime is functional, but APIs and packet formats may still change before `1.0`.

## License

MIT. See `LICENSE`.
