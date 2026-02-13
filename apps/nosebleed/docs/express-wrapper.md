# Express Wrapper Pattern

This pattern keeps `nosebleed` as the media/input runtime while your Node/Express service handles session lifecycle and token issuance.

## Recommended flow

1. Matchmaker decides `matchId`, players, and emulator assets.
2. Express launches one `nosebleed` process for that match with `--config`.
3. Express mints short-lived player/spectator tokens.
4. Browser receives URLs with `?token=...` and connects directly to `nosebleed`.

## Minimal middleware + routes

```js
import crypto from "node:crypto";
import express from "express";
import { spawn } from "node:child_process";
import path from "node:path";
import fs from "node:fs/promises";

const app = express();
app.use(express.json());

const NOSEBLEED_BIN = path.resolve("./target/debug/nosebleed");
const AUTH_SECRET = process.env.NOSEBLEED_AUTH_SECRET;

function base64url(input) {
  return Buffer.from(input).toString("base64url");
}

function signTicket(payload, secret) {
  const payloadJson = JSON.stringify(payload);
  const signature = crypto.createHmac("sha256", secret).update(payloadJson).digest("base64url");
  return `${base64url(payloadJson)}.${signature}`;
}

app.post("/matches/:matchId/start", async (req, res, next) => {
  try {
    const { matchId } = req.params;
    const port = Number(req.body.port || 8080);

    const configPath = path.resolve(`./runtime/${matchId}.config.json`);
    await fs.mkdir(path.dirname(configPath), { recursive: true });
    await fs.writeFile(
      configPath,
      JSON.stringify(
        {
          listen: `0.0.0.0:${port}`,
          core: req.body.corePath,
          content: req.body.contentPath,
          require_auth: true,
          reconnect_window_ms: 15000,
          session: {
            root_dir: "./target/sessions",
            id: matchId,
            copy_content: true
          }
        },
        null,
        2
      )
    );

    const child = spawn(NOSEBLEED_BIN, ["--config", configPath], {
      env: { ...process.env, NOSEBLEED_AUTH_SECRET: AUTH_SECRET },
      stdio: "inherit"
    });

    // Persist child.pid + host/port in your own match registry.
    res.json({ matchId, pid: child.pid, host: req.hostname, port });
  } catch (err) {
    next(err);
  }
});

app.post("/matches/:matchId/ticket", (req, res) => {
  const { matchId } = req.params;
  const { playerId, role = "player", allowedPorts = [0], ttlMs = 60_000, host, port } = req.body;

  const now = Date.now();
  const payload = {
    match_id: matchId,
    player_id: playerId,
    role,
    allowed_ports: allowedPorts,
    iat_unix_ms: now,
    exp_unix_ms: now + ttlMs
  };

  const token = signTicket(payload, AUTH_SECRET);
  const base = `http://${host}:${port}`;

  res.json({
    token,
    endpoints: {
      video_ws: `ws://${host}:${port}/ws/video?token=${encodeURIComponent(token)}`,
      audio_ws: `ws://${host}:${port}/ws/audio?token=${encodeURIComponent(token)}`,
      input_ws: `ws://${host}:${port}/ws/input?token=${encodeURIComponent(token)}`,
      webrtc_session: `${base}/webrtc/session?token=${encodeURIComponent(token)}`
    }
  });
});
```

## Notes

- Keep `NOSEBLEED_AUTH_SECRET` only on the server side.
- Keep ticket TTL short (30-120s).
- For scale, keep one `nosebleed` process per match/session and register it in your service discovery layer.

## Alternative: Long-Lived Worker

You can run one persistent `nosebleed` process and call control endpoints instead of spawning processes in Node:

- `POST http://nosebleed-host:8080/session/start`
- `POST http://nosebleed-host:8080/session/stop`
- `GET http://nosebleed-host:8080/session/status`

This shifts process lifecycle into `nosebleed` itself and keeps Express focused on matchmaking + token issuance.
