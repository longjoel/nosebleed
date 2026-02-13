# Virtual Arcade Blueprint

This blueprint maps your product idea to an implementation plan using `nosebleed` as the machine runtime.

## Product Model

### Core loop

1. A user lands on the home page and sees 6 live machine screens.
2. The user picks one machine and one side (`left` or `right`) to queue.
3. When a seat opens, the next queued player for that side is called.
4. Match runs until game-over/round-end.
5. Winner stays on the machine.
6. Loser returns to the general player pool (not queued).
7. Score is recorded; player can set a daily high score.

### Roles

- Spectator: can watch any machine stream, no queue required.
- Challenger: queued for one side on one machine.
- Active player: currently occupying a side on a machine.
- Arcade admin/system: resolves round outcomes and score submission.

## Machine Rules

### States

- `free_play`: one or both seats open, machine still playable/viewable.
- `match_live`: both seats occupied and competitive round active.
- `seat_call`: machine waiting for a called queued player to claim seat.
- `post_round`: winner/loser resolution and queue rotation.

### Winner-stays rotation

1. Determine `winner_side` and `loser_side`.
2. Winner remains in current side seat.
3. Loser seat is cleared.
4. Next queued challenger for loser side is called.
5. If no challenger exists, machine returns to `free_play`.

### Queue policy (per machine, per side)

- Independent queue for `left` and `right`.
- One user can hold at most one active queue ticket across the arcade.
- Queue entry expires if client disconnects and does not reconnect within grace period.
- Called player must claim seat within `claim_timeout` (example: 20s) or is skipped.
- Repeated no-shows trigger a short cooldown (example: 60s) before rejoin.

## Daily High Score Rules

- Score submitted at end of each round for both players.
- Keep:
  - `best_score_day` (per player, per machine, per day).
  - `best_score_day_global` (per player, all machines, per day).
- Day boundary should be explicit and configurable; default to UTC midnight.
- Leaderboards exposed for:
  - per-machine daily high scores.
  - global daily high scores.

## Service Blueprint

Use a two-layer architecture:

1. `nosebleed` runtime layer (already present):
- Per-machine media/input server (`/ws/video`, `/ws/audio`, `/ws/input`, `/webrtc/session`).
- Auth ticket validation for player/spectator role and allowed ports.

2. New arcade orchestration layer:
- Owns queues, player presence, machine assignment, winner-stays logic, and score tables.
- Starts/stops and configures each machine runtime.
- Issues short-lived signed tickets for machine runtime connections.
- Broadcasts machine + queue updates to web clients.

## Suggested Components

- `arcade-api` (HTTP + WebSocket):
  - machine overview
  - queue join/leave
  - call-seat / claim-seat
  - outcome + score submission
  - leaderboard queries
- `machine-manager`:
  - manages 6 runtime instances
  - health checks and restarts
  - stream endpoint registry
- `presence-service`:
  - tracks queued user liveness and reconnect grace windows
- `postgres`:
  - durable queue, rounds, and score history
- `redis` (optional but recommended):
  - low-latency pub/sub for live queue updates and timers

## Data Model (Logical)

### `machines`

- `id` (pk)
- `name`
- `status` (`free_play|seat_call|match_live|post_round`)
- `left_player_id` (nullable)
- `right_player_id` (nullable)
- `runtime_host`
- `runtime_port`
- `updated_at`

### `queue_entries`

- `id` (pk)
- `machine_id` (fk)
- `side` (`left|right`)
- `player_id` (fk)
- `state` (`queued|called|claimed|skipped|expired|cancelled`)
- `position`
- `joined_at`
- `called_at` (nullable)
- `expires_at` (nullable)

### `rounds`

- `id` (pk)
- `machine_id` (fk)
- `left_player_id`
- `right_player_id`
- `winner_player_id` (nullable)
- `loser_player_id` (nullable)
- `started_at`
- `ended_at` (nullable)
- `end_reason` (`normal|disconnect|forfeit|admin`)

### `scores`

- `id` (pk)
- `round_id` (fk)
- `machine_id` (fk)
- `player_id` (fk)
- `score`
- `recorded_at`
- `score_day` (date, normalized to configured day boundary)

## API Sketch

### Client-facing

- `GET /arcade/overview`
- `GET /machines/:id`
- `POST /machines/:id/queue/join` body: `{ "side": "left" | "right" }`
- `POST /machines/:id/queue/leave`
- `POST /machines/:id/claim-seat`
- `GET /leaderboards/daily?scope=global|machine&machine_id=:id&day=YYYY-MM-DD`
- `WS /ws/arcade` for live machine state + queue events

### Internal/admin

- `POST /machines/:id/rounds/start`
- `POST /machines/:id/rounds/:roundId/end` body includes winner + scores
- `POST /machines/:id/runtime/restart`

## Event Contracts (WebSocket)

- `machine_snapshot`
- `queue_joined`
- `queue_position_changed`
- `seat_called`
- `seat_claimed`
- `match_started`
- `score_recorded`
- `match_ended`
- `leaderboard_updated`

## Anti-Friction Guardrails

- Auto-drop stale queue entries if heartbeat misses threshold.
- Call countdown visible to all queued players.
- One-click requeue after loss.
- Soft backfill to `free_play` if only one side available.
- Spectator stream stays open even if user is not queued.

## MVP Milestones

1. `MVP-1`: 6-machine home wall + machine detail page + spectator streams.
2. `MVP-2`: per-side queue join/leave + live queue positions.
3. `MVP-3`: seat-call, claim timeout, winner-stays rotation.
4. `MVP-4`: round-end score ingest + daily leaderboards.
5. `MVP-5`: reconnect handling, no-show cooldowns, admin controls.
