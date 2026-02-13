import http from "node:http";

import WebSocket, { RawData, WebSocketServer } from "ws";

import { ArcadeState } from "./arcadeState.js";
import { NosebleedRuntimeManager } from "./runtimeManager.js";

export function attachWebSocketGateway(
  server: http.Server,
  arcadeState: ArcadeState,
  runtimeManager: NosebleedRuntimeManager
): void {
  const wsServer = new WebSocketServer({ noServer: true });

  server.on("upgrade", (request, socket, head) => {
    const host = request.headers.host;
    if (!host || !request.url) {
      socket.destroy();
      return;
    }

    const url = new URL(request.url, `http://${host}`);
    if (!url.pathname.startsWith("/ws/machines/")) {
      socket.destroy();
      return;
    }

    wsServer.handleUpgrade(request, socket, head, (client) => {
      routeGatewayConnection(client, url, arcadeState, runtimeManager);
    });
  });
}

function routeGatewayConnection(
  client: WebSocket,
  url: URL,
  arcadeState: ArcadeState,
  runtimeManager: NosebleedRuntimeManager
): void {
  try {
    const streamMatch = /^\/ws\/machines\/(\d+)\/(video|audio)$/.exec(url.pathname);
    if (streamMatch) {
      const machineId = Number.parseInt(streamMatch[1], 10);
      const channel = streamMatch[2] as "video" | "audio";
      const upstreamUrl = runtimeManager.upstreamWsUrl(machineId, channel);
      pipeSocket(client, new WebSocket(upstreamUrl));
      return;
    }

    const inputMatch = /^\/ws\/machines\/(\d+)\/input$/.exec(url.pathname);
    if (inputMatch) {
      const machineId = Number.parseInt(inputMatch[1], 10);
      const viewerId = url.searchParams.get("viewerId")?.trim() ?? "";
      if (!viewerId) {
        sendErrorAndClose(client, "viewerId is required for input");
        return;
      }

      const upstreamUrl = runtimeManager.upstreamWsUrl(machineId, "input");
      const upstream = new WebSocket(upstreamUrl);

      upstream.on("open", () => {
        if (client.readyState === WebSocket.OPEN) {
          client.send(JSON.stringify({ type: "ack", message: "input connected" }));
        }
      });

      upstream.on("message", (data, isBinary) => {
        if (client.readyState !== WebSocket.OPEN) {
          return;
        }
        client.send(data, { binary: isBinary });
      });

      upstream.on("close", () => {
        if (client.readyState === WebSocket.OPEN) {
          client.close(1011, "machine input upstream closed");
        }
      });

      upstream.on("error", () => {
        sendErrorAndClose(client, "machine input upstream error");
      });

      client.on("message", (raw) => {
        if (upstream.readyState !== WebSocket.OPEN) {
          return;
        }

        const port = arcadeState.viewerPort(machineId, viewerId);
        if (port === null) {
          sendErrorAndClose(client, "viewer no longer controls a seat");
          upstream.close();
          return;
        }

        const text = toUtf8(raw);
        if (!text) {
          sendErrorAndClose(client, "input payload must be UTF-8 JSON");
          upstream.close();
          return;
        }

        let parsed: Record<string, unknown>;
        try {
          parsed = JSON.parse(text) as Record<string, unknown>;
        } catch {
          sendErrorAndClose(client, "invalid JSON payload");
          upstream.close();
          return;
        }

        if (parsed.type === "input") {
          parsed.port = port;
        }

        upstream.send(JSON.stringify(parsed));
      });

      client.on("close", () => {
        if (upstream.readyState === WebSocket.OPEN || upstream.readyState === WebSocket.CONNECTING) {
          upstream.close();
        }
      });

      client.on("error", () => {
        if (upstream.readyState === WebSocket.OPEN || upstream.readyState === WebSocket.CONNECTING) {
          upstream.close();
        }
      });

      return;
    }

    sendErrorAndClose(client, "unsupported websocket route");
  } catch (error) {
    const message = error instanceof Error ? error.message : "websocket gateway error";
    sendErrorAndClose(client, message);
  }
}

function pipeSocket(client: WebSocket, upstream: WebSocket): void {
  upstream.on("message", (data, isBinary) => {
    if (client.readyState !== WebSocket.OPEN) {
      return;
    }
    client.send(data, { binary: isBinary });
  });

  upstream.on("close", () => {
    if (client.readyState === WebSocket.OPEN) {
      client.close(1000, "upstream closed");
    }
  });

  upstream.on("error", () => {
    sendErrorAndClose(client, "stream upstream error");
  });

  client.on("close", () => {
    if (upstream.readyState === WebSocket.OPEN || upstream.readyState === WebSocket.CONNECTING) {
      upstream.close();
    }
  });

  client.on("error", () => {
    if (upstream.readyState === WebSocket.OPEN || upstream.readyState === WebSocket.CONNECTING) {
      upstream.close();
    }
  });
}

function toUtf8(raw: RawData): string | null {
  if (typeof raw === "string") {
    return raw;
  }
  if (Buffer.isBuffer(raw)) {
    return raw.toString("utf8");
  }
  if (raw instanceof ArrayBuffer) {
    return Buffer.from(raw).toString("utf8");
  }
  if (Array.isArray(raw) && raw.every((item) => Buffer.isBuffer(item))) {
    return Buffer.concat(raw).toString("utf8");
  }
  return null;
}

function sendErrorAndClose(client: WebSocket, message: string): void {
  if (client.readyState === WebSocket.OPEN) {
    client.send(JSON.stringify({ type: "error", message }));
  }
  if (client.readyState !== WebSocket.CLOSED && client.readyState !== WebSocket.CLOSING) {
    client.close(1008, message.slice(0, 80));
  }
}
