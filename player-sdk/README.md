# @nosebleed/player-sdk

Browser TypeScript player library for consuming `nosebleed` video/audio/input streams.

## Install

```bash
cd player-sdk
npm install
npm run build
```

## Basic usage

```ts
import { NosebleedPlayer } from "@nosebleed/player-sdk";

const canvas = document.getElementById("screen") as HTMLCanvasElement;

const player = new NosebleedPlayer({
  baseUrl: "https://game-host.example.com",
  token: "<signed-ticket>",
  canvas,
  transport: "auto",
  enableAudio: true,
  onStatus: (s) => console.log("status", s),
  onError: (e) => console.error(e)
});

await player.enableAudio();
await player.connect();

player.sendInput({
  buttons: { a: true, start: false },
  axes: { lx: 0, ly: 0 }
});
```

## Kiosk preset

```ts
import { NosebleedPlayer } from "@nosebleed/player-sdk";

const player = new NosebleedPlayer(
  NosebleedPlayer.kioskPreset({
    baseUrl: "https://kiosk-node.local",
    token: "<ticket>",
    canvas: document.querySelector("#screen") as HTMLCanvasElement
  })
);

await player.connect();
```

Kiosk preset defaults:

- Uses WebSocket transport
- Keeps audio disabled
- Auto-reconnect enabled with 1000ms delay

## Dedicated instance preset

```ts
import { NosebleedPlayer } from "@nosebleed/player-sdk";

const player = new NosebleedPlayer(
  NosebleedPlayer.dedicatedPreset({
    baseUrl: "https://dedicated-node.example.com",
    token: "<ticket>",
    canvas: document.querySelector("#screen") as HTMLCanvasElement
  })
);

await player.enableAudio();
await player.connect();
```

## Control methods

- `connect(): Promise<void>`
- `disconnect(): void`
- `enableAudio(): Promise<void>`
- `sendInput(state, port?): void`

## Compatibility notes

- WebSocket path uses raw `NBF0` video and `NBA0` audio packets.
- WebRTC mode uses DataChannels (`video`, `audio`, `input`) and supports `NBV1` VP8 decode via WebCodecs when available.
