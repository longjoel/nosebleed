# @nosebleed/joel-nosebleed-arcade

Standalone arcade web application (Node + Express + TypeScript + React + Bootstrap) that orchestrates one or more `nosebleed` runtime instances.

## What it does

- Starts `nosebleed` machine processes (default: 1 machine).
- Hosts a React website where anyone can spectate the machine stream.
- Enforces two active player seats (`left` -> port `0`, `right` -> port `1`).
- Handles queue join/leave/claim flow for each side.
- Forwards player input through the arcade backend to the assigned machine instance.

## Run in dev

From repo root:

```bash
pnpm --filter @nosebleed/joel-nosebleed-arcade dev
```

- React UI (Vite): `http://127.0.0.1:5174`
- Arcade API + WebSocket gateway: `http://127.0.0.1:4300`

## Build and run

```bash
pnpm --filter @nosebleed/joel-nosebleed-arcade build
pnpm --filter @nosebleed/joel-nosebleed-arcade start
```

## Environment variables

- `ARCADE_PORT` (default `4300`)
- `ARCADE_MACHINE_COUNT` (default `1`)
- `NOSEBLEED_BASE_PORT` (default `19080`)
- `NOSEBLEED_REPO_ROOT` (default auto-detected)
- `NOSEBLEED_BIN` (default: `target/release/nosebleed` if present, else `cargo run -p nosebleed --`)
- `NOSEBLEED_CORE_PATH` (default: `test-core.so` if present, otherwise unset)
- `NOSEBLEED_ROM_PATH` (default: `roms/Joust.nes`, fallback: `roms/Joust (U) [!].nes`)

If `NOSEBLEED_CORE_PATH` is not set, `nosebleed` runs in mock mode. The ROM path is still passed for convenience when a core is configured.
