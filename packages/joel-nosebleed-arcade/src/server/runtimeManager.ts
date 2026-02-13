import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import http from "node:http";
import path from "node:path";

export type RuntimeStatus = "starting" | "online" | "error" | "stopped";

export interface MachineMeta {
  id: number;
  name: string;
  romTitle: string;
}

export interface RuntimeView {
  status: RuntimeStatus;
  listenPort: number;
  lastError?: string;
}

interface RuntimeRecord {
  meta: MachineMeta;
  listenPort: number;
  status: RuntimeStatus;
  lastError?: string;
  process: ReturnType<typeof spawn> | null;
}

interface RuntimeManagerConfig {
  repoRoot: string;
  machineCount: number;
  basePort: number;
  romPath?: string;
  corePath?: string;
  nosebleedBin?: string;
}

export class NosebleedRuntimeManager {
  private readonly repoRoot: string;
  private readonly romPath?: string;
  private readonly corePath?: string;
  private readonly explicitBinary?: string;
  private readonly records = new Map<number, RuntimeRecord>();

  constructor(config: RuntimeManagerConfig) {
    this.repoRoot = config.repoRoot;
    this.romPath = config.romPath;
    this.corePath = config.corePath;
    this.explicitBinary = config.nosebleedBin;

    for (let id = 1; id <= Math.max(1, config.machineCount); id += 1) {
      this.records.set(id, {
        meta: {
          id,
          name: id === 1 ? "Joust NES Cabinet" : `Joust NES Cabinet ${id}`,
          romTitle: "Joust.nes"
        },
        listenPort: config.basePort + id - 1,
        status: "stopped",
        process: null
      });
    }
  }

  machineMetas(): MachineMeta[] {
    return Array.from(this.records.values())
      .map((record) => record.meta)
      .sort((a, b) => a.id - b.id);
  }

  runtimeView(machineId: number): RuntimeView | undefined {
    const record = this.records.get(machineId);
    if (!record) {
      return undefined;
    }

    return {
      status: record.status,
      listenPort: record.listenPort,
      lastError: record.lastError
    };
  }

  upstreamWsUrl(machineId: number, channel: "video" | "audio" | "input"): string {
    const record = this.records.get(machineId);
    if (!record) {
      throw new Error(`machine ${machineId} not configured`);
    }
    return `ws://127.0.0.1:${record.listenPort}/ws/${channel}`;
  }

  async startAll(): Promise<void> {
    for (const record of this.records.values()) {
      await this.startRecord(record);
    }
  }

  async stopAll(): Promise<void> {
    await Promise.all(Array.from(this.records.values()).map((record) => this.stopRecord(record)));
  }

  private async startRecord(record: RuntimeRecord): Promise<void> {
    if (record.process) {
      return;
    }

    record.status = "starting";
    record.lastError = undefined;

    const { command, args } = this.buildLaunchCommand(record.listenPort);
    const child = spawn(command, args, {
      cwd: this.repoRoot,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"]
    });

    record.process = child;

    const prefix = `[nosebleed:${record.meta.id}] `;
    child.stdout.on("data", (chunk: Buffer) => {
      process.stdout.write(`${prefix}${chunk.toString()}`);
    });
    child.stderr.on("data", (chunk: Buffer) => {
      process.stderr.write(`${prefix}${chunk.toString()}`);
    });

    child.once("exit", (code, signal) => {
      record.process = null;
      if (record.status !== "stopped") {
        record.status = "error";
        record.lastError = `nosebleed exited (code=${code ?? "null"}, signal=${signal ?? "null"})`;
      }
    });

    child.once("error", (error) => {
      record.process = null;
      record.status = "error";
      record.lastError = `failed to launch nosebleed: ${error.message}`;
    });

    const ready = await waitForHealthz(record.listenPort, 12_000);
    if (!ready) {
      record.status = "error";
      record.lastError = "runtime health check timed out";
      return;
    }

    record.status = "online";
  }

  private async stopRecord(record: RuntimeRecord): Promise<void> {
    if (!record.process) {
      record.status = "stopped";
      return;
    }

    const child = record.process;
    record.status = "stopped";
    record.process = null;

    await new Promise<void>((resolve) => {
      if (child.exitCode !== null || child.signalCode !== null) {
        resolve();
        return;
      }

      const timeout = setTimeout(() => {
        child.kill("SIGKILL");
      }, 2000);

      child.once("exit", () => {
        clearTimeout(timeout);
        resolve();
      });

      child.kill("SIGTERM");
    });
  }

  private buildLaunchCommand(listenPort: number): { command: string; args: string[] } {
    const releaseBinary = path.join(this.repoRoot, "target", "release", "nosebleed");
    const command = this.explicitBinary ?? (existsSync(releaseBinary) ? releaseBinary : "cargo");

    const args: string[] = [];
    if (command === "cargo") {
      args.push("run", "-p", "nosebleed", "--");
    }

    args.push("--listen", `127.0.0.1:${listenPort}`);

    if (this.corePath) {
      args.push("--core", this.corePath);
    }
    if (this.romPath) {
      args.push("--content", this.romPath);
    }

    return { command, args };
  }
}

async function waitForHealthz(listenPort: number, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await probeHealthz(listenPort)) {
      return true;
    }
    await sleep(250);
  }
  return false;
}

function probeHealthz(listenPort: number): Promise<boolean> {
  return new Promise((resolve) => {
    const request = http.get(
      {
        host: "127.0.0.1",
        port: listenPort,
        path: "/healthz",
        timeout: 700
      },
      (response) => {
        response.resume();
        resolve(response.statusCode === 200);
      }
    );

    request.on("timeout", () => {
      request.destroy();
      resolve(false);
    });

    request.on("error", () => {
      resolve(false);
    });
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}
