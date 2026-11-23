#![allow(clippy::too_many_arguments)]
use std::cell::Cell;

use rusqlite::Connection;

use crate::storage::StorageError;

mod candidate_pool;
mod data_port;
mod fee_profiles;
mod lifecycle;
mod models;

#[cfg(test)]
#[path = "m4_schema/tests.rs"]
mod tests;

pub(crate) use models::detect_split_jump_ratio;
pub use models::{
    dedupe_bars_by_session_date, CandidatePoolQuery, CandidatePoolResult, CandidateQualityFilter,
    DeleteTrainingRunPreview, FeeChargingTier, FeeCurrencyMode, FeeProfileVersionRecord,
    FeeRoundingMode, PostFinishRevealData, RedrawHistoryEntry, SimCheckpointReason,
    SimCheckpointRecord, SimFillRecord, SimOrderRecord, SimOrderStatus, SimRunStatus,
    SimulationEventRecord, SimulationExportBundle, SimulationRecoveryBundle,
    SimulationRecoveryOutcome, SimulationRunRecord, SimulationStepCommit, SlippageMode,
    TrainingBarPortQuery, TrainingCandidateSegment, TrainingNoteRecord, TrainingSessionRecord,
};

thread_local! {
    static FAIL_NEXT_SCHEMA_5_MIGRATION: Cell<bool> = const { Cell::new(false) };
    pub(crate) static FAIL_NEXT_SIM_STEP_COMMIT: Cell<bool> = const { Cell::new(false) };
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
        "#,
    )?;

    recreate_schema_5_session_guard(tx)?;
    fee_profiles::seed_builtin_fee_profiles(tx)?;

    if from_schema < 5 && FAIL_NEXT_SCHEMA_5_MIGRATION.with(|flag| flag.replace(false)) {
        return Err(StorageError::MigrationFailed(
            "injected mid-migration fault before schema 5 commit".into(),
        ));
    }

    Ok(())
}

/// The M4 session guard; schema 6 replaces it with a version that also covers `abandoned`.
pub(crate) fn recreate_schema_5_session_guard(tx: &Connection) -> Result<(), StorageError> {
    let has_v6: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('training_session') WHERE name = 'timeframe')",
        [],
        |r| r.get(0),
    )?;
    if has_v6 {
        return Ok(());
    }
    tx.execute_batch(
        r#"
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
    Ok(())
}
