//! M5 training workbench services: practice accounts (`S-52`), order modify / single undo
//! (`S-51`), abandon / restart, drawings (`S-54`) and the account analysis (`S-53`).
//!
//! Every money amount is a `Decimal`; every mutation of the run goes through the same event log,
//! recovery and atomic commit path as the M4 commands.

use chrono::Utc;
use delta_core::{to_canonical_text, SimCommand, SimEvent, SimState, SIM_ENGINE_VERSION_M5};
use delta_data::{
    PracticeAccountEntryRecord, PracticeAccountRecord, PracticeEntryKind, SimCheckpointReason,
    SimCheckpointRecord, SimRunStatus, SimulationStepCommit, TrainingDrawingRecord,
    TrainingSessionRecord,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::service::{AppService, ErrorEnvelope};
use crate::training::{
    is_run_identity_masked, map_sim_err, map_storage_err, scrub_sim_event, sim_fill_to_record,
    sim_order_to_record, CreateTrainingRunRequest, TrainingMarketCategory, TrainingRunSummaryDto,
    TrainingStepReceiptDto,
};

/// Opening deposit of a newly created practice account (`S-52`).
pub const PRACTICE_OPENING_DEPOSIT: Decimal = Decimal::from_parts(100_000, 0, 0, false, 0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PracticeAccountDto {
    pub account_id: String,
    pub quote_currency: String,
    pub name: String,
    /// Balance = sum of all entries (funds not allocated to an open run).
    pub available: Decimal,
    /// Allocation held by the open run on this account (0 when none).
    pub allocated_to_open_run: Decimal,
    pub open_run_id: Option<String>,
    /// Available + mark-to-market equity of the open run at its last visible close.
    pub equity: Decimal,
    pub entry_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PracticeEntryDto {
    pub seq: i64,
    pub kind: PracticeEntryKind,
    pub amount: Decimal,
    pub run_id: Option<String>,
    pub note: String,
    pub created_at: String,
    pub balance_after: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PracticeTransferKind {
    Deposit,
    Withdrawal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifyTrainingOrderRequest {
    pub order_id: String,
    #[serde(default)]
    pub quantity: Option<Decimal>,
    #[serde(default)]
    pub limit_price: Option<Decimal>,
    #[serde(default)]
    pub stop_price: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingDrawingDto {
    pub drawing_id: String,
    pub kind: String,
    /// Anchors as (bar label index, price) pairs; never absolute dates (`S-54`).
    pub anchors: Vec<(i64, Decimal)>,
    pub color: String,
    pub line_style: String,
}

/// One candle of the higher-timeframe view; dates are omitted while the run is masked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregatedCandleDto {
    pub index: usize,
    pub open_at: Option<chrono::DateTime<Utc>>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub base_bar_count: usize,
    /// `false` = still forming (drawn with the "unfinished" style).
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingAggregatedViewDto {
    pub run_id: String,
    pub base_timeframe: String,
    pub view_timeframe: String,
    pub candles: Vec<AggregatedCandleDto>,
}

/// One row of the training history list (`S-53`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingHistoryRowDto {
    pub index: usize,
    pub run_id: String,
    pub status: SimRunStatus,
    pub market_category: String,
    pub timeframe: String,
    pub hidden_mode: bool,
    pub display_symbol: String,
    /// Training interval; `None` while the run is masked (hidden and not revealed).
    pub train_start_date: Option<chrono::NaiveDate>,
    pub train_end_date: Option<chrono::NaiveDate>,
    pub current_step: i64,
    pub total_steps: i64,
    pub total_return_pct: Decimal,
    pub total_pnl: Decimal,
    pub closed_trade_count: usize,
    pub win_rate_pct: Option<Decimal>,
    /// `练习账户 USDT` or `独立资金（M4）` / `独立资金`.
    pub funding_label: String,
    pub practice_account_id: Option<String>,
    pub engine_version: String,
}

const DRAWING_KINDS: [&str; 3] = ["ray", "horizontal", "segment"];
const LINE_STYLES: [&str; 3] = ["solid", "dashed", "dotted"];

fn practice_err(code: &'static str, msg: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new(code, msg.into(), false)
}

impl AppService {
    // ------------------------------------------------------------------
    // Practice accounts (`S-52`)
    // ------------------------------------------------------------------

    /// The practice account of a quote currency, created on first use with the opening deposit.
    pub(crate) fn ensure_practice_account(
        &mut self,
        quote_currency: &str,
    ) -> Result<PracticeAccountRecord, ErrorEnvelope> {
        if let Some(acc) = self
            .store()
            .get_practice_account_by_currency(quote_currency)
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?
        {
            return Ok(acc);
        }
        self.store_mut()
            .create_practice_account(quote_currency, PRACTICE_OPENING_DEPOSIT, Utc::now())
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))
    }

    /// The run that is still open (preparing / active / paused) on an account.
    pub(crate) fn open_run_on_account(
        &self,
        account_id: &str,
    ) -> Result<Option<String>, ErrorEnvelope> {
        use rusqlite::OptionalExtension;
        self.store()
            .conn()
            .query_row(
                "SELECT s.run_id FROM training_session s JOIN simulation_run r ON r.run_id = s.run_id
                 WHERE s.practice_account_id = ?1 AND r.status IN ('preparing', 'active', 'paused')
                 LIMIT 1",
                rusqlite::params![account_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| practice_err("PRACTICE_ACCOUNT_FAILED", e.to_string()))
    }

    /// Append the settlement entry of an ended practice-funded run (no-op for independent
    /// capital runs). The settlement is the equity at the last visible close; no exit fee is
    /// assumed (`S-52`).
    pub(crate) fn settle_practice_run(
        &mut self,
        run_id: &str,
        session: &TrainingSessionRecord,
        equity: Decimal,
        note: &str,
    ) -> Result<(), ErrorEnvelope> {
        let Some(account_id) = session.practice_account_id.clone() else {
            return Ok(());
        };
        let already = self
            .store()
            .list_practice_entries(&account_id)
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?
            .iter()
            .any(|e| {
                e.kind == PracticeEntryKind::RunSettlement && e.run_id.as_deref() == Some(run_id)
            });
        if already {
            return Ok(());
        }
        self.store_mut()
            .append_practice_entries(
                &account_id,
                &[(
                    PracticeEntryKind::RunSettlement,
                    equity,
                    Some(run_id.to_string()),
                    note.to_string(),
                )],
                Utc::now(),
            )
            .map(|_| ())
            .map_err(|e| map_storage_err("PRACTICE_SETTLEMENT_FAILED", e))
    }

    fn practice_account_dto(
        &self,
        acc: &PracticeAccountRecord,
    ) -> Result<PracticeAccountDto, ErrorEnvelope> {
        let entries = self
            .store()
            .list_practice_entries(&acc.id)
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?;
        let available: Decimal = entries.iter().map(|e| e.amount).sum();
        let open_run_id = self.open_run_on_account(&acc.id)?;
        let mut allocated = Decimal::ZERO;
        let mut open_equity = Decimal::ZERO;
        if let Some(run_id) = &open_run_id {
            allocated = -entries
                .iter()
                .filter(|e| e.run_id.as_deref() == Some(run_id.as_str()))
                .map(|e| e.amount)
                .sum::<Decimal>();
            open_equity = match self.read_verified_sim_state(run_id) {
                Ok((_, st)) => st.total_equity_at_last_close(),
                Err(_) => allocated,
            };
        }
        Ok(PracticeAccountDto {
            account_id: acc.id.clone(),
            quote_currency: acc.quote_currency.clone(),
            name: acc.name.clone(),
            available,
            allocated_to_open_run: allocated,
            open_run_id,
            equity: available + open_equity,
            entry_count: entries.len(),
        })
    }

    /// Practice account of a currency (created with the opening deposit on first use).
    pub fn show_practice_account(
        &mut self,
        quote_currency: &str,
    ) -> Result<PracticeAccountDto, ErrorEnvelope> {
        let acc = self.ensure_practice_account(quote_currency)?;
        self.practice_account_dto(&acc)
    }

    pub fn list_practice_accounts(&self) -> Result<Vec<PracticeAccountDto>, ErrorEnvelope> {
        let accounts = self
            .store()
            .list_practice_accounts()
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?;
        accounts
            .iter()
            .map(|a| self.practice_account_dto(a))
            .collect()
    }

    pub fn practice_account_history(
        &mut self,
        quote_currency: &str,
    ) -> Result<Vec<PracticeEntryDto>, ErrorEnvelope> {
        let acc = self.ensure_practice_account(quote_currency)?;
        let entries = self
            .store()
            .list_practice_entries(&acc.id)
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?;
        Ok(entries_to_dto(&entries))
    }

    /// Deposit or withdraw. A withdrawal may not exceed the available balance.
    pub fn transfer_practice_funds(
        &mut self,
        quote_currency: &str,
        kind: PracticeTransferKind,
        amount: Decimal,
    ) -> Result<PracticeAccountDto, ErrorEnvelope> {
        if amount <= Decimal::ZERO {
            return Err(practice_err(
                "INVALID_TRANSFER_AMOUNT",
                "划转金额必须大于 0",
            ));
        }
        let acc = self.ensure_practice_account(quote_currency)?;
        let available = self
            .store()
            .practice_account_balance(&acc.id)
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?;
        let (entry_kind, signed, note) = match kind {
            PracticeTransferKind::Deposit => (PracticeEntryKind::Deposit, amount, "入金"),
            PracticeTransferKind::Withdrawal => {
                if amount > available {
                    return Err(practice_err(
                        "WITHDRAWAL_EXCEEDS_AVAILABLE",
                        format!(
                            "出金 {} 超过可用余额 {}",
                            to_canonical_text(amount),
                            to_canonical_text(available)
                        ),
                    ));
                }
                (PracticeEntryKind::Withdrawal, -amount, "出金")
            }
        };
        self.store_mut()
            .append_practice_entries(
                &acc.id,
                &[(entry_kind, signed, None, note.to_string())],
                Utc::now(),
            )
            .map_err(|e| map_storage_err("PRACTICE_TRANSFER_FAILED", e))?;
        self.practice_account_dto(&acc)
    }

    // ------------------------------------------------------------------
    // Modify / undo (`S-51` ②③)
    // ------------------------------------------------------------------

    pub fn modify_training_order(
        &mut self,
        run_id: &str,
        req: ModifyTrainingOrderRequest,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let cmd = SimCommand::ModifyOrder {
            order_id: req.order_id,
            quantity: req.quantity,
            limit_price: req.limit_price,
            stop_price: req.stop_price,
        };
        self.commit_m5_command(run_id, cmd, "modify_order")
    }

    /// Undo the latest submit / quick fill / modify / cancel of the current bar. Never crosses a
    /// bar advance.
    pub fn undo_training_last(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, _) = self.verify_and_recover_sim_state(run_id)?;
        if bundle.run.engine_version != SIM_ENGINE_VERSION_M5 {
            return Err(practice_err(
                "ENGINE_FEATURE_UNAVAILABLE",
                format!(
                    "该局使用 {}，撤销只在 {SIM_ENGINE_VERSION_M5} 的局中可用",
                    bundle.run.engine_version
                ),
            ));
        }
        let (_, _, replayer) = self
            .compute_recovery_full(&bundle)
            .map_err(|f| f.into_envelope("RUN_DAMAGED"))?;
        let Some((target_seq, _)) = replayer.undo_target() else {
            return Err(practice_err(
                "UNDO_UNAVAILABLE",
                "本根 K 线内没有可撤销的操作（撤销不会越过已推进的 K 线）",
            ));
        };
        self.commit_m5_command(run_id, SimCommand::UndoLast { target_seq }, "undo_last")
    }

    /// Commit a non-advancing m5 command through the undo replayer; the order/fill projection is
    /// rebuilt from the resulting state in the same savepoint as the event.
    fn commit_m5_command(
        &mut self,
        run_id: &str,
        cmd: SimCommand,
        event_kind: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        if bundle.run.engine_version != SIM_ENGINE_VERSION_M5 {
            return Err(practice_err(
                "ENGINE_FEATURE_UNAVAILABLE",
                format!(
                    "该局使用 {}，改单与撤销只在 {SIM_ENGINE_VERSION_M5} 的局中可用",
                    bundle.run.engine_version
                ),
            ));
        }
        if !matches!(
            bundle.run.status,
            SimRunStatus::Active | SimRunStatus::Preparing
        ) {
            return Err(practice_err(
                "INVALID_RUN_STATUS",
                format!("当前状态 '{}' 不能改单或撤销", bundle.run.status.as_str()),
            ));
        }
        let session = bundle
            .session
            .clone()
            .ok_or_else(|| practice_err("SESSION_NOT_FOUND", "missing training_session"))?;
        let (_, _, mut replayer) = self
            .compute_recovery_full(&bundle)
            .map_err(|f| f.into_envelope("RUN_DAMAGED"))?;
        let new_seq = bundle.run.latest_seq + 1;
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| practice_err("SERIALIZE_FAILED", e.to_string()))?;
        let (next_state, events) = replayer
            .apply(&state, new_seq as u64, cmd, None)
            .map_err(map_sim_err)?;
        let new_state_hash = next_state.state_hash();
        let now = Utc::now();
        let step_commit = SimulationStepCommit {
            run_id: run_id.to_string(),
            expected_prev_seq: bundle.run.latest_seq,
            seq: new_seq,
            event_kind: event_kind.to_string(),
            command_json: cmd_json,
            events_json: serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string()),
            new_status: bundle.run.status,
            new_visible_until: session.visible_until,
            new_step_index: session.current_step_index,
            new_state_hash: new_state_hash.clone(),
            orders_to_upsert: Vec::new(),
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: None,
            committed_at: now,
        };
        let orders: Vec<_> = next_state
            .orders
            .values()
            .map(|o| sim_order_to_record(run_id, o))
            .collect();
        let fills: Vec<_> = next_state
            .fills
            .iter()
            .map(|f| sim_fill_to_record(run_id, f))
            .collect();
        self.with_savepoint("m5_command", "STEP_COMMIT_FAILED", |svc| {
            svc.store_mut()
                .commit_simulation_step_atomic(&step_commit)
                .map_err(|e| map_storage_err("STEP_COMMIT_FAILED", e))?;
            svc.store_mut()
                .replace_run_projection(run_id, &orders, &fills)
                .map_err(|e| map_storage_err("STEP_COMMIT_FAILED", e))
        })?;
        self.receipt_for(
            run_id,
            &bundle.run.quote_currency,
            new_seq,
            new_state_hash,
            &events,
        )
    }

    pub(crate) fn with_savepoint<T>(
        &mut self,
        name: &str,
        code: &'static str,
        f: impl FnOnce(&mut Self) -> Result<T, ErrorEnvelope>,
    ) -> Result<T, ErrorEnvelope> {
        self.store()
            .conn()
            .execute_batch(&format!("SAVEPOINT {name}"))
            .map_err(|e| practice_err(code, e.to_string()))?;
        let out = f(self);
        let sql = if out.is_ok() {
            format!("RELEASE {name}")
        } else {
            format!("ROLLBACK TO {name}; RELEASE {name}")
        };
        self.store()
            .conn()
            .execute_batch(&sql)
            .map_err(|e| practice_err(code, e.to_string()))?;
        out
    }

    fn receipt_for(
        &mut self,
        run_id: &str,
        quote_currency: &str,
        seq: i64,
        state_hash: String,
        events: &[SimEvent],
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run = self
            .store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        let session = self
            .store()
            .get_training_session(run_id)
            .map_err(|e| map_storage_err("SESSION_NOT_FOUND", e))?;
        let masked = is_run_identity_masked(&run, &session);
        let (close_to_idx, date_to_idx) = self.load_visible_index_maps(run_id)?;
        let events = events
            .iter()
            .map(|ev| scrub_sim_event(ev, masked, quote_currency, &close_to_idx, &date_to_idx))
            .collect();
        let summary = self.build_training_run_summary_dto(&run, &session)?;
        Ok(TrainingStepReceiptDto {
            run: summary,
            seq,
            state_hash,
            events,
            split_cancelled_order_ids: Vec::new(),
            corporate_action_notices: Vec::new(),
        })
    }

    // ------------------------------------------------------------------
    // Abandon / restart (`S-52`)
    // ------------------------------------------------------------------

    /// Abandon an unfinished run: open orders are cancelled, the run becomes `abandoned`
    /// (not resumable) and the practice account is settled like a finish.
    pub fn abandon_training_run(
        &mut self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let (bundle, state) = self.verify_and_recover_sim_state(run_id)?;
        if bundle.run.status.is_ended() {
            return Err(practice_err(
                "INVALID_RUN_STATUS",
                format!("该局已结束（{}），不能放弃", bundle.run.status.as_str()),
            ));
        }
        let session = bundle
            .session
            .clone()
            .ok_or_else(|| practice_err("SESSION_NOT_FOUND", "missing training_session"))?;
        let cmd = SimCommand::Finish;
        let cmd_json = serde_json::to_string(&cmd)
            .map_err(|e| practice_err("SERIALIZE_FAILED", e.to_string()))?;
        let (next_state, events) = delta_core::step(&state, cmd, None).map_err(map_sim_err)?;
        let orders_to_upsert = changed_orders(run_id, &state, &next_state);
        let new_seq = bundle.run.latest_seq + 1;
        let new_state_hash = next_state.state_hash();
        let now = Utc::now();
        let step_commit = SimulationStepCommit {
            run_id: run_id.to_string(),
            expected_prev_seq: bundle.run.latest_seq,
            seq: new_seq,
            event_kind: "finish".to_string(),
            command_json: cmd_json,
            events_json: serde_json::to_string(&events).unwrap_or_else(|_| "[]".to_string()),
            new_status: SimRunStatus::Abandoned,
            new_visible_until: session.visible_until,
            new_step_index: session.current_step_index,
            new_state_hash: new_state_hash.clone(),
            orders_to_upsert,
            fills_to_insert: Vec::new(),
            notes_to_upsert: Vec::new(),
            checkpoint: Some(SimCheckpointRecord {
                run_id: run_id.to_string(),
                seq: new_seq,
                reason: SimCheckpointReason::Finish,
                state_json: next_state.canonical_json(),
                state_hash: new_state_hash.clone(),
                created_at: now.to_rfc3339(),
            }),
            committed_at: now,
        };
        let equity = next_state.total_equity_at_last_close();
        self.with_savepoint("abandon_and_settle", "ABANDON_FAILED", |svc| {
            svc.store_mut()
                .commit_simulation_step_atomic(&step_commit)
                .map_err(|e| map_storage_err("ABANDON_FAILED", e))?;
            svc.settle_practice_run(run_id, &session, equity, "放弃本局结算")
        })?;
        self.receipt_for(
            run_id,
            &bundle.run.quote_currency,
            new_seq,
            new_state_hash,
            &events,
        )
    }

    /// "Restart" = abandon this run and start a new one with the same settings (`S-52`).
    pub fn restart_training_run(
        &mut self,
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
        let pool = self.load_pool_query_metadata(&run, &session)?;
        let req = CreateTrainingRunRequest {
            run_id: None,
            market_category: TrainingMarketCategory::parse_str(&session.market_category)
                .unwrap_or(TrainingMarketCategory::Any),
            instrument_id: pool.instrument_id.clone(),
            start_date: pool.start_date,
            end_date: pool.end_date,
            warmup_bars: pool.warmup_bars,
            history_bars: pool.history_len,
            training_bars: pool.train_len,
            reveal_bars: pool.reveal_len,
            seed: None,
            dataset_version: None,
            initial_cash: run.initial_cash,
            fee_profile_version_id: run.fee_profile_version_id.clone(),
            hidden_mode: session.hidden_mode,
            create_and_start: true,
            timeframe: delta_core::Timeframe::parse(&session.timeframe)
                .unwrap_or(delta_core::Timeframe::D1),
            use_practice_account: session.practice_account_id.is_some(),
            allocation: None,
        };
        if !run.status.is_ended() {
            self.abandon_training_run(run_id)?;
        }
        self.create_training_run(req)
    }

    /// Higher-timeframe view built only from the host-truncated visible base bars (`S-50`).
    pub fn get_training_aggregated_view(
        &self,
        run_id: &str,
        view: delta_core::Timeframe,
    ) -> Result<TrainingAggregatedViewDto, ErrorEnvelope> {
        let (bundle, _) = self.read_verified_sim_state(run_id)?;
        let session = bundle
            .session
            .as_ref()
            .ok_or_else(|| practice_err("SESSION_NOT_FOUND", "missing training_session"))?;
        let base = delta_core::Timeframe::parse(&session.timeframe)
            .ok_or_else(|| practice_err("INVALID_TIMEFRAME", session.timeframe.clone()))?;
        if !base.higher_views().contains(&view) {
            return Err(practice_err(
                "TIMEFRAME_VIEW_UNAVAILABLE",
                format!("{base} 局只能查看更高级别：{:?}", base.higher_views()),
            ));
        }
        let bars = self
            .store()
            .query_run_visible_final_bars(run_id, false)
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        let masked = is_run_identity_masked(&bundle.run, session);
        let candles = delta_core::aggregate_bars(&bars, base, view)
            .into_iter()
            .enumerate()
            .map(|(index, a)| AggregatedCandleDto {
                index,
                open_at: (!masked).then_some(a.open_at),
                open: a.open,
                high: a.high,
                low: a.low,
                close: a.close,
                volume: a.volume,
                base_bar_count: a.base_bar_count,
                complete: a.complete,
            })
            .collect();
        Ok(TrainingAggregatedViewDto {
            run_id: run_id.to_string(),
            base_timeframe: base.as_str().to_string(),
            view_timeframe: view.as_str().to_string(),
            candles,
        })
    }

    // ------------------------------------------------------------------
    // History and account analysis (`S-53`)
    // ------------------------------------------------------------------

    pub fn training_history(&self) -> Result<Vec<TrainingHistoryRowDto>, ErrorEnvelope> {
        let mut runs = self.list_training_runs()?;
        runs.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.run_id.cmp(&b.run_id))
        });
        let mut out = Vec::with_capacity(runs.len());
        for (i, run) in runs.into_iter().enumerate() {
            let report = self.get_training_report(&run.run_id).ok();
            let funding_label = match &run.practice_account_id {
                Some(id) => format!("练习账户 {}", id.trim_start_matches("PRACTICE:")),
                None if run.engine_version == delta_core::SIM_ENGINE_VERSION => {
                    "独立资金（M4）".to_string()
                }
                None => "独立资金".to_string(),
            };
            out.push(TrainingHistoryRowDto {
                index: i + 1,
                status: run.status,
                market_category: run.market_category.clone(),
                timeframe: run.timeframe.clone(),
                hidden_mode: run.hidden_mode,
                display_symbol: run.display_symbol.clone(),
                train_start_date: (!run.is_identity_masked)
                    .then_some(run.train_start_date)
                    .flatten(),
                train_end_date: (!run.is_identity_masked)
                    .then_some(run.train_end_date)
                    .flatten(),
                current_step: run.current_step_index,
                total_steps: run.total_steps,
                total_return_pct: report
                    .as_ref()
                    .map(|r| r.report.total_return_pct)
                    .unwrap_or_default(),
                total_pnl: report
                    .as_ref()
                    .map(|r| r.report.total_pnl)
                    .unwrap_or_default(),
                closed_trade_count: report
                    .as_ref()
                    .map(|r| r.report.win_rate.closed_trade_count)
                    .unwrap_or(0),
                win_rate_pct: report.as_ref().and_then(|r| r.report.win_rate.win_rate_pct),
                funding_label,
                practice_account_id: run.practice_account_id.clone(),
                engine_version: run.engine_version.clone(),
                run_id: run.run_id,
            });
        }
        Ok(out)
    }

    /// `m5-account-report-1` for one practice account; only practice-funded runs count (M4
    /// independent-capital runs are excluded), abandoned runs are included.
    pub fn practice_account_analysis(
        &mut self,
        quote_currency: &str,
    ) -> Result<delta_core::AccountReport, ErrorEnvelope> {
        let acc = self.ensure_practice_account(quote_currency)?;
        let entries = self
            .store()
            .list_practice_entries(&acc.id)
            .map_err(|e| map_storage_err("PRACTICE_ACCOUNT_FAILED", e))?;
        let mut runs = Vec::new();
        for summary in self.list_training_runs()? {
            if summary.practice_account_id.as_deref() != Some(acc.id.as_str()) {
                continue;
            }
            let rid = summary.run_id.as_str();
            let allocation = -entries
                .iter()
                .filter(|e| {
                    e.run_id.as_deref() == Some(rid) && e.kind == PracticeEntryKind::RunAllocation
                })
                .map(|e| e.amount)
                .sum::<Decimal>();
            let settlement = entries
                .iter()
                .find(|e| {
                    e.run_id.as_deref() == Some(rid) && e.kind == PracticeEntryKind::RunSettlement
                })
                .map(|e| e.amount);
            let (_, state) = self.read_verified_sim_state(rid)?;
            let report = self.get_training_report(rid)?.report;
            runs.push(delta_core::AccountRunInput {
                run_id: summary.run_id.clone(),
                market_category: summary.market_category.clone(),
                timeframe: summary.timeframe.clone(),
                allocation,
                settlement,
                mark_equity: state.total_equity_at_last_close(),
                closed_groups: report.closed_trade_groups.clone(),
                total_fees: report.total_fees_quote,
                planned_closed_group_count: report.plan_execution.planned_closed_group_count,
                plan_compliant_count: report.plan_execution.compliant_count,
                undo_count: state.undo_count,
            });
        }
        let inputs: Vec<delta_core::AccountEntryInput> = entries
            .iter()
            .map(|e| delta_core::AccountEntryInput {
                seq: e.seq,
                kind: e.kind.as_str().to_string(),
                amount: e.amount,
                run_id: e.run_id.clone(),
            })
            .collect();
        Ok(delta_core::compute_account_report(
            &acc.quote_currency,
            &inputs,
            &runs,
        ))
    }

    // ------------------------------------------------------------------
    // Drawings (`S-54`): saved per run, outside the matching hash.
    // ------------------------------------------------------------------

    pub fn list_training_drawings(
        &self,
        run_id: &str,
    ) -> Result<Vec<TrainingDrawingDto>, ErrorEnvelope> {
        let rows = self
            .store()
            .list_training_drawings(run_id, false)
            .map_err(|e| map_storage_err("DRAWING_LOAD_FAILED", e))?;
        rows.iter().map(drawing_to_dto).collect()
    }

    pub fn save_training_drawing(
        &mut self,
        run_id: &str,
        drawing: TrainingDrawingDto,
    ) -> Result<TrainingDrawingDto, ErrorEnvelope> {
        if !DRAWING_KINDS.contains(&drawing.kind.as_str()) {
            return Err(practice_err(
                "INVALID_DRAWING",
                format!("未知画线类型 {}", drawing.kind),
            ));
        }
        if !LINE_STYLES.contains(&drawing.line_style.as_str()) {
            return Err(practice_err(
                "INVALID_DRAWING",
                format!("未知线型 {}", drawing.line_style),
            ));
        }
        let need = if drawing.kind == "horizontal" { 1 } else { 2 };
        if drawing.anchors.len() != need {
            return Err(practice_err(
                "INVALID_DRAWING",
                format!("{} 需要 {need} 个锚点", drawing.kind),
            ));
        }
        self.store()
            .get_simulation_run(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        let now = Utc::now().to_rfc3339();
        let existing = self
            .store()
            .list_training_drawings(run_id, true)
            .map_err(|e| map_storage_err("DRAWING_LOAD_FAILED", e))?
            .into_iter()
            .find(|d| d.drawing_id == drawing.drawing_id);
        let anchors: Vec<(i64, String)> = drawing
            .anchors
            .iter()
            .map(|(i, p)| (*i, to_canonical_text(*p)))
            .collect();
        let rec = TrainingDrawingRecord {
            drawing_id: if drawing.drawing_id.is_empty() {
                format!("DRW-{}", uuid::Uuid::new_v4().simple())
            } else {
                drawing.drawing_id.clone()
            },
            run_id: run_id.to_string(),
            kind: drawing.kind.clone(),
            anchors_json: serde_json::to_string(&anchors).unwrap_or_else(|_| "[]".to_string()),
            color: drawing.color.clone(),
            line_style: drawing.line_style.clone(),
            deleted: false,
            created_at: existing
                .map(|e| e.created_at)
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        self.store_mut()
            .upsert_training_drawing(&rec)
            .map_err(|e| map_storage_err("DRAWING_SAVE_FAILED", e))?;
        drawing_to_dto(&rec)
    }

    pub fn delete_training_drawing(
        &mut self,
        run_id: &str,
        drawing_id: &str,
    ) -> Result<(), ErrorEnvelope> {
        let mut rec = self
            .store()
            .list_training_drawings(run_id, true)
            .map_err(|e| map_storage_err("DRAWING_LOAD_FAILED", e))?
            .into_iter()
            .find(|d| d.drawing_id == drawing_id)
            .ok_or_else(|| {
                practice_err("DRAWING_NOT_FOUND", format!("画线 {drawing_id} 不存在"))
            })?;
        rec.deleted = true;
        rec.updated_at = Utc::now().to_rfc3339();
        self.store_mut()
            .upsert_training_drawing(&rec)
            .map_err(|e| map_storage_err("DRAWING_SAVE_FAILED", e))
    }

    /// "Clear drawings" (the UI asks for confirmation first).
    pub fn clear_training_drawings(&mut self, run_id: &str) -> Result<usize, ErrorEnvelope> {
        let all = self.list_training_drawings(run_id)?;
        for d in &all {
            self.delete_training_drawing(run_id, &d.drawing_id)?;
        }
        Ok(all.len())
    }
}

// ----------------------------------------------------------------------
// AppHost wrappers (UI and CLI share these through the same service)
// ----------------------------------------------------------------------

impl crate::host::AppHost {
    pub fn show_practice_account(&self, ccy: &str) -> Result<PracticeAccountDto, ErrorEnvelope> {
        let ccy = ccy.to_string();
        self.execute(move |svc| svc.show_practice_account(&ccy))
    }

    pub fn list_practice_accounts(&self) -> Result<Vec<PracticeAccountDto>, ErrorEnvelope> {
        self.execute(move |svc| svc.list_practice_accounts())
    }

    pub fn practice_account_history(
        &self,
        ccy: &str,
    ) -> Result<Vec<PracticeEntryDto>, ErrorEnvelope> {
        let ccy = ccy.to_string();
        self.execute(move |svc| svc.practice_account_history(&ccy))
    }

    pub fn transfer_practice_funds(
        &self,
        ccy: &str,
        kind: PracticeTransferKind,
        amount: Decimal,
    ) -> Result<PracticeAccountDto, ErrorEnvelope> {
        let ccy = ccy.to_string();
        self.execute(move |svc| svc.transfer_practice_funds(&ccy, kind, amount))
    }

    pub fn practice_account_analysis(
        &self,
        ccy: &str,
    ) -> Result<delta_core::AccountReport, ErrorEnvelope> {
        let ccy = ccy.to_string();
        self.execute(move |svc| svc.practice_account_analysis(&ccy))
    }

    pub fn training_history(&self) -> Result<Vec<TrainingHistoryRowDto>, ErrorEnvelope> {
        self.execute(move |svc| svc.training_history())
    }

    pub fn modify_training_order(
        &self,
        run_id: &str,
        req: ModifyTrainingOrderRequest,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.modify_training_order(&run_id, req))
    }

    pub fn undo_training_last(
        &self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.undo_training_last(&run_id))
    }

    pub fn abandon_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.abandon_training_run(&run_id))
    }

    pub fn restart_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.restart_training_run(&run_id))
    }

    pub fn get_training_aggregated_view(
        &self,
        run_id: &str,
        view: delta_core::Timeframe,
    ) -> Result<TrainingAggregatedViewDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.get_training_aggregated_view(&run_id, view))
    }

    pub fn list_training_drawings(
        &self,
        run_id: &str,
    ) -> Result<Vec<TrainingDrawingDto>, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.list_training_drawings(&run_id))
    }

    pub fn save_training_drawing(
        &self,
        run_id: &str,
        drawing: TrainingDrawingDto,
    ) -> Result<TrainingDrawingDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.save_training_drawing(&run_id, drawing))
    }

    pub fn delete_training_drawing(
        &self,
        run_id: &str,
        drawing_id: &str,
    ) -> Result<(), ErrorEnvelope> {
        let (run_id, drawing_id) = (run_id.to_string(), drawing_id.to_string());
        self.execute(move |svc| svc.delete_training_drawing(&run_id, &drawing_id))
    }

    pub fn clear_training_drawings(&self, run_id: &str) -> Result<usize, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.clear_training_drawings(&run_id))
    }

    pub fn training_data_status(
        &self,
        instrument_id: &str,
        check_pool: bool,
    ) -> Result<crate::service::TrainingDataStatusDto, ErrorEnvelope> {
        let id = instrument_id.to_string();
        self.execute(move |svc| svc.training_data_status(&id, check_pool))
    }

    pub fn start_prepare_training_data(
        &self,
        request: crate::service::PrepareTrainingDataRequest,
        timeout: std::time::Duration,
    ) -> Result<crate::service::JobId, ErrorEnvelope> {
        self.execute(move |svc| Ok(svc.start_prepare_training_data(request, timeout)))
    }
}

fn changed_orders(
    run_id: &str,
    before: &SimState,
    after: &SimState,
) -> Vec<delta_data::SimOrderRecord> {
    after
        .orders
        .iter()
        .filter(|(oid, o)| before.orders.get(*oid) != Some(*o))
        .map(|(_, o)| sim_order_to_record(run_id, o))
        .collect()
}

fn entries_to_dto(entries: &[PracticeAccountEntryRecord]) -> Vec<PracticeEntryDto> {
    let mut balance = Decimal::ZERO;
    entries
        .iter()
        .map(|e| {
            balance += e.amount;
            PracticeEntryDto {
                seq: e.seq,
                kind: e.kind,
                amount: e.amount,
                run_id: e.run_id.clone(),
                note: e.note.clone(),
                created_at: e.created_at.clone(),
                balance_after: balance,
            }
        })
        .collect()
}

fn drawing_to_dto(rec: &TrainingDrawingRecord) -> Result<TrainingDrawingDto, ErrorEnvelope> {
    let raw: Vec<(i64, String)> = serde_json::from_str(&rec.anchors_json)
        .map_err(|e| practice_err("DRAWING_CORRUPT", e.to_string()))?;
    let anchors = raw
        .into_iter()
        .map(|(i, p)| {
            delta_core::parse_decimal_lossless(&p)
                .map(|d| (i, d))
                .map_err(|e| practice_err("DRAWING_CORRUPT", e.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(TrainingDrawingDto {
        drawing_id: rec.drawing_id.clone(),
        kind: rec.kind.clone(),
        anchors,
        color: rec.color.clone(),
        line_style: rec.line_style.clone(),
    })
}

#[cfg(test)]
#[path = "training_m5_tests.rs"]
mod tests;
