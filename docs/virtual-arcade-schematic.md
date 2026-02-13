# Virtual Arcade Schematic

This schematic complements `docs/virtual-arcade-blueprint.md` with executable-style diagrams.

## 1) System Architecture

```mermaid
flowchart LR
    subgraph Clients
        WEB[Web App]
        OBS[Spectator Browser]
        PLY[Queued/Active Player]
    end

    subgraph ArcadeControl[Arcade Orchestration Layer]
        API[Arcade API + WS Hub]
        QM[Queue Manager]
        SM[Score Manager]
        MM[Machine Manager]
    end

    subgraph Data
        PG[(Postgres)]
        RD[(Redis PubSub/Timers)]
    end

    subgraph Runtime[Machine Runtime Layer]
        M1[Machine 1<br/>nosebleed]
        M2[Machine 2<br/>nosebleed]
        M3[Machine 3<br/>nosebleed]
        M4[Machine 4<br/>nosebleed]
        M5[Machine 5<br/>nosebleed]
        M6[Machine 6<br/>nosebleed]
    end

    WEB --> API
    OBS --> API
    PLY --> API

    API <--> QM
    API <--> SM
    API <--> MM
    API <--> PG
    QM <--> PG
    SM <--> PG
    API <--> RD
    QM <--> RD

    MM --> M1
    MM --> M2
    MM --> M3
    MM --> M4
    MM --> M5
    MM --> M6

    WEB -. stream token .-> M1
    OBS -. video/audio .-> M1
    PLY -. input/video/audio .-> M1
```

## 2) Join Queue To Match Start Sequence

```mermaid
sequenceDiagram
    participant U as User Browser
    participant A as Arcade API
    participant Q as Queue Manager
    participant DB as Postgres
    participant N as nosebleed (Machine X)

    U->>A: POST /machines/X/queue/join { side }
    A->>Q: enqueue(user, machine, side)
    Q->>DB: insert queue_entry(state=queued)
    DB-->>Q: ok
    Q-->>A: queue position
    A-->>U: 200 + position
    A-->>U: WS queue_position_changed

    Note over Q: Opponent seat opens
    Q->>DB: update queue_entry(state=called, expires_at=t+20s)
    Q-->>A: seat_called(user, machine, side)
    A-->>U: WS seat_called + claim deadline

    U->>A: POST /machines/X/claim-seat
    A->>Q: claim seat if not expired
    Q->>DB: mark claimed + assign machine side
    Q-->>A: claimed
    A->>A: mint short-lived runtime token
    A-->>U: runtime endpoints + token

    U->>N: connect /ws/input?token=...
    U->>N: connect /ws/video?token=...
    Note over A,Q: when both sides claimed -> match_started
```

## 3) Single Machine State Diagram

```mermaid
stateDiagram-v2
    [*] --> FreePlay

    FreePlay --> SeatCall: queue has challenger for open side
    SeatCall --> FreePlay: claim timeout / no-show
    SeatCall --> MatchLive: both sides occupied

    MatchLive --> PostRound: round end event
    PostRound --> SeatCall: winner stays, loser seat backfilled from queue
    PostRound --> FreePlay: winner stays, no challenger available

    FreePlay --> MatchLive: two players manually occupied
    MatchLive --> FreePlay: both players leave/disconnect
```

## 4) Runtime Port Mapping (Recommended)

- Machine side `left` maps to input port `0`.
- Machine side `right` maps to input port `1`.
- Spectators receive media only; no input ports in token.
- Active player tokens include exactly one allowed port.

## 5) Tick/Timer Defaults

- `claim_timeout_ms`: `20000`
- `queue_presence_ttl_ms`: `15000`
- `reconnect_grace_ms`: `15000`
- `no_show_cooldown_ms`: `60000`
- `leaderboard_flush_interval_ms`: `1000`

Treat these as config values, not hardcoded constants.
