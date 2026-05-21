use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde::Serialize;

const CLAIM_TIMEOUT_MS: u64 = 20_000;
const NO_SHOW_COOLDOWN_MS: u64 = 60_000;
const DAY_MS: u64 = 86_400_000;
const MAX_PLAYER_NAME_LEN: usize = 24;
const SCORE_LIMIT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    FreePlay,
    SeatCall,
    MatchLive,
    PostRound,
}

#[derive(Debug)]
pub enum ArcadeError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
}

impl ArcadeError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::BadRequest(message) | Self::NotFound(message) | Self::Conflict(message) => {
                message
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ArcadeOverview {
    pub now_unix_ms: u64,
    pub claim_timeout_ms: u64,
    pub no_show_cooldown_ms: u64,
    pub machines: Vec<MachineSummaryView>,
    pub daily_global_top: Vec<DailyScoreView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineSummaryView {
    pub id: u32,
    pub name: String,
    pub status: MachineStatus,
    pub left_player: Option<String>,
    pub right_player: Option<String>,
    pub left_queue_len: usize,
    pub right_queue_len: usize,
    pub called: Option<SeatCallView>,
    pub last_round: Option<RoundResultView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineDetailView {
    pub id: u32,
    pub name: String,
    pub status: MachineStatus,
    pub left_player: Option<String>,
    pub right_player: Option<String>,
    pub left_queue: Vec<QueueEntryView>,
    pub right_queue: Vec<QueueEntryView>,
    pub called: Option<SeatCallView>,
    pub last_round: Option<RoundResultView>,
    pub daily_top: Vec<DailyScoreView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueEntryView {
    pub ticket_id: u64,
    pub player_name: String,
    pub joined_unix_ms: u64,
    pub position: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeatCallView {
    pub side: Side,
    pub ticket_id: u64,
    pub player_name: String,
    pub expires_unix_ms: u64,
    pub remaining_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoundResultView {
    pub round_id: u64,
    pub left_player: String,
    pub right_player: String,
    pub winner_side: Side,
    pub left_score: u32,
    pub right_score: u32,
    pub ended_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyScoreView {
    pub player_name: String,
    pub score: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct JoinResult {
    pub ticket_id: u64,
    pub position: usize,
    pub machine: MachineDetailView,
}

#[derive(Debug)]
pub struct ArcadeService {
    inner: std::sync::Mutex<ArcadeState>,
    claim_timeout_ms: u64,
    no_show_cooldown_ms: u64,
}

#[derive(Debug)]
struct ArcadeState {
    machines: Vec<MachineState>,
    next_ticket_id: u64,
    next_round_id: u64,
    cooldown_until: HashMap<String, u64>,
    daily_machine_scores: HashMap<(u64, u32, String), ScoreState>,
    daily_global_scores: HashMap<(u64, String), ScoreState>,
}

#[derive(Debug, Clone)]
struct MachineState {
    id: u32,
    name: String,
    status: MachineStatus,
    left_player: Option<SeatPlayer>,
    right_player: Option<SeatPlayer>,
    left_queue: Vec<QueueEntry>,
    right_queue: Vec<QueueEntry>,
    called: Option<SeatCall>,
    last_round: Option<RoundResult>,
}

#[derive(Debug, Clone)]
struct SeatPlayer {
    player_name: String,
    player_key: String,
}

#[derive(Debug, Clone)]
struct QueueEntry {
    ticket_id: u64,
    player_name: String,
    player_key: String,
    joined_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct SeatCall {
    side: Side,
    ticket_id: u64,
    player_name: String,
    player_key: String,
    expires_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct RoundResult {
    round_id: u64,
    left_player: String,
    right_player: String,
    winner_side: Side,
    left_score: u32,
    right_score: u32,
    ended_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct ScoreState {
    player_name: String,
    score: u32,
}

impl ArcadeService {
    pub fn new(machine_count: u32) -> Self {
        let safe_count = machine_count.max(1);
        let machines = (1..=safe_count)
            .map(|id| MachineState {
                id,
                name: format!("Machine {id}"),
                status: MachineStatus::FreePlay,
                left_player: None,
                right_player: None,
                left_queue: Vec::new(),
                right_queue: Vec::new(),
                called: None,
                last_round: None,
            })
            .collect();

        Self {
            inner: std::sync::Mutex::new(ArcadeState {
                machines,
                next_ticket_id: 1,
                next_round_id: 1,
                cooldown_until: HashMap::new(),
                daily_machine_scores: HashMap::new(),
                daily_global_scores: HashMap::new(),
            }),
            claim_timeout_ms: CLAIM_TIMEOUT_MS,
            no_show_cooldown_ms: NO_SHOW_COOLDOWN_MS,
        }
    }

    pub fn overview(&self) -> ArcadeOverview {
        let now = now_unix_ms();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.housekeeping(&mut inner, now);

        let day = day_key(now);
        let machines = inner
            .machines
            .iter()
            .map(|machine| machine_summary_view(machine, now))
            .collect();

        ArcadeOverview {
            now_unix_ms: now,
            claim_timeout_ms: self.claim_timeout_ms,
            no_show_cooldown_ms: self.no_show_cooldown_ms,
            machines,
            daily_global_top: collect_global_top(&inner, day),
        }
    }

    pub fn machine(&self, machine_id: u32) -> Result<MachineDetailView, ArcadeError> {
        let now = now_unix_ms();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.housekeeping(&mut inner, now);

        let day = day_key(now);
        let machine = find_machine(&inner, machine_id)?;
        Ok(machine_detail_view(
            machine,
            now,
            collect_machine_top(&inner, day, machine_id),
        ))
    }

    pub fn join_queue(
        &self,
        machine_id: u32,
        player_name: String,
        side: Side,
    ) -> Result<JoinResult, ArcadeError> {
        let now = now_unix_ms();
        let (display_name, player_key) = normalize_player_name(&player_name)?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.housekeeping(&mut inner, now);

        if let Some(until) = inner.cooldown_until.get(&player_key) {
            if *until > now {
                let remaining_seconds = ((*until - now) / 1_000).max(1);
                return Err(ArcadeError::Conflict(format!(
                    "player is cooling down after no-show; try again in {remaining_seconds}s"
                )));
            }
        }

        if player_is_seated(&inner, &player_key) {
            return Err(ArcadeError::Conflict(
                "player is already seated at a machine".to_string(),
            ));
        }

        if player_has_active_ticket(&inner, &player_key) {
            return Err(ArcadeError::Conflict(
                "player already has an active queue ticket".to_string(),
            ));
        }

        let ticket_id = inner.next_ticket_id;
        inner.next_ticket_id += 1;
        let machine_index = find_machine_index(&inner, machine_id)?;
        let position;
        {
            let machine = &mut inner.machines[machine_index];
            let queue = machine.side_queue_mut(side);
            if queue.iter().any(|entry| entry.player_key == player_key) {
                return Err(ArcadeError::Conflict(
                    "player is already queued on that side".to_string(),
                ));
            }

            queue.push(QueueEntry {
                ticket_id,
                player_name: display_name.clone(),
                player_key,
                joined_unix_ms: now,
            });

            position = queue.len();
            if machine.called.is_none() && machine.seat(side).is_none() {
                call_next_for_side(machine, side, now, self.claim_timeout_ms);
            } else {
                machine.status = derive_status(machine);
            }
        }

        let day = day_key(now);
        let detail = {
            let machine = find_machine(&inner, machine_id)?;
            machine_detail_view(machine, now, collect_machine_top(&inner, day, machine_id))
        };
        Ok(JoinResult {
            ticket_id,
            position,
            machine: detail,
        })
    }

    pub fn leave_queue(
        &self,
        machine_id: u32,
        ticket_id: u64,
    ) -> Result<MachineDetailView, ArcadeError> {
        if ticket_id == 0 {
            return Err(ArcadeError::BadRequest(
                "ticket_id must be a positive integer".to_string(),
            ));
        }

        let now = now_unix_ms();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.housekeeping(&mut inner, now);

        let machine_index = find_machine_index(&inner, machine_id)?;
        let mut removed_side = None;
        {
            let machine = &mut inner.machines[machine_index];
            for side in [Side::Left, Side::Right] {
                let queue = machine.side_queue_mut(side);
                if let Some(index) = queue.iter().position(|entry| entry.ticket_id == ticket_id) {
                    queue.remove(index);
                    removed_side = Some(side);
                    break;
                }
            }

            if removed_side.is_none() {
                return Err(ArcadeError::NotFound(format!(
                    "ticket {ticket_id} not found on machine {machine_id}"
                )));
            }

            if machine
                .called
                .as_ref()
                .is_some_and(|called| called.ticket_id == ticket_id)
            {
                if let Some(side) = removed_side {
                    machine.called = None;
                    call_next_for_side(machine, side, now, self.claim_timeout_ms);
                }
            } else {
                machine.status = derive_status(machine);
            }
        }

        let day = day_key(now);
        let machine = find_machine(&inner, machine_id)?;
        Ok(machine_detail_view(
            machine,
            now,
            collect_machine_top(&inner, day, machine_id),
        ))
    }

    pub fn claim_seat(
        &self,
        machine_id: u32,
        ticket_id: u64,
    ) -> Result<MachineDetailView, ArcadeError> {
        if ticket_id == 0 {
            return Err(ArcadeError::BadRequest(
                "ticket_id must be a positive integer".to_string(),
            ));
        }

        let now = now_unix_ms();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.housekeeping(&mut inner, now);

        let machine_index = find_machine_index(&inner, machine_id)?;
        {
            let machine = &mut inner.machines[machine_index];
            let called = machine.called.clone().ok_or_else(|| {
                ArcadeError::Conflict("no seat is currently being called".to_string())
            })?;

            if called.ticket_id != ticket_id {
                return Err(ArcadeError::Conflict(format!(
                    "ticket {ticket_id} is not currently called (active ticket: {})",
                    called.ticket_id
                )));
            }

            if machine.seat(called.side).is_some() {
                return Err(ArcadeError::Conflict(
                    "that side is already occupied".to_string(),
                ));
            }

            let queue = machine.side_queue_mut(called.side);
            if let Some(index) = queue.iter().position(|entry| entry.ticket_id == ticket_id) {
                queue.remove(index);
            }

            let side = called.side;
            *machine.seat_mut(side) = Some(SeatPlayer {
                player_name: called.player_name,
                player_key: called.player_key,
            });
            machine.called = None;

            let opposite = side.opposite();
            if machine.left_player.is_some() && machine.right_player.is_some() {
                machine.status = MachineStatus::MatchLive;
            } else if machine.seat(opposite).is_none() {
                call_next_for_side(machine, opposite, now, self.claim_timeout_ms);
                if machine.called.is_none() {
                    machine.status = MachineStatus::FreePlay;
                }
            } else {
                machine.status = MachineStatus::FreePlay;
            }
        }

        let day = day_key(now);
        let machine = find_machine(&inner, machine_id)?;
        Ok(machine_detail_view(
            machine,
            now,
            collect_machine_top(&inner, day, machine_id),
        ))
    }

    pub fn end_round(
        &self,
        machine_id: u32,
        winner_side: Side,
        left_score: u32,
        right_score: u32,
    ) -> Result<MachineDetailView, ArcadeError> {
        let now = now_unix_ms();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.housekeeping(&mut inner, now);

        let round_id = inner.next_round_id;
        inner.next_round_id += 1;
        let day = day_key(now);
        let machine_index = find_machine_index(&inner, machine_id)?;

        let (left_player_name, left_player_key, right_player_name, right_player_key);
        {
            let machine = &inner.machines[machine_index];
            let left_player = machine.left_player.as_ref().ok_or_else(|| {
                ArcadeError::Conflict("cannot end round without a left player".to_string())
            })?;
            let right_player = machine.right_player.as_ref().ok_or_else(|| {
                ArcadeError::Conflict("cannot end round without a right player".to_string())
            })?;
            left_player_name = left_player.player_name.clone();
            left_player_key = left_player.player_key.clone();
            right_player_name = right_player.player_name.clone();
            right_player_key = right_player.player_key.clone();
        }

        update_scores(
            &mut inner,
            day,
            machine_id,
            &left_player_name,
            &left_player_key,
            left_score,
        );
        update_scores(
            &mut inner,
            day,
            machine_id,
            &right_player_name,
            &right_player_key,
            right_score,
        );

        {
            let machine = &mut inner.machines[machine_index];
            machine.last_round = Some(RoundResult {
                round_id,
                left_player: left_player_name,
                right_player: right_player_name,
                winner_side,
                left_score,
                right_score,
                ended_unix_ms: now,
            });
            machine.status = MachineStatus::PostRound;
            machine.called = None;

            let loser_side = winner_side.opposite();
            *machine.seat_mut(loser_side) = None;

            call_next_for_side(machine, loser_side, now, self.claim_timeout_ms);
            if machine.called.is_none() {
                machine.status = derive_status(machine);
            }
        }

        let machine = find_machine(&inner, machine_id)?;
        Ok(machine_detail_view(
            machine,
            now,
            collect_machine_top(&inner, day, machine_id),
        ))
    }

    fn housekeeping(&self, inner: &mut ArcadeState, now: u64) {
        inner.cooldown_until.retain(|_, until| *until > now);

        for machine in &mut inner.machines {
            let Some(called) = machine.called.clone() else {
                machine.status = derive_status(machine);
                continue;
            };

            if called.expires_unix_ms > now {
                machine.status = MachineStatus::SeatCall;
                continue;
            }

            let queue = machine.side_queue_mut(called.side);
            queue.retain(|entry| entry.ticket_id != called.ticket_id);

            inner
                .cooldown_until
                .insert(called.player_key, now + self.no_show_cooldown_ms);

            machine.called = None;
            machine.status = MachineStatus::PostRound;
            call_next_for_side(machine, called.side, now, self.claim_timeout_ms);
            if machine.called.is_none() {
                machine.status = derive_status(machine);
            }
        }
    }
}

fn normalize_player_name(raw: &str) -> Result<(String, String), ArcadeError> {
    let condensed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = condensed.trim();
    if trimmed.is_empty() {
        return Err(ArcadeError::BadRequest(
            "player_name is required".to_string(),
        ));
    }

    let display_name: String = trimmed.chars().take(MAX_PLAYER_NAME_LEN).collect();
    let player_key = display_name.to_ascii_lowercase();
    Ok((display_name, player_key))
}

fn find_machine(inner: &ArcadeState, machine_id: u32) -> Result<&MachineState, ArcadeError> {
    inner
        .machines
        .iter()
        .find(|machine| machine.id == machine_id)
        .ok_or_else(|| ArcadeError::NotFound(format!("machine {machine_id} not found")))
}

fn find_machine_index(inner: &ArcadeState, machine_id: u32) -> Result<usize, ArcadeError> {
    inner
        .machines
        .iter()
        .position(|machine| machine.id == machine_id)
        .ok_or_else(|| ArcadeError::NotFound(format!("machine {machine_id} not found")))
}

fn player_has_active_ticket(inner: &ArcadeState, player_key: &str) -> bool {
    inner.machines.iter().any(|machine| {
        machine
            .left_queue
            .iter()
            .any(|entry| entry.player_key == player_key)
            || machine
                .right_queue
                .iter()
                .any(|entry| entry.player_key == player_key)
            || machine
                .called
                .as_ref()
                .is_some_and(|called| called.player_key == player_key)
    })
}

fn player_is_seated(inner: &ArcadeState, player_key: &str) -> bool {
    inner.machines.iter().any(|machine| {
        machine
            .left_player
            .as_ref()
            .is_some_and(|player| player.player_key == player_key)
            || machine
                .right_player
                .as_ref()
                .is_some_and(|player| player.player_key == player_key)
    })
}

fn derive_status(machine: &MachineState) -> MachineStatus {
    if machine.called.is_some() {
        MachineStatus::SeatCall
    } else if machine.left_player.is_some() && machine.right_player.is_some() {
        MachineStatus::MatchLive
    } else {
        MachineStatus::FreePlay
    }
}

fn call_next_for_side(machine: &mut MachineState, side: Side, now: u64, claim_timeout_ms: u64) {
    if machine.called.is_some() || machine.seat(side).is_some() {
        machine.status = derive_status(machine);
        return;
    }

    let next = machine.side_queue(side).first().map(|entry| SeatCall {
        side,
        ticket_id: entry.ticket_id,
        player_name: entry.player_name.clone(),
        player_key: entry.player_key.clone(),
        expires_unix_ms: now + claim_timeout_ms,
    });

    machine.called = next;
    machine.status = if machine.called.is_some() {
        MachineStatus::SeatCall
    } else {
        derive_status(machine)
    };
}

fn machine_summary_view(machine: &MachineState, now: u64) -> MachineSummaryView {
    MachineSummaryView {
        id: machine.id,
        name: machine.name.clone(),
        status: machine.status,
        left_player: machine
            .left_player
            .as_ref()
            .map(|player| player.player_name.clone()),
        right_player: machine
            .right_player
            .as_ref()
            .map(|player| player.player_name.clone()),
        left_queue_len: machine.left_queue.len(),
        right_queue_len: machine.right_queue.len(),
        called: machine
            .called
            .as_ref()
            .map(|called| seat_call_view(called, now)),
        last_round: machine.last_round.as_ref().map(round_result_view),
    }
}

fn machine_detail_view(
    machine: &MachineState,
    now: u64,
    daily_top: Vec<DailyScoreView>,
) -> MachineDetailView {
    MachineDetailView {
        id: machine.id,
        name: machine.name.clone(),
        status: machine.status,
        left_player: machine
            .left_player
            .as_ref()
            .map(|player| player.player_name.clone()),
        right_player: machine
            .right_player
            .as_ref()
            .map(|player| player.player_name.clone()),
        left_queue: queue_view(&machine.left_queue),
        right_queue: queue_view(&machine.right_queue),
        called: machine
            .called
            .as_ref()
            .map(|called| seat_call_view(called, now)),
        last_round: machine.last_round.as_ref().map(round_result_view),
        daily_top,
    }
}

fn queue_view(queue: &[QueueEntry]) -> Vec<QueueEntryView> {
    queue
        .iter()
        .enumerate()
        .map(|(index, entry)| QueueEntryView {
            ticket_id: entry.ticket_id,
            player_name: entry.player_name.clone(),
            joined_unix_ms: entry.joined_unix_ms,
            position: index + 1,
        })
        .collect()
}

fn seat_call_view(called: &SeatCall, now: u64) -> SeatCallView {
    SeatCallView {
        side: called.side,
        ticket_id: called.ticket_id,
        player_name: called.player_name.clone(),
        expires_unix_ms: called.expires_unix_ms,
        remaining_ms: called.expires_unix_ms.saturating_sub(now),
    }
}

fn round_result_view(round: &RoundResult) -> RoundResultView {
    RoundResultView {
        round_id: round.round_id,
        left_player: round.left_player.clone(),
        right_player: round.right_player.clone(),
        winner_side: round.winner_side,
        left_score: round.left_score,
        right_score: round.right_score,
        ended_unix_ms: round.ended_unix_ms,
    }
}

fn collect_machine_top(inner: &ArcadeState, day: u64, machine_id: u32) -> Vec<DailyScoreView> {
    let mut rows: Vec<DailyScoreView> = inner
        .daily_machine_scores
        .iter()
        .filter(|((score_day, score_machine_id, _), _)| {
            *score_day == day && *score_machine_id == machine_id
        })
        .map(|(_, score)| DailyScoreView {
            player_name: score.player_name.clone(),
            score: score.score,
        })
        .collect();

    rows.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.player_name.cmp(&b.player_name))
    });
    rows.truncate(SCORE_LIMIT);
    rows
}

fn collect_global_top(inner: &ArcadeState, day: u64) -> Vec<DailyScoreView> {
    let mut rows: Vec<DailyScoreView> = inner
        .daily_global_scores
        .iter()
        .filter(|((score_day, _), _)| *score_day == day)
        .map(|(_, score)| DailyScoreView {
            player_name: score.player_name.clone(),
            score: score.score,
        })
        .collect();

    rows.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.player_name.cmp(&b.player_name))
    });
    rows.truncate(SCORE_LIMIT);
    rows
}

fn update_scores(
    inner: &mut ArcadeState,
    day: u64,
    machine_id: u32,
    player_name: &str,
    player_key: &str,
    score: u32,
) {
    let machine_key = (day, machine_id, player_key.to_string());
    let machine_entry = inner
        .daily_machine_scores
        .entry(machine_key)
        .or_insert_with(|| ScoreState {
            player_name: player_name.to_string(),
            score,
        });
    if score >= machine_entry.score {
        machine_entry.score = score;
        machine_entry.player_name = player_name.to_string();
    }

    let global_key = (day, player_key.to_string());
    let global_entry = inner
        .daily_global_scores
        .entry(global_key)
        .or_insert_with(|| ScoreState {
            player_name: player_name.to_string(),
            score,
        });
    if score >= global_entry.score {
        global_entry.score = score;
        global_entry.player_name = player_name.to_string();
    }
}

fn now_unix_ms() -> u64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    duration.as_millis() as u64
}

fn day_key(now_unix_ms: u64) -> u64 {
    now_unix_ms / DAY_MS
}

impl MachineState {
    fn side_queue(&self, side: Side) -> &Vec<QueueEntry> {
        match side {
            Side::Left => &self.left_queue,
            Side::Right => &self.right_queue,
        }
    }

    fn side_queue_mut(&mut self, side: Side) -> &mut Vec<QueueEntry> {
        match side {
            Side::Left => &mut self.left_queue,
            Side::Right => &mut self.right_queue,
        }
    }

    fn seat(&self, side: Side) -> Option<&SeatPlayer> {
        match side {
            Side::Left => self.left_player.as_ref(),
            Side::Right => self.right_player.as_ref(),
        }
    }

    fn seat_mut(&mut self, side: Side) -> &mut Option<SeatPlayer> {
        match side {
            Side::Left => &mut self.left_player,
            Side::Right => &mut self.right_player,
        }
    }
}
