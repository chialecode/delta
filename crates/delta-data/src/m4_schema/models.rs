use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{parse_decimal_lossless, BarRecord, CorporateActionRecord};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::storage::{parse_rfc3339_utc, StorageError};

// ============================================================================
// Schema 5 Domain Persistence Records & Enums (`S-37`..`S-43`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeCurrencyMode {
    Quote,
    ReceivedAsset,
}

impl FeeCurrencyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Quote => "quote",
            Self::ReceivedAsset => "received_asset",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "quote" => Ok(Self::Quote),
            "received_asset" => Ok(Self::ReceivedAsset),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown fee_currency_mode '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeRoundingMode {
    HalfUp,
    HalfEven,
    Up,
    Down,
}

impl FeeRoundingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HalfUp => "half_up",
            Self::HalfEven => "half_even",
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "half_up" => Ok(Self::HalfUp),
            "half_even" => Ok(Self::HalfEven),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown fee rounding_mode '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeChargingTier {
    PerFill,
    PerOrder,
}

impl FeeChargingTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PerFill => "per_fill",
            Self::PerOrder => "per_order",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "per_fill" => Ok(Self::PerFill),
            "per_order" => Ok(Self::PerOrder),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown fee charging_tier '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlippageMode {
    None,
    Bps,
    Ticks,
}

impl SlippageMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bps => "bps",
            Self::Ticks => "ticks",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "none" => Ok(Self::None),
            "bps" => Ok(Self::Bps),
            "ticks" => Ok(Self::Ticks),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown slippage_mode '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeProfileVersionRecord {
    pub id: String,
    pub profile_key: String,
    pub version_num: i64,
    pub name: String,
    pub market_category: String,
    pub venue: Option<String>,
    pub notional_rate: Decimal,
    pub per_unit_fee: Decimal,
    pub fixed_fee_per_order: Decimal,
    pub min_fee_per_order: Option<Decimal>,
    pub max_fee_per_order: Option<Decimal>,
    pub fee_currency_mode: FeeCurrencyMode,
    pub rounding_mode: FeeRoundingMode,
    pub rounding_scale: u32,
    pub charging_tier: FeeChargingTier,
    pub slippage_mode: SlippageMode,
    pub slippage_value: Decimal,
    pub half_spread_bps: Decimal,
    pub rules_json: String,
    pub is_builtin: bool,
    pub is_scenario_assumption: bool,
    pub official_source_url: Option<String>,
    pub official_retrieved_at: Option<String>,
    pub assumption_note: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimRunStatus {
    Preparing,
    Active,
    Paused,
    Finished,
    Damaged,
    /// Schema 6 (`S-52`): ended by "give up" / "restart"; settled like `Finished`, not resumable.
    Abandoned,
}

impl SimRunStatus {
    /// Ended runs (finished or abandoned): reports, reveal and AI review are available.
    pub fn is_ended(&self) -> bool {
        matches!(self, Self::Finished | Self::Abandoned)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Abandoned => "abandoned",
            Self::Preparing => "preparing",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Finished => "finished",
            Self::Damaged => "damaged",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "preparing" => Ok(Self::Preparing),
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "finished" => Ok(Self::Finished),
            "damaged" => Ok(Self::Damaged),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown simulation_run status '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationRunRecord {
    pub run_id: String,
    pub kind: String,
    pub seed: u64,
    pub dataset_version: i64,
    pub engine_version: String,
    pub fee_profile_version_id: String,
    pub quote_currency: String,
    pub initial_cash: Decimal,
    pub status: SimRunStatus,
    pub parent_run_id: Option<String>,
    pub latest_seq: i64,
    pub latest_state_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedrawHistoryEntry {
    pub redraw_index: i64,
    pub previous_instrument_id: String,
    pub previous_train_start_at: String,
    pub next_instrument_id: String,
    pub next_train_start_at: String,
    pub redrawn_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingSessionRecord {
    pub run_id: String,
    pub hidden_mode: bool,
    pub market_category: String,
    pub display_symbol: String,
    pub real_instrument_id: String,
    pub candidate_pool_hash: String,
    pub redraw_count: i64,
    pub redraw_history: Vec<RedrawHistoryEntry>,
    pub warmup_start_at: DateTime<Utc>,
    pub history_start_at: DateTime<Utc>,
    pub train_start_at: DateTime<Utc>,
    pub train_end_at: DateTime<Utc>,
    pub reveal_end_at: DateTime<Utc>,
    pub visible_until: DateTime<Utc>,
    pub current_step_index: i64,
    pub total_steps: i64,
    pub revealed: bool,
    /// Schema 6 (`S-56`): frozen bar timeframe; `1d` for M4 rows.
    #[serde(default = "default_session_timeframe")]
    pub timeframe: String,
    /// Practice account funding this run (`S-52`); `None` = independent M4 capital.
    #[serde(default)]
    pub practice_account_id: Option<String>,
    /// Frozen fill-rule version (`S-51` ①); `None` for M4 runs.
    #[serde(default)]
    pub fill_rule_version: Option<String>,
}

fn default_session_timeframe() -> String {
    "1d".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimOrderStatus {
    Created,
    Accepted,
    Active,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
    Rejected,
}

impl SimOrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Accepted => "accepted",
            Self::Active => "active",
            Self::PartiallyFilled => "partially_filled",
            Self::Filled => "filled",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "created" => Ok(Self::Created),
            "accepted" => Ok(Self::Accepted),
            "active" => Ok(Self::Active),
            "partially_filled" => Ok(Self::PartiallyFilled),
            "filled" => Ok(Self::Filled),
            "cancelled" => Ok(Self::Cancelled),
            "expired" => Ok(Self::Expired),
            "rejected" => Ok(Self::Rejected),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown sim_order status '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimOrderRecord {
    pub order_id: String,
    pub run_id: String,
    pub instrument_id: String,
    pub order_seq: i64,
    pub order_type: String,
    pub side: String,
    pub tif: String,
    pub qty: Decimal,
    pub limit_price: Option<Decimal>,
    pub stop_price: Option<Decimal>,
    pub parent_group_id: Option<String>,
    pub group_role: Option<String>,
    pub status: SimOrderStatus,
    pub reject_code: Option<String>,
    pub cancel_reason: Option<String>,
    pub submitted_at: DateTime<Utc>,
    pub eligible_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimFillRecord {
    pub fill_id: String,
    pub run_id: String,
    pub order_id: String,
    pub instrument_id: String,
    pub bar_close_at: DateTime<Utc>,
    pub side: String,
    pub qty: Decimal,
    pub raw_price: Decimal,
    pub fill_price: Decimal,
    pub fee_amount: Decimal,
    pub fee_currency: String,
    pub ambiguous: bool,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationEventRecord {
    pub run_id: String,
    pub seq: i64,
    pub event_kind: String,
    pub command_json: String,
    pub events_json: String,
    pub visible_until: DateTime<Utc>,
    pub state_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimCheckpointReason {
    Initial,
    Periodic20,
    Pause,
    Finish,
}

impl SimCheckpointReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Periodic20 => "periodic_20",
            Self::Pause => "pause",
            Self::Finish => "finish",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, StorageError> {
        match raw.trim() {
            "initial" => Ok(Self::Initial),
            "periodic_20" => Ok(Self::Periodic20),
            "pause" => Ok(Self::Pause),
            "finish" => Ok(Self::Finish),
            other => Err(StorageError::IntegrityFailed(format!(
                "unknown sim_checkpoint reason '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimCheckpointRecord {
    pub run_id: String,
    pub seq: i64,
    pub reason: SimCheckpointReason,
    pub state_json: String,
    pub state_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingNoteRecord {
    pub note_id: String,
    pub run_id: String,
    pub bar_index: i64,
    pub visible_until: DateTime<Utc>,
    pub rationale: String,
    pub emotion_tag: Option<String>,
    pub linked_order_id: Option<String>,
    pub archived_journal_note_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

// ============================================================================
// Truncated Data Port & Recovery / Delete / Export Bundles
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingBarPortQuery {
    pub instrument_id: String,
    pub timeframe: String,
    pub source: Option<String>,
    pub dataset_version: i64,
    pub warmup_start_at: Option<DateTime<Utc>>,
    pub history_start_at: Option<DateTime<Utc>>,
    pub visible_until: DateTime<Utc>,
    pub previous_visible_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePoolQuery {
    pub dataset_version: i64,
    /// `"all"`, `"us_equity"`, or `"crypto_spot"`.
    pub market_category: String,
    pub instrument_id: Option<String>,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub warmup_bars: usize,
    pub history_len: usize,
    pub train_len: usize,
    pub reveal_len: usize,
    /// `"1d"` (all markets), `"4h"` / `"1h"` (crypto only, `S-50`). Absent in M4 metadata.
    #[serde(default = "default_pool_timeframe")]
    pub timeframe: String,
    #[serde(default)]
    pub quality_filter: CandidateQualityFilter,
}

fn default_pool_timeframe() -> String {
    "1d".to_string()
}

/// Which data-quality kinds exclude bars from a candidate pool (`S-49` ③).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CandidateQualityFilter {
    /// M4 rule kept for redrawing `m4-sim-1` runs: every recorded kind excludes its date.
    #[default]
    AllKinds,
    /// M5 rule: only kinds whose impact is `affects_price` exclude their date.
    PriceAffectingOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingCandidateSegment {
    pub instrument_id: String,
    pub venue: String,
    pub symbol: String,
    pub market_category: String,
    pub quote_currency: String,
    pub warmup_start_at: DateTime<Utc>,
    pub history_start_at: DateTime<Utc>,
    pub initial_visible_until: DateTime<Utc>,
    pub train_start_at: DateTime<Utc>,
    pub train_end_at: DateTime<Utc>,
    pub reveal_end_at: DateTime<Utc>,
    pub history_start_date: NaiveDate,
    pub train_start_date: NaiveDate,
    pub train_end_date: NaiveDate,
    pub reveal_end_date: NaiveDate,
    pub warmup_bar_count: usize,
    pub history_len: usize,
    pub train_len: usize,
    pub reveal_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePoolResult {
    pub dataset_version: i64,
    pub market_category: String,
    pub warmup_bars: usize,
    pub history_len: usize,
    pub train_len: usize,
    pub reveal_len: usize,
    pub candidate_pool_hash: String,
    pub candidates: Vec<TrainingCandidateSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostFinishRevealData {
    pub run_id: String,
    pub real_instrument_id: String,
    pub venue: String,
    pub real_symbol: String,
    pub dataset_version: i64,
    pub history_start_at: DateTime<Utc>,
    pub train_start_at: DateTime<Utc>,
    pub train_end_at: DateTime<Utc>,
    pub reveal_end_at: DateTime<Utc>,
    pub future_reveal_bars: Vec<BarRecord>,
    pub all_bars_through_reveal: Vec<BarRecord>,
    pub corporate_actions_through_reveal: Vec<CorporateActionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationStepCommit {
    pub run_id: String,
    pub expected_prev_seq: i64,
    pub seq: i64,
    pub event_kind: String,
    pub command_json: String,
    pub events_json: String,
    pub new_status: SimRunStatus,
    pub new_visible_until: DateTime<Utc>,
    pub new_step_index: i64,
    pub new_state_hash: String,
    pub orders_to_upsert: Vec<SimOrderRecord>,
    pub fills_to_insert: Vec<SimFillRecord>,
    pub notes_to_upsert: Vec<TrainingNoteRecord>,
    pub checkpoint: Option<SimCheckpointRecord>,
    pub committed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationRecoveryBundle {
    pub run: SimulationRunRecord,
    pub session: Option<TrainingSessionRecord>,
    pub fee_profile: FeeProfileVersionRecord,
    pub latest_checkpoint: Option<SimCheckpointRecord>,
    pub events_after_checkpoint: Vec<SimulationEventRecord>,
    pub all_events: Vec<SimulationEventRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimulationRecoveryOutcome {
    Recovered(Box<SimulationRecoveryBundle>),
    MarkedDamaged {
        run: Box<SimulationRunRecord>,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteTrainingRunPreview {
    pub run_id: String,
    pub status: SimRunStatus,
    pub parent_run_id: Option<String>,
    pub child_run_ids: Vec<String>,
    pub order_count: usize,
    pub fill_count: usize,
    pub note_count: usize,
    pub archived_journal_note_count: usize,
    pub checkpoint_count: usize,
    pub event_count: usize,
    /// Schema 6: drawings owned by the run (deleted with it).
    #[serde(default)]
    pub drawing_count: usize,
    /// Schema 6: practice-account entries of the run (kept; a reversal entry is appended).
    #[serde(default)]
    pub practice_entry_count: usize,
    /// Schema 6: amount of the reversal entry appended on delete (`None` = no practice funding).
    #[serde(default)]
    pub practice_reversal: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationExportBundle {
    pub fee_profile_versions: Vec<FeeProfileVersionRecord>,
    pub simulation_runs: Vec<SimulationRunRecord>,
    pub training_sessions: Vec<TrainingSessionRecord>,
    pub sim_orders: Vec<SimOrderRecord>,
    pub sim_fills: Vec<SimFillRecord>,
    pub simulation_events: Vec<SimulationEventRecord>,
    pub sim_checkpoints: Vec<SimCheckpointRecord>,
    pub training_notes: Vec<TrainingNoteRecord>,
    /// Schema 6 (`S-56`): practice accounts, their append-only entries and run drawings.
    #[serde(default)]
    pub practice_accounts: Vec<crate::m5_schema::PracticeAccountRecord>,
    #[serde(default)]
    pub practice_account_entries: Vec<crate::m5_schema::PracticeAccountEntryRecord>,
    #[serde(default)]
    pub training_drawings: Vec<crate::m5_schema::TrainingDrawingRecord>,
}

// ============================================================================
// Shared Row Mappers & Determinism Helpers
// ============================================================================

pub(crate) fn map_fee_profile_row(
    r: &rusqlite::Row<'_>,
) -> Result<FeeProfileVersionRecord, StorageError> {
    let notional_str: String = r.get(6)?;
    let per_unit_str: String = r.get(7)?;
    let fixed_str: String = r.get(8)?;
    let min_str: Option<String> = r.get(9)?;
    let max_str: Option<String> = r.get(10)?;
    let mode_str: String = r.get(11)?;
    let round_str: String = r.get(12)?;
    let scale_i64: i64 = r.get(13)?;
    let tier_str: String = r.get(14)?;
    let slip_mode_str: String = r.get(15)?;
    let slip_val_str: String = r.get(16)?;
    let spread_str: String = r.get(17)?;
    let is_builtin_i: i64 = r.get(19)?;
    let is_scen_i: i64 = r.get(20)?;

    Ok(FeeProfileVersionRecord {
        id: r.get(0)?,
        profile_key: r.get(1)?,
        version_num: r.get(2)?,
        name: r.get(3)?,
        market_category: r.get(4)?,
        venue: r.get(5)?,
        notional_rate: parse_decimal_lossless(&notional_str)?,
        per_unit_fee: parse_decimal_lossless(&per_unit_str)?,
        fixed_fee_per_order: parse_decimal_lossless(&fixed_str)?,
        min_fee_per_order: min_str.as_deref().map(parse_decimal_lossless).transpose()?,
        max_fee_per_order: max_str.as_deref().map(parse_decimal_lossless).transpose()?,
        fee_currency_mode: FeeCurrencyMode::parse(&mode_str)?,
        rounding_mode: FeeRoundingMode::parse(&round_str)?,
        rounding_scale: u32::try_from(scale_i64)
            .map_err(|_| StorageError::IntegrityFailed("negative rounding_scale".into()))?,
        charging_tier: FeeChargingTier::parse(&tier_str)?,
        slippage_mode: SlippageMode::parse(&slip_mode_str)?,
        slippage_value: parse_decimal_lossless(&slip_val_str)?,
        half_spread_bps: parse_decimal_lossless(&spread_str)?,
        rules_json: r.get(18)?,
        is_builtin: is_builtin_i != 0,
        is_scenario_assumption: is_scen_i != 0,
        official_source_url: r.get(21)?,
        official_retrieved_at: r.get(22)?,
        assumption_note: r.get(23)?,
        created_at: r.get(24)?,
    })
}

pub(crate) fn map_simulation_run_row(
    r: &rusqlite::Row<'_>,
) -> Result<SimulationRunRecord, StorageError> {
    let seed_str: String = r.get(2)?;
    let cash_str: String = r.get(7)?;
    let status_str: String = r.get(8)?;
    let seed = seed_str.parse::<u64>().map_err(|e| {
        StorageError::IntegrityFailed(format!("invalid run seed '{seed_str}': {e}"))
    })?;
    Ok(SimulationRunRecord {
        run_id: r.get(0)?,
        kind: r.get(1)?,
        seed,
        dataset_version: r.get(3)?,
        engine_version: r.get(4)?,
        fee_profile_version_id: r.get(5)?,
        quote_currency: r.get(6)?,
        initial_cash: parse_decimal_lossless(&cash_str)?,
        status: SimRunStatus::parse(&status_str)?,
        parent_run_id: r.get(9)?,
        latest_seq: r.get(10)?,
        latest_state_hash: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
        finished_at: r.get(14)?,
    })
}

pub(crate) fn map_training_session_row(
    r: &rusqlite::Row<'_>,
) -> Result<TrainingSessionRecord, StorageError> {
    let hidden_i: i64 = r.get(1)?;
    let redraw_json: String = r.get(7)?;
    let warmup_str: String = r.get(8)?;
    let hist_str: String = r.get(9)?;
    let train_start_str: String = r.get(10)?;
    let train_end_str: String = r.get(11)?;
    let reveal_end_str: String = r.get(12)?;
    let vis_str: String = r.get(13)?;
    let revealed_i: i64 = r.get(16)?;

    let redraw_history: Vec<RedrawHistoryEntry> = serde_json::from_str(&redraw_json)?;

    Ok(TrainingSessionRecord {
        run_id: r.get(0)?,
        hidden_mode: hidden_i != 0,
        market_category: r.get(2)?,
        display_symbol: r.get(3)?,
        real_instrument_id: r.get(4)?,
        candidate_pool_hash: r.get(5)?,
        redraw_count: r.get(6)?,
        redraw_history,
        warmup_start_at: parse_rfc3339_utc(&warmup_str, "warmup_start_at")?,
        history_start_at: parse_rfc3339_utc(&hist_str, "history_start_at")?,
        train_start_at: parse_rfc3339_utc(&train_start_str, "train_start_at")?,
        train_end_at: parse_rfc3339_utc(&train_end_str, "train_end_at")?,
        reveal_end_at: parse_rfc3339_utc(&reveal_end_str, "reveal_end_at")?,
        visible_until: parse_rfc3339_utc(&vis_str, "visible_until")?,
        current_step_index: r.get(14)?,
        total_steps: r.get(15)?,
        revealed: revealed_i != 0,
        timeframe: r.get(17)?,
        practice_account_id: r.get(18)?,
        fill_rule_version: r.get(19)?,
    })
}

pub(crate) fn map_sim_order_row(r: &rusqlite::Row<'_>) -> Result<SimOrderRecord, StorageError> {
    let qty_str: String = r.get(7)?;
    let lim_str: Option<String> = r.get(8)?;
    let stop_str: Option<String> = r.get(9)?;
    let status_str: String = r.get(12)?;
    let sub_str: String = r.get(15)?;
    let elig_str: Option<String> = r.get(16)?;
    let upd_str: String = r.get(17)?;

    Ok(SimOrderRecord {
        order_id: r.get(0)?,
        run_id: r.get(1)?,
        instrument_id: r.get(2)?,
        order_seq: r.get(3)?,
        order_type: r.get(4)?,
        side: r.get(5)?,
        tif: r.get(6)?,
        qty: parse_decimal_lossless(&qty_str)?,
        limit_price: lim_str.as_deref().map(parse_decimal_lossless).transpose()?,
        stop_price: stop_str
            .as_deref()
            .map(parse_decimal_lossless)
            .transpose()?,
        parent_group_id: r.get(10)?,
        group_role: r.get(11)?,
        status: SimOrderStatus::parse(&status_str)?,
        reject_code: r.get(13)?,
        cancel_reason: r.get(14)?,
        submitted_at: parse_rfc3339_utc(&sub_str, "submitted_at")?,
        eligible_at: elig_str
            .as_deref()
            .map(|s| parse_rfc3339_utc(s, "eligible_at"))
            .transpose()?,
        updated_at: parse_rfc3339_utc(&upd_str, "updated_at")?,
        payload_json: r.get(18)?,
    })
}

pub(crate) fn map_sim_fill_row(r: &rusqlite::Row<'_>) -> Result<SimFillRecord, StorageError> {
    let bar_close_str: String = r.get(4)?;
    let qty_str: String = r.get(6)?;
    let raw_px_str: String = r.get(7)?;
    let fill_px_str: String = r.get(8)?;
    let fee_amt_str: String = r.get(9)?;
    let ambig_i: i64 = r.get(11)?;

    Ok(SimFillRecord {
        fill_id: r.get(0)?,
        run_id: r.get(1)?,
        order_id: r.get(2)?,
        instrument_id: r.get(3)?,
        bar_close_at: parse_rfc3339_utc(&bar_close_str, "bar_close_at")?,
        side: r.get(5)?,
        qty: parse_decimal_lossless(&qty_str)?,
        raw_price: parse_decimal_lossless(&raw_px_str)?,
        fill_price: parse_decimal_lossless(&fill_px_str)?,
        fee_amount: parse_decimal_lossless(&fee_amt_str)?,
        fee_currency: r.get(10)?,
        ambiguous: ambig_i != 0,
        payload_json: r.get(12)?,
    })
}

pub(crate) fn map_simulation_event_row(
    r: &rusqlite::Row<'_>,
) -> Result<SimulationEventRecord, StorageError> {
    let vis_str: String = r.get(5)?;
    Ok(SimulationEventRecord {
        run_id: r.get(0)?,
        seq: r.get(1)?,
        event_kind: r.get(2)?,
        command_json: r.get(3)?,
        events_json: r.get(4)?,
        visible_until: parse_rfc3339_utc(&vis_str, "simulation_event.visible_until")?,
        state_hash: r.get(6)?,
        created_at: r.get(7)?,
    })
}

pub(crate) fn map_sim_checkpoint_row(
    r: &rusqlite::Row<'_>,
) -> Result<SimCheckpointRecord, StorageError> {
    let reason_str: String = r.get(2)?;
    Ok(SimCheckpointRecord {
        run_id: r.get(0)?,
        seq: r.get(1)?,
        reason: SimCheckpointReason::parse(&reason_str)?,
        state_json: r.get(3)?,
        state_hash: r.get(4)?,
        created_at: r.get(5)?,
    })
}

pub(crate) fn map_training_note_row(
    r: &rusqlite::Row<'_>,
) -> Result<TrainingNoteRecord, StorageError> {
    let vis_str: String = r.get(3)?;
    Ok(TrainingNoteRecord {
        note_id: r.get(0)?,
        run_id: r.get(1)?,
        bar_index: r.get(2)?,
        visible_until: parse_rfc3339_utc(&vis_str, "training_note.visible_until")?,
        rationale: r.get(4)?,
        emotion_tag: r.get(5)?,
        linked_order_id: r.get(6)?,
        archived_journal_note_id: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

pub(crate) fn source_priority(source: &str) -> u8 {
    match source.to_ascii_lowercase().as_str() {
        "yahoo" => 1,
        "tiingo" => 2,
        "eastmoney" => 3,
        "okx" => 4,
        "binance" => 5,
        _ => 10,
    }
}

/// Deduplicate multi-source bars deterministically by `source_priority`. Daily bars key on
/// `session_date` (unchanged since M4); intraday bars key on their UTC `open_at` (`S-50`).
pub fn dedupe_bars_by_session_date(bars: Vec<BarRecord>) -> Vec<BarRecord> {
    let mut by_date: BTreeMap<(NaiveDate, i64), BarRecord> = BTreeMap::new();
    for bar in bars {
        let key = (
            bar.session_date,
            if bar.timeframe == "1d" {
                0
            } else {
                bar.open_at.timestamp()
            },
        );
        match by_date.get(&key) {
            None => {
                by_date.insert(key, bar);
            }
            Some(existing) => {
                let new_prio = (
                    source_priority(&bar.source),
                    bar.source.as_str(),
                    bar.id.as_str(),
                );
                let cur_prio = (
                    source_priority(&existing.source),
                    existing.source.as_str(),
                    existing.id.as_str(),
                );
                if new_prio < cur_prio {
                    by_date.insert(key, bar);
                }
            }
        }
    }
    by_date.into_values().collect()
}

/// Return the matching common split ratio (`2, 3, 4, 5, 10, 20`) within 5% tolerance if
/// `prev_close -> curr_close` exhibits a split-like jump (forward or reverse).
pub(crate) fn detect_split_jump_ratio(prev_close: Decimal, curr_close: Decimal) -> Option<Decimal> {
    if prev_close <= Decimal::ZERO || curr_close <= Decimal::ZERO {
        return None;
    }
    let common_ratios = [
        Decimal::from(2),
        Decimal::from(3),
        Decimal::from(4),
        Decimal::from(5),
        Decimal::from(10),
        Decimal::from(20),
    ];
    let tol = Decimal::new(5, 2); // 5%
    let ratio = if prev_close >= curr_close {
        prev_close / curr_close
    } else {
        curr_close / prev_close
    };
    common_ratios
        .into_iter()
        .find(|cand| ((ratio - *cand) / *cand).abs() <= tol)
}

pub(crate) fn has_unexplained_split_jump(
    bars: &[BarRecord],
    split_dates: &BTreeSet<NaiveDate>,
) -> bool {
    for pair in bars.windows(2) {
        let prev = &pair[0];
        let curr = &pair[1];
        if split_dates.contains(&curr.session_date) {
            continue;
        }
        if detect_split_jump_ratio(prev.close, curr.close).is_some() {
            return true;
        }
    }
    false
}
