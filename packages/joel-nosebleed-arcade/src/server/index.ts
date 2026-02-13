import { existsSync } from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

import express, { Response } from "express";

import { ArcadeState, Side } from "./arcadeState.js";
import { NosebleedRuntimeManager, RuntimeView } from "./runtimeManager.js";
import { attachWebSocketGateway } from "./websocketGateway.js";

const dirname = path.dirname(fileURLToPath(import.meta.url));
const packageRoot = path.resolve(dirname, "../..");
const repoRoot = process.env.NOSEBLEED_REPO_ROOT
  ? path.resolve(process.env.NOSEBLEED_REPO_ROOT)
  : path.resolve(packageRoot, "../..");

const machineCount = parsePositiveInt(process.env.ARCADE_MACHINE_COUNT, 1);
const basePort = parsePositiveInt(process.env.NOSEBLEED_BASE_PORT, 19080);
const defaultRomPath = path.join(repoRoot, "roms", "Joust.nes");
const legacyRomPath = path.join(repoRoot, "roms", "Joust (U) [!].nes");
const defaultCorePath = path.join(repoRoot, "test-core.so");
const romPath = process.env.NOSEBLEED_ROM_PATH
  ? path.resolve(process.env.NOSEBLEED_ROM_PATH)
  : existsSync(defaultRomPath)
    ? defaultRomPath
    : legacyRomPath;
const corePath = process.env.NOSEBLEED_CORE_PATH
  ? path.resolve(process.env.NOSEBLEED_CORE_PATH)
  : existsSync(defaultCorePath)
    ? defaultCorePath
    : undefined;
const nosebleedBin = process.env.NOSEBLEED_BIN ? path.resolve(process.env.NOSEBLEED_BIN) : undefined;

const arcadePort = parsePositiveInt(process.env.ARCADE_PORT, 4300);

const runtimeManager = new NosebleedRuntimeManager({
  repoRoot,
  machineCount,
  basePort,
  romPath: existsSync(romPath) ? romPath : undefined,
  corePath,
  nosebleedBin
});
const arcadeState = new ArcadeState(runtimeManager.machineMetas());

const app = express();
app.use(express.json({ limit: "256kb" }));

app.get("/api/arcade/state", (_request, response) => {
  response.json(snapshot());
});

app.post("/api/arcade/queue/join", (request, response) => {
  try {
    const machineId = readMachineId(request.body.machineId);
    const viewerId = readString(request.body.viewerId, "viewerId");
    const playerName = readString(request.body.playerName, "playerName");
    const side = readSide(request.body.side);

    const ticketId = arcadeState.joinQueue(machineId, viewerId, playerName, side);
    response.json({ ticketId, state: snapshot() });
  } catch (error) {
    badRequest(response, error);
  }
});

app.post("/api/arcade/queue/leave", (request, response) => {
  try {
    const machineId = readMachineId(request.body.machineId);
    const viewerId = readString(request.body.viewerId, "viewerId");
    const ticketId = readTicketId(request.body.ticketId);

    arcadeState.leaveQueue(machineId, viewerId, ticketId);
    response.json({ state: snapshot() });
  } catch (error) {
    badRequest(response, error);
  }
});

app.post("/api/arcade/claim", (request, response) => {
  try {
    const machineId = readMachineId(request.body.machineId);
    const viewerId = readString(request.body.viewerId, "viewerId");
    const ticketId = readTicketId(request.body.ticketId);

    arcadeState.claimSeat(machineId, viewerId, ticketId);
    response.json({ state: snapshot() });
  } catch (error) {
    badRequest(response, error);
  }
});

app.post("/api/arcade/seat/leave", (request, response) => {
  try {
    const machineId = readMachineId(request.body.machineId);
    const viewerId = readString(request.body.viewerId, "viewerId");

    arcadeState.leaveSeat(machineId, viewerId);
    response.json({ state: snapshot() });
  } catch (error) {
    badRequest(response, error);
  }
});

app.post("/api/arcade/round/end", (request, response) => {
  try {
    const machineId = readMachineId(request.body.machineId);
    const winnerSide = readSide(request.body.winnerSide);
    const leftScore = readScore(request.body.leftScore);
    const rightScore = readScore(request.body.rightScore);

    arcadeState.endRound(machineId, winnerSide, leftScore, rightScore);
    response.json({ state: snapshot() });
  } catch (error) {
    badRequest(response, error);
  }
});

const clientDist = path.join(packageRoot, "dist", "client");
if (existsSync(clientDist)) {
  app.use(express.static(clientDist));
  app.get("*", (request, response, next) => {
    if (request.path.startsWith("/api") || request.path.startsWith("/ws")) {
      next();
      return;
    }
    response.sendFile(path.join(clientDist, "index.html"));
  });
} else {
  app.get("/", (_request, response) => {
    response.type("text/plain").send(
      "joel-nosebleed-arcade backend is running. Start Vite in dev mode or build the client bundle."
    );
  });
}

const server = http.createServer(app);
attachWebSocketGateway(server, arcadeState, runtimeManager);

let shuttingDown = false;

async function start(): Promise<void> {
  await runtimeManager.startAll();

  server.listen(arcadePort, "0.0.0.0", () => {
    process.stdout.write(
      `[arcade] listening on http://0.0.0.0:${arcadePort} (repoRoot=${repoRoot}, machines=${machineCount})\n`
    );
  });
}

async function shutdown(signal: NodeJS.Signals): Promise<void> {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;

  process.stdout.write(`[arcade] received ${signal}, shutting down\n`);

  await new Promise<void>((resolve) => {
    server.close(() => {
      resolve();
    });
  });

  await runtimeManager.stopAll();
  process.exit(0);
}

process.on("SIGINT", () => {
  void shutdown("SIGINT");
});
process.on("SIGTERM", () => {
  void shutdown("SIGTERM");
});

void start().catch(async (error) => {
  process.stderr.write(`[arcade] startup failed: ${String(error)}\n`);
  await runtimeManager.stopAll();
  process.exit(1);
});

function snapshot() {
  const runtimeByMachine = new Map<number, RuntimeView>();
  for (const machine of runtimeManager.machineMetas()) {
    const runtime = runtimeManager.runtimeView(machine.id);
    if (runtime) {
      runtimeByMachine.set(machine.id, runtime);
    }
  }
  return arcadeState.snapshot(runtimeByMachine);
}

function readMachineId(raw: unknown): number {
  const value = Number.parseInt(String(raw ?? "1"), 10);
  if (!Number.isFinite(value) || value < 1) {
    throw new Error("machineId must be a positive integer");
  }
  return value;
}

function readTicketId(raw: unknown): number {
  const value = Number.parseInt(String(raw ?? "0"), 10);
  if (!Number.isFinite(value) || value < 1) {
    throw new Error("ticketId must be a positive integer");
  }
  return value;
}

function readScore(raw: unknown): number {
  const value = Number.parseInt(String(raw ?? "0"), 10);
  if (!Number.isFinite(value) || value < 0) {
    throw new Error("score must be zero or a positive integer");
  }
  return value;
}

function readString(raw: unknown, field: string): string {
  if (typeof raw !== "string" || raw.trim().length === 0) {
    throw new Error(`${field} is required`);
  }
  return raw;
}

function readSide(raw: unknown): Side {
  const value = String(raw ?? "").toLowerCase();
  if (value === "left" || value === "right") {
    return value;
  }
  throw new Error("side must be left or right");
}

function parsePositiveInt(raw: string | undefined, fallback: number): number {
  const value = Number.parseInt(String(raw ?? ""), 10);
  if (!Number.isFinite(value) || value < 1) {
    return fallback;
  }
  return value;
}

function badRequest(response: Response, error: unknown): void {
  const message = error instanceof Error ? error.message : "bad request";
  response.status(400).json({ error: message });
}
