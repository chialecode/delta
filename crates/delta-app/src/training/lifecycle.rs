//! Training run lifecycle, execution, snapshot, report, replay, retry, and delete (W01..W04).

use std::collections::BTreeMap;

use chrono::Utc;
use delta_core::{
    compute_training_report, derive_point_in_time_split_adjusted_bars, preview_order_submission,
    sample_candidate_sequence, step as sim_step, to_canonical_text, BarRecord,
    CorporateActionRecord, CorporateActionType, OrderSide, OrderSubmitRequest, SimCommand,
    SimError, SimEvent, SimOrder, SimState, SimStepBar, TrainingNoteRef, TrainingReportInput,
    FILL_RULE_VERSION_M5, SIM_ENGINE_VERSION, SIM_ENGINE_VERSION_M5,
};
use delta_data::{
    CandidatePoolQuery, CandidateQualityFilter, DeleteTrainingRunPreview, SimCheckpointReason,
    SimCheckpointRecord, SimFillRecord, SimRunStatus, SimulationRunRecord, SimulationStepCommit,
    TrainingSessionRecord,
};
use rusqlite::{params, OptionalExtension};
use rust_decimal::Decimal;

use super::dto::*;
use super::scrub::*;
use crate::kline::{ChartMarkerDto, KlineViewport, MarkerKind};
use crate::service::{AppService, ErrorEnvelope};

impl AppService {
    pub(crate) fn build_training_run_summary_dto(
        &self,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        self.build_training_run_summary_with_context(run, session, None, None, None)
    }

    pub(crate) fn build_training_run_summary_with_context(
        &self,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
        preloaded_bars: Option<&[BarRecord]>,
        preloaded_inst: Option<(&str, &str)>,
        preloaded_fee_name: Option<&str>,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let masked = is_run_identity_masked(run, session);
        let fee_profile_name = match preloaded_fee_name {
            Some(name) => name.to_string(),
            None => self
                .store()
                .get_fee_profile_version(&run.fee_profile_version_id)
                .map(|fp| fp.name)
                .unwrap_or_else(|_| run.fee_profile_version_id.clone()),
        };
        let (real_venue, real_symbol) = match preloaded_inst {
            Some((v, s)) => (v.to_string(), s.to_string()),
            None => {
                let (_, v, s, _) = self
                    .load_sim_instrument_info(&session.real_instrument_id, &run.quote_currency)?;
                (v, s)
            }
        };
        let owned_bars;
        let vis_bars: &[BarRecord] = match preloaded_bars {
            Some(bars) => bars,
            None => {
                owned_bars = self
                    .store()
                    .query_run_visible_final_bars(&run.run_id, false)
                    .unwrap_or_default();
                &owned_bars
            }
        };
        let visible_bar_count = vis_bars.len().max(1);
        let current_bar_label = bar_label_for_one_based(visible_bar_count);

        let redraw_history = session
            .redraw_history
            .iter()
            .map(|entry| ScrubbedRedrawEntryDto {
                redraw_index: entry.redraw_index,
                previous_instrument_id: (!masked).then(|| entry.previous_instrument_id.clone()),
                previous_train_start_at: (!masked).then(|| entry.previous_train_start_at.clone()),
                next_instrument_id: (!masked).then(|| entry.next_instrument_id.clone()),
                next_train_start_at: (!masked).then(|| entry.next_train_start_at.clone()),
                redrawn_at: entry.redrawn_at.clone(),
            })
            .collect();

        Ok(TrainingRunSummaryDto {
            run_id: run.run_id.clone(),
            parent_run_id: run.parent_run_id.clone(),
            status: run.status,
            hidden_mode: session.hidden_mode,
            revealed: session.revealed,
            is_identity_masked: masked,
            simulated_training_badge: SIMULATED_TRAINING_BADGE.to_string(),
            market_category: session.market_category.clone(),
            display_symbol: if masked {
                session.display_symbol.clone()
            } else {
                real_symbol.clone()
            },
            instrument_id: if masked {
                HIDDEN_INSTRUMENT_ID.to_string()
            } else {
                session.real_instrument_id.clone()
            },
            venue: if masked {
                HIDDEN_VENUE_LABEL.to_string()
            } else {
                real_venue
            },
            symbol: if masked {
                session.display_symbol.clone()
            } else {
                real_symbol
            },
            seed: run.seed,
            candidate_pool_hash: session.candidate_pool_hash.clone(),
            redraw_count: session.redraw_count,
            redraw_history,
            dataset_version: run.dataset_version,
            engine_version: run.engine_version.clone(),
            fee_profile_version_id: run.fee_profile_version_id.clone(),
            fee_profile_name,
            quote_currency: run.quote_currency.clone(),
            initial_cash: run.initial_cash,
            current_step_index: session.current_step_index,
            total_steps: session.total_steps,
            visible_bar_count: vis_bars.len(),
            current_bar_label,
            visible_until: (!masked).then_some(session.visible_until),
            history_start_date: (!masked).then(|| {
                vis_bars.first().map(|b| b.session_date).unwrap_or_else(|| {
                    delta_data::SqliteStore::effective_visible_ex_date(
                        &session.real_instrument_id,
                        session.history_start_at,
                    )
                })
            }),
            train_start_date: (!masked).then(|| {
                delta_data::SqliteStore::effective_visible_ex_date(
                    &session.real_instrument_id,
                    session.train_start_at,
                )
            }),
            train_end_date: (!masked).then(|| {
                delta_data::SqliteStore::effective_visible_ex_date(
                    &session.real_instrument_id,
                    session.train_end_at,
                )
            }),
            latest_seq: run.latest_seq,
            latest_state_hash: run.latest_state_hash.clone(),
            created_at: run.created_at.clone(),
            updated_at: run.updated_at.clone(),
            finished_at: run.finished_at.clone(),
            timeframe: session.timeframe.clone(),
            practice_account_id: session.practice_account_id.clone(),
            fill_rule_version: session.fill_rule_version.clone(),
            operation_counts: None,
        })
    }

    // ------------------------------------------------------------------------
    // 5.1 Fee Profile Version Management (`S-41`, `M4-A-08`)
    // ------------------------------------------------------------------------

    pub fn list_fee_profile_versions(&self) -> Result<Vec<FeeProfileDto>, ErrorEnvelope> {
        let records = self
            .store()
            .list_fee_profile_versions()
            .map_err(|e| map_storage_err("FEE_PROFILE_LIST_FAILED", e))?;
        let mut out = Vec::with_capacity(records.len());
        for rec in records {
            let engine_profile = record_to_core_fee_profile(&rec)?;
            out.push(FeeProfileDto {
                record: rec,
                engine_profile,
            });
        }
        Ok(out)
    }

    pub fn get_fee_profile_version(&self, id: &str) -> Result<FeeProfileDto, ErrorEnvelope> {
        let rec = self
            .store()
            .get_fee_profile_version(id)
            .map_err(|e| map_storage_err("FEE_PROFILE_NOT_FOUND", e))?;
        let engine_profile = record_to_core_fee_profile(&rec)?;
        Ok(FeeProfileDto {
            record: rec,
            engine_profile,
        })
    }

    /// Create a new fee profile or append a new immutable version (`version_num = max + 1`)
    /// when editing an existing `profile_key` (`S-41`, `M4-A-08`). Existing runs remain bound
    /// to their frozen `fee_profile_version_id`.
    pub fn save_fee_profile_version(
        &mut self,
        draft: FeeProfileDraftDto,
    ) -> Result<FeeProfileDto, ErrorEnvelope> {
        let rec_draft = draft_to_fee_profile_record(&draft)?;
        let saved = self
            .store_mut()
            .create_next_fee_profile_version(rec_draft)
            .map_err(|e| map_storage_err("FEE_PROFILE_SAVE_FAILED", e))?;
        let engine_profile = record_to_core_fee_profile(&saved)?;
        Ok(FeeProfileDto {
            record: saved,
            engine_profile,
        })
    }

    pub fn delete_unused_fee_profile_version(&mut self, id: &str) -> Result<(), ErrorEnvelope> {
        self.store_mut()
            .delete_fee_profile_version(id)
            .map_err(|e| map_storage_err("FEE_PROFILE_DELETE_REFUSED", e))
    }

    // ------------------------------------------------------------------------
    // 5.2 Training Run Creation, Redraw, Start, Pause & Resume (`S-38`, `S-43`)
    // ------------------------------------------------------------------------

    fn save_pool_query_metadata(
        &self,
        run_id: &str,
        dataset_version: i64,
        query: &CandidatePoolQuery,
    ) -> Result<(), ErrorEnvelope> {
        let payload = serde_json::to_string(query)
            .map_err(|e| ErrorEnvelope::new("POOL_META_SERIALIZE", e.to_string(), false))?;
        self.store()
            .conn()
            .execute(
                "INSERT OR REPLACE INTO analysis_report (
                    id, report_type, scope_json, ledger_revision, dataset_version,
                    calculation_version, payload_json, status, created_at
                 ) VALUES (?1, 'training_pool_query', '{}', 0, ?2, ?3, ?4, 'internal', ?5)",
                params![
                    format!("REP:TRN-POOL:{run_id}"),
                    dataset_version,
                    SIM_ENGINE_VERSION,
                    payload,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|e| ErrorEnvelope::new("POOL_META_SAVE_FAILED", e.to_string(), false))?;
        Ok(())
    }

    pub(crate) fn load_pool_query_metadata(
        &self,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
    ) -> Result<CandidatePoolQuery, ErrorEnvelope> {
        let saved: Option<String> = self
            .store()
            .conn()
            .query_row(
                "SELECT payload_json FROM analysis_report WHERE id = ?1",
                params![format!("REP:TRN-POOL:{}", run.run_id)],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| ErrorEnvelope::new("POOL_META_LOAD_FAILED", e.to_string(), false))?;
        if let Some(json_str) = saved {
            if let Ok(q) = serde_json::from_str::<CandidatePoolQuery>(&json_str) {
                return Ok(q);
            }
        }

        let vis_bars = self
            .store()
            .query_run_visible_final_bars(&run.run_id, false)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        Ok(CandidatePoolQuery {
            dataset_version: run.dataset_version,
            market_category: session.market_category.clone(),
            warmup_bars: 200,
            history_len: vis_bars.len().max(1),
            train_len: usize::try_from(session.total_steps.max(1)).unwrap_or(250),
            reveal_len: 60,
            instrument_id: None,
            start_date: None,
            end_date: None,
            timeframe: session.timeframe.clone(),
            quality_filter: Default::default(),
        })
    }

    pub fn create_training_run(
        &mut self,
        req: CreateTrainingRunRequest,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        if req.timeframe.is_intraday() && req.market_category == TrainingMarketCategory::UsEquityEtf
        {
            return Err(ErrorEnvelope::new(
                "TIMEFRAME_NOT_SUPPORTED",
                "美股/ETF 本阶段只有日线；4 小时与 1 小时级别仅支持加密现货",
                false,
            ));
        }
        if let (true, Some(id)) = (req.timeframe.is_intraday(), req.instrument_id.as_deref()) {
            if !(id.starts_with("BINANCE:") || id.starts_with("OKX:")) {
                return Err(ErrorEnvelope::new(
                    "TIMEFRAME_NOT_SUPPORTED",
                    "美股/ETF 本阶段只有日线；4 小时与 1 小时级别仅支持加密现货",
                    false,
                ));
            }
        }
        if req.use_practice_account {
            if let Some(a) = req.allocation {
                if a <= Decimal::ZERO {
                    return Err(ErrorEnvelope::new(
                        "INVALID_ALLOCATION",
                        "划拨金额必须大于 0",
                        false,
                    ));
                }
            }
        } else if req.initial_cash <= Decimal::ZERO {
            return Err(ErrorEnvelope::new(
                "INVALID_INITIAL_CASH",
                "initial_cash must be strictly positive",
                false,
            ));
        }
        if req.history_bars == 0 || req.training_bars == 0 {
            return Err(ErrorEnvelope::new(
                "INVALID_WINDOW_LENGTH",
                "history_bars and training_bars must both be >= 1",
                false,
            ));
        }
        let dataset_version = self.resolve_training_dataset_version(req.dataset_version)?;
        let fee_profile = self
            .store()
            .get_fee_profile_version(&req.fee_profile_version_id)
            .map_err(|e| map_storage_err("FEE_PROFILE_NOT_FOUND", e))?;
        let _ = record_to_core_fee_profile(&fee_profile)?;

        let pool_query = CandidatePoolQuery {
            dataset_version,
            market_category: req.market_category.as_pool_str().to_string(),
            warmup_bars: req.warmup_bars,
            history_len: req.history_bars,
            train_len: req.training_bars,
            reveal_len: req.reveal_bars,
            instrument_id: req.instrument_id.clone(),
            start_date: req.start_date,
            end_date: req.end_date,
            timeframe: req.timeframe.as_str().to_string(),
            // M5 runs exclude only price-affecting quality kinds (`S-49` ③).
            quality_filter: CandidateQualityFilter::PriceAffectingOnly,
        };
        let pool = self
            .store()
            .query_training_candidate_pool(&pool_query)
            .map_err(|e| map_storage_err("CANDIDATE_POOL_FAILED", e))?;
        if pool.candidates.is_empty() {
            return Err(ErrorEnvelope::new(
                "EMPTY_CANDIDATE_POOL",
                "当前本地数据集中没有满足连续历史、无缺口及完整公司行动要求的训练候选区间",
                false,
            ));
        }

        let seed = match req.seed {
            Some(s) => s,
            None => generate_random_seed_u64()?,
        };
        let sample_key = format!("{seed}:{}", pool.candidate_pool_hash);
        let sampled_indices = sample_candidate_sequence(&sample_key, pool.candidates.len(), 0)
            .map_err(map_sim_err)?;
        let chosen = &pool.candidates[sampled_indices[0]];

        let display_symbol = if req.hidden_mode {
            hidden_display_symbol_for_category(&chosen.market_category)
        } else {
            chosen.symbol.clone()
        };

        // `S-52`: fund from the practice account of the run's quote currency.
        let practice = if req.use_practice_account {
            let account = self.ensure_practice_account(&chosen.quote_currency)?;
            if let Some(busy) = self.open_run_on_account(&account.id)? {
                return Err(ErrorEnvelope::new(
                    "PRACTICE_ACCOUNT_BUSY",
                    format!(
                        "练习账户 {} 还有未结束的一局（{busy}），请先结束或放弃当前这局",
                        account.quote_currency
                    ),
                    false,
                ));
            }
            let available = self
                .store()
                .practice_account_balance(&account.id)
                .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?;
            let amount = req.allocation.unwrap_or(available);
            if amount <= Decimal::ZERO || amount > available {
                return Err(ErrorEnvelope::new(
                    "INSUFFICIENT_PRACTICE_FUNDS",
                    format!(
                        "划拨金额 {} 超出练习账户可用 {}",
                        to_canonical_text(amount),
                        to_canonical_text(available)
                    ),
                    false,
                ));
            }
            Some((account.id, amount))
        } else {
            None
        };
        let initial_cash = practice
            .as_ref()
            .map(|(_, amount)| *amount)
            .unwrap_or(req.initial_cash);

        let init_state = self.build_initial_sim_state_for(
            SIM_ENGINE_VERSION_M5,
            req.timeframe.as_str(),
            &chosen.quote_currency,
            initial_cash,
            &fee_profile,
            &chosen.instrument_id,
            dataset_version,
            chosen.history_start_at,
            chosen.initial_visible_until,
        )?;
        let state_json = init_state.canonical_json();
        let state_hash = init_state.state_hash();

        let run_id = req
            .run_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| format!("TRN-{}", uuid::Uuid::new_v4()));
        let now_str = Utc::now().to_rfc3339();

        let run_rec = SimulationRunRecord {
            run_id: run_id.clone(),
            kind: "training".to_string(),
            seed,
            dataset_version,
            engine_version: SIM_ENGINE_VERSION_M5.to_string(),
            fee_profile_version_id: fee_profile.id.clone(),
            quote_currency: chosen.quote_currency.to_ascii_uppercase(),
            initial_cash,
            status: SimRunStatus::Preparing,
            parent_run_id: None,
            latest_seq: 0,
            latest_state_hash: state_hash.clone(),
            created_at: now_str.clone(),
            updated_at: now_str.clone(),
            finished_at: None,
        };
        let session_rec = TrainingSessionRecord {
            run_id: run_id.clone(),
            hidden_mode: req.hidden_mode,
            market_category: chosen.market_category.clone(),
            display_symbol,
            real_instrument_id: chosen.instrument_id.clone(),
            candidate_pool_hash: pool.candidate_pool_hash.clone(),
            redraw_count: 0,
            redraw_history: Vec::new(),
            warmup_start_at: chosen.warmup_start_at,
            history_start_at: chosen.history_start_at,
            train_start_at: chosen.train_start_at,
            train_end_at: chosen.train_end_at,
            reveal_end_at: chosen.reveal_end_at,
            visible_until: chosen.initial_visible_until,
            current_step_index: 0,
            total_steps: i64::try_from(chosen.train_len).unwrap_or(250),
            revealed: false,
            timeframe: req.timeframe.as_str().to_string(),
            practice_account_id: None,
            fill_rule_version: Some(FILL_RULE_VERSION_M5.to_string()),
        };
        let init_cp = SimCheckpointRecord {
            run_id: run_id.clone(),
            seq: 0,
            reason: SimCheckpointReason::Initial,
            state_json,
            state_hash,
            created_at: now_str,
        };

        // Run creation, allocation entry and pool metadata commit together.
        self.with_savepoint("create_training_run", "CREATE_TRAINING_RUN_FAILED", |svc| {
            svc.store_mut()
                .create_training_run(&run_rec, &session_rec, Some(&init_cp))
                .map_err(|e| map_storage_err("CREATE_TRAINING_RUN_FAILED", e))?;
            if let Some((account_id, amount)) = &practice {
                svc.store_mut()
                    .allocate_practice_run(&run_id, account_id, *amount, Utc::now())
                    .map_err(|e| {
                        if e.to_string().contains("PRACTICE_ACCOUNT_BUSY") {
                            ErrorEnvelope::new("PRACTICE_ACCOUNT_BUSY", e.to_string(), false)
                        } else {
                            map_storage_err("PRACTICE_ALLOCATION_FAILED", e)
                        }
                    })?;
            }
            svc.save_pool_query_metadata(&run_id, dataset_version, &pool_query)
        })?;

        if req.create_and_start {
            return self.start_training_run(&run_id);
        }
        self.get_training_run_summary(&run_id)
    }

    pub fn redraw_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run = self
            .store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        if run.status != SimRunStatus::Preparing {
            return Err(ErrorEnvelope::new(
                "REDRAW_FORBIDDEN_AFTER_START",
                format!(
                    "训练开始后不可换标的/区间（当前状态：{}）",
                    run.status.as_str()
                ),
                false,
            ));
        }
        let session = self
            .store()
            .get_training_session(run_id)
            .map_err(|e| map_storage_err("SESSION_NOT_FOUND", e))?;
        let fee_profile = self
            .store()
            .get_fee_profile_version(&run.fee_profile_version_id)
            .map_err(|e| map_storage_err("FEE_PROFILE_NOT_FOUND", e))?;

        let pool_query = self.load_pool_query_metadata(&run, &session)?;
        let pool = self
            .store()
            .query_training_candidate_pool(&pool_query)
            .map_err(|e| map_storage_err("CANDIDATE_POOL_FAILED", e))?;
        if pool.candidates.is_empty() {
            return Err(ErrorEnvelope::new(
                "EMPTY_CANDIDATE_POOL",
                "候选池为空，无法重新抽样",
                false,
            ));
        }
        if pool.candidate_pool_hash != session.candidate_pool_hash {
            return Err(ErrorEnvelope::new(
                "CANDIDATE_POOL_HASH_MISMATCH",
                "冻结候选池哈希与创建时不一致，拒绝非确定性重抽样",
                false,
            ));
        }

        let next_redraw_u32 = u32::try_from(session.redraw_count + 1)
            .map_err(|_| ErrorEnvelope::new("REDRAW_LIMIT", "redraw_count overflow", false))?;
        let sample_key = format!("{}:{}", run.seed, session.candidate_pool_hash);
        let sampled =
            sample_candidate_sequence(&sample_key, pool.candidates.len(), next_redraw_u32)
                .map_err(map_sim_err)?;
        let chosen = &pool.candidates[*sampled.last().expect("non-empty sample sequence")];

        if session.practice_account_id.is_some()
            && !chosen
                .quote_currency
                .eq_ignore_ascii_case(&run.quote_currency)
        {
            return Err(ErrorEnvelope::new(
                "PRACTICE_CURRENCY_MISMATCH",
                "重新抽到的标的报价币种与本局练习账户不同；请放弃本局后按目标币种新开一局",
                false,
            ));
        }
        let next_display_symbol = if session.hidden_mode {
            hidden_display_symbol_for_category(&chosen.market_category)
        } else {
            chosen.symbol.clone()
        };

        let init_state = self.build_initial_sim_state_for(
            &run.engine_version,
            &session.timeframe,
            &chosen.quote_currency,
            run.initial_cash,
            &fee_profile,
            &chosen.instrument_id,
            run.dataset_version,
            chosen.history_start_at,
            chosen.initial_visible_until,
        )?;
        let state_json = init_state.canonical_json();
        let state_hash = init_state.state_hash();
        let now = Utc::now();

        self.store_mut()
            .redraw_training_session_with_checkpoint(
                run_id,
                chosen,
                &next_display_symbol,
                Some(&state_json),
                &state_hash,
                now,
            )
            .map_err(|e| map_storage_err("REDRAW_FAILED", e))?;
        evict_training_slice_cache(run_id);

        self.get_training_run_summary(run_id)
    }

    pub fn start_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let (_, _) = self.verify_and_recover_sim_state(run_id)?;
        self.store_mut()
            .start_training_run(run_id, Utc::now())
            .map_err(|e| map_storage_err("START_TRAINING_FAILED", e))?;
        self.get_training_run_summary(run_id)
    }

    pub fn pause_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        if !matches!(
            bundle.run.status,
            SimRunStatus::Active | SimRunStatus::Paused
        ) {
            return Err(ErrorEnvelope::new(
                "INVALID_RUN_STATUS",
                format!(
                    "only an active training run can be paused (current status: {})",
                    bundle.run.status.as_str()
                ),
                false,
            ));
        }
        let now_str = Utc::now().to_rfc3339();
        let state_json = state.canonical_json();
        let state_hash = state.state_hash();
        let tx = self
            .store_mut()
            .conn_mut()
            .savepoint()
            .map_err(|e| ErrorEnvelope::new("PAUSE_FAILED", e.to_string(), false))?;
        tx.execute(
            "INSERT OR REPLACE INTO sim_checkpoint (run_id, seq, reason, state_json, state_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                bundle.run.latest_seq,
                SimCheckpointReason::Pause.as_str(),
                state_json,
                state_hash,
                now_str,
            ],
        )
        .map_err(|e| ErrorEnvelope::new("PAUSE_CHECKPOINT_FAILED", e.to_string(), false))?;
        tx.execute(
            "UPDATE simulation_run SET status = 'paused', updated_at = ?2 WHERE run_id = ?1",
            params![run_id, now_str],
        )
        .map_err(|e| ErrorEnvelope::new("PAUSE_STATUS_FAILED", e.to_string(), false))?;
        tx.commit()
            .map_err(|e| ErrorEnvelope::new("PAUSE_COMMIT_FAILED", e.to_string(), false))?;
        self.get_training_run_summary(run_id)
    }

    pub fn resume_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let (bundle, _) = self.verify_and_recover_sim_state(run_id)?;
        match bundle.run.status {
            SimRunStatus::Paused | SimRunStatus::Active => {
                let now_str = Utc::now().to_rfc3339();
                self.store()
                    .conn()
                    .execute(
                        "UPDATE simulation_run SET status = 'active', updated_at = ?2 WHERE run_id = ?1",
                        params![run_id, now_str],
                    )
                    .map_err(|e| ErrorEnvelope::new("RESUME_FAILED", e.to_string(), false))?;
                self.get_training_run_summary(run_id)
            }
            SimRunStatus::Preparing => self.start_training_run(run_id),
            other => Err(ErrorEnvelope::new(
                "INVALID_RUN_STATUS",
                format!(
                    "cannot resume training run '{}' from status '{}'",
                    run_id,
                    other.as_str()
                ),
                false,
            )),
        }
    }

    pub fn list_training_runs(&self) -> Result<Vec<TrainingRunSummaryDto>, ErrorEnvelope> {
        let runs = self
            .store()
            .list_simulation_runs()
            .map_err(|e| map_storage_err("LIST_RUNS_FAILED", e))?;
        let mut out = Vec::new();
        for run in runs {
            if run.kind != "training" {
                continue;
            }
            if let Ok(session) = self.store().get_training_session(&run.run_id) {
                out.push(self.build_training_run_summary_dto(&run, &session)?);
            }
        }
        Ok(out)
    }

    pub fn get_training_run_summary(
        &self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run = self
            .store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        let session = self
            .store()
            .get_training_session(run_id)
            .map_err(|e| map_storage_err("SESSION_NOT_FOUND", e))?;
        self.build_training_run_summary_dto(&run, &session)
    }

    // ------------------------------------------------------------------------
    // 5.3 Workspace Snapshot, Order Preview/Submit/Cancel, Advance, Finish & Reveal
    // ------------------------------------------------------------------------

    pub fn show_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingWorkspaceSnapshotDto, ErrorEnvelope> {
        self.get_training_workspace_snapshot(run_id, None, 320)
    }

    pub fn get_training_workspace_snapshot(
        &self,
        run_id: &str,
        viewport_override: Option<KlineViewport>,
        max_pixel_columns: usize,
    ) -> Result<TrainingWorkspaceSnapshotDto, ErrorEnvelope> {
        let (bundle, state) = self.read_verified_sim_state(run_id)?;
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;
        let masked = is_run_identity_masked(&bundle.run, session);
        let (spec, real_venue, real_symbol, is_crypto) =
            self.load_sim_instrument_info(&session.real_instrument_id, &bundle.run.quote_currency)?;

        let raw_bars_with_warmup = self
            .store()
            .query_run_visible_final_bars(run_id, true)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        let visible_cas = self
            .store()
            .query_run_visible_corporate_actions(run_id)
            .map_err(|e| map_storage_err("TRAINING_CA_PORT_FAILED", e))?;

        let warmup_count = raw_bars_with_warmup
            .iter()
            .take_while(|b| b.open_at < session.history_start_at)
            .count();
        let display_raw_bars = &raw_bars_with_warmup[warmup_count..];
        let total_display_bars = display_raw_bars.len();

        let mut close_to_idx = BTreeMap::new();
        for (idx, b) in display_raw_bars.iter().enumerate() {
            close_to_idx.insert(b.close_at, idx);
        }
        let history_bar_count = display_raw_bars
            .iter()
            .filter(|b| b.open_at < session.train_start_at)
            .count();

        let raw_notes = self
            .store()
            .list_training_notes(run_id)
            .map_err(|e| map_storage_err("NOTES_LOAD_FAILED", e))?;

        // Build markers mapped onto truncated visible display indices
        let mut raw_markers = Vec::new();
        for fill in &state.fills {
            if let Some(&b_idx) = close_to_idx.get(&fill.bar_close_at) {
                let marker_date = if masked {
                    synthetic_date_for_index(b_idx)
                } else {
                    fill.bar_session_date
                };
                raw_markers.push(ChartMarkerDto {
                    id: format!("MK:SIMFILL:{}", fill.fill_id),
                    kind: match fill.side {
                        OrderSide::Buy => MarkerKind::BuyFill,
                        OrderSide::Sell => MarkerKind::SellFill,
                    },
                    session_date: marker_date,
                    bar_index: b_idx,
                    price: fill.fill_price,
                    label: format!(
                        "{} {}",
                        fill.side.as_str(),
                        to_canonical_text(fill.gross_quantity)
                    ),
                    target_id: fill.fill_id.clone(),
                    target_revision: 1,
                });
            }
        }
        for note in &raw_notes {
            let b_idx = close_to_idx
                .range(..=note.visible_until)
                .next_back()
                .map(|(_, &idx)| idx)
                .unwrap_or_else(|| total_display_bars.saturating_sub(1));
            let price = display_raw_bars
                .get(b_idx)
                .map(|b| b.close)
                .unwrap_or(Decimal::ZERO);
            let marker_date = if masked {
                synthetic_date_for_index(b_idx)
            } else {
                display_raw_bars
                    .get(b_idx)
                    .map(|b| b.session_date)
                    .unwrap_or_else(|| synthetic_date_for_index(b_idx))
            };
            raw_markers.push(ChartMarkerDto {
                id: format!("MK:TRNNOTE:{}", note.note_id),
                kind: MarkerKind::Note,
                session_date: marker_date,
                bar_index: b_idx,
                price,
                label: note
                    .emotion_tag
                    .clone()
                    .unwrap_or_else(|| "训练笔记".to_string()),
                target_id: note.note_id.clone(),
                target_revision: 1,
            });
        }
        for ord in state.orders.values() {
            if ord.status.is_open_or_pending() && ord.has_trade_plan {
                if let Some(p) = ord
                    .order_type
                    .limit_price()
                    .or_else(|| ord.order_type.stop_price())
                {
                    let b_idx = total_display_bars.saturating_sub(1);
                    let marker_date = if masked {
                        synthetic_date_for_index(b_idx)
                    } else {
                        display_raw_bars
                            .get(b_idx)
                            .map(|b| b.session_date)
                            .unwrap_or_else(|| synthetic_date_for_index(b_idx))
                    };
                    raw_markers.push(ChartMarkerDto {
                        id: format!("MK:SIMPLAN:{}", ord.order_id),
                        kind: MarkerKind::Plan,
                        session_date: marker_date,
                        bar_index: b_idx,
                        price: p,
                        label: format!("{} {}", ord.group_role.as_str(), to_canonical_text(p)),
                        target_id: ord.order_id.clone(),
                        target_revision: 1,
                    });
                }
            }
        }

        let viewport = match viewport_override {
            Some(mut vp) => {
                vp.total_bars = total_display_bars;
                if vp.visible_end == 0 || vp.visible_end > total_display_bars {
                    let win = vp.visible_len().max(120).min(total_display_bars.max(1));
                    vp.visible_end = total_display_bars;
                    vp.visible_start = total_display_bars.saturating_sub(win);
                } else if vp.visible_start > vp.visible_end {
                    vp.visible_start = 0;
                }
                vp
            }
            None => KlineViewport::new(total_display_bars, 160),
        };

        let out_inst_id = if masked {
            HIDDEN_INSTRUMENT_ID
        } else {
            session.real_instrument_id.as_str()
        };
        let out_venue = if masked {
            HIDDEN_VENUE_LABEL
        } else {
            real_venue.as_str()
        };

        let (chart_slice, _, _) = build_training_chart_slice_with_warmup(
            run_id,
            bundle.run.dataset_version,
            session.visible_until,
            session.history_start_at,
            masked,
            out_inst_id,
            out_venue,
            is_crypto,
            &raw_bars_with_warmup,
            &visible_cas,
            &viewport,
            &raw_markers,
            max_pixel_columns,
        )?;

        let bar_labels: Vec<String> = (1..=total_display_bars)
            .map(bar_label_for_one_based)
            .collect();

        let mark_close = state
            .last_close_by_instrument
            .get(&session.real_instrument_id)
            .copied()
            .or_else(|| display_raw_bars.last().map(|b| b.close))
            .unwrap_or(Decimal::ZERO);
        let pos_qty = state.position_quantity(&session.real_instrument_id);
        let pos_cost = state.position_cost(&session.real_instrument_id);
        let pos_mkt_val = (pos_qty * mark_close).normalize();
        let unrealized = (pos_mkt_val - pos_cost).normalize();
        let total_equity = (state.cash_balance + pos_mkt_val).normalize();

        let account = TrainingAccountSummaryDto {
            quote_currency: state.quote_currency.clone(),
            initial_cash: state.initial_cash,
            cash_balance: state.cash_balance,
            reserved_cash: state.reserved_cash(),
            available_cash: state.available_cash(),
            position_market_value: pos_mkt_val,
            total_equity,
            realized_pnl: state.realized_pnl,
            unrealized_pnl: unrealized,
            dividend_income: state.dividend_income,
            total_fees_quote: state.total_fees_quote,
            lot_size: spec.lot_size,
            tick_size: spec.tick_size,
        };

        let position = if pos_qty > Decimal::ZERO {
            Some(TrainingPositionDto {
                instrument_id: out_inst_id.to_string(),
                display_symbol: if masked {
                    session.display_symbol.clone()
                } else {
                    real_symbol.clone()
                },
                quantity: pos_qty,
                available_quantity: state.available_position(&session.real_instrument_id),
                reserved_quantity: state.reserved_position(&session.real_instrument_id),
                total_cost_quote: pos_cost,
                avg_unit_cost: (pos_cost / pos_qty).normalize(),
                mark_close_price: mark_close,
                market_value_quote: pos_mkt_val,
                unrealized_pnl: unrealized,
            })
        } else {
            None
        };

        let mut all_orders_sorted: Vec<&SimOrder> = state.orders.values().collect();
        all_orders_sorted.sort_by_key(|o| o.order_seq);
        let all_orders: Vec<TrainingOrderDto> = all_orders_sorted
            .iter()
            .map(|o| scrub_order_dto(o, masked, &close_to_idx))
            .collect();
        let open_orders: Vec<TrainingOrderDto> = all_orders
            .iter()
            .filter(|o| o.status.is_open_or_pending())
            .cloned()
            .collect();

        let fills: Vec<TrainingFillDto> = state
            .fills
            .iter()
            .map(|f| scrub_fill_dto(f, masked, &bundle.run.quote_currency, &close_to_idx))
            .collect();
        let notes: Vec<TrainingNoteDto> = raw_notes
            .iter()
            .map(|n| scrub_note_dto(n, masked, &close_to_idx, history_bar_count))
            .collect();

        let run_summary = self.build_training_run_summary_with_context(
            &bundle.run,
            session,
            Some(display_raw_bars),
            Some((&real_venue, &real_symbol)),
            Some(&bundle.fee_profile.name),
        )?;
        let self_evaluation = self.get_training_self_evaluation(run_id)?;

        Ok(TrainingWorkspaceSnapshotDto {
            run: run_summary,
            account,
            position,
            open_orders,
            all_orders,
            fills,
            notes,
            self_evaluation,
            viewport,
            chart_slice,
            bar_labels,
        })
    }

    pub fn preview_training_order(
        &self,
        run_id: &str,
        req: TrainingOrderRequestDto,
    ) -> Result<TrainingOrderPreviewDto, ErrorEnvelope> {
        let (bundle, state) = self.read_verified_sim_state(run_id)?;
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;
        if !matches!(
            bundle.run.status,
            SimRunStatus::Preparing | SimRunStatus::Active
        ) {
            return Err(ErrorEnvelope::new(
                "INVALID_RUN_STATUS",
                format!(
                    "cannot preview orders while run status is '{}'",
                    bundle.run.status.as_str()
                ),
                false,
            ));
        }
        let masked = is_run_identity_masked(&bundle.run, session);
        let submit_req = OrderSubmitRequest {
            instrument_id: session.real_instrument_id.clone(),
            quantity: req.quantity,
            tif: req.tif,
            priority: req.priority.unwrap_or(0),
            kind: req.kind,
            eligible_at_hint: None,
            fill_rule: req.fill_rule,
        };
        match preview_order_submission(&state, &submit_req) {
            Ok(mut preview) => {
                if masked {
                    preview.instrument_id = HIDDEN_INSTRUMENT_ID.to_string();
                    if !preview
                        .fee_currency
                        .eq_ignore_ascii_case(&bundle.run.quote_currency)
                    {
                        preview.fee_currency = "BASE_ASSET".to_string();
                    }
                }
                Ok(TrainingOrderPreviewDto {
                    valid: true,
                    preview: Some(preview),
                    rejection: None,
                })
            }
            Err(SimError::OrderRejected(mut rej)) => {
                if masked {
                    rej.message = rej
                        .message
                        .replace(&session.real_instrument_id, HIDDEN_INSTRUMENT_ID);
                }
                Ok(TrainingOrderPreviewDto {
                    valid: false,
                    preview: None,
                    rejection: Some(*rej),
                })
            }
            Err(other) => Err(map_sim_err(other)),
        }
    }

    pub(crate) fn build_step_receipt(
        &self,
        run_id: &str,
        seq: i64,
        state_hash: String,
        events: &[SimEvent],
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let updated_run = self
            .store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        let updated_session = self
            .store()
            .get_training_session(run_id)
            .map_err(|e| map_storage_err("SESSION_NOT_FOUND", e))?;
        let masked = is_run_identity_masked(&updated_run, &updated_session);
        let vis_bars = self
            .store()
            .query_run_visible_final_bars(run_id, false)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        let (close_to_idx, date_to_idx) = build_visible_index_maps(&vis_bars);
        let scrubbed_events = events
            .iter()
            .map(|ev| {
                scrub_sim_event(
                    ev,
                    masked,
                    &updated_run.quote_currency,
                    &close_to_idx,
                    &date_to_idx,
                )
            })
            .collect();
        let summary = self.build_training_run_summary_with_context(
            &updated_run,
            &updated_session,
            Some(&vis_bars),
            None,
            None,
        )?;
        Ok(TrainingStepReceiptDto {
            run: summary,
            seq,
            state_hash,
            events: scrubbed_events,
            split_cancelled_order_ids: Vec::new(),
            corporate_action_notices: Vec::new(),
        })
    }

    pub fn submit_training_order(
        &mut self,
        run_id: &str,
        req: TrainingOrderRequestDto,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        if !matches!(
            bundle.run.status,
            SimRunStatus::Preparing | SimRunStatus::Active
        ) {
            return Err(ErrorEnvelope::new(
                "INVALID_RUN_STATUS",
                format!(
                    "cannot submit order when run status is '{}'",
                    bundle.run.status.as_str()
                ),
                false,
            ));
        }
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;
        let masked = is_run_identity_masked(&bundle.run, session);

        let submit_req = OrderSubmitRequest {
            instrument_id: session.real_instrument_id.clone(),
            quantity: req.quantity,
            tif: req.tif,
            priority: req.priority.unwrap_or(0),
            kind: req.kind,
            eligible_at_hint: None,
            fill_rule: req.fill_rule,
        };
        let cmd = SimCommand::SubmitOrder(submit_req);
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| ErrorEnvelope::new("SERIALIZE_FAILED", e.to_string(), false))?;

        let (next_state, events) = sim_step(&state, cmd, None)
            .map_err(|err| map_sim_err_scrubbed(err, masked, &session.real_instrument_id))?;

        // Every order changed by the command (a quick fill also fills it) and every new fill.
        let created_orders = changed_orders(run_id, &state, &next_state);
        let new_fills: Vec<SimFillRecord> = next_state.fills[state.fills.len()..]
            .iter()
            .map(|f| sim_fill_to_record(run_id, f))
            .collect();

        let new_seq = bundle.run.latest_seq + 1;
        let new_state_hash = next_state.state_hash();
        let now = Utc::now();
        // `m5-sim-2` keeps checkpoints on bar boundaries only (undo rebuilds from them).
        let checkpoint = (new_seq % 20 == 0 && !next_state.is_m5()).then(|| SimCheckpointRecord {
            run_id: run_id.to_string(),
            seq: new_seq,
            reason: SimCheckpointReason::Periodic20,
            state_json: next_state.canonical_json(),
            state_hash: new_state_hash.clone(),
            created_at: now.to_rfc3339(),
        });

        // Transitions `Preparing -> Active` atomically inside the step savepoint (`TRN-B09`).
        let step_commit = SimulationStepCommit {
            run_id: run_id.to_string(),
            expected_prev_seq: bundle.run.latest_seq,
            seq: new_seq,
            event_kind: "submit_order".to_string(),
            command_json: cmd_json,
            events_json: serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string()),
            new_status: SimRunStatus::Active,
            new_visible_until: session.visible_until,
            new_step_index: session.current_step_index,
            new_state_hash: new_state_hash.clone(),
            orders_to_upsert: created_orders,
            fills_to_insert: new_fills,
            notes_to_upsert: Vec::new(),
            checkpoint,
            committed_at: now,
        };
        self.store_mut()
            .commit_simulation_step_atomic(&step_commit)
            .map_err(|e| map_storage_err("STEP_COMMIT_FAILED", e))?;

        self.build_step_receipt(run_id, new_seq, new_state_hash, &events)
    }

    pub fn cancel_training_order(
        &mut self,
        run_id: &str,
        order_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        if !matches!(
            bundle.run.status,
            SimRunStatus::Active | SimRunStatus::Preparing
        ) {
            return Err(ErrorEnvelope::new(
                "INVALID_RUN_STATUS",
                format!(
                    "cannot cancel order when run status is '{}'",
                    bundle.run.status.as_str()
                ),
                false,
            ));
        }
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;

        let cmd = SimCommand::CancelOrder {
            order_id: order_id.to_string(),
            reason: Some("user_cancelled".to_string()),
        };
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| ErrorEnvelope::new("SERIALIZE_FAILED", e.to_string(), false))?;
        let (next_state, events) = sim_step(&state, cmd, None).map_err(map_sim_err)?;

        let updated_orders = changed_orders(run_id, &state, &next_state);

        let new_seq = bundle.run.latest_seq + 1;
        let new_state_hash = next_state.state_hash();
        let now = Utc::now();
        let checkpoint = (new_seq % 20 == 0 && !next_state.is_m5()).then(|| SimCheckpointRecord {
            run_id: run_id.to_string(),
            seq: new_seq,
            reason: SimCheckpointReason::Periodic20,
            state_json: next_state.canonical_json(),
            state_hash: new_state_hash.clone(),
            created_at: now.to_rfc3339(),
        });

        let step_commit = SimulationStepCommit {
            run_id: run_id.to_string(),
            expected_prev_seq: bundle.run.latest_seq,
            seq: new_seq,
            event_kind: "cancel_order".to_string(),
            command_json: cmd_json,
            events_json: serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string()),
            new_status: SimRunStatus::Active,
            new_visible_until: session.visible_until,
            new_step_index: session.current_step_index,
            new_state_hash: new_state_hash.clone(),
            orders_to_upsert: updated_orders,
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint,
            committed_at: now,
        };
        self.store_mut()
            .commit_simulation_step_atomic(&step_commit)
            .map_err(|e| map_storage_err("STEP_COMMIT_FAILED", e))?;

        self.build_step_receipt(run_id, new_seq, new_state_hash, &events)
    }

    pub fn advance_training_bar(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        if !matches!(
            bundle.run.status,
            SimRunStatus::Preparing | SimRunStatus::Active
        ) {
            return Err(ErrorEnvelope::new(
                "INVALID_RUN_STATUS",
                format!(
                    "cannot advance bar when run status is '{}'",
                    bundle.run.status.as_str()
                ),
                false,
            ));
        }
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;

        // Read strictly through the truncated & source-deduplicated TrainingDataPort (`M4-K01`)
        let Some(next_bar) = self
            .store()
            .query_next_training_step_bar(run_id)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?
        else {
            // Reached the end of the training window: transition cleanly to Finished!
            return self.finish_training_run(run_id);
        };
        let next_close_at = next_bar.close_at;

        let visible_cas = self
            .store()
            .query_training_visible_corporate_actions(
                &session.real_instrument_id,
                bundle.run.dataset_version,
                next_close_at,
            )
            .map_err(|e| map_storage_err("TRAINING_CA_PORT_FAILED", e))?;
        let step_cas: Vec<CorporateActionRecord> = visible_cas
            .into_iter()
            .filter(|ca| ca.ex_date == next_bar.session_date)
            .collect();

        let step_bar = SimStepBar::from_bar(next_bar);
        let cmd = SimCommand::AdvanceBar {
            corporate_actions: step_cas,
        };
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| ErrorEnvelope::new("SERIALIZE_FAILED", e.to_string(), false))?;

        let prev_fill_len = state.fills.len();
        let (next_state, advance_events) =
            sim_step(&state, cmd, Some(&step_bar)).map_err(map_sim_err)?;

        let new_step_index = session.current_step_index + 1;
        let reached_end =
            new_step_index >= session.total_steps || next_close_at >= session.train_end_at;

        // Collect all orders mutated during this bar advance and newly created fills
        let orders_to_upsert = changed_orders(run_id, &state, &next_state);
        let fills_to_insert: Vec<SimFillRecord> = next_state.fills[prev_fill_len..]
            .iter()
            .map(|f| sim_fill_to_record(run_id, f))
            .collect();

        let new_seq = bundle.run.latest_seq + 1;
        let new_state_hash = next_state.state_hash();
        let now = Utc::now();
        let periodic_due = if next_state.is_m5() {
            new_step_index % 20 == 0
        } else {
            new_seq % 20 == 0
        };
        let periodic_cp = (periodic_due && !reached_end).then(|| SimCheckpointRecord {
            run_id: run_id.to_string(),
            seq: new_seq,
            reason: SimCheckpointReason::Periodic20,
            state_json: next_state.canonical_json(),
            state_hash: new_state_hash.clone(),
            created_at: now.to_rfc3339(),
        });

        let step_commit = SimulationStepCommit {
            run_id: run_id.to_string(),
            expected_prev_seq: bundle.run.latest_seq,
            seq: new_seq,
            event_kind: "advance_bar".to_string(),
            command_json: cmd_json,
            events_json: serde_json::to_string(&advance_events)
                .unwrap_or_else(|_| "[]".to_string()),
            new_status: SimRunStatus::Active,
            new_visible_until: next_close_at,
            new_step_index,
            new_state_hash: new_state_hash.clone(),
            orders_to_upsert,
            fills_to_insert,
            notes_to_upsert: Vec::new(),
            checkpoint: periodic_cp,
            committed_at: now,
        };

        // Atomically commit advance_bar + auto-finish (+ practice settlement) when reaching the end (`TRN-B10`).
        let fin_receipt = if reached_end {
            Some(
                self.with_savepoint("advance_and_finish", "STEP_COMMIT_FAILED", |svc| {
                    svc.store_mut()
                        .commit_simulation_step_atomic(&step_commit)
                        .map_err(|e| map_storage_err("STEP_COMMIT_FAILED", e))?;
                    svc.finish_training_run(run_id)
                })?,
            )
        } else {
            self.store_mut()
                .commit_simulation_step_atomic(&step_commit)
                .map_err(|e| map_storage_err("STEP_COMMIT_FAILED", e))?;
            None
        };

        let final_seq = fin_receipt.as_ref().map_or(new_seq, |r| r.seq);
        let final_hash = fin_receipt
            .as_ref()
            .map_or(new_state_hash, |r| r.state_hash.clone());

        let updated_run = self
            .store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        let updated_session = self
            .store()
            .get_training_session(run_id)
            .map_err(|e| map_storage_err("SESSION_NOT_FOUND", e))?;
        let masked = is_run_identity_masked(&updated_run, &updated_session);
        let vis_bars = self
            .store()
            .query_run_visible_final_bars(run_id, false)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        let (close_to_idx, date_to_idx) = build_visible_index_maps(&vis_bars);

        let mut split_cancelled_order_ids = Vec::new();
        let mut corporate_action_notices = Vec::new();
        for ev in &advance_events {
            if let SimEvent::CorporateActionApplied {
                action_type,
                ratio_or_amount,
                cash_credited,
                cancelled_order_ids,
                assumption_marker,
                ..
            } = ev
            {
                split_cancelled_order_ids.extend(cancelled_order_ids.iter().cloned());
                match action_type {
                    CorporateActionType::Split => {
                        let cancel_note = if cancelled_order_ids.is_empty() {
                            String::new()
                        } else {
                            format!("；已自动撤销未成交挂单：{}", cancelled_order_ids.join(", "))
                        };
                        corporate_action_notices.push(format!(
                            "已按除权日盘前应用 {} 拆并股调整（总持仓成本不变）{cancel_note}",
                            to_canonical_text(*ratio_or_amount)
                        ));
                    }
                    CorporateActionType::CashDividend => {
                        let marker = assumption_marker
                            .as_deref()
                            .unwrap_or("ex_date_credit_assumption");
                        corporate_action_notices.push(format!(
                            "已按除息日盘前入账现金分红 {} {}（{}）",
                            to_canonical_text(*cash_credited),
                            updated_run.quote_currency,
                            marker
                        ));
                    }
                }
            }
        }

        // Scrub advance events once, then append already-scrubbed finish events (no double-scrubbing).
        let mut scrubbed_events: Vec<SimEvent> = advance_events
            .iter()
            .map(|ev| {
                scrub_sim_event(
                    ev,
                    masked,
                    &updated_run.quote_currency,
                    &close_to_idx,
                    &date_to_idx,
                )
            })
            .collect();
        if let Some(fin) = fin_receipt {
            scrubbed_events.extend(fin.events);
        }
        let summary = self.build_training_run_summary_with_context(
            &updated_run,
            &updated_session,
            Some(&vis_bars),
            None,
            None,
        )?;

        Ok(TrainingStepReceiptDto {
            run: summary,
            seq: final_seq,
            state_hash: final_hash,
            events: scrubbed_events,
            split_cancelled_order_ids,
            corporate_action_notices,
        })
    }

    pub fn finish_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;
        if bundle.run.status.is_ended() {
            let summary = self.build_training_run_summary_dto(&bundle.run, session)?;
            return Ok(TrainingStepReceiptDto {
                run: summary,
                seq: bundle.run.latest_seq,
                state_hash: bundle.run.latest_state_hash,
                events: Vec::new(),
                split_cancelled_order_ids: Vec::new(),
                corporate_action_notices: Vec::new(),
            });
        }

        let cmd = SimCommand::Finish;
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| ErrorEnvelope::new("SERIALIZE_FAILED", e.to_string(), false))?;
        let (next_state, events) = sim_step(&state, cmd, None).map_err(map_sim_err)?;

        let orders_to_upsert = changed_orders(run_id, &state, &next_state);

        let new_seq = bundle.run.latest_seq + 1;
        let new_state_hash = next_state.state_hash();
        let now = Utc::now();
        let finish_cp = SimCheckpointRecord {
            run_id: run_id.to_string(),
            seq: new_seq,
            reason: SimCheckpointReason::Finish,
            state_json: next_state.canonical_json(),
            state_hash: new_state_hash.clone(),
            created_at: now.to_rfc3339(),
        };

        let step_commit = SimulationStepCommit {
            run_id: run_id.to_string(),
            expected_prev_seq: bundle.run.latest_seq,
            seq: new_seq,
            event_kind: "finish".to_string(),
            command_json: cmd_json,
            events_json: serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string()),
            new_status: SimRunStatus::Finished,
            new_visible_until: session.visible_until,
            new_step_index: session.current_step_index,
            new_state_hash: new_state_hash.clone(),
            orders_to_upsert,
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: Some(finish_cp),
            committed_at: now,
        };
        let session_owned = session.clone();
        // Finishing and the practice-account settlement commit together (`S-52`).
        self.with_savepoint("finish_and_settle", "FINISH_COMMIT_FAILED", |svc| {
            svc.store_mut()
                .commit_simulation_step_atomic(&step_commit)
                .map_err(|e| map_storage_err("FINISH_COMMIT_FAILED", e))?;
            svc.settle_practice_run(
                run_id,
                &session_owned,
                next_state.total_equity_at_last_close(),
                "本局结算",
            )
        })?;

        self.build_step_receipt(run_id, new_seq, new_state_hash, &events)
    }

    pub fn reveal_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingRevealDto, ErrorEnvelope> {
        let (bundle, _) = self.verify_and_recover_sim_state(run_id)?;
        if !bundle.run.status.is_ended() {
            return Err(ErrorEnvelope::new(
                "REVEAL_BEFORE_FINISH_DENIED",
                format!(
                    "训练尚未结束（当前状态：{}），禁止揭晓真实标的、日期或训练后走势",
                    bundle.run.status.as_str()
                ),
                false,
            ));
        }

        let reveal_data = self
            .store()
            .query_post_finish_reveal(run_id)
            .map_err(|e| map_storage_err("REVEAL_QUERY_FAILED", e))?;
        let updated_session = self
            .store_mut()
            .mark_training_session_revealed(run_id)
            .map_err(|e| map_storage_err("REVEAL_MARK_FAILED", e))?;
        let updated_run = self
            .store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;

        let (_, _, _, is_crypto) = self.load_sim_instrument_info(
            &reveal_data.real_instrument_id,
            &updated_run.quote_currency,
        )?;
        let pit_through_reveal = derive_point_in_time_split_adjusted_bars(
            &reveal_data.all_bars_through_reveal,
            &reveal_data.corporate_actions_through_reveal,
            reveal_data.reveal_end_at,
        )
        .map_err(map_sim_err)?;

        let post_finish_bars: Vec<BarRecord> = pit_through_reveal
            .iter()
            .filter(|b| {
                b.close_at > reveal_data.train_end_at && b.close_at <= reveal_data.reveal_end_at
            })
            .cloned()
            .collect();

        let warmup_count = reveal_data
            .all_bars_through_reveal
            .iter()
            .take_while(|b| b.open_at < reveal_data.history_start_at)
            .count();
        let total_disp = reveal_data
            .all_bars_through_reveal
            .len()
            .saturating_sub(warmup_count);
        let vp = KlineViewport::new(total_disp, total_disp.max(120));
        let (full_chart_slice, _, _) = build_training_chart_slice_with_warmup(
            &format!("{run_id}:revealed"),
            reveal_data.dataset_version,
            reveal_data.reveal_end_at,
            reveal_data.history_start_at,
            false,
            &reveal_data.real_instrument_id,
            &reveal_data.venue,
            is_crypto,
            &reveal_data.all_bars_through_reveal,
            &reveal_data.corporate_actions_through_reveal,
            &vp,
            &[],
            320,
        )?;

        let summary = self.build_training_run_summary_dto(&updated_run, &updated_session)?;
        Ok(TrainingRevealDto {
            run: summary,
            real_instrument_id: reveal_data.real_instrument_id.clone(),
            venue: reveal_data.venue,
            real_symbol: reveal_data.real_symbol,
            train_start_date: delta_data::SqliteStore::effective_visible_ex_date(
                &reveal_data.real_instrument_id,
                reveal_data.train_start_at,
            ),
            train_end_date: delta_data::SqliteStore::effective_visible_ex_date(
                &reveal_data.real_instrument_id,
                reveal_data.train_end_at,
            ),
            reveal_end_date: delta_data::SqliteStore::effective_visible_ex_date(
                &reveal_data.real_instrument_id,
                reveal_data.reveal_end_at,
            ),
            post_finish_bars,
            raw_post_finish_bars: reveal_data.future_reveal_bars,
            full_chart_slice,
        })
    }

    // ------------------------------------------------------------------------
    // 5.4 Report Evaluation, Deterministic Replay, Retry & Delete (`W03`, `W04`)
    // ------------------------------------------------------------------------

    pub(crate) fn evaluate_report_for_state(
        &self,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
        state: SimState,
    ) -> Result<TrainingReportDto, ErrorEnvelope> {
        let masked = is_run_identity_masked(run, session);
        let vis_bars = self
            .store()
            .query_run_visible_final_bars(&run.run_id, false)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        let summary = self.build_training_run_summary_with_context(
            run,
            session,
            Some(&vis_bars),
            None,
            None,
        )?;
        let history_bar_count = vis_bars
            .iter()
            .filter(|b| b.open_at < session.train_start_at)
            .count();
        let stepped_training_bars: Vec<BarRecord> = vis_bars
            .into_iter()
            .filter(|b| b.open_at >= session.train_start_at && b.close_at <= session.visible_until)
            .collect();

        let visible_cas = self
            .store()
            .query_run_visible_corporate_actions(&run.run_id)
            .map_err(|e| map_storage_err("TRAINING_CA_PORT_FAILED", e))?;
        let raw_notes = self
            .store()
            .list_training_notes(&run.run_id)
            .map_err(|e| map_storage_err("NOTES_LOAD_FAILED", e))?;
        let note_refs: Vec<TrainingNoteRef> = raw_notes
            .iter()
            .map(|n| TrainingNoteRef {
                note_id: n.note_id.clone(),
                visible_until: n.visible_until,
                rationale: n.rationale.clone(),
                emotion_tag: n.emotion_tag.clone(),
                linked_order_id: n.linked_order_id.clone(),
            })
            .collect();

        let report_input = TrainingReportInput::from_state_and_bars(
            run.run_id.clone(),
            run.dataset_version,
            session.hidden_mode,
            state,
            &stepped_training_bars,
            visible_cas,
            note_refs,
        );
        let mut raw_report = compute_training_report(&report_input);
        if let Some(rule) = &session.fill_rule_version {
            raw_report.limitations.items.push(delta_core::SampleLimitationItem {
                code: "fill_rule".to_string(),
                message: format!(
                    "快捷单在下单时按当前已收盘 K 线的收盘价成交（另加不利点差与滑点、按最小价位不利取整并计费）；挂单、画线下单与止损止盈从下一根开盘起按路径撮合。成交规则版本 {rule}。"
                ),
            });
        }
        if session.practice_account_id.is_some() {
            raw_report
                .limitations
                .items
                .push(delta_core::SampleLimitationItem {
                    code: "practice_settlement".to_string(),
                    message:
                        "练习账户结算额按最后一根已显示 K 线收盘价计算的权益，不假设卖出费用。"
                            .to_string(),
                });
        }
        if run.status == SimRunStatus::Abandoned {
            raw_report
                .limitations
                .items
                .push(delta_core::SampleLimitationItem {
                    code: "abandoned".to_string(),
                    message: "本局已放弃，照常计入训练历史与账户分析。".to_string(),
                });
        }
        let (report, equity_bar_labels) = scrub_training_report(
            raw_report,
            masked,
            history_bar_count,
            session.train_start_at,
        );
        let self_evaluation = self.get_training_self_evaluation(&run.run_id)?;

        Ok(TrainingReportDto {
            run: summary,
            report,
            equity_bar_labels,
            self_evaluation,
        })
    }

    pub fn get_training_report(&self, run_id: &str) -> Result<TrainingReportDto, ErrorEnvelope> {
        let (bundle, state) = self.read_verified_sim_state(run_id)?;
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;
        self.evaluate_report_for_state(&bundle.run, session, state)
    }

    /// Deterministically replay a training run either from `Scratch` (`seq = 0`) or `FromCheckpoint`
    /// (picking an intermediate checkpoint `0 < seq < latest_seq` when available, otherwise `latest_checkpoint`)
    /// without mutating SQLite (`M4-K02`, `M4-A-10`).
    pub fn replay_training_run(
        &self,
        run_id: &str,
        mode: TrainingReplayMode,
    ) -> Result<TrainingReplayResultDto, ErrorEnvelope> {
        let bundle = self
            .store()
            .load_simulation_recovery_bundle(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        if bundle.run.status == SimRunStatus::Damaged {
            return Err(ErrorEnvelope::new(
                "RUN_DAMAGED",
                format!("cannot replay damaged training run '{run_id}'"),
                false,
            ));
        }
        let session = bundle.session.as_ref().ok_or_else(|| {
            ErrorEnvelope::new("SESSION_NOT_FOUND", "missing training_session", false)
        })?;

        let init_vis = self.find_initial_visible_until_for_session(&bundle.run, session)?;
        let scratch_init_state = self.build_initial_sim_state_for(
            &bundle.run.engine_version,
            &session.timeframe,
            &bundle.run.quote_currency,
            bundle.run.initial_cash,
            &bundle.fee_profile,
            &session.real_instrument_id,
            bundle.run.dataset_version,
            session.history_start_at,
            init_vis,
        )?;

        let (started_from_seq, initial_or_checkpoint_hash, final_state, full_step_hashes) =
            match mode {
                TrainingReplayMode::Scratch => {
                    let h0 = scratch_init_state.state_hash();
                    let (final_st, hashes) = self
                        .replay_events_on_state(
                            scratch_init_state,
                            &bundle.run,
                            session,
                            &bundle.all_events,
                        )
                        .map_err(|failure| failure.into_envelope("REPLAY_FAILED"))?;
                    (0, h0, final_st, hashes)
                }
                TrainingReplayMode::FromCheckpoint => {
                    let all_cps = self
                        .store()
                        .list_sim_checkpoints(run_id)
                        .map_err(|e| map_storage_err("CHECKPOINT_LOAD_FAILED", e))?;
                    // Prefer an intermediate checkpoint (`0 < seq < latest_seq`) so FromCheckpoint
                    // genuinely exercises checkpoint restoration + partial tail replay even on finished runs!
                    // For `m5-sim-2`, restrict to bar-boundary checkpoints BEFORE falling back (`TRN-B03`).
                    let valid_cps: Vec<&SimCheckpointRecord> = all_cps
                        .iter()
                        .filter(|cp| {
                            bundle.run.engine_version != SIM_ENGINE_VERSION_M5
                                || cp.seq == 0
                                || bundle
                                    .all_events
                                    .get((cp.seq - 1) as usize)
                                    .is_some_and(|e| {
                                        matches!(e.event_kind.as_str(), "advance_bar" | "finish")
                                    })
                        })
                        .collect();
                    let chosen_cp = valid_cps
                        .iter()
                        .rev()
                        .copied()
                        .find(|cp| cp.seq > 0 && cp.seq < bundle.run.latest_seq)
                        .or_else(|| valid_cps.last().copied());

                    match chosen_cp {
                        Some(cp) => {
                            let mut cp_state: SimState = serde_json::from_str(&cp.state_json)
                                .map_err(|e| {
                                    ErrorEnvelope::new("CORRUPT_CHECKPOINT", e.to_string(), false)
                                })?;
                            cp_state.normalize_fields();
                            let cp_hash = cp_state.state_hash();
                            if cp_hash != cp.state_hash {
                                return Err(ErrorEnvelope::new(
                                    "RUN_DAMAGED",
                                    "checkpoint state_json hash mismatch during replay",
                                    false,
                                ));
                            }
                            let prefix_hashes: Vec<(i64, String)> = bundle
                                .all_events
                                .iter()
                                .filter(|e| e.seq <= cp.seq)
                                .map(|e| (e.seq, e.state_hash.clone()))
                                .collect();
                            let tail_events: Vec<_> = bundle
                                .all_events
                                .iter()
                                .filter(|e| e.seq > cp.seq)
                                .cloned()
                                .collect();
                            let (final_st, tail_hashes) = self
                                .replay_events_on_state(
                                    cp_state,
                                    &bundle.run,
                                    session,
                                    &tail_events,
                                )
                                .map_err(|failure| failure.into_envelope("REPLAY_FAILED"))?;
                            let mut combined = prefix_hashes;
                            combined.extend(tail_hashes);
                            (cp.seq, cp_hash, final_st, combined)
                        }
                        None => {
                            let h0 = scratch_init_state.state_hash();
                            let (final_st, hashes) = self
                                .replay_events_on_state(
                                    scratch_init_state,
                                    &bundle.run,
                                    session,
                                    &bundle.all_events,
                                )
                                .map_err(|failure| failure.into_envelope("REPLAY_FAILED"))?;
                            (0, h0, final_st, hashes)
                        }
                    }
                }
            };

        let final_state_hash = final_state.state_hash();
        let matches_persisted_hashes = final_state_hash == bundle.run.latest_state_hash
            && full_step_hashes.len() == bundle.all_events.len()
            && full_step_hashes
                .iter()
                .zip(bundle.all_events.iter())
                .all(|((s1, h1), ev)| *s1 == ev.seq && h1 == &ev.state_hash);

        let report = self.evaluate_report_for_state(&bundle.run, session, final_state)?;

        Ok(TrainingReplayResultDto {
            run_id: run_id.to_string(),
            mode,
            started_from_seq,
            initial_or_checkpoint_hash,
            step_hashes: full_step_hashes,
            final_state_hash,
            matches_persisted_hashes,
            report,
        })
    }

    pub fn retry_training_run(
        &mut self,
        original_run_id: &str,
        new_run_id: Option<String>,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let orig_run = self
            .store()
            .get_simulation_run(original_run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        if orig_run.status == SimRunStatus::Damaged {
            return Err(ErrorEnvelope::new(
                "RUN_DAMAGED",
                format!("已损坏（damaged）的训练会话 '{original_run_id}' 不可重试"),
                false,
            ));
        }
        let orig_session = self
            .store()
            .get_training_session(original_run_id)
            .map_err(|e| map_storage_err("SESSION_NOT_FOUND", e))?;
        let fee_profile = self
            .store()
            .get_fee_profile_version(&orig_run.fee_profile_version_id)
            .map_err(|e| map_storage_err("FEE_PROFILE_NOT_FOUND", e))?;

        let init_vis = self.find_initial_visible_until_for_session(&orig_run, &orig_session)?;
        let init_state = self.build_initial_sim_state_for(
            &orig_run.engine_version,
            &orig_session.timeframe,
            &orig_run.quote_currency,
            orig_run.initial_cash,
            &fee_profile,
            &orig_session.real_instrument_id,
            orig_run.dataset_version,
            orig_session.history_start_at,
            init_vis,
        )?;
        let state_json = init_state.canonical_json();
        let state_hash = init_state.state_hash();

        let child_id = new_run_id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("TRN-RETRY-{}", uuid::Uuid::new_v4()));
        let now = Utc::now();
        let init_cp = SimCheckpointRecord {
            run_id: child_id.clone(),
            seq: 0,
            reason: SimCheckpointReason::Initial,
            state_json,
            state_hash: state_hash.clone(),
            created_at: now.to_rfc3339(),
        };

        self.store_mut()
            .create_retry_training_run(
                original_run_id,
                &child_id,
                init_vis,
                &state_hash,
                Some(&init_cp),
                now,
            )
            .map_err(|e| map_storage_err("RETRY_TRAINING_FAILED", e))?;

        if let Ok(pq) = self.load_pool_query_metadata(&orig_run, &orig_session) {
            let _ = self.save_pool_query_metadata(&child_id, orig_run.dataset_version, &pq);
        }

        self.get_training_run_summary(&child_id)
    }

    pub fn preview_delete_training_run(
        &self,
        run_id: &str,
    ) -> Result<DeleteTrainingRunPreview, ErrorEnvelope> {
        self.store()
            .preview_delete_training_run(run_id)
            .map_err(|e| map_storage_err("DELETE_PREVIEW_FAILED", e))
    }

    pub fn delete_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<DeleteTrainingRunPreview, ErrorEnvelope> {
        let preview = self.with_savepoint("delete_training_run", "DELETE_RUN_FAILED", |svc| {
            let preview = svc
                .store_mut()
                .delete_training_run(run_id)
                .map_err(|e| map_storage_err("DELETE_RUN_FAILED", e))?;
            svc.store()
                .conn()
                .execute(
                    "DELETE FROM analysis_report WHERE id IN (?1, ?2)",
                    params![
                        format!("REP:TRN-POOL:{run_id}"),
                        format!("REP:TRN-SELF:{run_id}")
                    ],
                )
                .map_err(|e| ErrorEnvelope::new("DELETE_RUN_FAILED", e.to_string(), false))?;
            Ok(preview)
        })?;
        evict_training_slice_cache(run_id);
        Ok(preview)
    }
}
