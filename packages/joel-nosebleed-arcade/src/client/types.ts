export type Side = "left" | "right";

export interface RuntimeView {
  status: "starting" | "online" | "error" | "stopped";
  listenPort: number;
  lastError?: string;
}

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
