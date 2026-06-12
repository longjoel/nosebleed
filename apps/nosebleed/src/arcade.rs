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
        let mut inner = self.inner.lock().unwrap_or_else(crate::lock_recover);
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
        let mut inner = self.inner.lock().unwrap_or_else(crate::lock_recover);
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
        let mut inner = self.inner.lock().unwrap_or_else(crate::lock_recover);
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
        let mut inner = self.inner.lock().unwrap_or_else(crate::lock_recover);
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
        let mut inner = self.inner.lock().unwrap_or_else(crate::lock_recover);
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
        let mut inner = self.inner.lock().unwrap_or_else(crate::lock_recover);
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ─────────────────────────────────────────────────────────

    fn service() -> ArcadeService {
        ArcadeService::new(2)
    }

    // ── Side parsing ────────────────────────────────────────────────────

    #[test]
    fn test_side_parse_left() {
        assert_eq!(Side::parse("left"), Some(Side::Left));
        assert_eq!(Side::parse("LEFT"), Some(Side::Left));
        assert_eq!(Side::parse(" Left "), Some(Side::Left));
    }

    #[test]
    fn test_side_parse_right() {
        assert_eq!(Side::parse("right"), Some(Side::Right));
        assert_eq!(Side::parse("RIGHT"), Some(Side::Right));
        assert_eq!(Side::parse(" Right "), Some(Side::Right));
    }

    #[test]
    fn test_side_parse_invalid() {
        assert_eq!(Side::parse(""), None);
        assert_eq!(Side::parse("foo"), None);
        assert_eq!(Side::parse("lefty"), None);
    }

    #[test]
    fn test_side_opposite() {
        assert_eq!(Side::Left.opposite(), Side::Right);
        assert_eq!(Side::Right.opposite(), Side::Left);
    }

    // ── Service construction ────────────────────────────────────────────

    #[test]
    fn test_arcade_service_new_creates_correct_machine_count() {
        let svc = service();
        let overview = svc.overview();
        assert_eq!(overview.machines.len(), 2);
    }

    #[test]
    fn test_arcade_service_new_at_least_one_machine() {
        let svc = ArcadeService::new(0);
        let overview = svc.overview();
        assert_eq!(overview.machines.len(), 1, "must create at least 1 machine");
    }

    #[test]
    fn test_arcade_service_new_starts_free_play() {
        let svc = service();
        let overview = svc.overview();
        for machine in &overview.machines {
            assert_eq!(machine.status, MachineStatus::FreePlay);
            assert!(machine.left_player.is_none());
            assert!(machine.right_player.is_none());
            assert_eq!(
                machine.left_queue_len, 0,
                "initial left queue should be empty"
            );
            assert_eq!(machine.right_queue_len, 0);
        }
    }

    #[test]
    fn test_arcade_overview_has_constants() {
        let svc = service();
        let overview = svc.overview();
        assert!(overview.claim_timeout_ms > 0);
        assert!(overview.no_show_cooldown_ms > 0);
        assert!(overview.now_unix_ms > 0);
    }

    // ── Seat claiming ───────────────────────────────────────────────────

    #[test]
    fn test_seat_claim_flow() {
        let svc = service();

        // Join left queue
        let join = svc
            .join_queue(1, "Alice".into(), Side::Left)
            .expect("Alice joins left queue");
        assert_eq!(join.position, 1);

        // Since no one is seated left and left queue was empty, Alice should be
        // called immediately. Claim the seat.
        let claim = svc
            .claim_seat(1, join.ticket_id)
            .expect("Alice claims seat");
        assert_eq!(claim.left_player, Some("Alice".to_string()));

        // Verify machine status — still FreePlay because right seat is empty
        assert_eq!(claim.status, MachineStatus::FreePlay);
    }

    #[test]
    fn test_same_player_cannot_claim_twice() {
        let svc = service();

        let join = svc
            .join_queue(1, "Bob".into(), Side::Right)
            .expect("Bob joins right");
        svc.claim_seat(1, join.ticket_id).expect("Bob claims seat");

        // Bob tries to claim again — should fail (no active call)
        let result = svc.claim_seat(1, join.ticket_id);
        assert!(result.is_err());
    }

    // ── Queue ordering (FIFO) ───────────────────────────────────────────

    #[test]
    fn test_queue_fifo_ordering() {
        let svc = service();

        // Alice and Bob join left queue
        let alice = svc
            .join_queue(1, "Alice".into(), Side::Left)
            .expect("Alice joins");
        let _bob = svc
            .join_queue(1, "Bob".into(), Side::Left)
            .expect("Bob joins");

        // Alice claims left seat
        svc.claim_seat(1, alice.ticket_id)
            .expect("Alice claims seat");

        // Charlie joins right queue and claims right seat so we can play a round
        let charlie = svc
            .join_queue(1, "Charlie".into(), Side::Right)
            .expect("Charlie joins");
        svc.claim_seat(1, charlie.ticket_id)
            .expect("Charlie claims seat");

        // End round — Alice loses, so left seat becomes free.
        // call_next_for_side should pick Bob from the queue.
        svc.end_round(1, Side::Right, 0, 10)
            .expect("end round, Charlie wins");

        // Now Bob should be called for the left seat
        let machine = svc.machine(1).expect("get machine 1");
        let called = machine.called.expect("should have a seat call for Bob");
        assert_eq!(called.player_name, "Bob");
    }

    // ── Player already seated ───────────────────────────────────────────

    #[test]
    fn test_player_already_seated_cannot_queue() {
        let svc = service();

        let join = svc
            .join_queue(1, "Charlie".into(), Side::Right)
            .expect("Charlie joins");
        svc.claim_seat(1, join.ticket_id).expect("Charlie claims");

        // Charlie tries to queue again
        let result = svc.join_queue(1, "Charlie".into(), Side::Left);
        assert!(result.is_err(), "already seated player should be rejected");
        if let Err(err) = result {
            assert!(matches!(err, ArcadeError::Conflict(_)));
        }
    }

    // ── Player already has ticket ───────────────────────────────────────

    #[test]
    fn test_player_with_active_ticket_cannot_queue_again() {
        let svc = service();

        svc.join_queue(1, "Dave".into(), Side::Left)
            .expect("Dave joins left");

        let result = svc.join_queue(1, "Dave".into(), Side::Right);
        assert!(
            result.is_err(),
            "player with active ticket should be rejected"
        );
    }

    // ── Machine removal / cleanup via leave_queue ───────────────────────

    #[test]
    fn test_leave_queue_removes_ticket() {
        let svc = service();

        let join = svc
            .join_queue(1, "Eve".into(), Side::Left)
            .expect("Eve joins");
        assert_eq!(join.position, 1);

        let machine = svc
            .leave_queue(1, join.ticket_id)
            .expect("Eve leaves queue");
        assert_eq!(
            machine.left_queue.len(),
            0,
            "leave queue should clear left queue"
        );
    }

    #[test]
    fn test_leave_queue_unknown_ticket_returns_not_found() {
        let svc = service();
        let result = svc.leave_queue(1, 99999);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ArcadeError::NotFound(_)));
    }

    #[test]
    fn test_leave_queue_zero_ticket_bad_request() {
        let svc = service();
        let result = svc.leave_queue(1, 0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ArcadeError::BadRequest(_)));
    }

    // ── Machine not found ───────────────────────────────────────────────

    #[test]
    fn test_machine_not_found() {
        let svc = service();
        let result = svc.machine(999);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ArcadeError::NotFound(_)));
    }

    // ── normalize_player_name ───────────────────────────────────────────

    #[test]
    fn test_normalize_player_name_trims_and_downcases_key() {
        let (display, key) = normalize_player_name("  Alice  ").expect("normalize name");
        assert_eq!(display, "Alice");
        assert_eq!(key, "alice");
    }

    #[test]
    fn test_normalize_player_name_empty_fails() {
        let result = normalize_player_name("");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ArcadeError::BadRequest(_)));
    }

    #[test]
    fn test_normalize_player_name_truncates_long_names() {
        let long = "a".repeat(50);
        let (display, key) = normalize_player_name(&long).expect("normalize long name");
        assert_eq!(display.len(), MAX_PLAYER_NAME_LEN);
        assert_eq!(key.len(), MAX_PLAYER_NAME_LEN);
    }

    // ── derive_status ───────────────────────────────────────────────────

    #[test]
    fn test_derive_status_free_play_both_empty() {
        let m = MachineState {
            id: 1,
            name: "test".into(),
            status: MachineStatus::FreePlay,
            left_player: None,
            right_player: None,
            left_queue: vec![],
            right_queue: vec![],
            called: None,
            last_round: None,
        };
        assert_eq!(derive_status(&m), MachineStatus::FreePlay);
    }

    #[test]
    fn test_derive_status_match_live_both_seated() {
        let m = MachineState {
            id: 1,
            name: "test".into(),
            status: MachineStatus::FreePlay,
            left_player: Some(SeatPlayer {
                player_name: "A".into(),
                player_key: "a".into(),
            }),
            right_player: Some(SeatPlayer {
                player_name: "B".into(),
                player_key: "b".into(),
            }),
            left_queue: vec![],
            right_queue: vec![],
            called: None,
            last_round: None,
        };
        assert_eq!(derive_status(&m), MachineStatus::MatchLive);
    }

    #[test]
    fn test_derive_status_seat_call() {
        let m = MachineState {
            id: 1,
            name: "test".into(),
            status: MachineStatus::FreePlay,
            left_player: None,
            right_player: None,
            left_queue: vec![],
            right_queue: vec![],
            called: Some(SeatCall {
                side: Side::Left,
                ticket_id: 1,
                player_name: "A".into(),
                player_key: "a".into(),
                expires_unix_ms: 99999,
            }),
            last_round: None,
        };
        assert_eq!(derive_status(&m), MachineStatus::SeatCall);
    }

    // ── end_round ───────────────────────────────────────────────────────

    #[test]
    fn test_end_round_requires_both_players() {
        let svc = service();

        // Only seat left player
        let join = svc
            .join_queue(1, "Frank".into(), Side::Left)
            .expect("Frank joins");
        svc.claim_seat(1, join.ticket_id).expect("Frank claims");

        let result = svc.end_round(1, Side::Left, 10, 5);
        assert!(
            result.is_err(),
            "end_round without right player should fail"
        );
    }

    // ── call_next_for_side edge cases ───────────────────────────────────

    #[test]
    fn test_call_next_for_side_noop_when_seat_occupied() {
        let mut m = MachineState {
            id: 1,
            name: "test".into(),
            status: MachineStatus::FreePlay,
            left_player: Some(SeatPlayer {
                player_name: "A".into(),
                player_key: "a".into(),
            }),
            right_player: None,
            left_queue: vec![QueueEntry {
                ticket_id: 10,
                player_name: "B".into(),
                player_key: "b".into(),
                joined_unix_ms: 0,
            }],
            right_queue: vec![],
            called: None,
            last_round: None,
        };
        // call_next_for_side left — should NOT override seated player
        call_next_for_side(&mut m, Side::Left, 1000, 20000);
        assert!(m.called.is_none(), "should not call when seat is occupied");
        assert_eq!(m.status, MachineStatus::FreePlay);
    }

    #[test]
    fn test_day_key_division() {
        let day = day_key(86_400_001);
        assert_eq!(day, 1, "86_400_001 ms is day 1");
        assert_eq!(day_key(0), 0);
        assert_eq!(day_key(86_399_999), 0);
        assert_eq!(day_key(86_400_000), 1);
    }
}
