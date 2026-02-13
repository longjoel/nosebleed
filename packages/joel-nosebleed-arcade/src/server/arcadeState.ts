import { MachineMeta, RuntimeView } from "./runtimeManager.js";

export type Side = "left" | "right";

export interface QueueEntryView {
  ticketId: number;
  viewerId: string;
  playerName: string;
  joinedUnixMs: number;
  position: number;
}

export interface SeatCallView {
  side: Side;
  ticketId: number;
  viewerId: string;
  playerName: string;
  calledUnixMs: number;
}

export interface SeatPlayerView {
  side: Side;
  port: number;
  viewerId: string;
  playerName: string;
  assignedUnixMs: number;
}

export interface RoundView {
  roundId: number;
  winnerSide: Side;
  leftScore: number;
  rightScore: number;
  endedUnixMs: number;
}

export interface DailyScoreView {
  playerName: string;
  score: number;
}

export interface MachineView {
  id: number;
  name: string;
  romTitle: string;
  status: "free_play" | "seat_call" | "match_live";
  runtime: RuntimeView | null;
  seats: {
    left: SeatPlayerView | null;
    right: SeatPlayerView | null;
  };
  called: {
    left: SeatCallView | null;
    right: SeatCallView | null;
  };
  queues: {
    left: QueueEntryView[];
    right: QueueEntryView[];
  };
  lastRound: RoundView | null;
  dailyTop: DailyScoreView[];
  streamPaths: {
    video: string;
    audio: string;
    input: string;
  };
}

export interface ArcadeSnapshot {
  nowUnixMs: number;
  machines: MachineView[];
  dailyTop: DailyScoreView[];
}

interface QueueEntry {
  ticketId: number;
  viewerId: string;
  playerName: string;
  side: Side;
  joinedUnixMs: number;
}

interface SeatCall {
  ticketId: number;
  viewerId: string;
  playerName: string;
  calledUnixMs: number;
}

interface SeatPlayer {
  viewerId: string;
  playerName: string;
  assignedUnixMs: number;
}

interface RoundRecord {
  roundId: number;
  winnerSide: Side;
  leftScore: number;
  rightScore: number;
  endedUnixMs: number;
}

interface ScoreRecord {
  playerName: string;
  score: number;
}

interface MachineRecord {
  meta: MachineMeta;
  leftQueue: QueueEntry[];
  rightQueue: QueueEntry[];
  leftPlayer: SeatPlayer | null;
  rightPlayer: SeatPlayer | null;
  calledLeft: SeatCall | null;
  calledRight: SeatCall | null;
  lastRound: RoundRecord | null;
}

export class ArcadeState {
  private readonly machines = new Map<number, MachineRecord>();
  private readonly dailyMachineScores = new Map<string, ScoreRecord>();
  private readonly dailyGlobalScores = new Map<string, ScoreRecord>();
  private nextTicketId = 1;
  private nextRoundId = 1;

  constructor(machineMetas: MachineMeta[]) {
    for (const meta of machineMetas) {
      this.machines.set(meta.id, {
        meta,
        leftQueue: [],
        rightQueue: [],
        leftPlayer: null,
        rightPlayer: null,
        calledLeft: null,
        calledRight: null,
        lastRound: null
      });
    }
  }

  snapshot(runtimeByMachine: Map<number, RuntimeView>): ArcadeSnapshot {
    const now = nowUnixMs();
    const day = dayKey(now);

    const machines = Array.from(this.machines.values())
      .sort((a, b) => a.meta.id - b.meta.id)
      .map((machine) => this.machineView(machine, runtimeByMachine.get(machine.meta.id) ?? null, day));

    return {
      nowUnixMs: now,
      machines,
      dailyTop: this.collectGlobalTop(day)
    };
  }

  joinQueue(machineId: number, viewerId: string, playerName: string, side: Side): number {
    const machine = this.getMachine(machineId);
    const normalizedName = sanitizePlayerName(playerName);
    const normalizedViewer = sanitizeViewerId(viewerId);

    if (this.viewerIsPresent(machine, normalizedViewer)) {
      throw new Error("viewer is already seated or queued");
    }

    const ticketId = this.nextTicketId;
    this.nextTicketId += 1;

    const entry: QueueEntry = {
      ticketId,
      viewerId: normalizedViewer,
      playerName: normalizedName,
      side,
      joinedUnixMs: nowUnixMs()
    };

    this.queueForSide(machine, side).push(entry);
    this.ensureSeatCall(machine, side);

    return ticketId;
  }

  leaveQueue(machineId: number, viewerId: string, ticketId: number): void {
    const machine = this.getMachine(machineId);
    const normalizedViewer = sanitizeViewerId(viewerId);

    const removedSide = this.removeTicket(machine.leftQueue, normalizedViewer, ticketId)
      ? "left"
      : this.removeTicket(machine.rightQueue, normalizedViewer, ticketId)
        ? "right"
        : null;

    if (!removedSide) {
      throw new Error("ticket not found for viewer");
    }

    if (removedSide === "left" && machine.calledLeft?.ticketId === ticketId) {
      machine.calledLeft = null;
    }
    if (removedSide === "right" && machine.calledRight?.ticketId === ticketId) {
      machine.calledRight = null;
    }

    this.ensureSeatCall(machine, removedSide);
  }

  claimSeat(machineId: number, viewerId: string, ticketId: number): void {
    const machine = this.getMachine(machineId);
    const normalizedViewer = sanitizeViewerId(viewerId);

    const leftCalled = machine.calledLeft;
    if (leftCalled && leftCalled.ticketId === ticketId && leftCalled.viewerId === normalizedViewer) {
      if (machine.leftPlayer) {
        throw new Error("left seat is already occupied");
      }
      this.removeTicket(machine.leftQueue, normalizedViewer, ticketId);
      machine.leftPlayer = {
        viewerId: normalizedViewer,
        playerName: leftCalled.playerName,
        assignedUnixMs: nowUnixMs()
      };
      machine.calledLeft = null;
      this.ensureSeatCall(machine, "right");
      return;
    }

    const rightCalled = machine.calledRight;
    if (rightCalled && rightCalled.ticketId === ticketId && rightCalled.viewerId === normalizedViewer) {
      if (machine.rightPlayer) {
        throw new Error("right seat is already occupied");
      }
      this.removeTicket(machine.rightQueue, normalizedViewer, ticketId);
      machine.rightPlayer = {
        viewerId: normalizedViewer,
        playerName: rightCalled.playerName,
        assignedUnixMs: nowUnixMs()
      };
      machine.calledRight = null;
      this.ensureSeatCall(machine, "left");
      return;
    }

    throw new Error("ticket is not currently called for this viewer");
  }

  leaveSeat(machineId: number, viewerId: string): void {
    const machine = this.getMachine(machineId);
    const normalizedViewer = sanitizeViewerId(viewerId);

    if (machine.leftPlayer?.viewerId === normalizedViewer) {
      machine.leftPlayer = null;
      this.ensureSeatCall(machine, "left");
      return;
    }

    if (machine.rightPlayer?.viewerId === normalizedViewer) {
      machine.rightPlayer = null;
      this.ensureSeatCall(machine, "right");
      return;
    }

    throw new Error("viewer is not occupying a seat");
  }

  endRound(machineId: number, winnerSide: Side, leftScore: number, rightScore: number): void {
    const machine = this.getMachine(machineId);
    const leftPlayer = machine.leftPlayer;
    const rightPlayer = machine.rightPlayer;

    if (!leftPlayer || !rightPlayer) {
      throw new Error("both seats must be occupied to end a round");
    }

    const loserSide: Side = winnerSide === "left" ? "right" : "left";
    if (loserSide === "left") {
      machine.leftPlayer = null;
    } else {
      machine.rightPlayer = null;
    }

    const now = nowUnixMs();
    machine.lastRound = {
      roundId: this.nextRoundId,
      winnerSide,
      leftScore,
      rightScore,
      endedUnixMs: now
    };
    this.nextRoundId += 1;

    const day = dayKey(now);
    this.updateScores(day, machineId, leftPlayer.viewerId, leftPlayer.playerName, leftScore);
    this.updateScores(day, machineId, rightPlayer.viewerId, rightPlayer.playerName, rightScore);

    this.ensureSeatCall(machine, loserSide);
  }

  viewerPort(machineId: number, viewerId: string): number | null {
    const machine = this.getMachine(machineId);
    const normalizedViewer = sanitizeViewerId(viewerId);

    if (machine.leftPlayer?.viewerId === normalizedViewer) {
      return 0;
    }
    if (machine.rightPlayer?.viewerId === normalizedViewer) {
      return 1;
    }
    return null;
  }

  private machineView(machine: MachineRecord, runtime: RuntimeView | null, day: string): MachineView {
    const status = deriveStatus(machine);

    return {
      id: machine.meta.id,
      name: machine.meta.name,
      romTitle: machine.meta.romTitle,
      status,
      runtime,
      seats: {
        left: machine.leftPlayer
          ? {
              side: "left",
              port: 0,
              viewerId: machine.leftPlayer.viewerId,
              playerName: machine.leftPlayer.playerName,
              assignedUnixMs: machine.leftPlayer.assignedUnixMs
            }
          : null,
        right: machine.rightPlayer
          ? {
              side: "right",
              port: 1,
              viewerId: machine.rightPlayer.viewerId,
              playerName: machine.rightPlayer.playerName,
              assignedUnixMs: machine.rightPlayer.assignedUnixMs
            }
          : null
      },
      called: {
        left: machine.calledLeft
          ? {
              side: "left",
              ticketId: machine.calledLeft.ticketId,
              viewerId: machine.calledLeft.viewerId,
              playerName: machine.calledLeft.playerName,
              calledUnixMs: machine.calledLeft.calledUnixMs
            }
          : null,
        right: machine.calledRight
          ? {
              side: "right",
              ticketId: machine.calledRight.ticketId,
              viewerId: machine.calledRight.viewerId,
              playerName: machine.calledRight.playerName,
              calledUnixMs: machine.calledRight.calledUnixMs
            }
          : null
      },
      queues: {
        left: machine.leftQueue.map((entry, index) => ({
          ticketId: entry.ticketId,
          viewerId: entry.viewerId,
          playerName: entry.playerName,
          joinedUnixMs: entry.joinedUnixMs,
          position: index + 1
        })),
        right: machine.rightQueue.map((entry, index) => ({
          ticketId: entry.ticketId,
          viewerId: entry.viewerId,
          playerName: entry.playerName,
          joinedUnixMs: entry.joinedUnixMs,
          position: index + 1
        }))
      },
      lastRound: machine.lastRound,
      dailyTop: this.collectMachineTop(day, machine.meta.id),
      streamPaths: {
        video: `/ws/machines/${machine.meta.id}/video`,
        audio: `/ws/machines/${machine.meta.id}/audio`,
        input: `/ws/machines/${machine.meta.id}/input`
      }
    };
  }

  private collectMachineTop(day: string, machineId: number): DailyScoreView[] {
    const rows: DailyScoreView[] = [];
    for (const [key, value] of this.dailyMachineScores.entries()) {
      const [scoreDay, scoreMachineId] = key.split(":", 2);
      if (scoreDay === day && Number.parseInt(scoreMachineId, 10) === machineId) {
        rows.push({
          playerName: value.playerName,
          score: value.score
        });
      }
    }

    rows.sort((a, b) => b.score - a.score || a.playerName.localeCompare(b.playerName));
    return rows.slice(0, 10);
  }

  private collectGlobalTop(day: string): DailyScoreView[] {
    const rows: DailyScoreView[] = [];
    for (const [key, value] of this.dailyGlobalScores.entries()) {
      const [scoreDay] = key.split(":", 1);
      if (scoreDay === day) {
        rows.push({
          playerName: value.playerName,
          score: value.score
        });
      }
    }

    rows.sort((a, b) => b.score - a.score || a.playerName.localeCompare(b.playerName));
    return rows.slice(0, 10);
  }

  private updateScores(
    day: string,
    machineId: number,
    viewerId: string,
    playerName: string,
    score: number
  ): void {
    const machineKey = `${day}:${machineId}:${viewerId}`;
    const machineExisting = this.dailyMachineScores.get(machineKey);
    if (!machineExisting || score >= machineExisting.score) {
      this.dailyMachineScores.set(machineKey, { playerName, score });
    }

    const globalKey = `${day}:${viewerId}`;
    const globalExisting = this.dailyGlobalScores.get(globalKey);
    if (!globalExisting || score >= globalExisting.score) {
      this.dailyGlobalScores.set(globalKey, { playerName, score });
    }
  }

  private ensureSeatCall(machine: MachineRecord, side: Side): void {
    const called = side === "left" ? machine.calledLeft : machine.calledRight;
    const occupied = side === "left" ? machine.leftPlayer : machine.rightPlayer;
    if (called || occupied) {
      return;
    }

    const queue = this.queueForSide(machine, side);
    const next = queue[0];
    if (!next) {
      return;
    }

    const seatCall: SeatCall = {
      ticketId: next.ticketId,
      viewerId: next.viewerId,
      playerName: next.playerName,
      calledUnixMs: nowUnixMs()
    };

    if (side === "left") {
      machine.calledLeft = seatCall;
    } else {
      machine.calledRight = seatCall;
    }
  }

  private viewerIsPresent(machine: MachineRecord, viewerId: string): boolean {
    if (machine.leftPlayer?.viewerId === viewerId || machine.rightPlayer?.viewerId === viewerId) {
      return true;
    }
    if (machine.calledLeft?.viewerId === viewerId || machine.calledRight?.viewerId === viewerId) {
      return true;
    }
    if (machine.leftQueue.some((entry) => entry.viewerId === viewerId)) {
      return true;
    }
    return machine.rightQueue.some((entry) => entry.viewerId === viewerId);
  }

  private removeTicket(queue: QueueEntry[], viewerId: string, ticketId: number): boolean {
    const index = queue.findIndex((entry) => entry.ticketId === ticketId && entry.viewerId === viewerId);
    if (index < 0) {
      return false;
    }
    queue.splice(index, 1);
    return true;
  }

  private queueForSide(machine: MachineRecord, side: Side): QueueEntry[] {
    return side === "left" ? machine.leftQueue : machine.rightQueue;
  }

  private getMachine(machineId: number): MachineRecord {
    const machine = this.machines.get(machineId);
    if (!machine) {
      throw new Error(`machine ${machineId} not found`);
    }
    return machine;
  }
}

function deriveStatus(machine: MachineRecord): "free_play" | "seat_call" | "match_live" {
  if (machine.leftPlayer && machine.rightPlayer) {
    return "match_live";
  }
  if (machine.calledLeft || machine.calledRight) {
    return "seat_call";
  }
  return "free_play";
}

function sanitizePlayerName(raw: string): string {
  const condensed = raw.trim().split(/\s+/g).join(" ");
  const value = condensed.slice(0, 24);
  if (!value) {
    throw new Error("playerName is required");
  }
  return value;
}

function sanitizeViewerId(raw: string): string {
  const value = raw.trim().slice(0, 64);
  if (!value) {
    throw new Error("viewerId is required");
  }
  return value;
}

function nowUnixMs(): number {
  return Date.now();
}

function dayKey(nowUnixMsValue: number): string {
  return new Date(nowUnixMsValue).toISOString().slice(0, 10);
}
