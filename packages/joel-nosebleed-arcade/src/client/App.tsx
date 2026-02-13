import { FormEvent, useCallback, useEffect, useMemo, useRef, useState } from "react";

import { ArcadeSnapshot, MachineView, QueueEntryView, Side } from "./types";
import { decodeFramePacket, renderFrame } from "./video";

const MACHINE_ID = 1;
const VIEWER_ID_KEY = "joel.nosebleed.arcade.viewerId";
const PLAYER_NAME_KEY = "joel.nosebleed.arcade.playerName";

const BUTTON_TEMPLATE: Record<string, boolean> = {
  up: false,
  down: false,
  left: false,
  right: false,
  a: false,
  b: false,
  start: false,
  select: false
};

const KEY_TO_BUTTON: Record<string, keyof typeof BUTTON_TEMPLATE> = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
  x: "a",
  z: "b",
  Enter: "start",
  Shift: "select"
};

type InputState = "disconnected" | "connecting" | "connected";

export default function App() {
  const viewerId = useMemo(getOrCreateViewerId, []);
  const [playerName, setPlayerName] = useState<string>(() => {
    const stored = localStorage.getItem(PLAYER_NAME_KEY)?.trim();
    return stored && stored.length > 0 ? stored : "";
  });

  const [snapshot, setSnapshot] = useState<ArcadeSnapshot | null>(null);
  const [actionError, setActionError] = useState<string>("");
  const [frameMeta, setFrameMeta] = useState<string>("waiting for video frame");
  const [streamState, setStreamState] = useState<string>("connecting");
  const [inputState, setInputState] = useState<InputState>("disconnected");
  const [inputAckCount, setInputAckCount] = useState<number>(0);

  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const inputSocketRef = useRef<WebSocket | null>(null);
  const buttonsRef = useRef<Record<string, boolean>>({ ...BUTTON_TEMPLATE });
  const sequenceRef = useRef(1);
  const lastPayloadRef = useRef("");

  const machine = useMemo(() => {
    return snapshot?.machines.find((candidate) => candidate.id === MACHINE_ID) ?? null;
  }, [snapshot]);

  const mySeat = useMemo(() => {
    if (!machine) {
      return null;
    }
    if (machine.seats.left?.viewerId === viewerId) {
      return machine.seats.left;
    }
    if (machine.seats.right?.viewerId === viewerId) {
      return machine.seats.right;
    }
    return null;
  }, [machine, viewerId]);

  const myCalledTicket = useMemo(() => {
    if (!machine) {
      return null;
    }
    if (machine.called.left?.viewerId === viewerId) {
      return machine.called.left;
    }
    if (machine.called.right?.viewerId === viewerId) {
      return machine.called.right;
    }
    return null;
  }, [machine, viewerId]);

  const myQueueTickets = useMemo(() => {
    if (!machine) {
      return [];
    }
    return [...machine.queues.left, ...machine.queues.right].filter((entry) => entry.viewerId === viewerId);
  }, [machine, viewerId]);

  const videoStreamPath = machine?.streamPaths.video ?? null;
  const inputStreamPath = machine?.streamPaths.input ?? null;
  const seatActive = Boolean(mySeat);

  function sendInputPacket(force: boolean): void {
    const socket = inputSocketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      return;
    }

    const payload = {
      type: "input",
      sequence: sequenceRef.current,
      buttons: { ...buttonsRef.current },
      axes: {}
    };

    const serialized = JSON.stringify(payload);
    if (!force && serialized === lastPayloadRef.current) {
      return;
    }

    lastPayloadRef.current = serialized;
    sequenceRef.current += 1;
    socket.send(serialized);
  }

  const refreshState = useCallback(async () => {
    const response = await fetch("/api/arcade/state");
    if (!response.ok) {
      throw new Error(await response.text());
    }
    const data = (await response.json()) as ArcadeSnapshot;
    setSnapshot(data);
  }, []);

  useEffect(() => {
    void refreshState().catch((error: unknown) => {
      setActionError(error instanceof Error ? error.message : "failed to fetch arcade state");
    });

    const timer = window.setInterval(() => {
      void refreshState().catch(() => {
        setActionError("failed to refresh arcade state");
      });
    }, 1500);

    return () => {
      window.clearInterval(timer);
    };
  }, [refreshState]);

  useEffect(() => {
    localStorage.setItem(PLAYER_NAME_KEY, playerName);
  }, [playerName]);

  useEffect(() => {
    if (!videoStreamPath) {
      return;
    }

    const socket = new WebSocket(toWebSocketUrl(videoStreamPath));
    socket.binaryType = "arraybuffer";
    setStreamState("connecting");

    socket.onopen = () => {
      setStreamState("connected");
    };

    socket.onclose = () => {
      setStreamState("disconnected");
    };

    socket.onerror = () => {
      setStreamState("error");
    };

    socket.onmessage = (event) => {
      if (!(event.data instanceof ArrayBuffer)) {
        return;
      }
      const frame = decodeFramePacket(event.data);
      if (!frame || !canvasRef.current) {
        return;
      }
      const meta = renderFrame(canvasRef.current, frame);
      setFrameMeta(meta);
    };

    return () => {
      socket.close();
    };
  }, [videoStreamPath]);

  useEffect(() => {
    if (!inputStreamPath || !seatActive) {
      if (inputSocketRef.current) {
        inputSocketRef.current.close();
        inputSocketRef.current = null;
      }
      setInputState("disconnected");
      setInputAckCount(0);
      return;
    }

    const inputUrl = `${inputStreamPath}?viewerId=${encodeURIComponent(viewerId)}`;
    const socket = new WebSocket(toWebSocketUrl(inputUrl));
    inputSocketRef.current = socket;
    setInputState("connecting");
    setInputAckCount(0);
    lastPayloadRef.current = "";

    socket.onopen = () => {
      setInputState("connected");
      sendInputPacket(true);
    };

    socket.onclose = () => {
      setInputState("disconnected");
    };

    socket.onerror = () => {
      setInputState("disconnected");
    };

    socket.onmessage = (event) => {
      if (typeof event.data !== "string") {
        return;
      }
      try {
        const parsed = JSON.parse(event.data) as {
          type?: string;
          message?: string;
          sequence?: number;
          server_time_ms?: number;
        };
        if (parsed.type === "ack") {
          setInputAckCount((current) => current + 1);
        }
        if (parsed.type === "error" && parsed.message) {
          setActionError(parsed.message);
        }
      } catch {
        // Ignore non-JSON payloads from upstream ack channel.
      }
    };

    return () => {
      socket.close();
      inputSocketRef.current = null;
      setInputState("disconnected");
      setInputAckCount(0);
    };
  }, [inputStreamPath, seatActive, viewerId]);

  useEffect(() => {
    if (!seatActive) {
      return;
    }

    const handleKey = (pressed: boolean) => (event: KeyboardEvent) => {
      const key = event.key.length === 1 ? event.key.toLowerCase() : event.key;
      const mapped = KEY_TO_BUTTON[key];
      if (!mapped) {
        return;
      }
      event.preventDefault();
      buttonsRef.current[mapped] = pressed;
      sendInputPacket(false);
    };

    const onKeyDown = handleKey(true);
    const onKeyUp = handleKey(false);

    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("keyup", onKeyUp);

    const keepalive = window.setInterval(() => {
      sendInputPacket(true);
    }, 120);

    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("keyup", onKeyUp);
      window.clearInterval(keepalive);
      buttonsRef.current = { ...BUTTON_TEMPLATE };
    };
  }, [seatActive]);

  const joinQueue = async (side: Side) => {
    setActionError("");
    try {
      const response = await fetch("/api/arcade/queue/join", {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          machineId: MACHINE_ID,
          viewerId,
          playerName,
          side
        })
      });
      const body = await response.json();
      if (!response.ok) {
        throw new Error(body.error ?? "failed to join queue");
      }
      setSnapshot(body.state as ArcadeSnapshot);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : "failed to join queue");
    }
  };

  const leaveQueue = async (ticketId: number) => {
    setActionError("");
    try {
      const response = await fetch("/api/arcade/queue/leave", {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          machineId: MACHINE_ID,
          viewerId,
          ticketId
        })
      });
      const body = await response.json();
      if (!response.ok) {
        throw new Error(body.error ?? "failed to leave queue");
      }
      setSnapshot(body.state as ArcadeSnapshot);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : "failed to leave queue");
    }
  };

  const claimSeat = async (ticketId: number) => {
    setActionError("");
    try {
      const response = await fetch("/api/arcade/claim", {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          machineId: MACHINE_ID,
          viewerId,
          ticketId
        })
      });
      const body = await response.json();
      if (!response.ok) {
        throw new Error(body.error ?? "failed to claim seat");
      }
      setSnapshot(body.state as ArcadeSnapshot);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : "failed to claim seat");
    }
  };

  const leaveSeat = async () => {
    setActionError("");
    try {
      const response = await fetch("/api/arcade/seat/leave", {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          machineId: MACHINE_ID,
          viewerId
        })
      });
      const body = await response.json();
      if (!response.ok) {
        throw new Error(body.error ?? "failed to leave seat");
      }
      setSnapshot(body.state as ArcadeSnapshot);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : "failed to leave seat");
    }
  };

  const submitRound = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!machine) {
      return;
    }

    const form = event.currentTarget;
    const formData = new FormData(form);

    const winnerSide = (formData.get("winnerSide") as string | null) ?? "left";
    const leftScore = Number.parseInt((formData.get("leftScore") as string | null) ?? "0", 10);
    const rightScore = Number.parseInt((formData.get("rightScore") as string | null) ?? "0", 10);

    try {
      const response = await fetch("/api/arcade/round/end", {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({
          machineId: machine.id,
          winnerSide,
          leftScore,
          rightScore
        })
      });
      const body = await response.json();
      if (!response.ok) {
        throw new Error(body.error ?? "failed to end round");
      }
      setSnapshot(body.state as ArcadeSnapshot);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : "failed to end round");
    }
  };

  return (
    <div className="container-fluid p-3 p-md-4 arcade-root">
      <div className="row g-3 align-items-center mb-3">
        <div className="col-md-8">
          <h1 className="h3 mb-1">Joel Nosebleed Arcade</h1>
          <p className="text-secondary mb-0">
            Open spectating for everyone. Two active player seats. Queue by side and claim when called.
          </p>
        </div>
        <div className="col-md-4 text-md-end text-secondary small">
          <div>viewerId: {viewerId}</div>
          <div>updated: {snapshot ? new Date(snapshot.nowUnixMs).toLocaleTimeString() : "-"}</div>
        </div>
      </div>

      <div className="row g-3">
        <div className="col-lg-8">
          <div className="card shadow-sm">
            <div className="card-header d-flex justify-content-between align-items-center">
              <strong>{machine?.name ?? "Machine"}</strong>
              <span className="badge text-bg-dark">stream {streamState}</span>
            </div>
            <div className="card-body">
              <div className="ratio ratio-4x3 arcade-screen-wrap mb-2">
                <canvas ref={canvasRef} width={320} height={240} className="arcade-canvas" />
              </div>
              <div className="small text-secondary">{frameMeta}</div>
              <div className="small text-secondary mt-1">ROM: {machine?.romTitle ?? "-"}</div>
            </div>
          </div>

          <div className="card mt-3 shadow-sm">
            <div className="card-header">Match Control</div>
            <div className="card-body">
              <form className="row g-2 align-items-end" onSubmit={submitRound}>
                <div className="col-sm-4">
                  <label className="form-label">Winner</label>
                  <select className="form-select" name="winnerSide" defaultValue="left">
                    <option value="left">Left</option>
                    <option value="right">Right</option>
                  </select>
                </div>
                <div className="col-sm-3">
                  <label className="form-label">Left score</label>
                  <input className="form-control" type="number" min={0} name="leftScore" defaultValue={0} />
                </div>
                <div className="col-sm-3">
                  <label className="form-label">Right score</label>
                  <input className="form-control" type="number" min={0} name="rightScore" defaultValue={0} />
                </div>
                <div className="col-sm-2 d-grid">
                  <button className="btn btn-outline-primary" type="submit">
                    End round
                  </button>
                </div>
              </form>
              {machine?.lastRound ? (
                <div className="small text-secondary mt-2">
                  Last round #{machine.lastRound.roundId}: winner {machine.lastRound.winnerSide.toUpperCase()} ({machine.lastRound.leftScore}-
                  {machine.lastRound.rightScore})
                </div>
              ) : null}
            </div>
          </div>
        </div>

        <div className="col-lg-4">
          <div className="card shadow-sm mb-3">
            <div className="card-header">Player Setup</div>
            <div className="card-body">
              <label className="form-label">Player name</label>
              <input
                className="form-control mb-2"
                maxLength={24}
                value={playerName}
                onChange={(event) => setPlayerName(event.target.value)}
                placeholder="Type your tag"
              />
              <div className="d-grid gap-2 d-sm-flex">
                <button
                  type="button"
                  className="btn btn-primary flex-grow-1"
                  onClick={() => joinQueue("left")}
                  disabled={!playerName.trim()}
                >
                  Join Left Queue
                </button>
                <button
                  type="button"
                  className="btn btn-primary flex-grow-1"
                  onClick={() => joinQueue("right")}
                  disabled={!playerName.trim()}
                >
                  Join Right Queue
                </button>
              </div>

              {myCalledTicket ? (
                <button className="btn btn-success w-100 mt-2" onClick={() => claimSeat(myCalledTicket.ticketId)}>
                  Claim {myCalledTicket.side.toUpperCase()} Seat (#{myCalledTicket.ticketId})
                </button>
              ) : null}

              {mySeat ? (
                <>
                  <div className="alert alert-success mt-2 mb-2">
                    Controls active on <strong>{mySeat.side.toUpperCase()}</strong> (port {mySeat.port})
                    <div className="small mt-1">Input socket: {inputState}</div>
                    <div className="small mt-1">Input acks: {inputAckCount}</div>
                  </div>
                  <button className="btn btn-outline-danger w-100" onClick={leaveSeat}>
                    Leave Seat
                  </button>
                </>
              ) : (
                <div className="small text-secondary mt-2">Input controls unlock only when you hold a seat.</div>
              )}

              {myQueueTickets.length > 0 ? (
                <div className="mt-3">
                  <div className="fw-semibold small mb-1">Your queue tickets</div>
                  {myQueueTickets.map((ticket) => (
                    <div className="d-flex align-items-center justify-content-between border rounded p-2 mb-1" key={ticket.ticketId}>
                      <div className="small">
                        #{ticket.ticketId} on {findTicketSide(machine, ticket)} (pos {ticket.position})
                      </div>
                      <button className="btn btn-sm btn-outline-secondary" onClick={() => leaveQueue(ticket.ticketId)}>
                        leave
                      </button>
                    </div>
                  ))}
                </div>
              ) : null}
            </div>
          </div>

          <div className="card shadow-sm mb-3">
            <div className="card-header">Seats & Queue</div>
            <div className="card-body">
              <SeatRow label="Left" playerName={machine?.seats.left?.playerName} called={machine?.called.left ?? null} />
              <SeatRow label="Right" playerName={machine?.seats.right?.playerName} called={machine?.called.right ?? null} />

              <div className="row mt-2">
                <div className="col-6">
                  <QueueList title="Left Queue" items={machine?.queues.left ?? []} />
                </div>
                <div className="col-6">
                  <QueueList title="Right Queue" items={machine?.queues.right ?? []} />
                </div>
              </div>
            </div>
          </div>

          <div className="card shadow-sm">
            <div className="card-header">Daily Top Scores</div>
            <div className="card-body">
              <ol className="mb-0 ps-3">
                {(machine?.dailyTop ?? []).length === 0 ? (
                  <li className="text-secondary">No scores yet today.</li>
                ) : (
                  (machine?.dailyTop ?? []).map((entry, index) => (
                    <li key={`${entry.playerName}-${index}`}>
                      <strong>{entry.playerName}</strong> <span className="text-secondary">{entry.score}</span>
                    </li>
                  ))
                )}
              </ol>
            </div>
          </div>
        </div>
      </div>

      <div className="mt-3 small text-secondary">
        Keyboard controls when seated: arrows = movement, Z = B, X = A, Enter = Start, Shift = Select.
      </div>

      {actionError ? <div className="alert alert-danger mt-3 mb-0">{actionError}</div> : null}
    </div>
  );
}

function SeatRow({ label, playerName, called }: { label: string; playerName?: string; called: MachineView["called"]["left"] }) {
  return (
    <div className="border rounded p-2 mb-2">
      <div className="d-flex justify-content-between">
        <span className="fw-semibold">{label}</span>
        <span className="text-secondary">{playerName ?? "open"}</span>
      </div>
      {called ? <div className="small text-warning">calling #{called.ticketId} ({called.playerName})</div> : null}
    </div>
  );
}

function QueueList({ title, items }: { title: string; items: QueueEntryView[] }) {
  return (
    <>
      <div className="fw-semibold small mb-1">{title}</div>
      <ol className="small ps-3 mb-0">
        {items.length === 0 ? <li className="text-secondary">empty</li> : null}
        {items.map((entry) => (
          <li key={entry.ticketId}>
            {entry.playerName} <span className="text-secondary">#{entry.ticketId}</span>
          </li>
        ))}
      </ol>
    </>
  );
}

function toWebSocketUrl(path: string): string {
  const scheme = window.location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${window.location.host}${path}`;
}

function getOrCreateViewerId(): string {
  const existing = sessionStorage.getItem(VIEWER_ID_KEY);
  if (existing && existing.trim().length > 0) {
    return existing;
  }
  const value = `viewer-${Math.random().toString(36).slice(2, 10)}`;
  sessionStorage.setItem(VIEWER_ID_KEY, value);
  return value;
}

function findTicketSide(machine: MachineView | null, ticket: QueueEntryView): Side {
  if (!machine) {
    return "left";
  }
  if (machine.queues.right.some((entry) => entry.ticketId === ticket.ticketId)) {
    return "right";
  }
  return "left";
}
