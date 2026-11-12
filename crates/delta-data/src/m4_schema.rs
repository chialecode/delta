#![allow(clippy::too_many_arguments)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use delta_core::{
    parse_decimal_lossless, to_canonical_text, AdjustmentKind, BarRecord, CalendarSessionKind,
    CorporateActionRecord, CorporateActionType, InstrumentKind,
};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::calendar::query_calendar_session;
use crate::m2_schema::{NoteDto, StructuredNoteFields};
use crate::storage::{SqliteStore, StorageError, CURRENT_SCHEMA_VERSION};

thread_local! {
    static FAIL_NEXT_SCHEMA_5_MIGRATION: Cell<bool> = const { Cell::new(false) };
    static FAIL_NEXT_SIM_STEP_COMMIT: Cell<bool> = const { Cell::new(false) };
}

pub fn set_schema_5_migration_fault_injection_for_test(fail: bool) {
    FAIL_NEXT_SCHEMA_5_MIGRATION.with(|flag| flag.set(fail));
}

pub(crate) fn swap_schema_5_migration_fault_for_test(fail: bool) -> bool {
    FAIL_NEXT_SCHEMA_5_MIGRATION.with(|flag| flag.replace(fail))
}

pub fn set_sim_step_fault_injection_for_test(fail: bool) {
    FAIL_NEXT_SIM_STEP_COMMIT.with(|flag| flag.set(fail));
}

/// Ordered in foreign-key dependency order (parents first; reverse order drops children first).
pub const SCHEMA_5_TABLES: &[&str] = &[
    "fee_profile_version",
    "simulation_run",
    "training_session",
    "sim_order",
    "sim_fill",
    "simulation_event",
    "sim_checkpoint",
    "training_note",
];

pub const BUILTIN_FEE_ZERO_COST_ID: &str = "FPV:BUILTIN:ZERO_COST:v1";
pub const BUILTIN_FEE_US_EQUITY_PER_SHARE_ID: &str = "FPV:BUILTIN:US_EQUITY_PER_SHARE_EXAMPLE:v1";
pub const BUILTIN_FEE_CRYPTO_RATE_ID: &str = "FPV:BUILTIN:CRYPTO_RATE_EXAMPLE:v1";

/// Apply all Schema 5 simulation & training DDL inside the caller's migration savepoint (`S-43`, `M4-A-11`).
pub fn apply_schema_5_ddl(tx: &Connection, from_schema: i64) -> Result<(), StorageError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS fee_profile_version (
            id TEXT PRIMARY KEY,
            profile_key TEXT NOT NULL,
            version_num INTEGER NOT NULL CHECK (version_num >= 1),
            name TEXT NOT NULL,
            market_category TEXT NOT NULL,
            venue TEXT,
            notional_rate TEXT NOT NULL,
            per_unit_fee TEXT NOT NULL,
            fixed_fee_per_order TEXT NOT NULL,
            min_fee_per_order TEXT,
            max_fee_per_order TEXT,
            fee_currency_mode TEXT NOT NULL CHECK (fee_currency_mode IN ('quote', 'received_asset')),
            rounding_mode TEXT NOT NULL CHECK (rounding_mode IN ('half_up', 'half_even', 'up', 'down')),
            rounding_scale INTEGER NOT NULL CHECK (rounding_scale >= 0 AND rounding_scale <= 18),
            charging_tier TEXT NOT NULL CHECK (charging_tier IN ('per_fill', 'per_order')),
            slippage_mode TEXT NOT NULL CHECK (slippage_mode IN ('none', 'bps', 'ticks')),
            slippage_value TEXT NOT NULL,
            half_spread_bps TEXT NOT NULL,
            rules_json TEXT NOT NULL,
            is_builtin INTEGER NOT NULL DEFAULT 0 CHECK (is_builtin IN (0, 1)),
            is_scenario_assumption INTEGER NOT NULL DEFAULT 1 CHECK (is_scenario_assumption IN (0, 1)),
            official_source_url TEXT,
            official_retrieved_at TEXT,
            assumption_note TEXT NOT NULL,
            created_at TEXT NOT NULL,
            UNIQUE (profile_key, version_num),
            CHECK (
                is_builtin = 0
                OR is_scenario_assumption = 1
                OR (
                    official_source_url IS NOT NULL
                    AND length(trim(official_source_url)) > 0
                    AND official_retrieved_at IS NOT NULL
                    AND length(trim(official_retrieved_at)) > 0
                )
            )
        );
        CREATE INDEX IF NOT EXISTS idx_fee_profile_key_ver
            ON fee_profile_version(profile_key, version_num DESC);

        CREATE TABLE IF NOT EXISTS simulation_run (
            run_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK (kind IN ('training', 'backtest')),
            seed TEXT NOT NULL,
            dataset_version INTEGER NOT NULL,
            engine_version TEXT NOT NULL,
            fee_profile_version_id TEXT NOT NULL,
            quote_currency TEXT NOT NULL,
            initial_cash TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('preparing', 'active', 'paused', 'finished', 'damaged')),
            parent_run_id TEXT,
            latest_seq INTEGER NOT NULL DEFAULT 0 CHECK (latest_seq >= 0),
            latest_state_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            finished_at TEXT,
            FOREIGN KEY (fee_profile_version_id) REFERENCES fee_profile_version(id)
        );
        CREATE INDEX IF NOT EXISTS idx_simulation_run_status_updated
            ON simulation_run(status, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_simulation_run_parent
            ON simulation_run(parent_run_id);
        CREATE INDEX IF NOT EXISTS idx_simulation_run_fee_profile
            ON simulation_run(fee_profile_version_id);

        CREATE TABLE IF NOT EXISTS training_session (
            run_id TEXT PRIMARY KEY,
            hidden_mode INTEGER NOT NULL CHECK (hidden_mode IN (0, 1)),
            market_category TEXT NOT NULL,
            display_symbol TEXT NOT NULL,
            real_instrument_id TEXT NOT NULL,
            candidate_pool_hash TEXT NOT NULL,
            redraw_count INTEGER NOT NULL DEFAULT 0 CHECK (redraw_count >= 0),
            redraw_history_json TEXT NOT NULL DEFAULT '[]',
            warmup_start_at TEXT NOT NULL,
            history_start_at TEXT NOT NULL,
            train_start_at TEXT NOT NULL,
            train_end_at TEXT NOT NULL,
            reveal_end_at TEXT NOT NULL,
            visible_until TEXT NOT NULL,
            current_step_index INTEGER NOT NULL DEFAULT 0 CHECK (current_step_index >= 0),
            total_steps INTEGER NOT NULL CHECK (total_steps >= 1),
            revealed INTEGER NOT NULL DEFAULT 0 CHECK (revealed IN (0, 1)),
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id)
        );

        CREATE TABLE IF NOT EXISTS sim_order (
            order_id TEXT NOT NULL,
            run_id TEXT NOT NULL,
            instrument_id TEXT NOT NULL,
            order_seq INTEGER NOT NULL CHECK (order_seq >= 1),
            order_type TEXT NOT NULL,
            side TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
            tif TEXT NOT NULL CHECK (tif IN ('day', 'gtc')),
            qty TEXT NOT NULL,
            limit_price TEXT,
            stop_price TEXT,
            parent_group_id TEXT,
            group_role TEXT,
            status TEXT NOT NULL CHECK (status IN ('created', 'accepted', 'active', 'partially_filled', 'filled', 'cancelled', 'expired', 'rejected')),
            reject_code TEXT,
            cancel_reason TEXT,
            submitted_at TEXT NOT NULL,
            eligible_at TEXT,
            updated_at TEXT NOT NULL,
            payload_json TEXT NOT NULL DEFAULT '{}',
            PRIMARY KEY (run_id, order_id),
            UNIQUE (run_id, order_seq),
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id)
        );
        CREATE INDEX IF NOT EXISTS idx_sim_order_run_status
            ON sim_order(run_id, status, order_seq);

        CREATE TABLE IF NOT EXISTS sim_fill (
            fill_id TEXT NOT NULL,
            run_id TEXT NOT NULL,
            order_id TEXT NOT NULL,
            instrument_id TEXT NOT NULL,
            bar_close_at TEXT NOT NULL,
            side TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
            qty TEXT NOT NULL,
            raw_price TEXT NOT NULL,
            fill_price TEXT NOT NULL,
            fee_amount TEXT NOT NULL,
            fee_currency TEXT NOT NULL,
            ambiguous INTEGER NOT NULL DEFAULT 0 CHECK (ambiguous IN (0, 1)),
            payload_json TEXT NOT NULL DEFAULT '{}',
            PRIMARY KEY (run_id, fill_id),
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id),
            FOREIGN KEY (run_id, order_id) REFERENCES sim_order(run_id, order_id)
        );
        CREATE INDEX IF NOT EXISTS idx_sim_fill_run_time
            ON sim_fill(run_id, bar_close_at, fill_id);

        CREATE TABLE IF NOT EXISTS simulation_event (
            run_id TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK (seq >= 1),
            event_kind TEXT NOT NULL,
            command_json TEXT NOT NULL,
            events_json TEXT NOT NULL,
            visible_until TEXT NOT NULL,
            state_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (run_id, seq),
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id)
        );

        CREATE TABLE IF NOT EXISTS sim_checkpoint (
            run_id TEXT NOT NULL,
            seq INTEGER NOT NULL CHECK (seq >= 0),
            reason TEXT NOT NULL CHECK (reason IN ('initial', 'periodic_20', 'pause', 'finish')),
            state_json TEXT NOT NULL,
            state_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (run_id, seq),
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id)
        );

        CREATE TABLE IF NOT EXISTS training_note (
            note_id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            bar_index INTEGER NOT NULL CHECK (bar_index >= 0),
            visible_until TEXT NOT NULL,
            rationale TEXT NOT NULL,
            emotion_tag TEXT,
            linked_order_id TEXT,
            archived_journal_note_id TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (run_id) REFERENCES simulation_run(run_id)
        );
        CREATE INDEX IF NOT EXISTS idx_training_note_run_bar
            ON training_note(run_id, bar_index, created_at);

        CREATE TRIGGER IF NOT EXISTS trg_fee_profile_version_immutable_update
        BEFORE UPDATE ON fee_profile_version
        WHEN OLD.is_builtin = 1
          OR EXISTS (SELECT 1 FROM simulation_run WHERE fee_profile_version_id = OLD.id)
        BEGIN
            SELECT RAISE(ABORT, 'fee_profile_version is immutable once built-in or referenced by simulation_run');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_fee_profile_version_immutable_delete
        BEFORE DELETE ON fee_profile_version
        WHEN OLD.is_builtin = 1
          OR EXISTS (SELECT 1 FROM simulation_run WHERE fee_profile_version_id = OLD.id)
        BEGIN
            SELECT RAISE(ABORT, 'fee_profile_version cannot be deleted once built-in or referenced by simulation_run');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_training_session_monotonic_guard
        BEFORE UPDATE ON training_session
        BEGIN
            SELECT CASE
                WHEN (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) != 'preparing'
                     AND (NEW.redraw_count != OLD.redraw_count OR NEW.real_instrument_id != OLD.real_instrument_id)
                THEN RAISE(ABORT, 'training_session segment cannot be redrawn after start')
                WHEN (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) != 'preparing'
                     AND (
                         NEW.current_step_index < OLD.current_step_index
                         OR strftime('%Y-%m-%dT%H:%M:%fZ', NEW.visible_until) < strftime('%Y-%m-%dT%H:%M:%fZ', OLD.visible_until)
                     )
                THEN RAISE(ABORT, 'training_session replay clock cannot move backwards')
                WHEN NEW.revealed = 1
                     AND (SELECT status FROM simulation_run WHERE run_id = OLD.run_id) != 'finished'
                THEN RAISE(ABORT, 'training_session can only be revealed after simulation_run is finished')
            END;
        END;
        "#,
    )?;

    seed_builtin_fee_profiles(tx)?;

    if from_schema < 5 && FAIL_NEXT_SCHEMA_5_MIGRATION.with(|flag| flag.replace(false)) {
        return Err(StorageError::MigrationFailed(
            "injected mid-migration fault before schema 5 commit".into(),
        ));
    }

    Ok(())
}

fn seed_builtin_fee_profiles(tx: &Connection) -> Result<(), StorageError> {
    let seeded_at = "2026-11-12T00:00:00+00:00";
    let builtins = [
        FeeProfileVersionRecord {
            id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
            profile_key: "zero_cost".to_string(),
            version_num: 1,
            name: "零成本（场景假设）".to_string(),
            market_category: "all".to_string(),
            venue: None,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::ZERO,
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage_mode: SlippageMode::None,
            slippage_value: Decimal::ZERO,
            half_spread_bps: Decimal::ZERO,
            rules_json: r#"{"template":"zero_cost","scenario_assumption":true}"#.to_string(),
            is_builtin: true,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note:
                "Scenario assumption template: zero commission, zero spread, zero slippage baseline."
                    .to_string(),
            created_at: seeded_at.to_string(),
        },
        FeeProfileVersionRecord {
            id: BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string(),
            profile_key: "us_equity_per_share_example".to_string(),
            version_num: 1,
            name: "美股按股收费示例（场景假设）".to_string(),
            market_category: "us_equity".to_string(),
            venue: None,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::new(5, 3), // 0.005 USD / share
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: Some(Decimal::ONE), // 1.00 USD min per order
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage_mode: SlippageMode::Ticks,
            slippage_value: Decimal::ONE,
            half_spread_bps: Decimal::from(2),
            rules_json: r#"{"template":"us_equity_per_share_example","scenario_assumption":true}"#
                .to_string(),
            is_builtin: true,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note:
                "Scenario assumption template (not an official broker fee schedule): 0.005 USD/share, 1.00 USD per-order minimum, 1 tick slippage, 2 bps half-spread."
                    .to_string(),
            created_at: seeded_at.to_string(),
        },
        FeeProfileVersionRecord {
            id: BUILTIN_FEE_CRYPTO_RATE_ID.to_string(),
            profile_key: "crypto_rate_example".to_string(),
            version_num: 1,
            name: "加密按成交额示例（场景假设）".to_string(),
            market_category: "crypto_spot".to_string(),
            venue: None,
            notional_rate: Decimal::new(1, 3), // 0.001 == 10 bps
            per_unit_fee: Decimal::ZERO,
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::Up,
            rounding_scale: 8,
            charging_tier: FeeChargingTier::PerFill,
            slippage_mode: SlippageMode::Bps,
            slippage_value: Decimal::from(5),
            half_spread_bps: Decimal::from(5),
            rules_json: r#"{"template":"crypto_rate_example","scenario_assumption":true}"#
                .to_string(),
            is_builtin: true,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note:
                "Scenario assumption template (not an official exchange fee schedule): 10 bps notional fee, 5 bps slippage, 5 bps half-spread."
                    .to_string(),
            created_at: seeded_at.to_string(),
        },
    ];

    for fp in &builtins {
        insert_fee_profile_on_conn(tx, fp, true)?;
    }
    Ok(())
}

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
}

impl SimRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
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
}

// ============================================================================
// Internal Parsing & Validation Helpers
// ============================================================================

fn parse_rfc3339_utc(raw: &str, field: &str) -> Result<DateTime<Utc>, StorageError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            StorageError::IntegrityFailed(format!("invalid {field} timestamp '{raw}': {e}"))
        })
}

fn validate_fee_profile(fp: &FeeProfileVersionRecord) -> Result<(), StorageError> {
    if fp.id.trim().is_empty() || fp.profile_key.trim().is_empty() || fp.name.trim().is_empty() {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version id, profile_key, and name must be non-empty".into(),
        ));
    }
    if fp.version_num < 1 {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version version_num must be >= 1".into(),
        ));
    }
    if fp.rounding_scale > 18 {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version rounding_scale must be <= 18".into(),
        ));
    }
    if fp.notional_rate < Decimal::ZERO
        || fp.per_unit_fee < Decimal::ZERO
        || fp.fixed_fee_per_order < Decimal::ZERO
        || fp.slippage_value < Decimal::ZERO
        || fp.half_spread_bps < Decimal::ZERO
    {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version fee/slippage/spread components must be non-negative".into(),
        ));
    }
    if let Some(min_fee) = fp.min_fee_per_order {
        if min_fee < Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "fee_profile_version min_fee_per_order must be non-negative".into(),
            ));
        }
        if let Some(max_fee) = fp.max_fee_per_order {
            if max_fee < min_fee {
                return Err(StorageError::IntegrityFailed(
                    "fee_profile_version max_fee_per_order cannot be less than min_fee_per_order"
                        .into(),
                ));
            }
        }
    }
    if let Some(max_fee) = fp.max_fee_per_order {
        if max_fee < Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "fee_profile_version max_fee_per_order must be non-negative".into(),
            ));
        }
    }
    if fp.assumption_note.trim().is_empty() {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version assumption_note must be non-empty".into(),
        ));
    }
    if fp.is_builtin && !fp.is_scenario_assumption {
        let has_url = fp
            .official_source_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_date = fp
            .official_retrieved_at
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        if !has_url || !has_date {
            return Err(StorageError::IntegrityFailed(
                "built-in platform fee preset requires official_source_url and official_retrieved_at (S-41)"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn insert_fee_profile_on_conn(
    conn: &Connection,
    fp: &FeeProfileVersionRecord,
    or_ignore: bool,
) -> Result<(), StorageError> {
    validate_fee_profile(fp)?;
    let verb = if or_ignore {
        "INSERT OR IGNORE"
    } else {
        "INSERT"
    };
    let sql = format!(
        "{verb} INTO fee_profile_version (
            id, profile_key, version_num, name, market_category, venue,
            notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
            fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
            slippage_mode, slippage_value, half_spread_bps, rules_json,
            is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
            assumption_note, created_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22, ?23,
            ?24, ?25
         )"
    );
    conn.execute(
        &sql,
        params![
            fp.id,
            fp.profile_key,
            fp.version_num,
            fp.name,
            fp.market_category,
            fp.venue,
            to_canonical_text(fp.notional_rate),
            to_canonical_text(fp.per_unit_fee),
            to_canonical_text(fp.fixed_fee_per_order),
            fp.min_fee_per_order.map(to_canonical_text),
            fp.max_fee_per_order.map(to_canonical_text),
            fp.fee_currency_mode.as_str(),
            fp.rounding_mode.as_str(),
            fp.rounding_scale,
            fp.charging_tier.as_str(),
            fp.slippage_mode.as_str(),
            to_canonical_text(fp.slippage_value),
            to_canonical_text(fp.half_spread_bps),
            fp.rules_json,
            if fp.is_builtin { 1 } else { 0 },
            if fp.is_scenario_assumption { 1 } else { 0 },
            fp.official_source_url,
            fp.official_retrieved_at,
            fp.assumption_note,
            fp.created_at,
        ],
    )?;
    Ok(())
}

fn map_fee_profile_row(r: &rusqlite::Row<'_>) -> Result<FeeProfileVersionRecord, StorageError> {
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

fn map_simulation_run_row(r: &rusqlite::Row<'_>) -> Result<SimulationRunRecord, StorageError> {
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

fn map_training_session_row(r: &rusqlite::Row<'_>) -> Result<TrainingSessionRecord, StorageError> {
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
    })
}

fn map_sim_order_row(r: &rusqlite::Row<'_>) -> Result<SimOrderRecord, StorageError> {
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

fn map_sim_fill_row(r: &rusqlite::Row<'_>) -> Result<SimFillRecord, StorageError> {
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

fn map_bar_row(r: &rusqlite::Row<'_>) -> Result<BarRecord, StorageError> {
    let s_date_str: String = r.get(4)?;
    let open_str: String = r.get(5)?;
    let close_str: String = r.get(6)?;
    let o_str: String = r.get(7)?;
    let h_str: String = r.get(8)?;
    let l_str: String = r.get(9)?;
    let c_str: String = r.get(10)?;
    let v_str: String = r.get(11)?;
    let adj_raw: String = r.get(13)?;
    let is_fin: i64 = r.get(14)?;

    let session_date = NaiveDate::parse_from_str(&s_date_str, "%Y-%m-%d").map_err(|e| {
        StorageError::IntegrityFailed(format!("invalid bar session_date '{s_date_str}': {e}"))
    })?;

    Ok(BarRecord {
        id: r.get(0)?,
        instrument_id: r.get(1)?,
        timeframe: r.get(2)?,
        session: r.get(3)?,
        session_date,
        open_at: parse_rfc3339_utc(&open_str, "bar.open_at")?,
        close_at: parse_rfc3339_utc(&close_str, "bar.close_at")?,
        open: parse_decimal_lossless(&o_str)?,
        high: parse_decimal_lossless(&h_str)?,
        low: parse_decimal_lossless(&l_str)?,
        close: parse_decimal_lossless(&c_str)?,
        volume: parse_decimal_lossless(&v_str)?,
        source: r.get(12)?,
        adjustment: AdjustmentKind::parse(&adj_raw).unwrap_or(AdjustmentKind::Raw),
        is_final: is_fin != 0,
        valid_from_version: r.get(15)?,
        valid_to_version: r.get(16)?,
        raw_payload_hash: r.get(17)?,
    })
}

fn map_corporate_action_row(r: &rusqlite::Row<'_>) -> Result<CorporateActionRecord, StorageError> {
    let act_type: String = r.get(2)?;
    let ex_str: String = r.get(3)?;
    let pay_str: Option<String> = r.get(4)?;
    let ratio_str: String = r.get(5)?;
    let ann_str: Option<String> = r.get(7)?;

    Ok(CorporateActionRecord {
        id: r.get(0)?,
        instrument_id: r.get(1)?,
        action_type: match act_type.as_str() {
            "split" => CorporateActionType::Split,
            "cash_dividend" => CorporateActionType::CashDividend,
            other => {
                return Err(StorageError::IntegrityFailed(format!(
                    "unknown corporate_action type '{other}'"
                )))
            }
        },
        ex_date: NaiveDate::parse_from_str(&ex_str, "%Y-%m-%d").map_err(|e| {
            StorageError::IntegrityFailed(format!(
                "invalid corporate_action ex_date '{ex_str}': {e}"
            ))
        })?,
        pay_date: pay_str
            .as_deref()
            .map(|s| {
                NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| {
                    StorageError::IntegrityFailed(format!(
                        "invalid corporate_action pay_date '{s}': {e}"
                    ))
                })
            })
            .transpose()?,
        ratio_or_amount: parse_decimal_lossless(&ratio_str)?,
        currency: r.get(6)?,
        announced_at: ann_str
            .as_deref()
            .map(|s| parse_rfc3339_utc(s, "corporate_action.announced_at"))
            .transpose()?,
        source: r.get(8)?,
        factor_version: r.get(9)?,
        raw_payload_hash: r.get(10)?,
    })
}

fn source_priority(source: &str) -> u8 {
    match source.to_ascii_lowercase().as_str() {
        "yahoo" => 1,
        "tiingo" => 2,
        "eastmoney" => 3,
        "okx" => 4,
        "binance" => 5,
        _ => 10,
    }
}

fn dedupe_bars_by_session_date(bars: Vec<BarRecord>) -> Vec<BarRecord> {
    let mut by_date: BTreeMap<NaiveDate, BarRecord> = BTreeMap::new();
    for bar in bars {
        match by_date.get(&bar.session_date) {
            None => {
                by_date.insert(bar.session_date, bar);
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
                    by_date.insert(bar.session_date, bar);
                }
            }
        }
    }
    by_date.into_values().collect()
}

fn has_unexplained_split_jump(bars: &[BarRecord], split_dates: &BTreeSet<NaiveDate>) -> bool {
    let common_ratios = [
        Decimal::from(2),
        Decimal::from(3),
        Decimal::from(4),
        Decimal::from(5),
        Decimal::from(10),
        Decimal::from(20),
    ];
    let tol = Decimal::new(5, 2); // 5%
    for pair in bars.windows(2) {
        let prev = &pair[0];
        let curr = &pair[1];
        if split_dates.contains(&curr.session_date)
            || prev.close <= Decimal::ZERO
            || curr.close <= Decimal::ZERO
        {
            continue;
        }
        let fwd_ratio = prev.close / curr.close;
        let rev_ratio = curr.close / prev.close;
        for cand in common_ratios {
            let fwd_rel = ((fwd_ratio - cand) / cand).abs();
            let rev_rel = ((rev_ratio - cand) / cand).abs();
            if fwd_rel <= tol || rev_rel <= tol {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// SqliteStore M4 Schema 5 & Truncated Training Data Port Implementation
// ============================================================================

impl SqliteStore {
    /// Downgrade an existing initialized library on disk to genuine v0.1.1 Schema 4
    /// (dropping all Schema 5 tables/triggers, setting `schema_version = 4`, clearing
    /// prior `before-schema-*` backups, and syncing `manifest.json`) so `M4-A-11` can
    /// populate real ledger events, notes, and AI sessions first and then exercise
    /// the real 4 -> 5 migration path!
    pub fn downgrade_to_schema_4_for_test(&mut self) -> Result<(), StorageError> {
        let tx = self.conn_mut().savepoint()?;
        tx.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS trg_training_session_monotonic_guard;
            DROP TRIGGER IF EXISTS trg_fee_profile_version_immutable_delete;
            DROP TRIGGER IF EXISTS trg_fee_profile_version_immutable_update;
            "#,
        )?;
        for table in SCHEMA_5_TABLES.iter().rev() {
            tx.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }
        tx.execute("UPDATE library SET schema_version = 4", [])?;
        tx.commit()?;

        let backups_dir = self.root_dir().join("backups");
        if backups_dir.is_dir() {
            for entry in fs::read_dir(&backups_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("before-schema-") {
                    let path = entry.path();
                    if path.is_dir() {
                        fs::remove_dir_all(path)?;
                    }
                }
            }
        }
        self.sync_manifest()?;
        Ok(())
    }

    /// Create an empty genuine Schema 4 library on disk for `M4-A-11` migration testing.
    pub fn create_schema_4_library_for_test(
        root_dir: &Path,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<(), StorageError> {
        let prev_fault = swap_schema_5_migration_fault_for_test(false);
        let result = (|| -> Result<(), StorageError> {
            let mut store = Self::open_or_init(root_dir, book_currency, report_currency, timezone)?;
            store.downgrade_to_schema_4_for_test()?;
            Ok(())
        })();
        let _ = swap_schema_5_migration_fault_for_test(prev_fault);
        result
    }

    /// List which Schema 5 tables are currently present in `sqlite_master` on disk (`M4-A-11`).
    pub fn list_schema_5_tables_on_disk(root_dir: &Path) -> Result<Vec<String>, StorageError> {
        let db_path = root_dir.join("delta.sqlite");
        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut present = Vec::new();
        for table in SCHEMA_5_TABLES {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1 AND type = 'table')",
                params![table],
                |r| r.get(0),
            )?;
            if exists {
                present.push((*table).to_string());
            }
        }
        Ok(present)
    }

    /// List pre-migration consistency backup directories created before upgrading from `from_schema`.
    pub fn list_pre_migration_backups(
        root_dir: &Path,
        from_schema: i64,
    ) -> Result<Vec<PathBuf>, StorageError> {
        let prefix = format!("before-schema-{from_schema}-");
        let backups_dir = root_dir.join("backups");
        if !backups_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&backups_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && entry.file_name().to_string_lossy().starts_with(&prefix) {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Check library `schema_version` compatibility against an explicit `max_supported_schema`
    /// (used to verify that v0.1.1 Schema 4 reader logic clearly refuses a Schema 5 library, `M4-A-11`).
    pub fn check_schema_compatibility_at_version(
        root_dir: &Path,
        max_supported_schema: i64,
    ) -> Result<i64, StorageError> {
        let schema = Self::verify_schema_version_on_disk(root_dir)?;
        if schema > max_supported_schema {
            return Err(StorageError::MigrationFailed(
                "database schema is newer than this build".into(),
            ));
        }
        Ok(schema)
    }

    /// Read current `library.ledger_revision` to verify training operations never touch real ledger state (`M4-A-11`).
    pub fn query_ledger_revision(&self) -> Result<i64, StorageError> {
        self.conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .map_err(StorageError::from)
    }

    fn ensure_known_dataset_version(&self, dataset_version: i64) -> Result<(), StorageError> {
        if dataset_version <= 0 {
            return Err(StorageError::IntegrityFailed(
                "dataset_version must be positive".into(),
            ));
        }
        let exists: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM dataset_version WHERE version_num = ?1)",
            params![dataset_version],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StorageError::IntegrityFailed(format!(
                "unknown dataset_version {dataset_version}"
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------------
    // 1. Fee Profile Versioning & Freezing (`S-41`, `M4-A-08`)
    // ------------------------------------------------------------------------

    pub fn insert_fee_profile_version(
        &mut self,
        fp: &FeeProfileVersionRecord,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        insert_fee_profile_on_conn(self.conn(), fp, false)?;
        self.get_fee_profile_version(&fp.id)
    }

    /// Editing an existing fee profile creates a new immutable version (`version_num = max + 1`)
    /// under the same `profile_key` while leaving existing versions untouched (`S-41`, `M4-A-08`).
    pub fn create_next_fee_profile_version(
        &mut self,
        mut draft: FeeProfileVersionRecord,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        if draft.profile_key.trim().is_empty() {
            return Err(StorageError::IntegrityFailed(
                "profile_key must be non-empty".into(),
            ));
        }
        let tx = self.conn_mut().savepoint()?;
        let next_ver: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version_num), 0) + 1 FROM fee_profile_version WHERE profile_key = ?1",
            params![draft.profile_key],
            |r| r.get(0),
        )?;
        draft.version_num = next_ver;
        draft.is_builtin = false;
        draft.id = format!(
            "FPV:{}:v{}",
            draft.profile_key.trim().to_ascii_uppercase(),
            next_ver
        );
        if draft.created_at.trim().is_empty() {
            draft.created_at = Utc::now().to_rfc3339();
        }
        insert_fee_profile_on_conn(&tx, &draft, false)?;
        tx.commit()?;
        self.get_fee_profile_version(&draft.id)
    }

    /// Attempting to mutate an in-use or built-in `fee_profile_version` in place is strictly rejected (`M4-A-08`).
    /// An unreferenced non-builtin draft may be updated only before any `simulation_run` references it.
    pub fn mutate_fee_profile_version_in_place(
        &mut self,
        fp: &FeeProfileVersionRecord,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        validate_fee_profile(fp)?;
        let existing = self.get_fee_profile_version(&fp.id)?;
        if existing.is_builtin {
            return Err(StorageError::IntegrityFailed(format!(
                "built-in fee_profile_version '{}' is immutable; create a new profile version instead",
                fp.id
            )));
        }
        let in_use: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM simulation_run WHERE fee_profile_version_id = ?1)",
            params![fp.id],
            |r| r.get(0),
        )?;
        if in_use {
            return Err(StorageError::IntegrityFailed(format!(
                "fee_profile_version '{}' is referenced by an existing simulation_run and is immutable; create a new version instead",
                fp.id
            )));
        }
        self.conn().execute(
            "UPDATE fee_profile_version
             SET name = ?2,
                 market_category = ?3,
                 venue = ?4,
                 notional_rate = ?5,
                 per_unit_fee = ?6,
                 fixed_fee_per_order = ?7,
                 min_fee_per_order = ?8,
                 max_fee_per_order = ?9,
                 fee_currency_mode = ?10,
                 rounding_mode = ?11,
                 rounding_scale = ?12,
                 charging_tier = ?13,
                 slippage_mode = ?14,
                 slippage_value = ?15,
                 half_spread_bps = ?16,
                 rules_json = ?17,
                 is_scenario_assumption = ?18,
                 official_source_url = ?19,
                 official_retrieved_at = ?20,
                 assumption_note = ?21
             WHERE id = ?1",
            params![
                fp.id,
                fp.name,
                fp.market_category,
                fp.venue,
                to_canonical_text(fp.notional_rate),
                to_canonical_text(fp.per_unit_fee),
                to_canonical_text(fp.fixed_fee_per_order),
                fp.min_fee_per_order.map(to_canonical_text),
                fp.max_fee_per_order.map(to_canonical_text),
                fp.fee_currency_mode.as_str(),
                fp.rounding_mode.as_str(),
                fp.rounding_scale,
                fp.charging_tier.as_str(),
                fp.slippage_mode.as_str(),
                to_canonical_text(fp.slippage_value),
                to_canonical_text(fp.half_spread_bps),
                fp.rules_json,
                if fp.is_scenario_assumption { 1 } else { 0 },
                fp.official_source_url,
                fp.official_retrieved_at,
                fp.assumption_note,
            ],
        )?;
        self.get_fee_profile_version(&fp.id)
    }

    pub fn delete_fee_profile_version(&mut self, id: &str) -> Result<(), StorageError> {
        let existing = self.get_fee_profile_version(id)?;
        if existing.is_builtin {
            return Err(StorageError::IntegrityFailed(format!(
                "built-in fee_profile_version '{id}' cannot be deleted"
            )));
        }
        let in_use: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM simulation_run WHERE fee_profile_version_id = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if in_use {
            return Err(StorageError::IntegrityFailed(format!(
                "fee_profile_version '{id}' is referenced by a simulation_run and cannot be deleted"
            )));
        }
        self.conn()
            .execute("DELETE FROM fee_profile_version WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get_fee_profile_version(
        &self,
        id: &str,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT id, profile_key, version_num, name, market_category, venue,
                        notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
                        fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
                        slippage_mode, slippage_value, half_spread_bps, rules_json,
                        is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
                        assumption_note, created_at
                 FROM fee_profile_version
                 WHERE id = ?1",
                params![id],
                |r| Ok(map_fee_profile_row(r)),
            )
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "fee_profile_version '{id}' not found"
            ))),
        }
    }

    pub fn list_fee_profile_versions(&self) -> Result<Vec<FeeProfileVersionRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT id, profile_key, version_num, name, market_category, venue,
                    notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
                    fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
                    slippage_mode, slippage_value, half_spread_bps, rules_json,
                    is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
                    assumption_note, created_at
             FROM fee_profile_version
             ORDER BY is_builtin DESC, profile_key ASC, version_num DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok(map_fee_profile_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------------
    // 2. Simulation Run & Training Session Lifecycle (`S-38`, `S-43`)
    // ------------------------------------------------------------------------

    fn validate_training_session_bounds(
        session: &TrainingSessionRecord,
    ) -> Result<(), StorageError> {
        if session.total_steps < 1 || session.current_step_index < 0 {
            return Err(StorageError::IntegrityFailed(
                "training_session total_steps must be >= 1 and current_step_index >= 0".into(),
            ));
        }
        if session.current_step_index > session.total_steps {
            return Err(StorageError::IntegrityFailed(
                "training_session current_step_index cannot exceed total_steps".into(),
            ));
        }
        if session.warmup_start_at > session.history_start_at {
            return Err(StorageError::IntegrityFailed(
                "training_session warmup_start_at cannot exceed history_start_at".into(),
            ));
        }
        if session.history_start_at > session.visible_until {
            return Err(StorageError::IntegrityFailed(
                "training_session history_start_at cannot exceed visible_until".into(),
            ));
        }
        if session.train_start_at > session.train_end_at {
            return Err(StorageError::IntegrityFailed(
                "training_session train_start_at cannot exceed train_end_at".into(),
            ));
        }
        if session.train_end_at > session.reveal_end_at {
            return Err(StorageError::IntegrityFailed(
                "training_session train_end_at cannot exceed reveal_end_at".into(),
            ));
        }
        Ok(())
    }

    /// Persist a newly created `simulation_run` (`kind = "training"`) + `training_session`
    /// (and optional initial `sim_checkpoint` at `seq = 0`) in one atomic transaction.
    /// Per `S-38`, `seed` and `candidate_pool_hash` are persisted immediately on creation.
    pub fn create_training_run(
        &mut self,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
        initial_checkpoint: Option<&SimCheckpointRecord>,
    ) -> Result<(SimulationRunRecord, TrainingSessionRecord), StorageError> {
        if run.run_id.trim().is_empty() || run.run_id != session.run_id {
            return Err(StorageError::IntegrityFailed(
                "run_id must be non-empty and match training_session.run_id".into(),
            ));
        }
        if run.kind != "training" {
            return Err(StorageError::IntegrityFailed(
                "create_training_run requires kind = 'training'".into(),
            ));
        }
        if run.initial_cash <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "simulation_run initial_cash must be positive".into(),
            ));
        }
        if run.latest_state_hash.trim().is_empty() {
            return Err(StorageError::IntegrityFailed(
                "simulation_run latest_state_hash must be non-empty".into(),
            ));
        }
        if session.revealed && run.status != SimRunStatus::Finished {
            return Err(StorageError::IntegrityFailed(
                "training_session cannot start revealed before run is finished".into(),
            ));
        }
        self.ensure_known_dataset_version(run.dataset_version)?;
        let _ = self.get_fee_profile_version(&run.fee_profile_version_id)?;
        Self::validate_training_session_bounds(session)?;

        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO simulation_run (
                run_id, kind, seed, dataset_version, engine_version,
                fee_profile_version_id, quote_currency, initial_cash, status,
                parent_run_id, latest_seq, latest_state_hash,
                created_at, updated_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                run.run_id,
                run.kind,
                run.seed.to_string(),
                run.dataset_version,
                run.engine_version,
                run.fee_profile_version_id,
                run.quote_currency.to_ascii_uppercase(),
                to_canonical_text(run.initial_cash),
                run.status.as_str(),
                run.parent_run_id,
                run.latest_seq,
                run.latest_state_hash,
                run.created_at,
                run.updated_at,
                run.finished_at,
            ],
        )?;

        let redraw_json = serde_json::to_string(&session.redraw_history)?;
        tx.execute(
            "INSERT INTO training_session (
                run_id, hidden_mode, market_category, display_symbol, real_instrument_id,
                candidate_pool_hash, redraw_count, redraw_history_json,
                warmup_start_at, history_start_at, train_start_at, train_end_at, reveal_end_at,
                visible_until, current_step_index, total_steps, revealed
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                session.run_id,
                if session.hidden_mode { 1 } else { 0 },
                session.market_category,
                session.display_symbol,
                session.real_instrument_id,
                session.candidate_pool_hash,
                session.redraw_count,
                redraw_json,
                session.warmup_start_at.to_rfc3339(),
                session.history_start_at.to_rfc3339(),
                session.train_start_at.to_rfc3339(),
                session.train_end_at.to_rfc3339(),
                session.reveal_end_at.to_rfc3339(),
                session.visible_until.to_rfc3339(),
                session.current_step_index,
                session.total_steps,
                if session.revealed { 1 } else { 0 },
            ],
        )?;

        if let Some(cp) = initial_checkpoint {
            if cp.run_id != run.run_id || cp.seq != run.latest_seq {
                return Err(StorageError::IntegrityFailed(
                    "initial sim_checkpoint must match run_id and latest_seq".into(),
                ));
            }
            tx.execute(
                "INSERT INTO sim_checkpoint (run_id, seq, reason, state_json, state_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    cp.run_id,
                    cp.seq,
                    cp.reason.as_str(),
                    cp.state_json,
                    cp.state_hash,
                    cp.created_at,
                ],
            )?;
        }

        tx.commit()?;
        Ok((
            self.get_simulation_run(&run.run_id)?,
            self.get_training_session(&run.run_id)?,
        ))
    }

    pub fn get_simulation_run(&self, run_id: &str) -> Result<SimulationRunRecord, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT run_id, kind, seed, dataset_version, engine_version,
                        fee_profile_version_id, quote_currency, initial_cash, status,
                        parent_run_id, latest_seq, latest_state_hash,
                        created_at, updated_at, finished_at
                 FROM simulation_run
                 WHERE run_id = ?1",
                params![run_id],
                |r| Ok(map_simulation_run_row(r)),
            )
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "simulation_run '{run_id}' not found"
            ))),
        }
    }

    pub fn list_simulation_runs(&self) -> Result<Vec<SimulationRunRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT run_id, kind, seed, dataset_version, engine_version,
                    fee_profile_version_id, quote_currency, initial_cash, status,
                    parent_run_id, latest_seq, latest_state_hash,
                    created_at, updated_at, finished_at
             FROM simulation_run
             ORDER BY created_at DESC, run_id ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok(map_simulation_run_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn get_training_session(
        &self,
        run_id: &str,
    ) -> Result<TrainingSessionRecord, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT run_id, hidden_mode, market_category, display_symbol, real_instrument_id,
                        candidate_pool_hash, redraw_count, redraw_history_json,
                        warmup_start_at, history_start_at, train_start_at, train_end_at, reveal_end_at,
                        visible_until, current_step_index, total_steps, revealed
                 FROM training_session
                 WHERE run_id = ?1",
                params![run_id],
                |r| Ok(map_training_session_row(r)),
            )
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "training_session '{run_id}' not found"
            ))),
        }
    }

    /// Explicitly redraw segment BEFORE start (`status == 'preparing'`, `S-38`, `M4-A-02`).
    /// Increments `redraw_count`, appends to `redraw_history_json`, and updates segment bounds.
    /// Once `status != 'preparing'`, redraw is strictly refused!
    pub fn redraw_training_session_before_start(
        &mut self,
        run_id: &str,
        next_segment: &TrainingCandidateSegment,
        next_display_symbol: &str,
        new_state_hash: &str,
        redrawn_at: DateTime<Utc>,
    ) -> Result<TrainingSessionRecord, StorageError> {
        self.redraw_training_session_with_checkpoint(
            run_id,
            next_segment,
            next_display_symbol,
            None,
            new_state_hash,
            redrawn_at,
        )
    }

    /// Atomically redraw a `'preparing'` training session, updating `training_session` (including
    /// `market_category`), `simulation_run`, and optional initial `sim_checkpoint` (`seq = 0`)
    /// inside a single SQLite savepoint (`S-38`, `M4-A-02`, `M4-A-09`).
    pub fn redraw_training_session_with_checkpoint(
        &mut self,
        run_id: &str,
        next_segment: &TrainingCandidateSegment,
        next_display_symbol: &str,
        new_initial_checkpoint_json: Option<&str>,
        new_state_hash: &str,
        redrawn_at: DateTime<Utc>,
    ) -> Result<TrainingSessionRecord, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if run.status != SimRunStatus::Preparing {
            return Err(StorageError::IntegrityFailed(format!(
                "cannot redraw training_session '{run_id}' once started (status = '{}')",
                run.status.as_str()
            )));
        }
        let mut session = self.get_training_session(run_id)?;
        let next_redraw_index = session.redraw_count + 1;
        session.redraw_history.push(RedrawHistoryEntry {
            redraw_index: next_redraw_index,
            previous_instrument_id: session.real_instrument_id.clone(),
            previous_train_start_at: session.train_start_at.to_rfc3339(),
            next_instrument_id: next_segment.instrument_id.clone(),
            next_train_start_at: next_segment.train_start_at.to_rfc3339(),
            redrawn_at: redrawn_at.to_rfc3339(),
        });
        session.redraw_count = next_redraw_index;
        session.market_category = next_segment.market_category.clone();
        session.display_symbol = next_display_symbol.to_string();
        session.real_instrument_id = next_segment.instrument_id.clone();
        session.warmup_start_at = next_segment.warmup_start_at;
        session.history_start_at = next_segment.history_start_at;
        session.train_start_at = next_segment.train_start_at;
        session.train_end_at = next_segment.train_end_at;
        session.reveal_end_at = next_segment.reveal_end_at;
        session.visible_until = next_segment.initial_visible_until;
        session.current_step_index = 0;
        session.total_steps = i64::try_from(next_segment.train_len)
            .map_err(|_| StorageError::IntegrityFailed("train_len overflow".into()))?;
        Self::validate_training_session_bounds(&session)?;

        let redraw_json = serde_json::to_string(&session.redraw_history)?;
        let now_rfc3339 = redrawn_at.to_rfc3339();
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "UPDATE training_session
             SET display_symbol = ?2,
                 real_instrument_id = ?3,
                 redraw_count = ?4,
                 redraw_history_json = ?5,
                 warmup_start_at = ?6,
                 history_start_at = ?7,
                 train_start_at = ?8,
                 train_end_at = ?9,
                 reveal_end_at = ?10,
                 visible_until = ?11,
                 current_step_index = 0,
                 total_steps = ?12,
                 market_category = ?13
             WHERE run_id = ?1",
            params![
                run_id,
                session.display_symbol,
                session.real_instrument_id,
                session.redraw_count,
                redraw_json,
                session.warmup_start_at.to_rfc3339(),
                session.history_start_at.to_rfc3339(),
                session.train_start_at.to_rfc3339(),
                session.train_end_at.to_rfc3339(),
                session.reveal_end_at.to_rfc3339(),
                session.visible_until.to_rfc3339(),
                session.total_steps,
                session.market_category,
            ],
        )?;
        tx.execute(
            "UPDATE simulation_run
             SET quote_currency = ?2,
                 latest_state_hash = ?3,
                 updated_at = ?4
             WHERE run_id = ?1",
            params![
                run_id,
                next_segment.quote_currency.to_ascii_uppercase(),
                new_state_hash,
                now_rfc3339,
            ],
        )?;
        if let Some(state_json) = new_initial_checkpoint_json {
            tx.execute(
                "INSERT OR REPLACE INTO sim_checkpoint (run_id, seq, reason, state_json, state_hash, created_at)
                 VALUES (?1, 0, 'initial', ?2, ?3, ?4)",
                params![run_id, state_json, new_state_hash, now_rfc3339],
            )?;
        }
        tx.commit()?;
        self.get_training_session(run_id)
    }

    /// Transition a `'preparing'` run to `'active'` (locking redraws permanently, `S-38`).
    pub fn start_training_run(
        &mut self,
        run_id: &str,
        started_at: DateTime<Utc>,
    ) -> Result<SimulationRunRecord, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if run.status != SimRunStatus::Preparing {
            return Err(StorageError::IntegrityFailed(format!(
                "simulation_run '{run_id}' cannot transition from '{}' to 'active' via start_training_run",
                run.status.as_str()
            )));
        }
        self.conn().execute(
            "UPDATE simulation_run SET status = 'active', updated_at = ?2 WHERE run_id = ?1",
            params![run_id, started_at.to_rfc3339()],
        )?;
        self.get_simulation_run(run_id)
    }

    /// Retry an existing training run by creating a brand new run referencing `original_run_id`
    /// (`parent_run_id = Some(original_run_id)`) with identical sampled segment, frozen dataset/fee/engine
    /// versions, and initial cash, reset to initial replay clock (`S-43`, `M4-A-10`).
    /// The original run, its orders, fills, notes, and events remain 100% untouched.
    pub fn create_retry_training_run(
        &mut self,
        original_run_id: &str,
        new_run_id: &str,
        initial_visible_until: DateTime<Utc>,
        initial_state_hash: &str,
        initial_checkpoint: Option<&SimCheckpointRecord>,
        created_at: DateTime<Utc>,
    ) -> Result<(SimulationRunRecord, TrainingSessionRecord), StorageError> {
        if original_run_id == new_run_id {
            return Err(StorageError::IntegrityFailed(
                "retry run must have a new distinct run_id".into(),
            ));
        }
        let orig_run = self.get_simulation_run(original_run_id)?;
        let orig_session = self.get_training_session(original_run_id)?;
        if initial_visible_until < orig_session.history_start_at
            || initial_visible_until > orig_session.train_start_at
        {
            return Err(StorageError::IntegrityFailed(
                "retry initial_visible_until must lie between history_start_at and train_start_at"
                    .into(),
            ));
        }

        let now_str = created_at.to_rfc3339();
        let new_run = SimulationRunRecord {
            run_id: new_run_id.to_string(),
            kind: "training".to_string(),
            seed: orig_run.seed,
            dataset_version: orig_run.dataset_version,
            engine_version: orig_run.engine_version,
            fee_profile_version_id: orig_run.fee_profile_version_id,
            quote_currency: orig_run.quote_currency,
            initial_cash: orig_run.initial_cash,
            status: SimRunStatus::Active,
            parent_run_id: Some(orig_run.run_id),
            latest_seq: 0,
            latest_state_hash: initial_state_hash.to_string(),
            created_at: now_str.clone(),
            updated_at: now_str,
            finished_at: None,
        };
        let new_session = TrainingSessionRecord {
            run_id: new_run_id.to_string(),
            hidden_mode: orig_session.hidden_mode,
            market_category: orig_session.market_category,
            display_symbol: orig_session.display_symbol,
            real_instrument_id: orig_session.real_instrument_id,
            candidate_pool_hash: orig_session.candidate_pool_hash,
            redraw_count: 0,
            redraw_history: Vec::new(),
            warmup_start_at: orig_session.warmup_start_at,
            history_start_at: orig_session.history_start_at,
            train_start_at: orig_session.train_start_at,
            train_end_at: orig_session.train_end_at,
            reveal_end_at: orig_session.reveal_end_at,
            visible_until: initial_visible_until,
            current_step_index: 0,
            total_steps: orig_session.total_steps,
            revealed: false,
        };

        self.create_training_run(&new_run, &new_session, initial_checkpoint)
    }

    // ------------------------------------------------------------------------
    // 3. SQL-Level Truncated Training Data Port Queries (`S-37`, `M4-K01`, `M4-A-01`, `M4-A-02`)
    // ------------------------------------------------------------------------

    /// SQL-level truncated bar query (`S-37`, `M4-K01`, `M4-A-01`):
    /// - Enforces clock monotonicity (`visible_until >= previous_visible_until`).
    /// - Enforces `warmup_start_at <= history_start_at <= visible_until`.
    /// - Binds `dataset_version`, `is_final = 1`, optional `warmup_start_at`, and
    ///   `close_at <= ?visible_until` directly in SQLite's `WHERE` clause so future bars
    ///   (including extreme or poison sentinels) are NEVER read from SQLite!
    pub fn query_training_visible_final_bars(
        &self,
        query: &TrainingBarPortQuery,
    ) -> Result<Vec<BarRecord>, StorageError> {
        self.ensure_known_dataset_version(query.dataset_version)?;

        if let Some(prev) = query.previous_visible_until {
            if query.visible_until < prev {
                return Err(StorageError::IntegrityFailed(
                    "replay clock violation: visible_until cannot move backwards".into(),
                ));
            }
        }
        if let Some(hist) = query.history_start_at {
            if hist > query.visible_until {
                return Err(StorageError::IntegrityFailed(
                    "history_start_at cannot exceed visible_until".into(),
                ));
            }
            if let Some(warm) = query.warmup_start_at {
                if warm > hist {
                    return Err(StorageError::IntegrityFailed(
                        "warmup_start_at cannot exceed history_start_at".into(),
                    ));
                }
            }
        }
        if let Some(warm) = query.warmup_start_at {
            if warm > query.visible_until {
                return Err(StorageError::IntegrityFailed(
                    "warmup_start_at cannot exceed visible_until".into(),
                ));
            }
        }

        let warmup_str = query.warmup_start_at.map(|dt| dt.to_rfc3339());
        let cutoff_str = query.visible_until.to_rfc3339();

        let mut stmt = self.conn().prepare_cached(
            "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                    open, high, low, close, volume, source, adjustment, is_final,
                    valid_from_version, valid_to_version, raw_payload_hash
             FROM bar
             WHERE instrument_id = ?1
               AND timeframe = ?2
               AND adjustment = 'raw'
               AND (?3 IS NULL OR source = ?3)
               AND is_final = 1
               AND valid_from_version <= ?4
               AND (valid_to_version IS NULL OR ?4 < valid_to_version)
               AND (
                   ?5 IS NULL
                   OR strftime('%Y-%m-%dT%H:%M:%fZ', open_at) >= strftime('%Y-%m-%dT%H:%M:%fZ', ?5)
               )
               AND strftime('%Y-%m-%dT%H:%M:%fZ', close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?6)
             ORDER BY open_at ASC, id ASC",
        )?;

        let rows = stmt.query_map(
            params![
                query.instrument_id,
                query.timeframe,
                query.source.as_deref(),
                query.dataset_version,
                warmup_str,
                cutoff_str,
            ],
            |r| Ok(map_bar_row(r)),
        )?;

        let mut raw_bars = Vec::new();
        for row in rows {
            raw_bars.push(row??);
        }

        if query.source.is_none() {
            Ok(dedupe_bars_by_session_date(raw_bars))
        } else {
            Ok(raw_bars)
        }
    }

    /// Convenience wrapper reading visible finalized bars strictly bounded by a persisted training run's
    /// frozen `dataset_version` and `training_session.visible_until`.
    pub fn query_run_visible_final_bars(
        &self,
        run_id: &str,
        include_warmup: bool,
    ) -> Result<Vec<BarRecord>, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        let session = self.get_training_session(run_id)?;
        let lower = if include_warmup {
            Some(session.warmup_start_at)
        } else {
            Some(session.history_start_at)
        };
        self.query_training_visible_final_bars(&TrainingBarPortQuery {
            instrument_id: session.real_instrument_id,
            timeframe: "1d".to_string(),
            source: None,
            dataset_version: run.dataset_version,
            warmup_start_at: lower,
            history_start_at: Some(session.history_start_at),
            visible_until: session.visible_until,
            previous_visible_until: None,
        })
    }

    /// Resolve the immediate next deduplicated finalized `1d` raw bar strictly inside
    /// `(session.visible_until, session.train_end_at]` with `open_at >= session.train_start_at`
    /// (`S-37`, `M4-K01`, `M4-K03`).
    ///
    /// Security & Sentinel Invariant:
    /// - Session discovery queries ONLY metadata columns (`session_date`, `open_at`, `close_at`),
    ///   and full `BarRecord` rows are materialized ONLY for the single candidate `session_date`,
    ///   so future bars beyond the immediate next session (including extreme or non-decimal poison
    ///   sentinels) are NEVER parsed by `map_bar_row`.
    /// - Multi-source rows on that `session_date` are deduplicated via `dedupe_bars_by_session_date`
    ///   using identical `source_priority` rules as `query_training_visible_final_bars` and
    ///   `query_training_candidate_pool`, preventing same-date `close_at` skew across sources from
    ///   causing false `MISSING_STEP_BAR` failures or duplicate session steps.
    pub fn query_next_training_step_bar(
        &self,
        run_id: &str,
    ) -> Result<Option<BarRecord>, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        let session = self.get_training_session(run_id)?;
        self.ensure_known_dataset_version(run.dataset_version)?;

        let vis_str = session.visible_until.to_rfc3339();
        let train_start_str = session.train_start_at.to_rfc3339();
        let train_end_str = session.train_end_at.to_rfc3339();

        let mut cur_date_opt: Option<String> = self.conn().query_row(
            "SELECT MAX(session_date)
             FROM bar
             WHERE instrument_id = ?1
               AND timeframe = '1d'
               AND adjustment = 'raw'
               AND is_final = 1
               AND valid_from_version <= ?2
               AND (valid_to_version IS NULL OR ?2 < valid_to_version)
               AND strftime('%Y-%m-%dT%H:%M:%fZ', close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?3)",
            params![session.real_instrument_id, run.dataset_version, vis_str],
            |r| r.get(0),
        )?;

        loop {
            let next_date_opt: Option<String> = self.conn().query_row(
                "SELECT MIN(session_date)
                 FROM bar
                 WHERE instrument_id = ?1
                   AND timeframe = '1d'
                   AND adjustment = 'raw'
                   AND is_final = 1
                   AND valid_from_version <= ?2
                   AND (valid_to_version IS NULL OR ?2 < valid_to_version)
                   AND (?3 IS NULL OR session_date > ?3)
                   AND strftime('%Y-%m-%dT%H:%M:%fZ', open_at) >= strftime('%Y-%m-%dT%H:%M:%fZ', ?4)
                   AND strftime('%Y-%m-%dT%H:%M:%fZ', close_at) > strftime('%Y-%m-%dT%H:%M:%fZ', ?5)
                   AND strftime('%Y-%m-%dT%H:%M:%fZ', close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?6)",
                params![
                    session.real_instrument_id,
                    run.dataset_version,
                    cur_date_opt.as_deref(),
                    train_start_str,
                    vis_str,
                    train_end_str,
                ],
                |r| r.get(0),
            )?;

            let Some(next_date_str) = next_date_opt else {
                return Ok(None);
            };

            let mut stmt = self.conn().prepare_cached(
                "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                        open, high, low, close, volume, source, adjustment, is_final,
                        valid_from_version, valid_to_version, raw_payload_hash
                 FROM bar
                 WHERE instrument_id = ?1
                   AND timeframe = '1d'
                   AND adjustment = 'raw'
                   AND is_final = 1
                   AND valid_from_version <= ?2
                   AND (valid_to_version IS NULL OR ?2 < valid_to_version)
                   AND session_date = ?3
                 ORDER BY open_at ASC, id ASC",
            )?;
            let rows = stmt.query_map(
                params![
                    session.real_instrument_id,
                    run.dataset_version,
                    next_date_str,
                ],
                |r| Ok(map_bar_row(r)),
            )?;
            let mut raw_bars = Vec::new();
            for row in rows {
                raw_bars.push(row??);
            }
            let mut deduped = dedupe_bars_by_session_date(raw_bars);
            let Some(bar) = deduped.pop() else {
                cur_date_opt = Some(next_date_str);
                continue;
            };

            if bar.close_at > session.train_end_at {
                return Ok(None);
            }
            if bar.open_at < session.train_start_at || bar.close_at <= session.visible_until {
                cur_date_opt = Some(next_date_str);
                continue;
            }
            return Ok(Some(bar));
        }
    }

    /// Derive the effective `ex_date` cutoff (`NaiveDate`) corresponding to `visible_until`:
    /// For `CRYPTO_24_7` (`OKX:` / `BINANCE:`), bars span `[D 00:00Z, (D+1) 00:00Z)`, so an exact
    /// midnight timestamp `(D+1) 00:00:00.000000000Z` corresponds to session `D`.
    pub fn effective_visible_ex_date(
        instrument_id: &str,
        visible_until: DateTime<Utc>,
    ) -> NaiveDate {
        let upper = instrument_id.to_ascii_uppercase();
        let is_crypto = upper.starts_with("OKX:")
            || upper.starts_with("BINANCE:")
            || upper.contains(":CRYPTO:");
        if is_crypto && visible_until.time() == NaiveTime::MIN {
            (visible_until - chrono::Duration::nanoseconds(1)).date_naive()
        } else {
            visible_until.date_naive()
        }
    }

    /// SQL-level truncated corporate action query (`S-37`, `S-42`, `M4-K01`, `M4-A-01`):
    /// Always binds `factor_version <= ?dataset_version` AND `ex_date <= ?max_ex_date` in SQLite's
    /// `WHERE` clause so future splits/dividends (including sentinel rows) are NEVER read from SQLite!
    pub fn query_training_visible_corporate_actions_until_date(
        &self,
        instrument_id: &str,
        dataset_version: i64,
        max_ex_date: NaiveDate,
    ) -> Result<Vec<CorporateActionRecord>, StorageError> {
        self.ensure_known_dataset_version(dataset_version)?;
        let cutoff_date_str = max_ex_date.to_string();

        let mut stmt = self.conn().prepare_cached(
            "SELECT id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
             FROM corporate_action
             WHERE instrument_id = ?1
               AND factor_version <= ?2
               AND ex_date <= ?3
               AND factor_version = (
                   SELECT MAX(c.factor_version)
                   FROM corporate_action c
                   WHERE c.instrument_id = corporate_action.instrument_id
                     AND c.action_type = corporate_action.action_type
                     AND c.ex_date = corporate_action.ex_date
                     AND c.source = corporate_action.source
                     AND c.factor_version <= ?2
               )
             ORDER BY ex_date ASC, action_type ASC, source ASC, id ASC",
        )?;

        let rows = stmt.query_map(
            params![instrument_id, dataset_version, cutoff_date_str],
            |r| Ok(map_corporate_action_row(r)),
        )?;

        // Deduplicate deterministically per (ex_date, action_type) using source priority.
        let mut by_key: BTreeMap<(NaiveDate, u8), CorporateActionRecord> = BTreeMap::new();
        for row in rows {
            let rec = row??;
            let type_ord = match rec.action_type {
                CorporateActionType::Split => 1u8,
                CorporateActionType::CashDividend => 2u8,
            };
            let key = (rec.ex_date, type_ord);
            match by_key.get(&key) {
                None => {
                    by_key.insert(key, rec);
                }
                Some(existing) => {
                    let new_p = (
                        source_priority(&rec.source),
                        rec.source.as_str(),
                        rec.id.as_str(),
                    );
                    let cur_p = (
                        source_priority(&existing.source),
                        existing.source.as_str(),
                        existing.id.as_str(),
                    );
                    if new_p < cur_p {
                        by_key.insert(key, rec);
                    }
                }
            }
        }
        Ok(by_key.into_values().collect())
    }

    /// SQL-level truncated corporate actions up to `visible_until` (`ex_date <= effective_date(visible_until)`).
    pub fn query_training_visible_corporate_actions(
        &self,
        instrument_id: &str,
        dataset_version: i64,
        visible_until: DateTime<Utc>,
    ) -> Result<Vec<CorporateActionRecord>, StorageError> {
        let max_ex_date = Self::effective_visible_ex_date(instrument_id, visible_until);
        self.query_training_visible_corporate_actions_until_date(
            instrument_id,
            dataset_version,
            max_ex_date,
        )
    }

    /// Fetch visible corporate actions for a persisted training run bounded by its `visible_until`.
    pub fn query_run_visible_corporate_actions(
        &self,
        run_id: &str,
    ) -> Result<Vec<CorporateActionRecord>, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        let session = self.get_training_session(run_id)?;
        self.query_training_visible_corporate_actions(
            &session.real_instrument_id,
            run.dataset_version,
            session.visible_until,
        )
    }

    /// Fetch candidate pool segments for a frozen `dataset_version` (`S-38`, `S-42`, `M4-A-02`):
    /// Finds instruments & contiguous finalized-bar windows of length `history_len + train_len + reveal_len`
    /// with:
    /// 1. NO missing calendar sessions or `coverage_gap`s,
    /// 2. NO `partial` / `warning` / `data_quality_issue` flags in the segment,
    /// 3. NO incomplete corporate actions (missing/zero/invalid split ratio or dividend amount,
    ///    conflicting multi-source ratios, or unexplained split-like price jumps),
    ///
    /// returning deterministically sorted candidates and canonical SHA-256 `candidate_pool_hash`.
    pub fn query_training_candidate_pool(
        &self,
        query: &CandidatePoolQuery,
    ) -> Result<CandidatePoolResult, StorageError> {
        self.ensure_known_dataset_version(query.dataset_version)?;
        if query.history_len == 0 || query.train_len == 0 {
            return Err(StorageError::IntegrityFailed(
                "history_len and train_len must both be >= 1".into(),
            ));
        }
        let required_len = query
            .history_len
            .checked_add(query.train_len)
            .and_then(|n| n.checked_add(query.reveal_len))
            .ok_or_else(|| StorageError::IntegrityFailed("segment length overflow".into()))?;

        let norm_cat = query.market_category.trim().to_ascii_lowercase();
        if !matches!(norm_cat.as_str(), "all" | "us_equity" | "crypto_spot") {
            return Err(StorageError::IntegrityFailed(format!(
                "unsupported market_category '{}' (expected 'all', 'us_equity', or 'crypto_spot')",
                query.market_category
            )));
        }

        let registry = self.load_instrument_registry()?;
        let mut instruments: Vec<_> = registry.instruments.values().cloned().collect();
        instruments.sort_by(|a, b| a.id.cmp(&b.id));

        let mut candidates = Vec::new();

        for inst in instruments {
            if let Some(ref target_id) = query.instrument_id {
                if &inst.id != target_id {
                    continue;
                }
            }
            let inst_cat = match inst.kind {
                InstrumentKind::SpotEquity | InstrumentKind::SpotEtf => "us_equity",
                InstrumentKind::SpotCrypto => "crypto_spot",
            };
            if norm_cat != "all" && inst_cat != norm_cat {
                continue;
            }

            // Load finalized raw bars at frozen dataset_version.
            let mut bar_stmt = self.conn().prepare_cached(
                "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                        open, high, low, close, volume, source, adjustment, is_final,
                        valid_from_version, valid_to_version, raw_payload_hash
                 FROM bar
                 WHERE instrument_id = ?1
                   AND timeframe = '1d'
                   AND adjustment = 'raw'
                   AND is_final = 1
                   AND valid_from_version <= ?2
                   AND (valid_to_version IS NULL OR ?2 < valid_to_version)
                 ORDER BY session_date ASC, open_at ASC, id ASC",
            )?;
            let bar_rows = bar_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok(map_bar_row(r))
            })?;
            let mut raw_bars = Vec::new();
            let mut corrupt_bar_found = false;
            for r in bar_rows {
                match r? {
                    Ok(b) => raw_bars.push(b),
                    Err(_) => {
                        corrupt_bar_found = true;
                        break;
                    }
                }
            }
            if corrupt_bar_found {
                continue;
            }
            let mut bars = dedupe_bars_by_session_date(raw_bars);
            if let Some(sd) = query.start_date {
                bars.retain(|b| b.session_date >= sd);
            }
            if let Some(ed) = query.end_date {
                bars.retain(|b| b.session_date <= ed);
            }
            if bars.len() < required_len {
                continue;
            }

            // Load coverage_gap intervals for this instrument up to dataset_version.
            let mut gap_stmt = self.conn().prepare_cached(
                "SELECT start_date, end_date
                 FROM coverage_gap
                 WHERE instrument_id = ?1
                   AND dataset_version <= ?2
                   AND resolution != 'resolved'",
            )?;
            let mut stored_gaps: Vec<(NaiveDate, NaiveDate)> = Vec::new();
            for row in gap_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })? {
                let (s_str, e_str) = row?;
                if let (Ok(s), Ok(e)) = (
                    NaiveDate::parse_from_str(&s_str, "%Y-%m-%d"),
                    NaiveDate::parse_from_str(&e_str, "%Y-%m-%d"),
                ) {
                    stored_gaps.push((s, e));
                }
            }

            // Load data_quality_issue dates/kinds for this instrument up to dataset_version.
            let mut dqi_stmt = self.conn().prepare_cached(
                "SELECT dataset_version, session_date, kind
                 FROM data_quality_issue
                 WHERE instrument_id = ?1
                   AND dataset_version <= ?2",
            )?;
            let mut quality_issue_dates: BTreeSet<NaiveDate> = BTreeSet::new();
            let mut instrument_missing_ca_globally = false;
            for row in dqi_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (ver, d_str, kind) = row?;
                let kind_lc = kind.to_ascii_lowercase();
                // Ignore historical `data_revision` audit notices from strictly older versions;
                // exclude all warnings, partial, missing_corporate_action, price_mismatch, etc.
                if kind_lc == "data_revision" && ver < query.dataset_version {
                    continue;
                }
                if kind_lc == "missing_corporate_action" && ver == query.dataset_version {
                    instrument_missing_ca_globally = true;
                }
                if let Ok(d) = NaiveDate::parse_from_str(&d_str, "%Y-%m-%d") {
                    quality_issue_dates.insert(d);
                } else {
                    instrument_missing_ca_globally = true;
                }
            }
            if instrument_missing_ca_globally && quality_issue_dates.is_empty() {
                continue;
            }

            // Inspect raw corporate_action rows for this instrument at factor_version <= dataset_version
            // to detect incomplete ratios/amounts or multi-source disagreements.
            let mut ca_stmt = self.conn().prepare_cached(
                "SELECT action_type, ex_date, ratio_or_amount, currency, source
                 FROM corporate_action
                 WHERE instrument_id = ?1
                   AND factor_version <= ?2
                   AND factor_version = (
                       SELECT MAX(c.factor_version)
                       FROM corporate_action c
                       WHERE c.instrument_id = corporate_action.instrument_id
                         AND c.action_type = corporate_action.action_type
                         AND c.ex_date = corporate_action.ex_date
                         AND c.source = corporate_action.source
                         AND c.factor_version <= ?2
                   )
                 ORDER BY ex_date ASC",
            )?;
            let mut bad_ca_dates: BTreeSet<NaiveDate> = BTreeSet::new();
            let mut valid_splits_by_date: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
            let mut valid_divs_by_date: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
            let mut unparseable_ca_date = false;

            for row in ca_stmt.query_map(params![inst.id, query.dataset_version], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })? {
                let (act_type, ex_str, ratio_str, _ccy, _src) = row?;
                let Ok(ex_date) = NaiveDate::parse_from_str(&ex_str, "%Y-%m-%d") else {
                    unparseable_ca_date = true;
                    break;
                };
                let trimmed = ratio_str.trim();
                let Ok(val) = parse_decimal_lossless(trimmed) else {
                    bad_ca_dates.insert(ex_date);
                    continue;
                };
                if val <= Decimal::ZERO {
                    bad_ca_dates.insert(ex_date);
                    continue;
                }
                match act_type.as_str() {
                    "split" => {
                        if let Some(&prev_val) = valid_splits_by_date.get(&ex_date) {
                            if prev_val != val {
                                bad_ca_dates.insert(ex_date);
                            }
                        } else {
                            valid_splits_by_date.insert(ex_date, val);
                        }
                    }
                    "cash_dividend" => {
                        if let Some(&prev_val) = valid_divs_by_date.get(&ex_date) {
                            if prev_val != val {
                                bad_ca_dates.insert(ex_date);
                            }
                        } else {
                            valid_divs_by_date.insert(ex_date, val);
                        }
                    }
                    _ => {
                        bad_ca_dates.insert(ex_date);
                    }
                }
            }
            if unparseable_ca_date {
                continue;
            }

            let split_date_set: BTreeSet<NaiveDate> =
                valid_splits_by_date.keys().copied().collect();
            let cal_venue = match inst.venue.as_str() {
                "NASDAQ" | "XNAS" => "XNAS",
                "NYSE" | "XNYS" => "XNYS",
                _ => "CRYPTO_24_7",
            };

            // Precompute whether each bar and each adjacent pair `(i-1, i)` is calendar-contiguous & clean.
            let bar_valid: Vec<bool> = bars
                .iter()
                .map(|b| {
                    b.is_final
                        && b.open > Decimal::ZERO
                        && b.high >= b.open.max(b.close)
                        && b.low <= b.open.min(b.close)
                        && b.low > Decimal::ZERO
                        && b.close > Decimal::ZERO
                        && b.volume >= Decimal::ZERO
                        && b.close_at > b.open_at
                        && !quality_issue_dates.contains(&b.session_date)
                        && !bad_ca_dates.contains(&b.session_date)
                        && matches!(
                            query_calendar_session(cal_venue, b.session_date).kind,
                            CalendarSessionKind::Regular
                                | CalendarSessionKind::EarlyClose
                                | CalendarSessionKind::Crypto24x7
                        )
                })
                .collect();

            // `adjacent_clean[i]` is true iff there are NO missing trading sessions, calendar_unknowns,
            // quality issues, or incomplete CAs strictly between `bars[i]` and `bars[i + 1]`.
            let mut adjacent_clean = vec![false; bars.len().saturating_sub(1)];
            for i in 0..adjacent_clean.len() {
                let d_prev = bars[i].session_date;
                let d_next = bars[i + 1].session_date;
                if d_next <= d_prev {
                    continue;
                }
                let mut ok = true;
                let mut cur = d_prev.checked_add_days(chrono::Days::new(1));
                while let Some(d) = cur {
                    if d >= d_next {
                        break;
                    }
                    let sess = query_calendar_session(cal_venue, d);
                    match sess.kind {
                        CalendarSessionKind::Closed => {}
                        CalendarSessionKind::CalendarUnknown
                        | CalendarSessionKind::Regular
                        | CalendarSessionKind::EarlyClose
                        | CalendarSessionKind::Crypto24x7 => {
                            // Missing trading session or unknown calendar day!
                            ok = false;
                            break;
                        }
                    }
                    if quality_issue_dates.contains(&d) || bad_ca_dates.contains(&d) {
                        ok = false;
                        break;
                    }
                    cur = d.checked_add_days(chrono::Days::new(1));
                }
                adjacent_clean[i] = ok;
            }

            let max_start = bars.len() - required_len;
            for start_idx in 0..=max_start {
                let end_idx = start_idx + required_len - 1;
                if !(start_idx..=end_idx).all(|idx| bar_valid[idx]) {
                    continue;
                }
                if !(start_idx..end_idx).all(|idx| adjacent_clean[idx]) {
                    continue;
                }

                let seg_start_date = bars[start_idx].session_date;
                let seg_end_date = bars[end_idx].session_date;

                if stored_gaps
                    .iter()
                    .any(|(gs, ge)| *gs <= seg_end_date && *ge >= seg_start_date)
                {
                    continue;
                }
                if quality_issue_dates
                    .range(seg_start_date..=seg_end_date)
                    .next()
                    .is_some()
                {
                    continue;
                }
                if bad_ca_dates
                    .range(seg_start_date..=seg_end_date)
                    .next()
                    .is_some()
                {
                    continue;
                }

                // Walk backwards up to `query.warmup_bars` contiguous clean bars for indicator warmup (`S-37`).
                let mut warmup_idx = start_idx;
                let mut taken_warmup = 0usize;
                while taken_warmup < query.warmup_bars && warmup_idx > 0 {
                    let prev_idx = warmup_idx - 1;
                    if !bar_valid[prev_idx] || !adjacent_clean[prev_idx] {
                        break;
                    }
                    let prev_date = bars[prev_idx].session_date;
                    if stored_gaps
                        .iter()
                        .any(|(gs, ge)| *gs <= prev_date && *ge >= prev_date)
                    {
                        break;
                    }
                    warmup_idx = prev_idx;
                    taken_warmup += 1;
                }

                // Check for unexplained split-like jumps across `[warmup_idx..=end_idx]`.
                if has_unexplained_split_jump(&bars[warmup_idx..=end_idx], &split_date_set) {
                    continue;
                }

                let hist_end_idx = start_idx + query.history_len - 1;
                let train_start_idx = start_idx + query.history_len;
                let train_end_idx = train_start_idx + query.train_len - 1;

                candidates.push(TrainingCandidateSegment {
                    instrument_id: inst.id.clone(),
                    venue: inst.venue.clone(),
                    symbol: inst.symbol.clone(),
                    market_category: inst_cat.to_string(),
                    quote_currency: inst.quote_asset_id.clone(),
                    warmup_start_at: bars[warmup_idx].open_at,
                    history_start_at: bars[start_idx].open_at,
                    initial_visible_until: bars[hist_end_idx].close_at,
                    train_start_at: bars[train_start_idx].open_at,
                    train_end_at: bars[train_end_idx].close_at,
                    reveal_end_at: bars[end_idx].close_at,
                    history_start_date: bars[start_idx].session_date,
                    train_start_date: bars[train_start_idx].session_date,
                    train_end_date: bars[train_end_idx].session_date,
                    reveal_end_date: bars[end_idx].session_date,
                    warmup_bar_count: taken_warmup,
                    history_len: query.history_len,
                    train_len: query.train_len,
                    reveal_len: query.reveal_len,
                });
            }
        }

        candidates.sort_by(|a, b| {
            a.instrument_id
                .cmp(&b.instrument_id)
                .then_with(|| a.history_start_at.cmp(&b.history_start_at))
                .then_with(|| a.train_start_at.cmp(&b.train_start_at))
                .then_with(|| a.train_end_at.cmp(&b.train_end_at))
                .then_with(|| a.reveal_end_at.cmp(&b.reveal_end_at))
        });

        let mut hasher = Sha256::new();
        hasher.update(
            format!(
                "m4-candidate-pool-v1|dsv={}|cat={}|w={}|h={}|t={}|r={}|count={}\n",
                query.dataset_version,
                norm_cat,
                query.warmup_bars,
                query.history_len,
                query.train_len,
                query.reveal_len,
                candidates.len()
            )
            .as_bytes(),
        );
        for c in &candidates {
            hasher.update(
                format!(
                    "{}|{}|{}|{}|{}|{}|{}\n",
                    c.instrument_id,
                    c.warmup_start_at.to_rfc3339(),
                    c.history_start_at.to_rfc3339(),
                    c.initial_visible_until.to_rfc3339(),
                    c.train_start_at.to_rfc3339(),
                    c.train_end_at.to_rfc3339(),
                    c.reveal_end_at.to_rfc3339(),
                )
                .as_bytes(),
            );
        }
        let candidate_pool_hash = hex::encode(hasher.finalize());

        Ok(CandidatePoolResult {
            dataset_version: query.dataset_version,
            market_category: norm_cat,
            warmup_bars: query.warmup_bars,
            history_len: query.history_len,
            train_len: query.train_len,
            reveal_len: query.reveal_len,
            candidate_pool_hash,
            candidates,
        })
    }

    /// Post-finish reveal query (`M4-A-01`, `M4-A-14`):
    /// Strictly succeeds ONLY when `simulation_run.status = 'finished'` in SQLite (enforced in both
    /// Rust and SQLite `WHERE` predicates!).
    pub fn query_post_finish_reveal(
        &self,
        run_id: &str,
    ) -> Result<PostFinishRevealData, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if run.status != SimRunStatus::Finished {
            return Err(StorageError::IntegrityFailed(format!(
                "post-finish reveal refused: simulation_run '{run_id}' status is '{}' (must be 'finished')",
                run.status.as_str()
            )));
        }
        let session = self.get_training_session(run_id)?;
        let registry = self.load_instrument_registry()?;
        let (venue, real_symbol) = match registry.instruments.get(&session.real_instrument_id) {
            Some(inst) => (inst.venue.clone(), inst.symbol.clone()),
            None => {
                let parts: Vec<&str> = session.real_instrument_id.splitn(2, ':').collect();
                if parts.len() == 2 {
                    (parts[0].to_string(), parts[1].to_string())
                } else {
                    ("UNKNOWN".to_string(), session.real_instrument_id.clone())
                }
            }
        };

        let mut stmt = self.conn().prepare_cached(
            "SELECT b.id, b.instrument_id, b.timeframe, b.session, b.session_date, b.open_at, b.close_at,
                    b.open, b.high, b.low, b.close, b.volume, b.source, b.adjustment, b.is_final,
                    b.valid_from_version, b.valid_to_version, b.raw_payload_hash
             FROM bar b
             WHERE EXISTS (
                 SELECT 1 FROM simulation_run sr
                 WHERE sr.run_id = ?1 AND sr.status = 'finished'
             )
               AND b.instrument_id = ?2
               AND b.timeframe = '1d'
               AND b.adjustment = 'raw'
               AND b.is_final = 1
               AND b.valid_from_version <= ?3
               AND (b.valid_to_version IS NULL OR ?3 < b.valid_to_version)
               AND strftime('%Y-%m-%dT%H:%M:%fZ', b.open_at) >= strftime('%Y-%m-%dT%H:%M:%fZ', ?4)
               AND strftime('%Y-%m-%dT%H:%M:%fZ', b.close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?5)
             ORDER BY b.open_at ASC, b.id ASC",
        )?;
        let rows = stmt.query_map(
            params![
                run_id,
                session.real_instrument_id,
                run.dataset_version,
                session.warmup_start_at.to_rfc3339(),
                session.reveal_end_at.to_rfc3339(),
            ],
            |r| Ok(map_bar_row(r)),
        )?;
        let mut raw_bars = Vec::new();
        for row in rows {
            raw_bars.push(row??);
        }
        let all_bars_through_reveal = dedupe_bars_by_session_date(raw_bars);
        let future_reveal_bars: Vec<BarRecord> = all_bars_through_reveal
            .iter()
            .filter(|b| b.close_at > session.train_end_at && b.close_at <= session.reveal_end_at)
            .cloned()
            .collect();

        let corporate_actions_through_reveal = self.query_training_visible_corporate_actions(
            &session.real_instrument_id,
            run.dataset_version,
            session.reveal_end_at,
        )?;

        Ok(PostFinishRevealData {
            run_id: run.run_id,
            real_instrument_id: session.real_instrument_id,
            venue,
            real_symbol,
            dataset_version: run.dataset_version,
            history_start_at: session.history_start_at,
            train_start_at: session.train_start_at,
            train_end_at: session.train_end_at,
            reveal_end_at: session.reveal_end_at,
            future_reveal_bars,
            all_bars_through_reveal,
            corporate_actions_through_reveal,
        })
    }

    pub fn mark_training_session_revealed(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingSessionRecord, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if run.status != SimRunStatus::Finished {
            return Err(StorageError::IntegrityFailed(format!(
                "cannot mark training_session '{run_id}' revealed while status is '{}'",
                run.status.as_str()
            )));
        }
        self.conn().execute(
            "UPDATE training_session SET revealed = 1 WHERE run_id = ?1",
            params![run_id],
        )?;
        self.get_training_session(run_id)
    }

    // ------------------------------------------------------------------------
    // 4. Atomic Step Commit, Orders/Fills/Notes, Recovery & Delete (`S-43`, `M4-A-09`..`M4-A-13`)
    // ------------------------------------------------------------------------

    /// Atomically commit a single simulation command step (`simulation_event`, `simulation_run`,
    /// `training_session`, `sim_order`, `sim_fill`, `training_note`, and optional `sim_checkpoint`)
    /// inside one SQLite savepoint so mid-step crash/abort leaves zero half-steps and zero duplicate fills (`M4-A-09`).
    /// Never touches `library.ledger_revision` or any real ledger table (`M4-A-11`).
    pub fn commit_simulation_step_atomic(
        &mut self,
        step: &SimulationStepCommit,
    ) -> Result<SimulationRunRecord, StorageError> {
        if step.new_state_hash.trim().is_empty() {
            return Err(StorageError::IntegrityFailed(
                "new_state_hash must be non-empty".into(),
            ));
        }
        if step.seq != step.expected_prev_seq + 1 || step.seq < 1 {
            return Err(StorageError::IntegrityFailed(format!(
                "invalid step sequence: expected_prev_seq={}, seq={}",
                step.expected_prev_seq, step.seq
            )));
        }
        if step.new_status == SimRunStatus::Damaged {
            return Err(StorageError::IntegrityFailed(
                "use mark_simulation_run_damaged to transition a run to damaged".into(),
            ));
        }

        let tx = self.conn_mut().savepoint()?;

        let (cur_status_str, cur_seq): (String, i64) = tx
            .query_row(
                "SELECT status, latest_seq FROM simulation_run WHERE run_id = ?1",
                params![step.run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StorageError::IntegrityFailed(format!("simulation_run '{}' not found", step.run_id))
            })?;
        let cur_status = SimRunStatus::parse(&cur_status_str)?;
        if matches!(cur_status, SimRunStatus::Finished | SimRunStatus::Damaged) {
            return Err(StorageError::IntegrityFailed(format!(
                "cannot commit step to simulation_run '{}' in terminal status '{}'",
                step.run_id,
                cur_status.as_str()
            )));
        }
        if cur_seq != step.expected_prev_seq {
            return Err(StorageError::IntegrityFailed(format!(
                "optimistic sequence mismatch on run '{}': expected {}, found {}",
                step.run_id, step.expected_prev_seq, cur_seq
            )));
        }

        let cur_session: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT visible_until, current_step_index, total_steps FROM training_session WHERE run_id = ?1",
                params![step.run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        if let Some((old_vis_str, old_step_idx, total_steps)) = &cur_session {
            let old_vis = parse_rfc3339_utc(old_vis_str, "training_session.visible_until")?;
            if step.new_visible_until < old_vis {
                return Err(StorageError::IntegrityFailed(
                    "replay clock violation: new_visible_until < old_visible_until".into(),
                ));
            }
            if step.new_step_index < *old_step_idx || step.new_step_index > *total_steps {
                return Err(StorageError::IntegrityFailed(format!(
                    "invalid step_index progression: {} -> {} (total_steps={})",
                    old_step_idx, step.new_step_index, total_steps
                )));
            }
        }

        let committed_str = step.committed_at.to_rfc3339();
        let vis_str = step.new_visible_until.to_rfc3339();

        tx.execute(
            "INSERT INTO simulation_event (
                run_id, seq, event_kind, command_json, events_json,
                visible_until, state_hash, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                step.run_id,
                step.seq,
                step.event_kind,
                step.command_json,
                step.events_json,
                vis_str,
                step.new_state_hash,
                committed_str,
            ],
        )?;

        for ord in &step.orders_to_upsert {
            if ord.run_id != step.run_id {
                return Err(StorageError::IntegrityFailed(
                    "sim_order.run_id does not match step.run_id".into(),
                ));
            }
            tx.execute(
                "INSERT INTO sim_order (
                    order_id, run_id, instrument_id, order_seq, order_type, side, tif,
                    qty, limit_price, stop_price, parent_group_id, group_role,
                    status, reject_code, cancel_reason, submitted_at, eligible_at, updated_at, payload_json
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19
                 )
                 ON CONFLICT(run_id, order_id) DO UPDATE SET
                    status = excluded.status,
                    reject_code = excluded.reject_code,
                    cancel_reason = excluded.cancel_reason,
                    eligible_at = excluded.eligible_at,
                    updated_at = excluded.updated_at,
                    payload_json = excluded.payload_json",
                params![
                    ord.order_id,
                    ord.run_id,
                    ord.instrument_id,
                    ord.order_seq,
                    ord.order_type,
                    ord.side,
                    ord.tif,
                    to_canonical_text(ord.qty),
                    ord.limit_price.map(to_canonical_text),
                    ord.stop_price.map(to_canonical_text),
                    ord.parent_group_id,
                    ord.group_role,
                    ord.status.as_str(),
                    ord.reject_code,
                    ord.cancel_reason,
                    ord.submitted_at.to_rfc3339(),
                    ord.eligible_at.map(|dt| dt.to_rfc3339()),
                    ord.updated_at.to_rfc3339(),
                    ord.payload_json,
                ],
            )?;
        }

        for fill in &step.fills_to_insert {
            if fill.run_id != step.run_id {
                return Err(StorageError::IntegrityFailed(
                    "sim_fill.run_id does not match step.run_id".into(),
                ));
            }
            tx.execute(
                "INSERT INTO sim_fill (
                    fill_id, run_id, order_id, instrument_id, bar_close_at,
                    side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    fill.fill_id,
                    fill.run_id,
                    fill.order_id,
                    fill.instrument_id,
                    fill.bar_close_at.to_rfc3339(),
                    fill.side,
                    to_canonical_text(fill.qty),
                    to_canonical_text(fill.raw_price),
                    to_canonical_text(fill.fill_price),
                    to_canonical_text(fill.fee_amount),
                    fill.fee_currency,
                    if fill.ambiguous { 1 } else { 0 },
                    fill.payload_json,
                ],
            )?;
        }

        for note in &step.notes_to_upsert {
            if note.run_id != step.run_id {
                return Err(StorageError::IntegrityFailed(
                    "training_note.run_id does not match step.run_id".into(),
                ));
            }
            upsert_training_note_on_conn(&tx, note)?;
        }

        let finished_at_opt = if step.new_status == SimRunStatus::Finished {
            Some(committed_str.clone())
        } else {
            None
        };
        tx.execute(
            "UPDATE simulation_run
             SET status = ?2,
                 latest_seq = ?3,
                 latest_state_hash = ?4,
                 updated_at = ?5,
                 finished_at = COALESCE(?6, finished_at)
             WHERE run_id = ?1",
            params![
                step.run_id,
                step.new_status.as_str(),
                step.seq,
                step.new_state_hash,
                committed_str,
                finished_at_opt,
            ],
        )?;

        if cur_session.is_some() {
            tx.execute(
                "UPDATE training_session
                 SET visible_until = ?2,
                     current_step_index = ?3
                 WHERE run_id = ?1",
                params![step.run_id, vis_str, step.new_step_index],
            )?;
        }

        if let Some(ref cp) = step.checkpoint {
            if cp.run_id != step.run_id
                || cp.seq != step.seq
                || cp.state_hash != step.new_state_hash
            {
                return Err(StorageError::IntegrityFailed(
                    "sim_checkpoint must match step run_id, seq, and new_state_hash".into(),
                ));
            }
            tx.execute(
                "INSERT OR REPLACE INTO sim_checkpoint (
                    run_id, seq, reason, state_json, state_hash, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    cp.run_id,
                    cp.seq,
                    cp.reason.as_str(),
                    cp.state_json,
                    cp.state_hash,
                    cp.created_at,
                ],
            )?;
        }

        if FAIL_NEXT_SIM_STEP_COMMIT.with(|flag| flag.replace(false)) {
            return Err(StorageError::IntegrityFailed(
                "injected mid-step crash before transaction commit".into(),
            ));
        }

        tx.commit()?;
        self.get_simulation_run(&step.run_id)
    }

    pub fn list_sim_orders(&self, run_id: &str) -> Result<Vec<SimOrderRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT order_id, run_id, instrument_id, order_seq, order_type, side, tif,
                    qty, limit_price, stop_price, parent_group_id, group_role,
                    status, reject_code, cancel_reason, submitted_at, eligible_at, updated_at, payload_json
             FROM sim_order
             WHERE run_id = ?1
             ORDER BY order_seq ASC, order_id ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| Ok(map_sim_order_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn get_sim_order(
        &self,
        run_id: &str,
        order_id: &str,
    ) -> Result<SimOrderRecord, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT order_id, run_id, instrument_id, order_seq, order_type, side, tif,
                        qty, limit_price, stop_price, parent_group_id, group_role,
                        status, reject_code, cancel_reason, submitted_at, eligible_at, updated_at, payload_json
                 FROM sim_order
                 WHERE run_id = ?1 AND order_id = ?2",
                params![run_id, order_id],
                |r| Ok(map_sim_order_row(r)),
            )
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "sim_order '{order_id}' not found in run '{run_id}'"
            ))),
        }
    }

    pub fn list_sim_fills(&self, run_id: &str) -> Result<Vec<SimFillRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT fill_id, run_id, order_id, instrument_id, bar_close_at,
                    side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
             FROM sim_fill
             WHERE run_id = ?1
             ORDER BY bar_close_at ASC, fill_id ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| Ok(map_sim_fill_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn get_sim_fill(&self, run_id: &str, fill_id: &str) -> Result<SimFillRecord, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT fill_id, run_id, order_id, instrument_id, bar_close_at,
                        side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
                 FROM sim_fill
                 WHERE run_id = ?1 AND fill_id = ?2",
                params![run_id, fill_id],
                |r| Ok(map_sim_fill_row(r)),
            )
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "sim_fill '{fill_id}' not found in run '{run_id}'"
            ))),
        }
    }

    pub fn list_simulation_events(
        &self,
        run_id: &str,
        after_seq: Option<i64>,
    ) -> Result<Vec<SimulationEventRecord>, StorageError> {
        let min_seq = after_seq.unwrap_or(0);
        let mut stmt = self.conn().prepare(
            "SELECT run_id, seq, event_kind, command_json, events_json,
                    visible_until, state_hash, created_at
             FROM simulation_event
             WHERE run_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![run_id, min_seq], |r| {
            let vis_str: String = r.get(5)?;
            Ok(
                parse_rfc3339_utc(&vis_str, "simulation_event.visible_until").map(|vis| {
                    SimulationEventRecord {
                        run_id: r.get(0).unwrap_or_default(),
                        seq: r.get(1).unwrap_or_default(),
                        event_kind: r.get(2).unwrap_or_default(),
                        command_json: r.get(3).unwrap_or_default(),
                        events_json: r.get(4).unwrap_or_default(),
                        visible_until: vis,
                        state_hash: r.get(6).unwrap_or_default(),
                        created_at: r.get(7).unwrap_or_default(),
                    }
                }),
            )
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn list_sim_checkpoints(
        &self,
        run_id: &str,
    ) -> Result<Vec<SimCheckpointRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT run_id, seq, reason, state_json, state_hash, created_at
             FROM sim_checkpoint
             WHERE run_id = ?1
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            let reason_str: String = r.get(2)?;
            Ok(
                SimCheckpointReason::parse(&reason_str).map(|reason| SimCheckpointRecord {
                    run_id: r.get(0).unwrap_or_default(),
                    seq: r.get(1).unwrap_or_default(),
                    reason,
                    state_json: r.get(3).unwrap_or_default(),
                    state_hash: r.get(4).unwrap_or_default(),
                    created_at: r.get(5).unwrap_or_default(),
                }),
            )
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn latest_sim_checkpoint(
        &self,
        run_id: &str,
    ) -> Result<Option<SimCheckpointRecord>, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT run_id, seq, reason, state_json, state_hash, created_at
                 FROM sim_checkpoint
                 WHERE run_id = ?1
                 ORDER BY seq DESC
                 LIMIT 1",
                params![run_id],
                |r| {
                    let reason_str: String = r.get(2)?;
                    Ok(
                        SimCheckpointReason::parse(&reason_str).map(|reason| SimCheckpointRecord {
                            run_id: r.get(0).unwrap_or_default(),
                            seq: r.get(1).unwrap_or_default(),
                            reason,
                            state_json: r.get(3).unwrap_or_default(),
                            state_hash: r.get(4).unwrap_or_default(),
                            created_at: r.get(5).unwrap_or_default(),
                        }),
                    )
                },
            )
            .optional()?;
        row.transpose()
    }

    pub fn upsert_training_note(
        &mut self,
        note: &TrainingNoteRecord,
    ) -> Result<TrainingNoteRecord, StorageError> {
        let tx = self.conn_mut().savepoint()?;
        upsert_training_note_on_conn(&tx, note)?;
        tx.commit()?;
        self.get_training_note(&note.note_id)
    }

    pub fn get_training_note(&self, note_id: &str) -> Result<TrainingNoteRecord, StorageError> {
        let row = self
            .conn()
            .query_row(
                "SELECT note_id, run_id, bar_index, visible_until, rationale, emotion_tag,
                        linked_order_id, archived_journal_note_id, created_at, updated_at
                 FROM training_note
                 WHERE note_id = ?1",
                params![note_id],
                |r| {
                    let vis_str: String = r.get(3)?;
                    Ok(
                        parse_rfc3339_utc(&vis_str, "training_note.visible_until").map(|vis| {
                            TrainingNoteRecord {
                                note_id: r.get(0).unwrap_or_default(),
                                run_id: r.get(1).unwrap_or_default(),
                                bar_index: r.get(2).unwrap_or_default(),
                                visible_until: vis,
                                rationale: r.get(4).unwrap_or_default(),
                                emotion_tag: r.get(5).unwrap_or_default(),
                                linked_order_id: r.get(6).unwrap_or_default(),
                                archived_journal_note_id: r.get(7).unwrap_or_default(),
                                created_at: r.get(8).unwrap_or_default(),
                                updated_at: r.get(9).unwrap_or_default(),
                            }
                        }),
                    )
                },
            )
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "training_note '{note_id}' not found"
            ))),
        }
    }

    pub fn list_training_notes(
        &self,
        run_id: &str,
    ) -> Result<Vec<TrainingNoteRecord>, StorageError> {
        let mut stmt = self.conn().prepare(
            "SELECT note_id, run_id, bar_index, visible_until, rationale, emotion_tag,
                    linked_order_id, archived_journal_note_id, created_at, updated_at
             FROM training_note
             WHERE run_id = ?1
             ORDER BY bar_index ASC, created_at ASC, note_id ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            let vis_str: String = r.get(3)?;
            Ok(
                parse_rfc3339_utc(&vis_str, "training_note.visible_until").map(|vis| {
                    TrainingNoteRecord {
                        note_id: r.get(0).unwrap_or_default(),
                        run_id: r.get(1).unwrap_or_default(),
                        bar_index: r.get(2).unwrap_or_default(),
                        visible_until: vis,
                        rationale: r.get(4).unwrap_or_default(),
                        emotion_tag: r.get(5).unwrap_or_default(),
                        linked_order_id: r.get(6).unwrap_or_default(),
                        archived_journal_note_id: r.get(7).unwrap_or_default(),
                        created_at: r.get(8).unwrap_or_default(),
                        updated_at: r.get(9).unwrap_or_default(),
                    }
                }),
            )
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// Link an already-created journal `note.id` onto `training_note.archived_journal_note_id`
    /// without altering the original training note's `rationale`, `emotion_tag`, `bar_index`,
    /// `visible_until`, or `linked_order_id` (`S-45`, `M4-A-13`).
    pub fn link_archived_journal_note_id(
        &mut self,
        run_id: &str,
        training_note_id: &str,
        journal_note_id: &str,
    ) -> Result<TrainingNoteRecord, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if run.status != SimRunStatus::Finished {
            return Err(StorageError::IntegrityFailed(
                "training notes can only be archived after simulation_run is finished".into(),
            ));
        }
        let existing = self.get_training_note(training_note_id)?;
        if existing.run_id != run_id {
            return Err(StorageError::IntegrityFailed(
                "training_note does not belong to the specified run_id".into(),
            ));
        }
        self.conn().execute(
            "UPDATE training_note SET archived_journal_note_id = ?2 WHERE note_id = ?1 AND run_id = ?3",
            params![training_note_id, journal_note_id, run_id],
        )?;
        self.get_training_note(training_note_id)
    }

    /// Archive a finished training note into the normal M2 journal (`note` / `note_revision`),
    /// explicitly labelled `"来自模拟训练"` (`simulated_training`), preserving the original
    /// `training_note` fields unchanged and never touching `economic_event` or `ledger_revision` (`M4-A-13`).
    pub fn archive_training_note_to_journal(
        &mut self,
        run_id: &str,
        training_note_id: &str,
        journal_note_id: &str,
        title: &str,
    ) -> Result<NoteDto, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if run.status != SimRunStatus::Finished {
            return Err(StorageError::IntegrityFailed(
                "training notes can only be archived after simulation_run is finished".into(),
            ));
        }
        let tnote = self.get_training_note(training_note_id)?;
        if tnote.run_id != run_id {
            return Err(StorageError::IntegrityFailed(
                "training_note does not belong to the specified run_id".into(),
            ));
        }
        let body = format!(
            "> 来源：来自模拟训练（run_id: `{}`, bar_index: `{}`）\n\n{}",
            run_id, tnote.bar_index, tnote.rationale
        );
        let structured = StructuredNoteFields {
            schema_version: 1,
            thesis: tnote.rationale.clone(),
            invalidation: String::new(),
            planned_risk: String::new(),
            actual_execution: tnote
                .linked_order_id
                .as_ref()
                .map(|oid| format!("sim_order:{oid}"))
                .unwrap_or_default(),
            emotion: tnote.emotion_tag.clone().unwrap_or_default(),
            conclusion: "来自模拟训练".to_string(),
        };
        let mut tags = vec!["来自模拟训练".to_string(), "simulated_training".to_string()];
        if let Some(ref emo) = tnote.emotion_tag {
            if !emo.trim().is_empty() {
                tags.push(emo.clone());
            }
        }
        self.save_note_revision(
            journal_note_id,
            title,
            "post_trade",
            true,
            &body,
            &structured,
            &tags,
        )?;
        self.add_note_link(
            journal_note_id,
            "training_note",
            training_note_id,
            1,
            None,
            "archived_from_training",
        )?;
        self.link_archived_journal_note_id(run_id, training_note_id, journal_note_id)?;
        self.get_note(journal_note_id)
    }

    // ------------------------------------------------------------------------
    // 5. Recovery Bundle, Hash Verification & Damage Marking (`S-43`, `M4-A-09`)
    // ------------------------------------------------------------------------

    pub fn load_simulation_recovery_bundle(
        &self,
        run_id: &str,
    ) -> Result<SimulationRecoveryBundle, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        let session = self
            .conn()
            .query_row(
                "SELECT run_id, hidden_mode, market_category, display_symbol, real_instrument_id,
                        candidate_pool_hash, redraw_count, redraw_history_json,
                        warmup_start_at, history_start_at, train_start_at, train_end_at, reveal_end_at,
                        visible_until, current_step_index, total_steps, revealed
                 FROM training_session WHERE run_id = ?1",
                params![run_id],
                |r| Ok(map_training_session_row(r)),
            )
            .optional()?
            .transpose()?;
        let fee_profile = self.get_fee_profile_version(&run.fee_profile_version_id)?;
        let latest_checkpoint = self.latest_sim_checkpoint(run_id)?;
        let after_seq = latest_checkpoint.as_ref().map(|cp| cp.seq).unwrap_or(0);
        let events_after_checkpoint = self.list_simulation_events(run_id, Some(after_seq))?;
        let all_events = self.list_simulation_events(run_id, None)?;

        Ok(SimulationRecoveryBundle {
            run,
            session,
            fee_profile,
            latest_checkpoint,
            events_after_checkpoint,
            all_events,
        })
    }

    /// Mark a `simulation_run` as `'damaged'` when recovery state-hash or sequence verification fails (`M4-A-09`).
    pub fn mark_simulation_run_damaged(
        &mut self,
        run_id: &str,
        _reason: &str,
    ) -> Result<SimulationRunRecord, StorageError> {
        let now = Utc::now().to_rfc3339();
        let affected = self.conn().execute(
            "UPDATE simulation_run SET status = 'damaged', updated_at = ?2 WHERE run_id = ?1",
            params![run_id, now],
        )?;
        if affected == 0 {
            return Err(StorageError::IntegrityFailed(format!(
                "simulation_run '{run_id}' not found"
            )));
        }
        self.get_simulation_run(run_id)
    }

    /// Verify structural sequence/checkpoint/latest hashes AND caller-recomputed replay hashes;
    /// if any mismatch is detected, atomically mark the run `'damaged'` instead of continuing (`M4-A-09`).
    pub fn verify_or_mark_run_damaged<F>(
        &mut self,
        run_id: &str,
        recompute_hashes: F,
    ) -> Result<SimulationRecoveryOutcome, StorageError>
    where
        F: FnOnce(&SimulationRecoveryBundle) -> Result<Vec<(i64, String)>, String>,
    {
        let bundle = self.load_simulation_recovery_bundle(run_id)?;
        if bundle.run.status == SimRunStatus::Damaged {
            return Ok(SimulationRecoveryOutcome::MarkedDamaged {
                run: Box::new(bundle.run),
                reason: "simulation_run was already marked damaged".into(),
            });
        }

        let mark_damaged =
            |store: &mut Self, reason: String| -> Result<SimulationRecoveryOutcome, StorageError> {
                let damaged = store.mark_simulation_run_damaged(run_id, &reason)?;
                Ok(SimulationRecoveryOutcome::MarkedDamaged {
                    run: Box::new(damaged),
                    reason,
                })
            };

        // 1. Check contiguous event log `1..=latest_seq`.
        if bundle.all_events.len() as i64 != bundle.run.latest_seq {
            let reason = format!(
                "event count ({}) does not match latest_seq ({})",
                bundle.all_events.len(),
                bundle.run.latest_seq
            );
            return mark_damaged(self, reason);
        }
        for (idx, ev) in bundle.all_events.iter().enumerate() {
            let expected_seq = (idx as i64) + 1;
            if ev.seq != expected_seq {
                let reason = format!(
                    "non-contiguous simulation_event seq: expected {expected_seq}, got {}",
                    ev.seq
                );
                return mark_damaged(self, reason);
            }
        }

        // 2. Check latest event hash matches `simulation_run.latest_state_hash`.
        if let Some(last_ev) = bundle.all_events.last() {
            if last_ev.state_hash != bundle.run.latest_state_hash {
                let reason = format!(
                    "latest simulation_event hash ({}) != simulation_run.latest_state_hash ({})",
                    last_ev.state_hash, bundle.run.latest_state_hash
                );
                return mark_damaged(self, reason);
            }
        }

        // 3. Check `latest_checkpoint` hash matches corresponding `simulation_event`.
        if let Some(ref cp) = bundle.latest_checkpoint {
            if cp.seq > bundle.run.latest_seq {
                let reason = format!(
                    "checkpoint seq ({}) exceeds run latest_seq ({})",
                    cp.seq, bundle.run.latest_seq
                );
                return mark_damaged(self, reason);
            }
            if cp.seq > 0 {
                let ev = &bundle.all_events[(cp.seq - 1) as usize];
                if ev.state_hash != cp.state_hash {
                    let reason = format!(
                        "checkpoint hash at seq {} ({}) != event hash ({})",
                        cp.seq, cp.state_hash, ev.state_hash
                    );
                    return mark_damaged(self, reason);
                }
            }
        }

        // 4. Verify caller-recomputed state hashes across replayed commands.
        match recompute_hashes(&bundle) {
            Err(err_reason) => mark_damaged(self, err_reason),
            Ok(recomputed) => {
                let stored_by_seq: BTreeMap<i64, &str> = bundle
                    .all_events
                    .iter()
                    .map(|e| (e.seq, e.state_hash.as_str()))
                    .collect();
                for (seq, actual_hash) in recomputed {
                    if seq == 0 {
                        if let Some(ref cp) = bundle.latest_checkpoint {
                            if cp.seq == 0 && cp.state_hash != actual_hash {
                                let reason = format!(
                                    "initial checkpoint hash mismatch: stored {}, recomputed {}",
                                    cp.state_hash, actual_hash
                                );
                                return mark_damaged(self, reason);
                            }
                        }
                        continue;
                    }
                    match stored_by_seq.get(&seq) {
                        None => {
                            let reason =
                                format!("recomputed hash returned unknown event seq {seq}");
                            return mark_damaged(self, reason);
                        }
                        Some(&expected_hash) if expected_hash != actual_hash => {
                            let reason = format!(
                                "state_hash mismatch at seq {seq}: stored {expected_hash}, recomputed {actual_hash}"
                            );
                            return mark_damaged(self, reason);
                        }
                        Some(_) => {}
                    }
                }
                Ok(SimulationRecoveryOutcome::Recovered(Box::new(bundle)))
            }
        }
    }

    // ------------------------------------------------------------------------
    // 6. Delete Preview, Atomic Single-Run Delete & Open Export Bundle (`M4-A-11`)
    // ------------------------------------------------------------------------

    pub fn preview_delete_training_run(
        &self,
        run_id: &str,
    ) -> Result<DeleteTrainingRunPreview, StorageError> {
        preview_delete_on_conn(self.conn(), run_id)
    }

    /// Delete a single training run and all of its owned rows in one atomic transaction
    /// without affecting other runs, parent/child runs, archived journal notes, or real ledger (`M4-A-11`).
    pub fn delete_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<DeleteTrainingRunPreview, StorageError> {
        let tx = self.conn_mut().savepoint()?;
        let preview = preview_delete_on_conn(&tx, run_id)?;
        tx.execute(
            "DELETE FROM training_note WHERE run_id = ?1",
            params![run_id],
        )?;
        tx.execute("DELETE FROM sim_fill WHERE run_id = ?1", params![run_id])?;
        tx.execute("DELETE FROM sim_order WHERE run_id = ?1", params![run_id])?;
        tx.execute(
            "DELETE FROM sim_checkpoint WHERE run_id = ?1",
            params![run_id],
        )?;
        tx.execute(
            "DELETE FROM simulation_event WHERE run_id = ?1",
            params![run_id],
        )?;
        tx.execute(
            "DELETE FROM training_session WHERE run_id = ?1",
            params![run_id],
        )?;
        tx.execute(
            "DELETE FROM simulation_run WHERE run_id = ?1",
            params![run_id],
        )?;
        tx.commit()?;
        Ok(preview)
    }

    /// Snapshot all 8 Schema 5 tables in deterministic order for library open-format export (`S-43`, `M4-A-11`).
    pub fn export_simulation_tables_snapshot(
        &self,
    ) -> Result<SimulationExportBundle, StorageError> {
        let fee_profile_versions = self.list_fee_profile_versions()?;
        let mut simulation_runs = self.list_simulation_runs()?;
        simulation_runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));

        let mut training_sessions = Vec::new();
        let mut sim_orders = Vec::new();
        let mut sim_fills = Vec::new();
        let mut simulation_events = Vec::new();
        let mut sim_checkpoints = Vec::new();
        let mut training_notes = Vec::new();

        for run in &simulation_runs {
            if let Ok(sess) = self.get_training_session(&run.run_id) {
                training_sessions.push(sess);
            }
            sim_orders.extend(self.list_sim_orders(&run.run_id)?);
            sim_fills.extend(self.list_sim_fills(&run.run_id)?);
            simulation_events.extend(self.list_simulation_events(&run.run_id, None)?);
            sim_checkpoints.extend(self.list_sim_checkpoints(&run.run_id)?);
            training_notes.extend(self.list_training_notes(&run.run_id)?);
        }

        Ok(SimulationExportBundle {
            fee_profile_versions,
            simulation_runs,
            training_sessions,
            sim_orders,
            sim_fills,
            simulation_events,
            sim_checkpoints,
            training_notes,
        })
    }
}

fn upsert_training_note_on_conn(
    conn: &Connection,
    note: &TrainingNoteRecord,
) -> Result<(), StorageError> {
    if note.note_id.trim().is_empty() || note.rationale.trim().is_empty() {
        return Err(StorageError::IntegrityFailed(
            "training_note note_id and rationale must be non-empty".into(),
        ));
    }
    if note.bar_index < 0 {
        return Err(StorageError::IntegrityFailed(
            "training_note bar_index must be >= 0".into(),
        ));
    }
    if let Some(ref oid) = note.linked_order_id {
        let ord_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sim_order WHERE run_id = ?1 AND order_id = ?2)",
            params![note.run_id, oid],
            |r| r.get(0),
        )?;
        if !ord_exists {
            return Err(StorageError::IntegrityFailed(format!(
                "linked_order_id '{oid}' does not exist in run '{}'",
                note.run_id
            )));
        }
    }
    conn.execute(
        "INSERT INTO training_note (
            note_id, run_id, bar_index, visible_until, rationale,
            emotion_tag, linked_order_id, archived_journal_note_id, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(note_id) DO UPDATE SET
            rationale = excluded.rationale,
            emotion_tag = excluded.emotion_tag,
            linked_order_id = excluded.linked_order_id,
            archived_journal_note_id = COALESCE(excluded.archived_journal_note_id, training_note.archived_journal_note_id),
            updated_at = excluded.updated_at
         WHERE training_note.run_id = excluded.run_id",
        params![
            note.note_id,
            note.run_id,
            note.bar_index,
            note.visible_until.to_rfc3339(),
            note.rationale,
            note.emotion_tag,
            note.linked_order_id,
            note.archived_journal_note_id,
            note.created_at,
            note.updated_at,
        ],
    )?;
    let owner_run: String = conn.query_row(
        "SELECT run_id FROM training_note WHERE note_id = ?1",
        params![note.note_id],
        |r| r.get(0),
    )?;
    if owner_run != note.run_id {
        return Err(StorageError::IntegrityFailed(format!(
            "training_note '{}' already belongs to another run '{}'",
            note.note_id, owner_run
        )));
    }
    Ok(())
}

fn preview_delete_on_conn(
    conn: &Connection,
    run_id: &str,
) -> Result<DeleteTrainingRunPreview, StorageError> {
    let (status_str, parent_run_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, parent_run_id FROM simulation_run WHERE run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StorageError::IntegrityFailed(format!("simulation_run '{run_id}' not found"))
        })?;
    let status = SimRunStatus::parse(&status_str)?;

    let mut child_stmt = conn.prepare(
        "SELECT run_id FROM simulation_run WHERE parent_run_id = ?1 ORDER BY run_id ASC",
    )?;
    let child_run_ids: Vec<String> = child_stmt
        .query_map(params![run_id], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let count_for = |table: &str| -> Result<usize, StorageError> {
        let c: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE run_id = ?1"),
            params![run_id],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(c).unwrap_or(0))
    };

    let archived_cnt: i64 = conn.query_row(
        "SELECT COUNT(*) FROM training_note WHERE run_id = ?1 AND archived_journal_note_id IS NOT NULL",
        params![run_id],
        |r| r.get(0),
    )?;

    let _ = CURRENT_SCHEMA_VERSION;
    Ok(DeleteTrainingRunPreview {
        run_id: run_id.to_string(),
        status,
        parent_run_id,
        child_run_ids,
        order_count: count_for("sim_order")?,
        fill_count: count_for("sim_fill")?,
        note_count: count_for("training_note")?,
        archived_journal_note_count: usize::try_from(archived_cnt).unwrap_or(0),
        checkpoint_count: count_for("sim_checkpoint")?,
        event_count: count_for("simulation_event")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m2_schema::{AiMessageRecord, AiRunRecord, ModelConnectionRecord};
    use delta_core::{CoverageGap, DataQualityIssue};

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new(prefix: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "delta_m4_schema_{prefix}_{}_{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&dir).expect("create isolated temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_equity_bar(instrument_id: &str, d: NaiveDate, close_px: &str, dsv: i64) -> BarRecord {
        let sess = query_calendar_session("XNAS", d);
        let open_at = sess.open_at.expect("equity trading open_at");
        let close_at = sess.close_at.expect("equity trading close_at");
        let px = parse_decimal_lossless(close_px).unwrap();
        BarRecord {
            id: format!("BAR:{instrument_id}:1d:raw:yahoo:{d}:{dsv}"),
            instrument_id: instrument_id.to_string(),
            timeframe: "1d".to_string(),
            session: sess.kind.as_str().to_string(),
            session_date: d,
            open_at,
            close_at,
            open: px,
            high: px + Decimal::from(2),
            low: (px - Decimal::ONE).max(Decimal::ONE),
            close: px,
            volume: Decimal::from(100_000),
            source: "yahoo".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: dsv,
            valid_to_version: None,
            raw_payload_hash: None,
        }
    }

    fn make_crypto_bar(instrument_id: &str, d: NaiveDate, close_px: &str, dsv: i64) -> BarRecord {
        let sess = query_calendar_session("CRYPTO_24_7", d);
        let open_at = sess.open_at.expect("crypto open_at");
        let close_at = sess.close_at.expect("crypto close_at");
        let px = parse_decimal_lossless(close_px).unwrap();
        BarRecord {
            id: format!("BAR:{instrument_id}:1d:raw:okx:{d}:{dsv}"),
            instrument_id: instrument_id.to_string(),
            timeframe: "1d".to_string(),
            session: "24x7".to_string(),
            session_date: d,
            open_at,
            close_at,
            open: px,
            high: px + Decimal::from(50),
            low: (px - Decimal::from(50)).max(Decimal::ONE),
            close: px,
            volume: Decimal::from(1_500),
            source: "okx".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: dsv,
            valid_to_version: None,
            raw_payload_hash: None,
        }
    }

    fn trading_days_from(venue: &str, start: NaiveDate, count: usize) -> Vec<NaiveDate> {
        let mut out = Vec::with_capacity(count);
        let mut cur = start;
        while out.len() < count {
            let sess = query_calendar_session(venue, cur);
            if matches!(
                sess.kind,
                CalendarSessionKind::Regular
                    | CalendarSessionKind::EarlyClose
                    | CalendarSessionKind::Crypto24x7
            ) {
                out.push(cur);
            }
            cur = cur.checked_add_days(chrono::Days::new(1)).unwrap();
        }
        out
    }

    #[test]
    fn test_m4_k01_and_a01_sql_truncation_with_sentinels_clock_monotonicity_and_reveal_gate() {
        let tmp = TestTempDir::new("k01_a01");
        let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

        let aapl = store
            .register_market_instrument("NASDAQ", "AAPL", false)
            .unwrap();
        let btc = store
            .register_market_instrument("OKX", "BTC/USDT", false)
            .unwrap();

        let dsv = store
            .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
            .unwrap()
            .version_num;

        // 12 contiguous XNAS trading days starting 2025-01-02:
        // [0..2] warmup, [2..5] history, [5..9] train, [9..12] reveal.
        let eq_days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 1, 2).unwrap(), 12);
        let eq_bars: Vec<BarRecord> = eq_days
            .iter()
            .enumerate()
            .map(|(idx, &d)| make_equity_bar(&aapl, d, &(100 + idx as i64).to_string(), dsv))
            .collect();
        store
            .commit_bars_and_checkpoint(&eq_bars, dsv, None)
            .unwrap();

        // Insert a non-final bar inside the visible window to verify `is_final = 1` SQL filtering.
        store
            .conn()
            .execute(
                "INSERT INTO bar (
                    id, instrument_id, timeframe, session, session_date, open_at, close_at,
                    open, high, low, close, volume, source, adjustment, is_final,
                    valid_from_version, valid_to_version, raw_payload_hash
                 ) VALUES (
                    'BAR:UNFINALIZED', ?1, '1d', 'regular', ?2, ?3, ?4,
                    '7777777', '7777777', '7777777', '7777777', '1', 'intraday_preview', 'raw', 0,
                    ?5, NULL, NULL
                 )",
                params![
                    aapl,
                    eq_days[4].to_string(),
                    eq_bars[4].open_at.to_rfc3339(),
                    eq_bars[4].close_at.to_rfc3339(),
                    dsv,
                ],
            )
            .unwrap();

        // Insert historical + future corporate actions.
        let past_div = CorporateActionRecord {
            id: "CA:AAPL:PAST_DIV".to_string(),
            instrument_id: aapl.clone(),
            action_type: CorporateActionType::CashDividend,
            ex_date: eq_days[3],
            pay_date: Some(eq_days[4]),
            ratio_or_amount: parse_decimal_lossless("0.25").unwrap(),
            currency: Some("USD".to_string()),
            announced_at: None,
            source: "yahoo".to_string(),
            factor_version: dsv,
            raw_payload_hash: None,
        };
        let future_split = CorporateActionRecord {
            id: "CA:AAPL:FUTURE_SENTINEL_SPLIT".to_string(),
            instrument_id: aapl.clone(),
            action_type: CorporateActionType::Split,
            ex_date: eq_days[6],
            pay_date: None,
            ratio_or_amount: Decimal::from(999_999),
            currency: None,
            announced_at: None,
            source: "yahoo".to_string(),
            factor_version: dsv,
            raw_payload_hash: None,
        };
        let future_div = CorporateActionRecord {
            id: "CA:AAPL:FUTURE_POISON_DIV".to_string(),
            instrument_id: aapl.clone(),
            action_type: CorporateActionType::CashDividend,
            ex_date: eq_days[10],
            pay_date: None,
            ratio_or_amount: Decimal::from(888_888),
            currency: Some("USD".to_string()),
            announced_at: None,
            source: "yahoo".to_string(),
            factor_version: dsv,
            raw_payload_hash: None,
        };
        store
            .upsert_corporate_actions(&[past_div, future_split, future_div])
            .unwrap();

        // Also commit 4 crypto bars and a future action on day[2] to verify UTC midnight ex_date boundary.
        let crypto_days = trading_days_from(
            "CRYPTO_24_7",
            NaiveDate::from_ymd_opt(2025, 2, 1).unwrap(),
            4,
        );
        let crypto_bars: Vec<BarRecord> = crypto_days
            .iter()
            .enumerate()
            .map(|(i, &d)| make_crypto_bar(&btc, d, &(90_000 + i as i64 * 100).to_string(), dsv))
            .collect();
        store
            .commit_bars_and_checkpoint(&crypto_bars, dsv, None)
            .unwrap();
        store
            .upsert_corporate_actions(&[CorporateActionRecord {
                id: "CA:BTC:NEXT_DAY".to_string(),
                instrument_id: btc.clone(),
                action_type: CorporateActionType::Split,
                ex_date: crypto_days[2],
                pay_date: None,
                ratio_or_amount: Decimal::from(555_555),
                currency: None,
                announced_at: None,
                source: "okx".to_string(),
                factor_version: dsv,
                raw_payload_hash: None,
            }])
            .unwrap();

        // Overwrite future bars (idx >= 5) and future corporate actions in SQLite with:
        // (a) extreme 999999999 sentinels, and
        // (b) non-decimal poison text ("POISON_FUTURE_*") that would fail parsing if SQL ever read those rows!
        store
            .conn()
            .execute(
                "UPDATE bar SET open='999999999', high='999999999', low='999999999', close='999999999'
                 WHERE instrument_id = ?1 AND session_date = ?2",
                params![aapl, eq_days[5].to_string()],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE bar SET open='POISON_FUTURE_BAR', high='POISON_FUTURE_BAR', low='POISON_FUTURE_BAR', close='POISON_FUTURE_BAR'
                 WHERE instrument_id = ?1 AND session_date >= ?2",
                params![aapl, eq_days[6].to_string()],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE corporate_action SET ratio_or_amount = 'POISON_FUTURE_DIV'
                 WHERE id = 'CA:AAPL:FUTURE_POISON_DIV'",
                [],
            )
            .unwrap();

        let warmup_start = eq_bars[0].open_at;
        let history_start = eq_bars[2].open_at;
        let initial_visible_until = eq_bars[4].close_at;

        // 1. SQL-level bar truncation with warmup + Z-format timestamp offset normalization.
        let vis_bars_with_warmup = store
            .query_training_visible_final_bars(&TrainingBarPortQuery {
                instrument_id: aapl.clone(),
                timeframe: "1d".to_string(),
                source: None,
                dataset_version: dsv,
                warmup_start_at: Some(warmup_start),
                history_start_at: Some(history_start),
                visible_until: initial_visible_until,
                previous_visible_until: None,
            })
            .expect("SQL truncation must never read future sentinel or poison bars");
        assert_eq!(vis_bars_with_warmup.len(), 5);
        assert!(vis_bars_with_warmup.iter().all(|b| b.is_final
            && b.close_at <= initial_visible_until
            && b.close < Decimal::from(200)));

        // Also verify `query_bars_at_version` pushes `visible_until` into SQL and does not touch poison rows.
        let at_ver_truncated = store
            .query_bars_at_version(
                &aapl,
                "1d",
                AdjustmentKind::Raw,
                Some("yahoo"),
                dsv,
                Some(initial_visible_until),
            )
            .expect("query_bars_at_version SQL cutoff must avoid future poison rows");
        assert_eq!(at_ver_truncated.len(), 5);

        // 2. SQL-level corporate action truncation (`ex_date <= effective_date(visible_until)`).
        let vis_actions = store
            .query_training_visible_corporate_actions(&aapl, dsv, initial_visible_until)
            .expect("SQL corporate_action truncation must never read future sentinel/poison CAs");
        assert_eq!(vis_actions.len(), 1);
        assert_eq!(vis_actions[0].id, "CA:AAPL:PAST_DIV");

        // Crypto midnight boundary: `crypto_bars[1].close_at` is `crypto_days[2] 00:00:00Z`,
        // which belongs to session `crypto_days[1]`, so `ex_date == crypto_days[2]` must NOT leak!
        let crypto_vis_actions = store
            .query_training_visible_corporate_actions(&btc, dsv, crypto_bars[1].close_at)
            .unwrap();
        assert!(
            crypto_vis_actions.is_empty(),
            "crypto midnight close_at must not leak next session's corporate action"
        );

        // 3. Monotonic replay clock and warmup/history bound enforcement.
        let backward_err = store
            .query_training_visible_final_bars(&TrainingBarPortQuery {
                instrument_id: aapl.clone(),
                timeframe: "1d".to_string(),
                source: None,
                dataset_version: dsv,
                warmup_start_at: Some(warmup_start),
                history_start_at: Some(history_start),
                visible_until: eq_bars[3].close_at,
                previous_visible_until: Some(initial_visible_until),
            })
            .unwrap_err();
        assert!(backward_err.to_string().contains("visible_until"));

        // 4. Create an active training run and verify reveal gate + SQLite monotonic trigger.
        let now_str = "2026-11-12T10:00:00+00:00".to_string();
        let run = SimulationRunRecord {
            run_id: "RUN-K01".to_string(),
            kind: "training".to_string(),
            seed: 42,
            dataset_version: dsv,
            engine_version: "m4-v1".to_string(),
            fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
            quote_currency: "USD".to_string(),
            initial_cash: Decimal::from(100_000),
            status: SimRunStatus::Active,
            parent_run_id: None,
            latest_seq: 0,
            latest_state_hash: "HASH-0".to_string(),
            created_at: now_str.clone(),
            updated_at: now_str,
            finished_at: None,
        };
        let session = TrainingSessionRecord {
            run_id: "RUN-K01".to_string(),
            hidden_mode: true,
            market_category: "us_equity".to_string(),
            display_symbol: "标的 A".to_string(),
            real_instrument_id: aapl.clone(),
            candidate_pool_hash: "POOL-HASH".to_string(),
            redraw_count: 0,
            redraw_history: Vec::new(),
            warmup_start_at: warmup_start,
            history_start_at: history_start,
            train_start_at: eq_bars[5].open_at,
            train_end_at: eq_bars[8].close_at,
            reveal_end_at: eq_bars[11].close_at,
            visible_until: initial_visible_until,
            current_step_index: 0,
            total_steps: 4,
            revealed: false,
        };
        store.create_training_run(&run, &session, None).unwrap();

        let run_bars_no_warmup = store
            .query_run_visible_final_bars("RUN-K01", false)
            .unwrap();
        assert_eq!(run_bars_no_warmup.len(), 3); // eq_days[2..=4]
        let run_bars_with_warmup = store.query_run_visible_final_bars("RUN-K01", true).unwrap();
        assert_eq!(run_bars_with_warmup.len(), 5); // eq_days[0..=4]

        // Premature reveal MUST fail while status == 'active'.
        assert!(store.query_post_finish_reveal("RUN-K01").is_err());
        assert!(store.mark_training_session_revealed("RUN-K01").is_err());
        assert!(store
            .conn()
            .execute(
                "UPDATE training_session SET revealed = 1 WHERE run_id = 'RUN-K01'",
                []
            )
            .is_err());
        assert!(store
            .conn()
            .execute(
                "UPDATE training_session SET visible_until = ?1 WHERE run_id = 'RUN-K01'",
                params![eq_bars[3].close_at.to_rfc3339()],
            )
            .is_err());

        // Restore valid numeric sentinel values on eq_days[5..12] and finish the run, then verify reveal succeeds.
        store
            .conn()
            .execute(
                "UPDATE bar SET open='999999999', high='999999999', low='999999999', close='999999999'
                 WHERE instrument_id = ?1 AND session_date >= ?2",
                params![aapl, eq_days[6].to_string()],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE corporate_action SET ratio_or_amount = '888888'
                 WHERE id = 'CA:AAPL:FUTURE_POISON_DIV'",
                [],
            )
            .unwrap();

        store
            .commit_simulation_step_atomic(&SimulationStepCommit {
                run_id: "RUN-K01".to_string(),
                expected_prev_seq: 0,
                seq: 1,
                event_kind: "finish_session".to_string(),
                command_json: r#"{"cmd":"finish"}"#.to_string(),
                events_json: r#"[{"ev":"session_finished"}]"#.to_string(),
                new_status: SimRunStatus::Finished,
                new_visible_until: eq_bars[8].close_at,
                new_step_index: 4,
                new_state_hash: "HASH-FIN".to_string(),
                orders_to_upsert: Vec::new(),
                fills_to_insert: Vec::new(),
                notes_to_upsert: Vec::new(),
                checkpoint: Some(SimCheckpointRecord {
                    run_id: "RUN-K01".to_string(),
                    seq: 1,
                    reason: SimCheckpointReason::Finish,
                    state_json: r#"{"status":"finished"}"#.to_string(),
                    state_hash: "HASH-FIN".to_string(),
                    created_at: "2026-11-12T10:05:00+00:00".to_string(),
                }),
                committed_at: parse_rfc3339_utc("2026-11-12T10:05:00+00:00", "committed_at")
                    .unwrap(),
            })
            .unwrap();

        let reveal = store
            .query_post_finish_reveal("RUN-K01")
            .expect("post-finish reveal must succeed once status == 'finished'");
        assert_eq!(reveal.real_instrument_id, aapl);
        assert_eq!(reveal.real_symbol, "AAPL");
        assert_eq!(reveal.future_reveal_bars.len(), 3); // eq_days[9..=11]
        assert_eq!(reveal.all_bars_through_reveal.len(), 12);
        assert_eq!(reveal.corporate_actions_through_reveal.len(), 3);

        let revealed_sess = store.mark_training_session_revealed("RUN-K01").unwrap();
        assert!(revealed_sess.revealed);
    }

    #[test]
    fn test_m4_a02_candidate_pool_exclusions_determinism_and_redraw_rules() {
        let tmp = TestTempDir::new("a02_pool");
        let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

        let inst_clean = store
            .register_market_instrument("NASDAQ", "CLEAN", false)
            .unwrap();
        let inst_calgap = store
            .register_market_instrument("NASDAQ", "CALGAP", false)
            .unwrap();
        let inst_covgap = store
            .register_market_instrument("NASDAQ", "COVGAP", false)
            .unwrap();
        let inst_warn = store
            .register_market_instrument("NASDAQ", "QUALWARN", false)
            .unwrap();
        let inst_badca = store
            .register_market_instrument("NASDAQ", "BADCA", false)
            .unwrap();
        let inst_unexp = store
            .register_market_instrument("NASDAQ", "UNEXPSPLIT", false)
            .unwrap();
        let inst_crypto = store
            .register_market_instrument("OKX", "ETH/USDT", false)
            .unwrap();

        let dsv = store
            .create_dataset_version("yahoo", "personal", "{}", "warning", "exact")
            .unwrap()
            .version_num;

        // 10 trading days; segment requirement: history=3 + train=4 + reveal=2 = 9 bars (plus warmup=1 -> 10 bars).
        let eq_days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 3, 3).unwrap(), 10);
        let crypto_days = trading_days_from(
            "CRYPTO_24_7",
            NaiveDate::from_ymd_opt(2025, 3, 3).unwrap(),
            10,
        );

        // 1. CLEAN equity with a valid 4-for-1 split on eq_days[5] (400 -> 100).
        let clean_bars: Vec<BarRecord> = eq_days
            .iter()
            .enumerate()
            .map(|(i, &d)| {
                let px = if i < 5 { "400" } else { "100" };
                make_equity_bar(&inst_clean, d, px, dsv)
            })
            .collect();
        store
            .commit_bars_and_checkpoint(&clean_bars, dsv, None)
            .unwrap();
        store
            .upsert_corporate_actions(&[CorporateActionRecord {
                id: "CA:CLEAN:SPLIT4".to_string(),
                instrument_id: inst_clean.clone(),
                action_type: CorporateActionType::Split,
                ex_date: eq_days[5],
                pay_date: None,
                ratio_or_amount: Decimal::from(4),
                currency: None,
                announced_at: None,
                source: "yahoo".to_string(),
                factor_version: dsv,
                raw_payload_hash: None,
            }])
            .unwrap();

        // 2. Clean 24x7 crypto instrument.
        let eth_bars: Vec<BarRecord> = crypto_days
            .iter()
            .enumerate()
            .map(|(i, &d)| {
                make_crypto_bar(&inst_crypto, d, &(2500 + i as i64 * 10).to_string(), dsv)
            })
            .collect();
        store
            .commit_bars_and_checkpoint(&eth_bars, dsv, None)
            .unwrap();

        // 3. CALGAP: 11 trading days generated, skip index 4 so 10 bars exist but there is a missing trading session!
        let eq_days_11 =
            trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 3, 3).unwrap(), 11);
        let calgap_bars: Vec<BarRecord> = eq_days_11
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 4)
            .map(|(_, &d)| make_equity_bar(&inst_calgap, d, "150", dsv))
            .collect();
        store
            .commit_bars_and_checkpoint(&calgap_bars, dsv, None)
            .unwrap();

        // 4. COVGAP: has all 10 bars but an unresolved `coverage_gap` recorded on eq_days[4].
        let covgap_bars: Vec<BarRecord> = eq_days
            .iter()
            .map(|&d| make_equity_bar(&inst_covgap, d, "150", dsv))
            .collect();
        store
            .commit_bars_and_checkpoint(&covgap_bars, dsv, None)
            .unwrap();
        let gap = CoverageGap {
            id: "GAP:COVGAP:1".to_string(),
            dataset_version: dsv,
            instrument_id: inst_covgap.clone(),
            start_date: eq_days[4],
            end_date: eq_days[4],
            gap_kind: "provider_missing".to_string(),
            resolution: "open".to_string(),
        };
        store
            .conn()
            .execute(
                "INSERT INTO coverage_gap (id, dataset_version, instrument_id, start_date, end_date, gap_kind, resolution)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    gap.id,
                    gap.dataset_version,
                    gap.instrument_id,
                    gap.start_date.to_string(),
                    gap.end_date.to_string(),
                    gap.gap_kind,
                    gap.resolution
                ],
            )
            .unwrap();

        // 5. QUALWARN: has `data_quality_issue` warning on eq_days[6].
        let warn_bars: Vec<BarRecord> = eq_days
            .iter()
            .map(|&d| make_equity_bar(&inst_warn, d, "150", dsv))
            .collect();
        store
            .commit_bars_and_checkpoint(&warn_bars, dsv, None)
            .unwrap();
        store
            .record_quality_issues(&[DataQualityIssue {
                id: "DQI:QUALWARN:1".to_string(),
                dataset_version: dsv,
                instrument_id: inst_warn.clone(),
                session_date: eq_days[6],
                kind: "partial".to_string(),
                sources: "yahoo".to_string(),
                detail: "partial/warning bar".to_string(),
            }])
            .unwrap();

        // 6. BADCA: has an incomplete corporate action (`ratio_or_amount = ''` / missing amount) on eq_days[5].
        let badca_bars: Vec<BarRecord> = eq_days
            .iter()
            .map(|&d| make_equity_bar(&inst_badca, d, "150", dsv))
            .collect();
        store
            .commit_bars_and_checkpoint(&badca_bars, dsv, None)
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO corporate_action (
                    id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
                 ) VALUES (?1, ?2, 'cash_dividend', ?3, NULL, '', 'USD', NULL, 'yahoo', ?4, NULL)",
                params!["CA:BADCA:EMPTY", inst_badca, eq_days[5].to_string(), dsv],
            )
            .unwrap();

        // 7. UNEXPSPLIT: 400 -> 100 drop on eq_days[5] with NO corporate_action row!
        let unexp_bars: Vec<BarRecord> = eq_days
            .iter()
            .enumerate()
            .map(|(i, &d)| {
                let px = if i < 5 { "400" } else { "100" };
                make_equity_bar(&inst_unexp, d, px, dsv)
            })
            .collect();
        store
            .commit_bars_and_checkpoint(&unexp_bars, dsv, None)
            .unwrap();

        let query_all = CandidatePoolQuery {
            dataset_version: dsv,
            market_category: "all".to_string(),
            instrument_id: None,
            start_date: None,
            end_date: None,
            warmup_bars: 1,
            history_len: 3,
            train_len: 4,
            reveal_len: 2,
        };
        let pool_1 = store.query_training_candidate_pool(&query_all).unwrap();
        let pool_2 = store.query_training_candidate_pool(&query_all).unwrap();
        assert_eq!(pool_1.candidate_pool_hash, pool_2.candidate_pool_hash);
        assert_eq!(pool_1.candidates, pool_2.candidates);

        let selected_instruments: BTreeSet<String> = pool_1
            .candidates
            .iter()
            .map(|c| c.instrument_id.clone())
            .collect();
        assert_eq!(
            selected_instruments,
            BTreeSet::from([inst_clean.clone(), inst_crypto.clone()]),
            "only clean equity and clean crypto segments may enter candidate pool"
        );
        // 10 bars with required_len = 9 yields 2 windows per clean instrument: start_idx=0 (warmup=0) and start_idx=1 (warmup=1).
        assert_eq!(pool_1.candidates.len(), 4);
        let warmup_1_seg = pool_1
            .candidates
            .iter()
            .find(|c| c.instrument_id == inst_clean && c.warmup_bar_count == 1)
            .expect("clean equity segment with 1 warmup bar");
        assert!(warmup_1_seg.warmup_start_at < warmup_1_seg.history_start_at);

        // Verify category filter.
        let pool_eq = store
            .query_training_candidate_pool(&CandidatePoolQuery {
                market_category: "us_equity".to_string(),
                ..query_all.clone()
            })
            .unwrap();
        assert!(pool_eq
            .candidates
            .iter()
            .all(|c| c.instrument_id == inst_clean));

        // Verify redraw BEFORE start succeeds & records history, and redraw AFTER start is refused (`S-38`, `M4-A-02`).
        let seg0 = &pool_1.candidates[0];
        let seg1 = &pool_1.candidates[3];
        let now_str = "2026-11-12T11:00:00+00:00".to_string();
        store
            .create_training_run(
                &SimulationRunRecord {
                    run_id: "RUN-REDRAW".to_string(),
                    kind: "training".to_string(),
                    seed: 777,
                    dataset_version: dsv,
                    engine_version: "m4-v1".to_string(),
                    fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
                    quote_currency: seg0.quote_currency.clone(),
                    initial_cash: Decimal::from(50_000),
                    status: SimRunStatus::Preparing,
                    parent_run_id: None,
                    latest_seq: 0,
                    latest_state_hash: "H0".to_string(),
                    created_at: now_str.clone(),
                    updated_at: now_str,
                    finished_at: None,
                },
                &TrainingSessionRecord {
                    run_id: "RUN-REDRAW".to_string(),
                    hidden_mode: true,
                    market_category: "all".to_string(),
                    display_symbol: "标的 A".to_string(),
                    real_instrument_id: seg0.instrument_id.clone(),
                    candidate_pool_hash: pool_1.candidate_pool_hash.clone(),
                    redraw_count: 0,
                    redraw_history: Vec::new(),
                    warmup_start_at: seg0.warmup_start_at,
                    history_start_at: seg0.history_start_at,
                    train_start_at: seg0.train_start_at,
                    train_end_at: seg0.train_end_at,
                    reveal_end_at: seg0.reveal_end_at,
                    visible_until: seg0.initial_visible_until,
                    current_step_index: 0,
                    total_steps: seg0.train_len as i64,
                    revealed: false,
                },
                None,
            )
            .unwrap();

        let redrawn = store
            .redraw_training_session_before_start(
                "RUN-REDRAW",
                seg1,
                "标的 B",
                "H0-REDRAWN",
                parse_rfc3339_utc("2026-11-12T11:01:00+00:00", "redrawn_at").unwrap(),
            )
            .unwrap();
        assert_eq!(redrawn.redraw_count, 1);
        assert_eq!(redrawn.redraw_history.len(), 1);
        assert_eq!(redrawn.real_instrument_id, seg1.instrument_id);
        assert_eq!(redrawn.display_symbol, "标的 B");

        store
            .start_training_run(
                "RUN-REDRAW",
                parse_rfc3339_utc("2026-11-12T11:02:00+00:00", "started_at").unwrap(),
            )
            .unwrap();

        assert!(store
            .redraw_training_session_before_start(
                "RUN-REDRAW",
                seg0,
                "标的 C",
                "H-ILLEGAL",
                parse_rfc3339_utc("2026-11-12T11:03:00+00:00", "redrawn_at").unwrap(),
            )
            .is_err());
        assert!(store
            .conn()
            .execute(
                "UPDATE training_session SET redraw_count = 99 WHERE run_id = 'RUN-REDRAW'",
                []
            )
            .is_err());
    }

    #[test]
    fn test_m4_a08_fee_profile_immutability_versioning_and_builtin_provenance() {
        let tmp = TestTempDir::new("a08_fees");
        let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

        let dsv = store
            .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let aapl = store
            .register_market_instrument("NASDAQ", "AAPL", false)
            .unwrap();
        let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 4, 1).unwrap(), 4);
        let bars: Vec<BarRecord> = days
            .iter()
            .map(|&d| make_equity_bar(&aapl, d, "180", dsv))
            .collect();
        store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();

        // 1. Verify all 3 built-in S-41 scenario-assumption templates exist and are immutable.
        let builtins = store.list_fee_profile_versions().unwrap();
        assert_eq!(builtins.len(), 3);
        assert!(builtins
            .iter()
            .all(|b| b.is_builtin && b.is_scenario_assumption && !b.assumption_note.is_empty()));

        let mut try_mutate_builtin = store
            .get_fee_profile_version(BUILTIN_FEE_ZERO_COST_ID)
            .unwrap();
        try_mutate_builtin.fixed_fee_per_order = Decimal::from(5);
        assert!(store
            .mutate_fee_profile_version_in_place(&try_mutate_builtin)
            .is_err());
        assert!(store
            .delete_fee_profile_version(BUILTIN_FEE_ZERO_COST_ID)
            .is_err());

        // 2. Unverified built-in platform preset without official URL & retrieval date must be rejected (S-41).
        let unverified_builtin = FeeProfileVersionRecord {
            id: "FPV:BAD:BROKER:v1".to_string(),
            profile_key: "bad_broker".to_string(),
            version_num: 1,
            name: "Unverified Broker Preset".to_string(),
            market_category: "us_equity".to_string(),
            venue: Some("NASDAQ".to_string()),
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::new(5, 3),
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: Some(Decimal::ONE),
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage_mode: SlippageMode::None,
            slippage_value: Decimal::ZERO,
            half_spread_bps: Decimal::ZERO,
            rules_json: "{}".to_string(),
            is_builtin: true,
            is_scenario_assumption: false,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note: "Missing official provenance".to_string(),
            created_at: "2026-11-12T00:00:00+00:00".to_string(),
        };
        assert!(store
            .insert_fee_profile_version(&unverified_builtin)
            .is_err());

        // 3. Create custom profile v1, bind run_1 to v1, verify v1 is frozen, then create v2 for run_2.
        let v1_draft = FeeProfileVersionRecord {
            id: String::new(),
            profile_key: "my_us_desk".to_string(),
            version_num: 1,
            name: "My US Desk v1".to_string(),
            market_category: "us_equity".to_string(),
            venue: None,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::new(5, 3),
            fixed_fee_per_order: Decimal::ONE,
            min_fee_per_order: Some(Decimal::ONE),
            max_fee_per_order: Some(Decimal::from(25)),
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage_mode: SlippageMode::Ticks,
            slippage_value: Decimal::ONE,
            half_spread_bps: Decimal::from(2),
            rules_json: r#"{"desk":"v1"}"#.to_string(),
            is_builtin: false,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note: "Custom desk assumption v1".to_string(),
            created_at: "2026-11-12T12:00:00+00:00".to_string(),
        };
        let v1 = store.create_next_fee_profile_version(v1_draft).unwrap();
        assert_eq!(v1.id, "FPV:MY_US_DESK:v1");
        assert_eq!(v1.version_num, 1);

        let make_run = |run_id: &str, fp_id: &str| {
            (
                SimulationRunRecord {
                    run_id: run_id.to_string(),
                    kind: "training".to_string(),
                    seed: 101,
                    dataset_version: dsv,
                    engine_version: "m4-v1".to_string(),
                    fee_profile_version_id: fp_id.to_string(),
                    quote_currency: "USD".to_string(),
                    initial_cash: Decimal::from(100_000),
                    status: SimRunStatus::Active,
                    parent_run_id: None,
                    latest_seq: 0,
                    latest_state_hash: "H0".to_string(),
                    created_at: "2026-11-12T12:01:00+00:00".to_string(),
                    updated_at: "2026-11-12T12:01:00+00:00".to_string(),
                    finished_at: None,
                },
                TrainingSessionRecord {
                    run_id: run_id.to_string(),
                    hidden_mode: false,
                    market_category: "us_equity".to_string(),
                    display_symbol: "NASDAQ:AAPL".to_string(),
                    real_instrument_id: aapl.clone(),
                    candidate_pool_hash: "POOL-1".to_string(),
                    redraw_count: 0,
                    redraw_history: Vec::new(),
                    warmup_start_at: bars[0].open_at,
                    history_start_at: bars[0].open_at,
                    train_start_at: bars[1].open_at,
                    train_end_at: bars[2].close_at,
                    reveal_end_at: bars[3].close_at,
                    visible_until: bars[0].close_at,
                    current_step_index: 0,
                    total_steps: 2,
                    revealed: false,
                },
            )
        };

        let (r1, s1) = make_run("RUN-FP-1", &v1.id);
        store.create_training_run(&r1, &s1, None).unwrap();

        // Attempting to mutate or delete referenced `v1` must fail in Rust AND raw SQL!
        let mut mutated_v1 = v1.clone();
        mutated_v1.fixed_fee_per_order = Decimal::from(99);
        assert!(store
            .mutate_fee_profile_version_in_place(&mutated_v1)
            .is_err());
        assert!(store.delete_fee_profile_version(&v1.id).is_err());
        assert!(store
            .conn()
            .execute(
                "UPDATE fee_profile_version SET fixed_fee_per_order = '99' WHERE id = ?1",
                params![v1.id],
            )
            .is_err());
        assert!(store
            .conn()
            .execute(
                "DELETE FROM fee_profile_version WHERE id = ?1",
                params![v1.id],
            )
            .is_err());

        // Creating next version creates `v2` cleanly while keeping `v1` unchanged on `RUN-FP-1`.
        let mut v2_draft = v1.clone();
        v2_draft.name = "My US Desk v2".to_string();
        v2_draft.fixed_fee_per_order = Decimal::from(3);
        let v2 = store.create_next_fee_profile_version(v2_draft).unwrap();
        assert_eq!(v2.id, "FPV:MY_US_DESK:v2");
        assert_eq!(v2.version_num, 2);

        let (r2, s2) = make_run("RUN-FP-2", &v2.id);
        store.create_training_run(&r2, &s2, None).unwrap();

        let r1_fp = store
            .get_fee_profile_version(
                &store
                    .get_simulation_run("RUN-FP-1")
                    .unwrap()
                    .fee_profile_version_id,
            )
            .unwrap();
        let r2_fp = store
            .get_fee_profile_version(
                &store
                    .get_simulation_run("RUN-FP-2")
                    .unwrap()
                    .fee_profile_version_id,
            )
            .unwrap();
        assert_eq!(r1_fp.fixed_fee_per_order, Decimal::ONE);
        assert_eq!(r2_fp.fixed_fee_per_order, Decimal::from(3));
    }

    #[test]
    fn test_m4_a09_and_a10_atomic_step_rollback_checkpoint_recovery_damage_and_retry() {
        let tmp = TestTempDir::new("a09_a10");
        let mut store = SqliteStore::open_or_init(tmp.path(), "USD", "USD", "UTC").unwrap();

        let dsv = store
            .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let aapl = store
            .register_market_instrument("NASDAQ", "AAPL", false)
            .unwrap();
        let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 5, 1).unwrap(), 30);
        let bars: Vec<BarRecord> = days
            .iter()
            .enumerate()
            .map(|(i, &d)| make_equity_bar(&aapl, d, &(200 + i as i64).to_string(), dsv))
            .collect();
        store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();

        let init_cp = SimCheckpointRecord {
            run_id: "RUN-A09".to_string(),
            seq: 0,
            reason: SimCheckpointReason::Initial,
            state_json: r#"{"step":0}"#.to_string(),
            state_hash: "HASH-0".to_string(),
            created_at: "2026-11-12T13:00:00+00:00".to_string(),
        };
        store
            .create_training_run(
                &SimulationRunRecord {
                    run_id: "RUN-A09".to_string(),
                    kind: "training".to_string(),
                    seed: 20261112,
                    dataset_version: dsv,
                    engine_version: "m4-v1".to_string(),
                    fee_profile_version_id: BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string(),
                    quote_currency: "USD".to_string(),
                    initial_cash: Decimal::from(100_000),
                    status: SimRunStatus::Active,
                    parent_run_id: None,
                    latest_seq: 0,
                    latest_state_hash: "HASH-0".to_string(),
                    created_at: "2026-11-12T13:00:00+00:00".to_string(),
                    updated_at: "2026-11-12T13:00:00+00:00".to_string(),
                    finished_at: None,
                },
                &TrainingSessionRecord {
                    run_id: "RUN-A09".to_string(),
                    hidden_mode: true,
                    market_category: "us_equity".to_string(),
                    display_symbol: "标的 A".to_string(),
                    real_instrument_id: aapl.clone(),
                    candidate_pool_hash: "POOL-A09".to_string(),
                    redraw_count: 0,
                    redraw_history: Vec::new(),
                    warmup_start_at: bars[0].open_at,
                    history_start_at: bars[1].open_at,
                    train_start_at: bars[3].open_at,
                    train_end_at: bars[27].close_at,
                    reveal_end_at: bars[29].close_at,
                    visible_until: bars[2].close_at,
                    current_step_index: 0,
                    total_steps: 25,
                    revealed: false,
                },
                Some(&init_cp),
            )
            .unwrap();

        // Advance 21 steps: periodic checkpoint at seq=20, order+fill+note on seq=21.
        for seq in 1..=21i64 {
            let vis = bars[(2 + seq) as usize].close_at;
            let state_hash = format!("HASH-{seq}");
            let checkpoint = if seq == 20 {
                Some(SimCheckpointRecord {
                    run_id: "RUN-A09".to_string(),
                    seq: 20,
                    reason: SimCheckpointReason::Periodic20,
                    state_json: r#"{"step":20}"#.to_string(),
                    state_hash: state_hash.clone(),
                    created_at: "2026-11-12T13:20:00+00:00".to_string(),
                })
            } else {
                None
            };
            let (orders, fills, notes) = if seq == 21 {
                (
                    vec![SimOrderRecord {
                        order_id: "ORD-1".to_string(),
                        run_id: "RUN-A09".to_string(),
                        instrument_id: aapl.clone(),
                        order_seq: 1,
                        order_type: "market".to_string(),
                        side: "buy".to_string(),
                        tif: "day".to_string(),
                        qty: Decimal::from(10),
                        limit_price: None,
                        stop_price: None,
                        parent_group_id: None,
                        group_role: None,
                        status: SimOrderStatus::Filled,
                        reject_code: None,
                        cancel_reason: None,
                        submitted_at: bars[22].close_at,
                        eligible_at: Some(bars[23].open_at),
                        updated_at: vis,
                        payload_json: "{}".to_string(),
                    }],
                    vec![SimFillRecord {
                        fill_id: "FILL-1".to_string(),
                        run_id: "RUN-A09".to_string(),
                        order_id: "ORD-1".to_string(),
                        instrument_id: aapl.clone(),
                        bar_close_at: vis,
                        side: "buy".to_string(),
                        qty: Decimal::from(10),
                        raw_price: Decimal::from(223),
                        fill_price: parse_decimal_lossless("223.05").unwrap(),
                        fee_amount: Decimal::ONE,
                        fee_currency: "USD".to_string(),
                        ambiguous: false,
                        payload_json: "{}".to_string(),
                    }],
                    vec![TrainingNoteRecord {
                        note_id: "TNOTE-1".to_string(),
                        run_id: "RUN-A09".to_string(),
                        bar_index: 21,
                        visible_until: vis,
                        rationale: "突破20日高点买入".to_string(),
                        emotion_tag: Some("专注".to_string()),
                        linked_order_id: Some("ORD-1".to_string()),
                        archived_journal_note_id: None,
                        created_at: "2026-11-12T13:21:00+00:00".to_string(),
                        updated_at: "2026-11-12T13:21:00+00:00".to_string(),
                    }],
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };

            store
                .commit_simulation_step_atomic(&SimulationStepCommit {
                    run_id: "RUN-A09".to_string(),
                    expected_prev_seq: seq - 1,
                    seq,
                    event_kind: "next_bar".to_string(),
                    command_json: format!(r#"{{"seq":{seq}}}"#),
                    events_json: "[]".to_string(),
                    new_status: SimRunStatus::Active,
                    new_visible_until: vis,
                    new_step_index: seq,
                    new_state_hash: state_hash,
                    orders_to_upsert: orders,
                    fills_to_insert: fills,
                    notes_to_upsert: notes,
                    checkpoint,
                    committed_at: vis,
                })
                .unwrap();
        }

        // Inject mid-step crash on seq=22 with order+fill: transaction MUST roll back atomically!
        set_sim_step_fault_injection_for_test(true);
        let crash_res = store.commit_simulation_step_atomic(&SimulationStepCommit {
            run_id: "RUN-A09".to_string(),
            expected_prev_seq: 21,
            seq: 22,
            event_kind: "next_bar".to_string(),
            command_json: r#"{"seq":22}"#.to_string(),
            events_json: "[]".to_string(),
            new_status: SimRunStatus::Paused,
            new_visible_until: bars[24].close_at,
            new_step_index: 22,
            new_state_hash: "HASH-22".to_string(),
            orders_to_upsert: vec![SimOrderRecord {
                order_id: "ORD-CRASH".to_string(),
                run_id: "RUN-A09".to_string(),
                instrument_id: aapl.clone(),
                order_seq: 2,
                order_type: "market".to_string(),
                side: "sell".to_string(),
                tif: "day".to_string(),
                qty: Decimal::from(10),
                limit_price: None,
                stop_price: None,
                parent_group_id: None,
                group_role: None,
                status: SimOrderStatus::Filled,
                reject_code: None,
                cancel_reason: None,
                submitted_at: bars[23].close_at,
                eligible_at: Some(bars[24].open_at),
                updated_at: bars[24].close_at,
                payload_json: "{}".to_string(),
            }],
            fills_to_insert: vec![SimFillRecord {
                fill_id: "FILL-CRASH".to_string(),
                run_id: "RUN-A09".to_string(),
                order_id: "ORD-CRASH".to_string(),
                instrument_id: aapl.clone(),
                bar_close_at: bars[24].close_at,
                side: "sell".to_string(),
                qty: Decimal::from(10),
                raw_price: Decimal::from(224),
                fill_price: Decimal::from(224),
                fee_amount: Decimal::ONE,
                fee_currency: "USD".to_string(),
                ambiguous: false,
                payload_json: "{}".to_string(),
            }],
            notes_to_upsert: Vec::new(),
            checkpoint: None,
            committed_at: bars[24].close_at,
        });
        assert!(crash_res.is_err());

        // Zero half-step artifacts!
        let run_after_crash = store.get_simulation_run("RUN-A09").unwrap();
        let sess_after_crash = store.get_training_session("RUN-A09").unwrap();
        assert_eq!(run_after_crash.latest_seq, 21);
        assert_eq!(run_after_crash.latest_state_hash, "HASH-21");
        assert_eq!(run_after_crash.status, SimRunStatus::Active);
        assert_eq!(sess_after_crash.current_step_index, 21);
        assert_eq!(sess_after_crash.visible_until, bars[23].close_at);
        assert_eq!(store.list_sim_orders("RUN-A09").unwrap().len(), 1);
        assert_eq!(store.list_sim_fills("RUN-A09").unwrap().len(), 1);
        assert_eq!(
            store.list_simulation_events("RUN-A09", None).unwrap().len(),
            21
        );

        // Verify recovery bundle: latest_checkpoint is seq=20 and events_after_checkpoint has seq=21.
        let outcome = store
            .verify_or_mark_run_damaged("RUN-A09", |bundle| {
                assert_eq!(bundle.latest_checkpoint.as_ref().unwrap().seq, 20);
                assert_eq!(bundle.events_after_checkpoint.len(), 1);
                assert_eq!(bundle.events_after_checkpoint[0].seq, 21);
                Ok(vec![(21, "HASH-21".to_string())])
            })
            .unwrap();
        assert!(matches!(outcome, SimulationRecoveryOutcome::Recovered(_)));

        // Verify retry (`M4-A-10`): creates independent child run with same segment/versions/cash, leaving parent untouched!
        let retry_cp = SimCheckpointRecord {
            run_id: "RUN-A09-RETRY".to_string(),
            seq: 0,
            reason: SimCheckpointReason::Initial,
            state_json: r#"{"step":0}"#.to_string(),
            state_hash: "HASH-0".to_string(),
            created_at: "2026-11-12T13:30:00+00:00".to_string(),
        };
        let (child_run, child_sess) = store
            .create_retry_training_run(
                "RUN-A09",
                "RUN-A09-RETRY",
                bars[2].close_at,
                "HASH-0",
                Some(&retry_cp),
                parse_rfc3339_utc("2026-11-12T13:30:00+00:00", "retry_at").unwrap(),
            )
            .unwrap();
        assert_eq!(child_run.parent_run_id.as_deref(), Some("RUN-A09"));
        assert_eq!(child_run.seed, 20261112);
        assert_eq!(child_run.dataset_version, dsv);
        assert_eq!(
            child_run.fee_profile_version_id,
            BUILTIN_FEE_US_EQUITY_PER_SHARE_ID
        );
        assert_eq!(child_run.latest_seq, 0);
        assert_eq!(child_sess.real_instrument_id, aapl);
        assert_eq!(child_sess.visible_until, bars[2].close_at);
        assert_eq!(child_sess.current_step_index, 0);
        assert!(store.list_sim_orders("RUN-A09-RETRY").unwrap().is_empty());
        assert_eq!(store.list_sim_orders("RUN-A09").unwrap().len(), 1);

        // Corrupt `state_hash` in SQLite on `RUN-A09` and verify recovery marks it `'damaged'` (`M4-A-09`).
        store
            .conn()
            .execute(
                "UPDATE simulation_event SET state_hash = 'TAMPERED-HASH' WHERE run_id = 'RUN-A09' AND seq = 21",
                [],
            )
            .unwrap();
        let damaged_outcome = store
            .verify_or_mark_run_damaged("RUN-A09", |_b| Ok(vec![(21, "HASH-21".to_string())]))
            .unwrap();
        match damaged_outcome {
            SimulationRecoveryOutcome::MarkedDamaged { run, reason } => {
                assert_eq!(run.status, SimRunStatus::Damaged);
                assert!(reason.contains("TAMPERED-HASH"));
            }
            other => panic!("expected MarkedDamaged, got {other:?}"),
        }
        assert_eq!(
            store.get_simulation_run("RUN-A09").unwrap().status,
            SimRunStatus::Damaged
        );
        // Damaged runs must reject further step commits!
        assert!(store
            .commit_simulation_step_atomic(&SimulationStepCommit {
                run_id: "RUN-A09".to_string(),
                expected_prev_seq: 21,
                seq: 22,
                event_kind: "next_bar".to_string(),
                command_json: "{}".to_string(),
                events_json: "[]".to_string(),
                new_status: SimRunStatus::Active,
                new_visible_until: bars[24].close_at,
                new_step_index: 22,
                new_state_hash: "HASH-22".to_string(),
                orders_to_upsert: Vec::new(),
                fills_to_insert: Vec::new(),
                notes_to_upsert: Vec::new(),
                checkpoint: None,
                committed_at: bars[24].close_at,
            })
            .is_err());
    }

    #[test]
    fn test_m4_a11_and_a13_schema_5_migration_backup_ledger_isolation_delete_and_note_archive() {
        let tmp = TestTempDir::new("a11_a13");
        let lib_dir = tmp.path().join("lib_v4_to_v5");
        let pre_mig_restore_dir = tmp.path().join("restored_v4_pre_migration");
        let v5_backup_dir = tmp.path().join("backup_v5");
        let v5_restore_dir = tmp.path().join("restored_v5");
        let future_v6_dir = tmp.path().join("lib_v6_unsupported");

        // 1. Build a genuine v0.1.1 Schema 4 library populated with real ledger, M2 notes, and AI session rows.
        {
            let mut store = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC").unwrap();
            store.downgrade_to_schema_4_for_test().unwrap();

            store
                .upsert_account(
                    "ACC-REAL",
                    "Real Brokerage",
                    "broker",
                    "ibkr",
                    None,
                    "active",
                )
                .unwrap();
            store
                .conn()
                .execute("UPDATE library SET ledger_revision = 7", [])
                .unwrap();
            store
                .conn()
                .execute(
                    "INSERT INTO economic_event (
                        id, account_id, type, occurred_at, sequence_no, recorded_at,
                        asset_id, instrument_id, quantity, price, quote_currency,
                        fee_amount, fee_currency, cost_known, correction_group_id,
                        revision, active_in_correction_group
                     ) VALUES (
                        'EV-REAL-1', 'ACC-REAL', 'buy', '2025-01-10T15:00:00Z', 1, '2025-01-10T15:00:00Z',
                        'ASSET:EQUITY:XNAS:AAPL', 'INST:EQUITY:XNAS:AAPL', '25', '185.50', 'USD',
                        '1.25', 'USD', 1, 'CG-REAL-1', 1, 1
                     )",
                    [],
                )
                .unwrap();
            store
                .conn()
                .execute(
                    "INSERT INTO holding_projection (
                        account_id, asset_id, quantity, carrying_cost, cost_known, ledger_revision
                     ) VALUES ('ACC-REAL', 'ASSET:EQUITY:XNAS:AAPL', '25', '4638.75', 1, 7)",
                    [],
                )
                .unwrap();

            store
                .save_note_revision(
                    "NOTE-REAL-1",
                    "实盘复盘笔记",
                    "post_trade",
                    true,
                    "这是v0.1.1真实交易笔记。",
                    &StructuredNoteFields::default(),
                    &["实盘".to_string()],
                )
                .unwrap();

            store
                .upsert_model_connection(&ModelConnectionRecord {
                    id: "MC-LOCAL".to_string(),
                    protocol: "openai_compatible".to_string(),
                    base_url: "http://127.0.0.1:11434/v1".to_string(),
                    model_id: "qwen2.5".to_string(),
                    credential_ref: None,
                    context_window: 32768,
                    capabilities_json: "{}".to_string(),
                    timeouts_json: "{}".to_string(),
                    budget_json: "{}".to_string(),
                    updated_at: "2025-01-10T16:00:00Z".to_string(),
                })
                .unwrap();
            let ai_sess = store.create_ai_session("AISESS-V4").unwrap();
            assert_eq!(ai_sess.schema_version, 1);
            store
                .create_ai_run(&AiRunRecord {
                    id: "AIRUN-V4".to_string(),
                    session_id: "AISESS-V4".to_string(),
                    generation: 1,
                    scope_json: "{}".to_string(),
                    dataset_version: 0,
                    ledger_revision: 7,
                    tool_schema_hash: "TSH-1".to_string(),
                    model_connection_id: "MC-LOCAL".to_string(),
                    budget_json: "{}".to_string(),
                    state: "completed".to_string(),
                    checkpoint_ref: None,
                    report_json: None,
                    usage_json: None,
                    created_at: "2025-01-10T16:01:00Z".to_string(),
                    updated_at: "2025-01-10T16:01:00Z".to_string(),
                })
                .unwrap();
            store
                .append_ai_message(&AiMessageRecord {
                    id: "AIMSG-V4".to_string(),
                    session_id: "AISESS-V4".to_string(),
                    run_id: "AIRUN-V4".to_string(),
                    turn_id: 1,
                    sequence: 1,
                    kind: "assistant".to_string(),
                    payload_json: r#"{"text":"v4 ai summary"}"#.to_string(),
                    status: "completed".to_string(),
                    created_at: "2025-01-10T16:01:05Z".to_string(),
                })
                .unwrap();
            store.sync_manifest().unwrap();
        }

        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            4
        );
        assert!(SqliteStore::list_schema_5_tables_on_disk(&lib_dir)
            .unwrap()
            .is_empty());

        // 2. Fault injection during 4 -> 5 migration rolls back DDL cleanly AFTER creating pre-migration backup!
        set_schema_5_migration_fault_injection_for_test(true);
        let mig_err = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
            .err()
            .expect("injected schema 5 migration fault must fail");
        assert!(matches!(mig_err, StorageError::MigrationFailed(_)));
        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            4
        );
        assert!(SqliteStore::list_schema_5_tables_on_disk(&lib_dir)
            .unwrap()
            .is_empty());

        let pre_backups = SqliteStore::list_pre_migration_backups(&lib_dir, 4).unwrap();
        assert_eq!(pre_backups.len(), 1);
        let restored_v4_ver = SqliteStore::restore_consistent_backup_without_migration(
            &pre_backups[0],
            &pre_mig_restore_dir,
        )
        .unwrap();
        assert_eq!(restored_v4_ver, 4);

        // 3. Upgrade cleanly to Schema 5 and verify preservation of real ledger, M2 notes, and AI sessions.
        let mut store = SqliteStore::open_or_init(&lib_dir, "USD", "USD", "UTC")
            .expect("upgrade Schema 4 -> Schema 5");
        assert_eq!(
            SqliteStore::verify_schema_version_on_disk(&lib_dir).unwrap(),
            5
        );
        assert_eq!(
            SqliteStore::list_schema_5_tables_on_disk(&lib_dir)
                .unwrap()
                .len(),
            SCHEMA_5_TABLES.len()
        );
        assert_eq!(store.query_ledger_revision().unwrap(), 7);
        assert_eq!(store.get_note("NOTE-REAL-1").unwrap().title, "实盘复盘笔记");
        assert_eq!(store.list_ai_messages("AISESS-V4").unwrap().len(), 1);

        // Verify v0.1.1 Schema-4 compatibility check rejects Schema 5 clearly, and `schema_version > 5` is rejected!
        let v4_reader_err = SqliteStore::check_schema_compatibility_at_version(&lib_dir, 4)
            .expect_err("Schema 4 reader must refuse Schema 5 library");
        assert!(v4_reader_err
            .to_string()
            .contains("database schema is newer than this build"));

        {
            let mut s6 = SqliteStore::open_or_init(&future_v6_dir, "USD", "USD", "UTC").unwrap();
            s6.conn_mut()
                .execute("UPDATE library SET schema_version = 6", [])
                .unwrap();
        }
        let v6_err = SqliteStore::open_or_init(&future_v6_dir, "USD", "USD", "UTC")
            .err()
            .expect("opening schema 6 must fail clearly");
        assert!(v6_err
            .to_string()
            .contains("database schema is newer than this build"));

        // 4. Populate training runs (`RUN-PARENT`, `RUN-CHILD`, `RUN-OTHER`), orders, fills, notes, and archive a note (`M4-A-13`),
        //    verifying `ledger_revision` and real ledger tables remain 100% untouched!
        let dsv = store
            .create_dataset_version("yahoo", "personal", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let aapl = store
            .register_market_instrument("NASDAQ", "AAPL", false)
            .unwrap();
        let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2025, 6, 2).unwrap(), 6);
        let bars: Vec<BarRecord> = days
            .iter()
            .map(|&d| make_equity_bar(&aapl, d, "195", dsv))
            .collect();
        store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();

        for rid in ["RUN-PARENT", "RUN-OTHER"] {
            let cp0 = SimCheckpointRecord {
                run_id: rid.to_string(),
                seq: 0,
                reason: SimCheckpointReason::Initial,
                state_json: "{}".to_string(),
                state_hash: "H0".to_string(),
                created_at: "2026-11-12T14:00:00+00:00".to_string(),
            };
            store
                .create_training_run(
                    &SimulationRunRecord {
                        run_id: rid.to_string(),
                        kind: "training".to_string(),
                        seed: 99,
                        dataset_version: dsv,
                        engine_version: "m4-v1".to_string(),
                        fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
                        quote_currency: "USD".to_string(),
                        initial_cash: Decimal::from(100_000),
                        status: SimRunStatus::Active,
                        parent_run_id: None,
                        latest_seq: 0,
                        latest_state_hash: "H0".to_string(),
                        created_at: "2026-11-12T14:00:00+00:00".to_string(),
                        updated_at: "2026-11-12T14:00:00+00:00".to_string(),
                        finished_at: None,
                    },
                    &TrainingSessionRecord {
                        run_id: rid.to_string(),
                        hidden_mode: true,
                        market_category: "us_equity".to_string(),
                        display_symbol: "标的 A".to_string(),
                        real_instrument_id: aapl.clone(),
                        candidate_pool_hash: "POOL-A11".to_string(),
                        redraw_count: 0,
                        redraw_history: Vec::new(),
                        warmup_start_at: bars[0].open_at,
                        history_start_at: bars[0].open_at,
                        train_start_at: bars[1].open_at,
                        train_end_at: bars[3].close_at,
                        reveal_end_at: bars[5].close_at,
                        visible_until: bars[0].close_at,
                        current_step_index: 0,
                        total_steps: 3,
                        revealed: false,
                    },
                    Some(&cp0),
                )
                .unwrap();

            store
                .commit_simulation_step_atomic(&SimulationStepCommit {
                    run_id: rid.to_string(),
                    expected_prev_seq: 0,
                    seq: 1,
                    event_kind: "finish".to_string(),
                    command_json: "{}".to_string(),
                    events_json: "[]".to_string(),
                    new_status: SimRunStatus::Finished,
                    new_visible_until: bars[3].close_at,
                    new_step_index: 3,
                    new_state_hash: "H1".to_string(),
                    orders_to_upsert: vec![SimOrderRecord {
                        order_id: "ORD-1".to_string(),
                        run_id: rid.to_string(),
                        instrument_id: aapl.clone(),
                        order_seq: 1,
                        order_type: "market".to_string(),
                        side: "buy".to_string(),
                        tif: "day".to_string(),
                        qty: Decimal::from(5),
                        limit_price: None,
                        stop_price: None,
                        parent_group_id: None,
                        group_role: None,
                        status: SimOrderStatus::Filled,
                        reject_code: None,
                        cancel_reason: None,
                        submitted_at: bars[1].close_at,
                        eligible_at: Some(bars[2].open_at),
                        updated_at: bars[2].close_at,
                        payload_json: "{}".to_string(),
                    }],
                    fills_to_insert: vec![SimFillRecord {
                        fill_id: "FILL-1".to_string(),
                        run_id: rid.to_string(),
                        order_id: "ORD-1".to_string(),
                        instrument_id: aapl.clone(),
                        bar_close_at: bars[2].close_at,
                        side: "buy".to_string(),
                        qty: Decimal::from(5),
                        raw_price: Decimal::from(195),
                        fill_price: Decimal::from(195),
                        fee_amount: Decimal::ZERO,
                        fee_currency: "USD".to_string(),
                        ambiguous: false,
                        payload_json: "{}".to_string(),
                    }],
                    notes_to_upsert: vec![TrainingNoteRecord {
                        note_id: format!("TN-{rid}"),
                        run_id: rid.to_string(),
                        bar_index: 2,
                        visible_until: bars[2].close_at,
                        rationale: "缩量回踩支撑做多".to_string(),
                        emotion_tag: Some("冷静".to_string()),
                        linked_order_id: Some("ORD-1".to_string()),
                        archived_journal_note_id: None,
                        created_at: "2026-11-12T14:02:00+00:00".to_string(),
                        updated_at: "2026-11-12T14:02:00+00:00".to_string(),
                    }],
                    checkpoint: Some(SimCheckpointRecord {
                        run_id: rid.to_string(),
                        seq: 1,
                        reason: SimCheckpointReason::Finish,
                        state_json: r#"{"done":true}"#.to_string(),
                        state_hash: "H1".to_string(),
                        created_at: "2026-11-12T14:05:00+00:00".to_string(),
                    }),
                    committed_at: bars[3].close_at,
                })
                .unwrap();
        }

        store
            .create_retry_training_run(
                "RUN-PARENT",
                "RUN-CHILD",
                bars[0].close_at,
                "H0",
                None,
                parse_rfc3339_utc("2026-11-12T14:10:00+00:00", "retry_at").unwrap(),
            )
            .unwrap();

        // Archive `TN-RUN-PARENT` into M2 journal (`M4-A-13`).
        let archived_note = store
            .archive_training_note_to_journal(
                "RUN-PARENT",
                "TN-RUN-PARENT",
                "NOTE-ARCHIVED-SIM-1",
                "模拟训练归档复盘",
            )
            .unwrap();
        assert!(archived_note.body_markdown.contains("来自模拟训练"));
        assert!(archived_note.tags.iter().any(|t| t == "来自模拟训练"));

        let tn_after_archive = store.get_training_note("TN-RUN-PARENT").unwrap();
        assert_eq!(tn_after_archive.rationale, "缩量回踩支撑做多");
        assert_eq!(tn_after_archive.emotion_tag.as_deref(), Some("冷静"));
        assert_eq!(tn_after_archive.linked_order_id.as_deref(), Some("ORD-1"));
        assert_eq!(
            tn_after_archive.archived_journal_note_id.as_deref(),
            Some("NOTE-ARCHIVED-SIM-1")
        );

        // Verify `ledger_revision` and real ledger event counts were NEVER touched!
        assert_eq!(store.query_ledger_revision().unwrap(), 7);
        let real_ev_count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM economic_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(real_ev_count, 1);

        // 5. Verify export snapshot + backup/restore row counts across all 8 Schema 5 tables (`M4-A-11`).
        let export_snap = store.export_simulation_tables_snapshot().unwrap();
        assert_eq!(export_snap.fee_profile_versions.len(), 3);
        assert_eq!(export_snap.simulation_runs.len(), 3);
        assert_eq!(export_snap.training_sessions.len(), 3);
        assert_eq!(export_snap.sim_orders.len(), 2);
        assert_eq!(export_snap.sim_fills.len(), 2);
        assert_eq!(export_snap.simulation_events.len(), 2);
        assert_eq!(export_snap.sim_checkpoints.len(), 4);
        assert_eq!(export_snap.training_notes.len(), 2);

        let backup_manifest = store.create_consistent_backup(&v5_backup_dir).unwrap();
        for tbl in SCHEMA_5_TABLES {
            assert!(
                backup_manifest
                    .table_row_counts
                    .get(*tbl)
                    .copied()
                    .unwrap_or(0)
                    > 0,
                "backup manifest must record non-zero row count for {tbl}"
            );
        }
        let restored_v5 =
            SqliteStore::restore_consistent_backup(&v5_backup_dir, &v5_restore_dir).unwrap();
        assert_eq!(
            restored_v5.export_simulation_tables_snapshot().unwrap(),
            export_snap
        );
        assert_eq!(restored_v5.query_ledger_revision().unwrap(), 7);
        drop(restored_v5);

        // 6. Preview & delete `RUN-PARENT` in one transaction without affecting `RUN-CHILD`, `RUN-OTHER`,
        //    archived journal note, or real ledger (`M4-A-11`).
        let preview = store.preview_delete_training_run("RUN-PARENT").unwrap();
        assert_eq!(preview.child_run_ids, vec!["RUN-CHILD".to_string()]);
        assert_eq!(preview.order_count, 1);
        assert_eq!(preview.fill_count, 1);
        assert_eq!(preview.note_count, 1);
        assert_eq!(preview.archived_journal_note_count, 1);
        assert_eq!(preview.checkpoint_count, 2);
        assert_eq!(preview.event_count, 1);

        let deleted = store.delete_training_run("RUN-PARENT").unwrap();
        assert_eq!(deleted, preview);
        assert!(store.get_simulation_run("RUN-PARENT").is_err());
        assert!(store.list_sim_orders("RUN-PARENT").unwrap().is_empty());
        assert!(store.list_sim_fills("RUN-PARENT").unwrap().is_empty());

        // Child run, other run, archived journal note, real note, and real ledger remain completely intact!
        let surviving_child = store.get_simulation_run("RUN-CHILD").unwrap();
        assert_eq!(surviving_child.parent_run_id.as_deref(), Some("RUN-PARENT"));
        assert_eq!(store.list_sim_fills("RUN-OTHER").unwrap().len(), 1);
        assert_eq!(
            store.get_note("NOTE-ARCHIVED-SIM-1").unwrap().title,
            "模拟训练归档复盘"
        );
        assert_eq!(store.get_note("NOTE-REAL-1").unwrap().title, "实盘复盘笔记");
        assert_eq!(store.query_ledger_revision().unwrap(), 7);
    }
}
