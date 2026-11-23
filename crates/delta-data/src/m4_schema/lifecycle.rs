use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use delta_core::{parse_decimal_lossless, to_canonical_text};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;

use crate::m2_schema::notes::{add_note_link_in, save_note_revision_in};
use crate::m2_schema::{NoteDto, StructuredNoteFields};
use crate::m4_schema::models::{
    map_sim_checkpoint_row, map_sim_fill_row, map_sim_order_row, map_simulation_event_row,
    map_simulation_run_row, map_training_note_row, map_training_session_row,
    DeleteTrainingRunPreview, RedrawHistoryEntry, SimCheckpointRecord, SimFillRecord,
    SimOrderRecord, SimRunStatus, SimulationEventRecord, SimulationExportBundle,
    SimulationRecoveryBundle, SimulationRecoveryOutcome, SimulationRunRecord, SimulationStepCommit,
    TrainingCandidateSegment, TrainingNoteRecord, TrainingSessionRecord,
};
use crate::m4_schema::{
    swap_schema_5_migration_fault_for_test, FAIL_NEXT_SIM_STEP_COMMIT, SCHEMA_5_TABLES,
};
use crate::m5_schema::{PracticeAccountEntryRecord, PracticeEntryKind, TrainingDrawingRecord};
use crate::storage::{parse_rfc3339_utc, SqliteStore, StorageError};

impl SqliteStore {
    /// Downgrade an existing initialized library on disk to genuine v0.1.1 Schema 4
    /// (dropping all Schema 5 tables/triggers, setting `schema_version = 4`, clearing
    /// prior `before-schema-*` backups, and syncing `manifest.json`).
    pub fn downgrade_to_schema_4_for_test(&mut self) -> Result<(), StorageError> {
        self.downgrade_to_schema_5_for_test()?;
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

    /// Check library `schema_version` compatibility against an explicit `max_supported_schema`.
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

    // ------------------------------------------------------------------------
    // Simulation Run & Training Session Lifecycle (`S-38`, `S-43`)
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
        if session.revealed && !run.status.is_ended() {
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
                visible_until, current_step_index, total_steps, revealed,
                timeframe, practice_account_id, fill_rule_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                session.run_id,
                i32::from(session.hidden_mode),
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
                i32::from(session.revealed),
                session.timeframe,
                session.practice_account_id,
                session.fill_rule_version,
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT run_id, kind, seed, dataset_version, engine_version,
                    fee_profile_version_id, quote_currency, initial_cash, status,
                    parent_run_id, latest_seq, latest_state_hash,
                    created_at, updated_at, finished_at
             FROM simulation_run
             WHERE run_id = ?1",
        )?;
        let row = stmt
            .query_row(params![run_id], |r| Ok(map_simulation_run_row(r)))
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "simulation_run '{run_id}' not found"
            ))),
        }
    }

    pub fn list_simulation_runs(&self) -> Result<Vec<SimulationRunRecord>, StorageError> {
        let mut stmt = self.conn().prepare_cached(
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

    pub fn get_training_session_optional(
        &self,
        run_id: &str,
    ) -> Result<Option<TrainingSessionRecord>, StorageError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT run_id, hidden_mode, market_category, display_symbol, real_instrument_id,
                    candidate_pool_hash, redraw_count, redraw_history_json,
                    warmup_start_at, history_start_at, train_start_at, train_end_at, reveal_end_at,
                    visible_until, current_step_index, total_steps, revealed,
                    timeframe, practice_account_id, fill_rule_version
             FROM training_session
             WHERE run_id = ?1",
        )?;
        stmt.query_row(params![run_id], |r| Ok(map_training_session_row(r)))
            .optional()?
            .transpose()
    }

    pub fn get_training_session(
        &self,
        run_id: &str,
    ) -> Result<TrainingSessionRecord, StorageError> {
        self.get_training_session_optional(run_id)?.ok_or_else(|| {
            StorageError::IntegrityFailed(format!("training_session '{run_id}' not found"))
        })
    }

    /// Explicitly redraw segment BEFORE start (`status == 'preparing'`, `S-38`, `M4-A-02`).
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

    /// Atomically redraw a `'preparing'` training session, updating `training_session`,
    /// `simulation_run`, and optional initial `sim_checkpoint` (`seq = 0`) in one savepoint.
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
            timeframe: orig_session.timeframe,
            practice_account_id: None,
            fill_rule_version: orig_session.fill_rule_version,
        };

        self.create_training_run(&new_run, &new_session, initial_checkpoint)
    }

    // ------------------------------------------------------------------------
    // Atomic Step Commit, Orders/Fills/Notes, Recovery & Delete (`S-43`, `M4-A-09`..`M4-A-13`)
    // ------------------------------------------------------------------------

    /// Atomically commit a single simulation command step inside one SQLite savepoint (`M4-A-09`).
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
        if matches!(
            cur_status,
            SimRunStatus::Finished | SimRunStatus::Damaged | SimRunStatus::Abandoned
        ) {
            return Err(StorageError::IntegrityFailed(format!(
                "cannot commit step to simulation_run '{}' in terminal status '{}'",
                step.run_id,
                cur_status.as_str()
            )));
        }
        if step.new_status == SimRunStatus::Preparing && cur_status != SimRunStatus::Preparing {
            return Err(StorageError::IntegrityFailed(format!(
                "cannot transition simulation_run '{}' from '{}' back to 'preparing'",
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

        {
            let mut order_stmt = tx.prepare_cached(
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
                    tif = excluded.tif,
                    qty = excluded.qty,
                    limit_price = excluded.limit_price,
                    stop_price = excluded.stop_price,
                    status = excluded.status,
                    reject_code = excluded.reject_code,
                    cancel_reason = excluded.cancel_reason,
                    eligible_at = excluded.eligible_at,
                    updated_at = excluded.updated_at,
                    payload_json = excluded.payload_json",
            )?;
            for ord in &step.orders_to_upsert {
                if ord.run_id != step.run_id {
                    return Err(StorageError::IntegrityFailed(
                        "sim_order.run_id does not match step.run_id".into(),
                    ));
                }
                order_stmt.execute(params![
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
                ])?;
            }

            let mut fill_stmt = tx.prepare_cached(
                "INSERT INTO sim_fill (
                    fill_id, run_id, order_id, instrument_id, bar_close_at,
                    side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for fill in &step.fills_to_insert {
                if fill.run_id != step.run_id {
                    return Err(StorageError::IntegrityFailed(
                        "sim_fill.run_id does not match step.run_id".into(),
                    ));
                }
                fill_stmt.execute(params![
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
                    i32::from(fill.ambiguous),
                    fill.payload_json,
                ])?;
            }
        }

        for note in &step.notes_to_upsert {
            if note.run_id != step.run_id {
                return Err(StorageError::IntegrityFailed(
                    "training_note.run_id does not match step.run_id".into(),
                ));
            }
            upsert_training_note_on_conn(&tx, note)?;
        }

        let finished_at_opt = if step.new_status.is_ended() {
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
        let mut stmt = self.conn().prepare_cached(
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT order_id, run_id, instrument_id, order_seq, order_type, side, tif,
                    qty, limit_price, stop_price, parent_group_id, group_role,
                    status, reject_code, cancel_reason, submitted_at, eligible_at, updated_at, payload_json
             FROM sim_order
             WHERE run_id = ?1 AND order_id = ?2",
        )?;
        let row = stmt
            .query_row(params![run_id, order_id], |r| Ok(map_sim_order_row(r)))
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "sim_order '{order_id}' not found in run '{run_id}'"
            ))),
        }
    }

    pub fn list_sim_fills(&self, run_id: &str) -> Result<Vec<SimFillRecord>, StorageError> {
        let mut stmt = self.conn().prepare_cached(
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT fill_id, run_id, order_id, instrument_id, bar_close_at,
                    side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
             FROM sim_fill
             WHERE run_id = ?1 AND fill_id = ?2",
        )?;
        let row = stmt
            .query_row(params![run_id, fill_id], |r| Ok(map_sim_fill_row(r)))
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT run_id, seq, event_kind, command_json, events_json,
                    visible_until, state_hash, created_at
             FROM simulation_event
             WHERE run_id = ?1 AND seq > ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![run_id, min_seq], |r| {
            Ok(map_simulation_event_row(r))
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT run_id, seq, reason, state_json, state_hash, created_at
             FROM sim_checkpoint
             WHERE run_id = ?1
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| Ok(map_sim_checkpoint_row(r)))?;
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT run_id, seq, reason, state_json, state_hash, created_at
             FROM sim_checkpoint
             WHERE run_id = ?1
             ORDER BY seq DESC
             LIMIT 1",
        )?;
        stmt.query_row(params![run_id], |r| Ok(map_sim_checkpoint_row(r)))
            .optional()?
            .transpose()
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT note_id, run_id, bar_index, visible_until, rationale, emotion_tag,
                    linked_order_id, archived_journal_note_id, created_at, updated_at
             FROM training_note
             WHERE note_id = ?1",
        )?;
        let row = stmt
            .query_row(params![note_id], |r| Ok(map_training_note_row(r)))
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
        let mut stmt = self.conn().prepare_cached(
            "SELECT note_id, run_id, bar_index, visible_until, rationale, emotion_tag,
                    linked_order_id, archived_journal_note_id, created_at, updated_at
             FROM training_note
             WHERE run_id = ?1
             ORDER BY bar_index ASC, created_at ASC, note_id ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| Ok(map_training_note_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// Link an already-created journal `note.id` onto `training_note.archived_journal_note_id`
    /// without altering the original training note's fields (`S-45`, `M4-A-13`).
    pub fn link_archived_journal_note_id(
        &mut self,
        run_id: &str,
        training_note_id: &str,
        journal_note_id: &str,
    ) -> Result<TrainingNoteRecord, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if !run.status.is_ended() {
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

    /// Archive a finished training note into the normal M2 journal (`note` / `note_revision`)
    /// inside a single atomic savepoint, explicitly labelled `"来自模拟训练"` (`simulated_training`),
    /// preserving original `training_note` fields unchanged (`M4-A-13`).
    pub fn archive_training_note_to_journal(
        &mut self,
        run_id: &str,
        training_note_id: &str,
        journal_note_id: &str,
        title: &str,
    ) -> Result<NoteDto, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if !run.status.is_ended() {
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
        let tx = self.conn_mut().savepoint()?;
        save_note_revision_in(
            &tx,
            journal_note_id,
            title,
            "post_trade",
            true,
            &body,
            &structured,
            &tags,
        )?;
        add_note_link_in(
            &tx,
            journal_note_id,
            "training_note",
            training_note_id,
            1,
            None,
            "archived_from_training",
        )?;
        let updated = tx.execute(
            "UPDATE training_note SET archived_journal_note_id = ?2 WHERE note_id = ?1 AND run_id = ?3",
            params![training_note_id, journal_note_id, run_id],
        )?;
        if updated != 1 {
            return Err(StorageError::IntegrityFailed(format!(
                "training_note '{training_note_id}' not found in run '{run_id}'"
            )));
        }
        tx.commit()?;
        self.get_note(journal_note_id)
    }

    // ------------------------------------------------------------------------
    // Recovery Bundle, Hash Verification & Damage Marking (`S-43`, `M4-A-09`)
    // ------------------------------------------------------------------------

    pub fn load_simulation_recovery_bundle(
        &self,
        run_id: &str,
    ) -> Result<SimulationRecoveryBundle, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        let session = self.get_training_session_optional(run_id)?;
        let fee_profile = self.get_fee_profile_version(&run.fee_profile_version_id)?;
        let latest_checkpoint = self.latest_sim_checkpoint(run_id)?;
        let after_seq = latest_checkpoint.as_ref().map(|cp| cp.seq).unwrap_or(0);
        let all_events = self.list_simulation_events(run_id, None)?;
        let events_after_checkpoint = all_events
            .iter()
            .filter(|e| e.seq > after_seq)
            .cloned()
            .collect();

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

        // 3. Check `latest_checkpoint` hash matches corresponding `simulation_event` (or `latest_state_hash` at seq 0).
        if let Some(ref cp) = bundle.latest_checkpoint {
            if cp.seq > bundle.run.latest_seq {
                let reason = format!(
                    "checkpoint seq ({}) exceeds run latest_seq ({})",
                    cp.seq, bundle.run.latest_seq
                );
                return mark_damaged(self, reason);
            }
            if cp.seq == 0 && bundle.run.latest_seq == 0 {
                if cp.state_hash != bundle.run.latest_state_hash {
                    let reason = format!(
                        "initial checkpoint hash ({}) != simulation_run.latest_state_hash ({})",
                        cp.state_hash, bundle.run.latest_state_hash
                    );
                    return mark_damaged(self, reason);
                }
            } else if cp.seq > 0 {
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
    // Delete Preview, Atomic Single-Run Delete & Open Export Bundle (`M4-A-11`)
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
        crate::m5_schema::append_delete_reversal_in(&tx, run_id, chrono::Utc::now())?;
        tx.execute(
            "DELETE FROM training_drawing WHERE run_id = ?1",
            params![run_id],
        )?;
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

    /// Snapshot all Schema 5 + Schema 6 simulation tables in deterministic order using bulk queries (`S-43`, `M4-A-11`).
    pub fn export_simulation_tables_snapshot(
        &self,
    ) -> Result<SimulationExportBundle, StorageError> {
        let fee_profile_versions = self.list_fee_profile_versions()?;
        let mut simulation_runs = self.list_simulation_runs()?;
        simulation_runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));

        let mut training_sessions = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT run_id, hidden_mode, market_category, display_symbol, real_instrument_id,
                        candidate_pool_hash, redraw_count, redraw_history_json,
                        warmup_start_at, history_start_at, train_start_at, train_end_at, reveal_end_at,
                        visible_until, current_step_index, total_steps, revealed,
                        timeframe, practice_account_id, fill_rule_version
                 FROM training_session
                 ORDER BY run_id ASC",
            )?;
            for row in stmt.query_map([], |r| Ok(map_training_session_row(r)))? {
                training_sessions.push(row??);
            }
        }

        let mut sim_orders = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT order_id, run_id, instrument_id, order_seq, order_type, side, tif,
                        qty, limit_price, stop_price, parent_group_id, group_role,
                        status, reject_code, cancel_reason, submitted_at, eligible_at, updated_at, payload_json
                 FROM sim_order
                 ORDER BY run_id ASC, order_seq ASC, order_id ASC",
            )?;
            for row in stmt.query_map([], |r| Ok(map_sim_order_row(r)))? {
                sim_orders.push(row??);
            }
        }

        let mut sim_fills = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT fill_id, run_id, order_id, instrument_id, bar_close_at,
                        side, qty, raw_price, fill_price, fee_amount, fee_currency, ambiguous, payload_json
                 FROM sim_fill
                 ORDER BY run_id ASC, bar_close_at ASC, fill_id ASC",
            )?;
            for row in stmt.query_map([], |r| Ok(map_sim_fill_row(r)))? {
                sim_fills.push(row??);
            }
        }

        let mut simulation_events = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT run_id, seq, event_kind, command_json, events_json,
                        visible_until, state_hash, created_at
                 FROM simulation_event
                 ORDER BY run_id ASC, seq ASC",
            )?;
            for row in stmt.query_map([], |r| Ok(map_simulation_event_row(r)))? {
                simulation_events.push(row??);
            }
        }

        let mut sim_checkpoints = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT run_id, seq, reason, state_json, state_hash, created_at
                 FROM sim_checkpoint
                 ORDER BY run_id ASC, seq ASC",
            )?;
            for row in stmt.query_map([], |r| Ok(map_sim_checkpoint_row(r)))? {
                sim_checkpoints.push(row??);
            }
        }

        let mut training_notes = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT note_id, run_id, bar_index, visible_until, rationale, emotion_tag,
                        linked_order_id, archived_journal_note_id, created_at, updated_at
                 FROM training_note
                 ORDER BY run_id ASC, bar_index ASC, created_at ASC, note_id ASC",
            )?;
            for row in stmt.query_map([], |r| Ok(map_training_note_row(r)))? {
                training_notes.push(row??);
            }
        }

        let mut training_drawings = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT drawing_id, run_id, kind, anchors_json, color, line_style, deleted, created_at, updated_at
                 FROM training_drawing
                 ORDER BY run_id ASC, created_at ASC, drawing_id ASC",
            )?;
            for row in stmt.query_map([], |r| {
                Ok(TrainingDrawingRecord {
                    drawing_id: r.get(0)?,
                    run_id: r.get(1)?,
                    kind: r.get(2)?,
                    anchors_json: r.get(3)?,
                    color: r.get(4)?,
                    line_style: r.get(5)?,
                    deleted: r.get::<_, i64>(6)? != 0,
                    created_at: r.get(7)?,
                    updated_at: r.get(8)?,
                })
            })? {
                training_drawings.push(row?);
            }
        }

        let practice_accounts = self.list_practice_accounts()?;
        let mut practice_account_entries = Vec::new();
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT e.account_id, e.seq, e.kind, e.amount, e.run_id, e.note, e.created_at
                 FROM practice_account_entry e
                 JOIN practice_account a ON a.id = e.account_id
                 ORDER BY a.quote_currency ASC, e.seq ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })?;
            for row in rows {
                let (account_id, seq, kind, amount, run_id, note, created_at) = row?;
                practice_account_entries.push(PracticeAccountEntryRecord {
                    account_id,
                    seq,
                    kind: PracticeEntryKind::parse(&kind)?,
                    amount: parse_decimal_lossless(&amount)
                        .map_err(|e| StorageError::IntegrityFailed(e.to_string()))?,
                    run_id,
                    note,
                    created_at,
                });
            }
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
            practice_accounts,
            practice_account_entries,
            training_drawings,
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

    let mut child_stmt = conn.prepare_cached(
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
        drawing_count: count_for("training_drawing")?,
        practice_entry_count: count_for("practice_account_entry")?,
        practice_reversal: crate::m5_schema::practice_reversal_for_run(conn, run_id)?,
    })
}
