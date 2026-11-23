use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use delta_core::{BarRecord, CorporateActionRecord, CorporateActionType};
use rusqlite::{params, OptionalExtension};

use crate::m4_schema::models::{
    dedupe_bars_by_session_date, source_priority, PostFinishRevealData, TrainingBarPortQuery,
    TrainingSessionRecord,
};
use crate::storage::{map_bar_row, map_corporate_action_row, SqliteStore, StorageError};

impl SqliteStore {
    pub(crate) fn ensure_known_dataset_version(
        &self,
        dataset_version: i64,
    ) -> Result<(), StorageError> {
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
    // SQL-Level Truncated Training Data Port Queries (`S-37`, `M4-K01`, `M4-A-01`, `M4-A-02`)
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
               -- Index-friendly superset bounds (date-padded, valid for any offset format);
               -- the exact strftime predicates above decide membership.
               AND (?5 IS NULL OR open_at >= date(?5, '-2 days'))
               AND open_at <= date(?6, '+3 days')
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
            timeframe: session.timeframe.clone(),
            source: None,
            dataset_version: run.dataset_version,
            warmup_start_at: lower,
            history_start_at: Some(session.history_start_at),
            visible_until: session.visible_until,
            previous_visible_until: None,
        })
    }

    /// Intraday (`S-50`) next step bar: the earliest finalized bar of the run's timeframe whose
    /// close lies in `(visible_until, train_end_at]`. Only metadata is scanned; full rows are
    /// materialized for that single `open_at` (same sentinel invariant as the daily path),
    /// with RFC3339-normalized timestamp matching and post-dedupe bounds verification.
    fn query_next_intraday_training_step_bar(
        &self,
        session: &TrainingSessionRecord,
        dataset_version: i64,
        vis_str: &str,
        train_start_str: &str,
        train_end_str: &str,
    ) -> Result<Option<BarRecord>, StorageError> {
        let mut after_open_opt: Option<String> = None;

        loop {
            let next_open: Option<String> = self
                .conn()
                .query_row(
                    "SELECT open_at FROM bar
                     WHERE instrument_id = ?1 AND timeframe = ?2 AND adjustment = 'raw' AND is_final = 1
                       AND valid_from_version <= ?3 AND (valid_to_version IS NULL OR ?3 < valid_to_version)
                       AND (?7 IS NULL OR strftime('%Y-%m-%dT%H:%M:%fZ', open_at) > strftime('%Y-%m-%dT%H:%M:%fZ', ?7))
                       AND strftime('%Y-%m-%dT%H:%M:%fZ', open_at) >= strftime('%Y-%m-%dT%H:%M:%fZ', ?4)
                       AND strftime('%Y-%m-%dT%H:%M:%fZ', close_at) > strftime('%Y-%m-%dT%H:%M:%fZ', ?5)
                       AND strftime('%Y-%m-%dT%H:%M:%fZ', close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?6)
                       AND open_at >= date(?4, '-2 days')
                       AND open_at >= date(?5, '-2 days')
                       AND open_at <= date(?6, '+3 days')
                     ORDER BY strftime('%Y-%m-%dT%H:%M:%fZ', open_at) ASC LIMIT 1",
                    params![
                        session.real_instrument_id,
                        session.timeframe,
                        dataset_version,
                        train_start_str,
                        vis_str,
                        train_end_str,
                        after_open_opt.as_deref(),
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(open_at) = next_open else {
                return Ok(None);
            };
            let mut stmt = self.conn().prepare_cached(
                "SELECT id, instrument_id, timeframe, session, session_date, open_at, close_at,
                        open, high, low, close, volume, source, adjustment, is_final,
                        valid_from_version, valid_to_version, raw_payload_hash
                 FROM bar
                 WHERE instrument_id = ?1 AND timeframe = ?2 AND adjustment = 'raw' AND is_final = 1
                   AND valid_from_version <= ?3 AND (valid_to_version IS NULL OR ?3 < valid_to_version)
                   AND strftime('%Y-%m-%dT%H:%M:%fZ', open_at) = strftime('%Y-%m-%dT%H:%M:%fZ', ?4)
                   AND open_at >= date(?4, '-2 days')
                   AND open_at <= date(?4, '+2 days')
                 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map(
                params![
                    session.real_instrument_id,
                    session.timeframe,
                    dataset_version,
                    open_at,
                ],
                |r| Ok(map_bar_row(r)),
            )?;
            let mut raw = Vec::new();
            for row in rows {
                raw.push(row??);
            }
            let Some(bar) = dedupe_bars_by_session_date(raw).pop() else {
                after_open_opt = Some(open_at);
                continue;
            };
            if bar.close_at > session.train_end_at {
                return Ok(None);
            }
            if bar.open_at < session.train_start_at || bar.close_at <= session.visible_until {
                after_open_opt = Some(open_at);
                continue;
            }
            return Ok(Some(bar));
        }
    }

    /// Resolve the immediate next deduplicated finalized bar strictly inside
    /// `(session.visible_until, session.train_end_at]` with `open_at >= session.train_start_at`
    /// (`S-37`, `M4-K01`, `M4-K03`).
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

        if session.timeframe != "1d" {
            return self.query_next_intraday_training_step_bar(
                &session,
                run.dataset_version,
                &vis_str,
                &train_start_str,
                &train_end_str,
            );
        }

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

    /// Post-finish reveal query (`M4-A-01`, `M4-A-14`, `S-52`):
    /// Strictly succeeds ONLY when `simulation_run.status IN ('finished', 'abandoned')` in SQLite
    /// (enforced in both Rust and SQLite `WHERE` predicates), using the session's frozen `timeframe`.
    pub fn query_post_finish_reveal(
        &self,
        run_id: &str,
    ) -> Result<PostFinishRevealData, StorageError> {
        let run = self.get_simulation_run(run_id)?;
        if !run.status.is_ended() {
            return Err(StorageError::IntegrityFailed(format!(
                "post-finish reveal refused: simulation_run '{run_id}' status is '{}' (must be finished or abandoned)",
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
                 WHERE sr.run_id = ?1 AND sr.status IN ('finished', 'abandoned')
             )
               AND b.instrument_id = ?2
               AND b.timeframe = ?6
               AND b.adjustment = 'raw'
               AND b.is_final = 1
               AND b.valid_from_version <= ?3
               AND (b.valid_to_version IS NULL OR ?3 < b.valid_to_version)
               AND strftime('%Y-%m-%dT%H:%M:%fZ', b.open_at) >= strftime('%Y-%m-%dT%H:%M:%fZ', ?4)
               AND strftime('%Y-%m-%dT%H:%M:%fZ', b.close_at) <= strftime('%Y-%m-%dT%H:%M:%fZ', ?5)
               AND b.open_at >= date(?4, '-2 days')
               AND b.open_at <= date(?5, '+3 days')
             ORDER BY b.open_at ASC, b.id ASC",
        )?;
        let rows = stmt.query_map(
            params![
                run_id,
                session.real_instrument_id,
                run.dataset_version,
                session.warmup_start_at.to_rfc3339(),
                session.reveal_end_at.to_rfc3339(),
                session.timeframe,
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
        if !run.status.is_ended() {
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
}
