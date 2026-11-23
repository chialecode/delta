//! Application host thread (`AppHost`), scoped query contract (`AnalysisScope`, `ResultEnvelope`,
//! `EvidenceRef`), capability catalog, and deterministic read/write dispatch (`M2-W01`, `S-13`, `S-14`, `M2-A-01`).

mod queries;
mod scope;

pub use scope::{
    AnalysisScope, CapabilityCatalog, CapabilityDescriptor, DataQualitySummaryDto, EvidenceRef,
    HoldingsAndLotsDto, OverviewMetricsDto, ResultEnvelope, SavedReportStatusDto, ScopeTarget,
    TransactionFilterDto, TransactionRowDto, AI_READONLY_TOOL_IDS, AI_TRAINING_READONLY_TOOL_IDS,
    M2_CALCULATION_VERSION,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use delta_core::{
    parse_decimal_lossless, AdjustmentKind, BarRecord, ManualValuation, PeriodBreakdownReport,
};
use delta_data::{
    AccountBalanceSnapshotDto, AccountRecord, AttachmentDto, ChartContextDto, CommitImportReceipt,
    ImportPreview, InstrumentSearchHit, LibraryManifest, NoteDto, PortfolioRecord,
    ReconciliationIssueRecord, StatementPreset, StructuredNoteFields, TradePlanDto,
    WatchlistGroupDto,
};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::kline::{KlineViewport, KlineWindowSliceDto};
use crate::paths::ResolvedAppPaths;
use crate::process::apply_process_quiet_guards;
use crate::service::{
    AppService, ErrorEnvelope, ExportManifestDto, JobController, JobId, JobSnapshot, SyncRequest,
    SyncSummaryDto,
};

const MAX_CACHED_JOB_POLLS: usize = 256;

// ============================================================================
// `AppHost` Single-Writer Owner Thread (`S-14`, `M2-K01`, `M2-A-01`)
// ============================================================================

type BoxedHostTask = Box<dyn FnOnce(&mut AppService, u64) + Send + 'static>;

enum HostMessage {
    Execute {
        generation: u64,
        task: BoxedHostTask,
    },
    Shutdown,
}

struct AppHostInner {
    sender: mpsc::Sender<HostMessage>,
    active_generation: Arc<AtomicU64>,
    last_job_polls: Mutex<HashMap<u64, (Instant, JobSnapshot)>>,
    join_handle: Mutex<Option<JoinHandle<()>>>,
    jobs: Arc<JobController>,
}

impl Drop for AppHostInner {
    fn drop(&mut self) {
        // Cancel and wait for background workers before tearing down the owner thread so
        // directory locks and SQLite handles close cleanly even when callers drop `AppHost`
        // without calling `shutdown()` explicitly (`APP-B19`).
        let _ = self.jobs.cancel_all_and_wait(Duration::from_secs(5));
        let _ = self.sender.send(HostMessage::Shutdown);
        let mut guard = self.join_handle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
}

/// Thread-safe handle to the single-writer `AppService` owner thread (`S-14`).
/// UI and background tasks send typed closures/requests over standard library channels
/// so the UI thread never blocks on SQLite or network I/O, and stale generations are
/// rejected before execution.
#[derive(Clone)]
pub struct AppHost {
    inner: Arc<AppHostInner>,
}

impl AppHost {
    pub fn spawn(
        paths: ResolvedAppPaths,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
    ) -> Result<Self, ErrorEnvelope> {
        let svc = AppService::open(paths, book_currency, report_currency, timezone, true)?;
        Ok(Self::from_service(svc))
    }

    pub fn from_service(mut service: AppService) -> Self {
        let jobs = service.job_controller();
        let _ = service.store_mut().mark_interrupted_runs_on_startup();
        let active_generation = Arc::new(AtomicU64::new(1));
        let active_gen_worker = Arc::clone(&active_generation);
        let (sender, receiver) = mpsc::channel::<HostMessage>();

        let handle = thread::Builder::new()
            .name("delta-app-host".to_string())
            .spawn(move || {
                apply_process_quiet_guards();
                while let Ok(msg) = receiver.recv() {
                    match msg {
                        HostMessage::Execute { generation, task } => {
                            let current = active_gen_worker.load(Ordering::SeqCst);
                            task(&mut service, current.max(generation));
                        }
                        HostMessage::Shutdown => break,
                    }
                }
            })
            .expect("failed to spawn delta-app-host thread");

        Self {
            inner: Arc::new(AppHostInner {
                sender,
                active_generation,
                last_job_polls: Mutex::new(HashMap::new()),
                join_handle: Mutex::new(Some(handle)),
                jobs,
            }),
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.inner.active_generation.load(Ordering::SeqCst)
    }

    pub fn advance_generation(&self) -> u64 {
        self.inner.active_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Waits for resource release on the caller's background worker before closing SQLite.
    pub fn shutdown(&self) -> Result<(), ErrorEnvelope> {
        self.inner
            .jobs
            .cancel_all_and_wait(Duration::from_secs(10))?;
        self.advance_generation();
        self.inner
            .sender
            .send(HostMessage::Shutdown)
            .map_err(|_| ErrorEnvelope::new("HOST_CLOSED", "宿主已关闭", false))?;
        if let Some(handle) = self
            .inner
            .join_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle
                .join()
                .map_err(|_| ErrorEnvelope::new("HOST_SHUTDOWN_FAILED", "宿主关闭失败", false))?;
        }
        Ok(())
    }

    pub fn execute_with_gen<F, R>(&self, generation: u64, f: F) -> Result<R, ErrorEnvelope>
    where
        F: FnOnce(&mut AppService) -> Result<R, ErrorEnvelope> + Send + 'static,
        R: Send + 'static,
    {
        if generation != self.current_generation() {
            return Err(ErrorEnvelope::new(
                "STALE_GENERATION",
                "stale request generation rejected",
                false,
            ));
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<R, ErrorEnvelope>>(1);
        let active_ref = Arc::clone(&self.inner.active_generation);
        let task: BoxedHostTask = Box::new(move |svc: &mut AppService, _gen_now: u64| {
            let active_now = active_ref.load(Ordering::SeqCst);
            let res = if generation != active_now {
                Err(ErrorEnvelope::new(
                    "STALE_GENERATION",
                    "stale request generation rejected",
                    false,
                ))
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(svc))).unwrap_or_else(
                    |_| {
                        Err(ErrorEnvelope::new(
                            "HOST_TASK_FAILED",
                            "application task failed",
                            false,
                        ))
                    },
                )
            };
            let _ = reply_tx.send(res);
        });

        self.inner
            .sender
            .send(HostMessage::Execute { generation, task })
            .map_err(|_| {
                ErrorEnvelope::new("HOST_CLOSED", "AppHost owner thread has shut down", false)
            })?;

        reply_rx
            .recv_timeout(Duration::from_secs(180))
            .map_err(|err| match err {
                mpsc::RecvTimeoutError::Timeout => {
                    ErrorEnvelope::new("HOST_TIMEOUT", "AppHost task timed out", true)
                }
                mpsc::RecvTimeoutError::Disconnected => ErrorEnvelope::new(
                    "HOST_DISCONNECTED",
                    "AppHost owner thread dropped reply channel",
                    false,
                ),
            })?
    }

    pub fn execute<F, R>(&self, f: F) -> Result<R, ErrorEnvelope>
    where
        F: FnOnce(&mut AppService) -> Result<R, ErrorEnvelope> + Send + 'static,
        R: Send + 'static,
    {
        self.execute_with_gen(self.current_generation(), f)
    }

    fn cache_job_poll(&self, job_id: JobId, snap: JobSnapshot) {
        let mut guard = self
            .inner
            .last_job_polls
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(job_id.0, (Instant::now(), snap));
        if guard.len() > MAX_CACHED_JOB_POLLS {
            guard.retain(|&id, (at, cached)| {
                id == job_id.0
                    || (!cached.worker_finished && at.elapsed() < Duration::from_secs(60))
            });
            if guard.len() > MAX_CACHED_JOB_POLLS {
                let oldest_id = guard
                    .iter()
                    .filter(|(&id, _)| id != job_id.0)
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(&id, _)| id);
                if let Some(id) = oldest_id {
                    guard.remove(&id);
                }
            }
        }
    }

    // ------------------------------------------------------------------------
    // Typed Scoped Queries (`S-13`, `S-14`, `M2-A-01`)
    // ------------------------------------------------------------------------

    pub fn query_scope_describe(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Value>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_scope_describe(&scope, &req, generation)
        })
    }

    pub fn query_overview(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<OverviewMetricsDto>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_overview(&scope, &req, generation)
        })
    }

    pub fn query_period_breakdown(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<PeriodBreakdownReport>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_period_breakdown(&scope, &req, generation)
        })
    }

    pub fn query_transactions(
        &self,
        scope: AnalysisScope,
        filter: TransactionFilterDto,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<TransactionRowDto>>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_transactions(&scope, &filter, &req, generation)
        })
    }

    pub fn query_holdings_lots(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<HoldingsAndLotsDto>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_holdings_lots(&scope, &req, generation)
        })
    }

    pub fn get_scoped_transaction(
        &self,
        scope: AnalysisScope,
        id: &str,
        generation: u64,
    ) -> Result<TransactionRowDto, ErrorEnvelope> {
        let id = id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.get_scoped_transaction(&scope, &id, generation)
        })
    }

    pub fn query_reconciliation_issues(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<ReconciliationIssueRecord>>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_reconciliation_issues(&scope, &req, generation)
        })
    }

    pub fn query_data_quality(
        &self,
        scope: AnalysisScope,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<DataQualitySummaryDto>, ErrorEnvelope> {
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_data_quality(&scope, &req, generation)
        })
    }

    pub fn query_notes(
        &self,
        scope: AnalysisScope,
        query: &str,
        only_model_accessible: bool,
        req_id: &str,
        generation: u64,
    ) -> Result<ResultEnvelope<Vec<NoteDto>>, ErrorEnvelope> {
        let q = query.to_string();
        let req = req_id.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.query_notes(&scope, &q, only_model_accessible, &req, generation)
        })
    }

    // ------------------------------------------------------------------------
    // Workbench Action Helpers (Accounts, Portfolios, Imports, Sync, Notes, Ops)
    // ------------------------------------------------------------------------

    pub fn upsert_account(
        &self,
        id: &str,
        name: &str,
        account_type: &str,
        provider: &str,
        external_ref: Option<&str>,
        status: &str,
    ) -> Result<AccountRecord, ErrorEnvelope> {
        let id = id.to_string();
        let name = name.to_string();
        let account_type = account_type.to_string();
        let provider = provider.to_string();
        let external_ref = external_ref.map(str::to_string);
        let status = status.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_account(
                    &id,
                    &name,
                    &account_type,
                    &provider,
                    external_ref.as_deref(),
                    &status,
                )
                .map_err(|e| ErrorEnvelope::new("ACCOUNT_UPSERT_FAILED", e.to_string(), false))
        })
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>, ErrorEnvelope> {
        self.execute(|svc| {
            svc.store()
                .list_accounts()
                .map_err(|e| ErrorEnvelope::new("ACCOUNT_LIST_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_portfolio(
        &self,
        id: &str,
        name: &str,
        member_accounts: Vec<String>,
        child_portfolios: Vec<String>,
    ) -> Result<PortfolioRecord, ErrorEnvelope> {
        let id = id.to_string();
        let name = name.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_portfolio(&id, &name, &member_accounts, &child_portfolios)
                .map_err(|e| ErrorEnvelope::new("PORTFOLIO_UPSERT_FAILED", e.to_string(), false))
        })
    }

    pub fn list_portfolios(&self) -> Result<Vec<PortfolioRecord>, ErrorEnvelope> {
        self.execute(|svc| {
            svc.store()
                .list_portfolios()
                .map_err(|e| ErrorEnvelope::new("PORTFOLIO_LIST_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_manual_valuation(&self, mv: ManualValuation) -> Result<(), ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_manual_valuation(&mv)
                .map_err(|e| ErrorEnvelope::new("MANUAL_VALUATION_FAILED", e.to_string(), false))
        })
    }

    pub fn list_manual_valuations(
        &self,
        account_scope: Option<Vec<String>>,
    ) -> Result<Vec<ManualValuation>, ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store()
                .list_manual_valuations(account_scope.as_deref())
                .map_err(|e| ErrorEnvelope::new("MANUAL_VALUATION_FAILED", e.to_string(), false))
        })
    }

    pub fn get_saved_report_status(
        &self,
        report_id: &str,
    ) -> Result<SavedReportStatusDto, ErrorEnvelope> {
        let id = report_id.to_string();
        self.execute(move |svc| svc.get_saved_report_status(&id))
    }

    pub fn import_statement(
        &self,
        account_id: &str,
        preset: StatementPreset,
        file_path: PathBuf,
        commit: bool,
    ) -> Result<(ImportPreview, Option<CommitImportReceipt>), ErrorEnvelope> {
        let acc = account_id.to_string();
        self.execute(move |svc| svc.import_statement(&acc, preset, &file_path, commit))
    }

    pub fn resolve_import_row(&self, row_id: &str, include: bool) -> Result<i64, ErrorEnvelope> {
        let rid = row_id.to_string();
        self.execute(move |svc| svc.resolve_import_row(&rid, include))
    }

    pub fn list_held_import_rows(&self) -> Result<Vec<String>, ErrorEnvelope> {
        self.execute(|svc| {
            let mut statement = svc
                .store()
                .conn()
                .prepare(
                    "SELECT id FROM import_row WHERE validation_state = 'suspected_duplicate' ORDER BY id LIMIT 1000",
                )
                .map_err(|error| ErrorEnvelope::new("HELD_QUERY_FAILED", error.to_string(), false))?;
            let rows = statement
                .query_map([], |row| row.get(0))
                .map_err(|error| ErrorEnvelope::new("HELD_QUERY_FAILED", error.to_string(), false))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    ErrorEnvelope::new("HELD_QUERY_FAILED", error.to_string(), false)
                });
            rows
        })
    }

    pub fn amend_event(
        &self,
        original_event_id: &str,
        new_quantity: Decimal,
        new_price: Option<Decimal>,
        new_fee: Decimal,
    ) -> Result<i64, ErrorEnvelope> {
        let id = original_event_id.to_string();
        self.execute(move |svc| svc.amend_event(&id, new_quantity, new_price, new_fee))
    }

    pub fn reconcile_account(
        &self,
        account_id: &str,
        snapshots: Vec<AccountBalanceSnapshotDto>,
    ) -> Result<Vec<ReconciliationIssueRecord>, ErrorEnvelope> {
        let acc = account_id.to_string();
        self.execute(move |svc| svc.reconcile_account(&acc, &snapshots))
    }

    pub fn start_market_sync(
        &self,
        request: SyncRequest,
        timeout: Duration,
    ) -> Result<JobId, ErrorEnvelope> {
        self.execute(move |svc| Ok(svc.start_market_sync(request, timeout)))
    }

    /// Poll a background job with `<= 10 Hz` (100 ms) progress throttling (`S-14`).
    pub fn poll_job_throttled(&self, job_id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        {
            let guard = self
                .inner
                .last_job_polls
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((last_instant, cached_snap)) = guard.get(&job_id.0) {
                if last_instant.elapsed() < Duration::from_millis(100) {
                    return Ok(cached_snap.clone());
                }
            }
        }
        let snap = self.inner.jobs.snapshot(job_id)?;
        self.cache_job_poll(job_id, snap.clone());
        Ok(snap)
    }

    pub fn cancel_job(&self, job_id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        let snap = self.inner.jobs.cancel_job(job_id)?;
        self.cache_job_poll(job_id, snap.clone());
        Ok(snap)
    }

    pub fn wait_job(
        &self,
        job_id: JobId,
        max_wait: Duration,
    ) -> Result<JobSnapshot, ErrorEnvelope> {
        self.inner.jobs.wait_job(job_id, max_wait)
    }

    pub fn wait_job_finished(
        &self,
        job_id: JobId,
        timeout: Duration,
    ) -> Result<JobSnapshot, ErrorEnvelope> {
        self.inner.jobs.wait_finished(job_id, timeout)
    }

    pub fn search_instruments(
        &self,
        query: &str,
    ) -> Result<Vec<InstrumentSearchHit>, ErrorEnvelope> {
        let q = query.to_string();
        self.execute(move |svc| {
            svc.store()
                .search_instruments(&q)
                .map_err(|e| ErrorEnvelope::new("SEARCH_INSTRUMENTS_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_watchlist_group(
        &self,
        id: &str,
        name: &str,
        sort_order: i64,
    ) -> Result<(), ErrorEnvelope> {
        let id = id.to_string();
        let name = name.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_watchlist_group(&id, &name, sort_order)
                .map_err(|e| ErrorEnvelope::new("WATCHLIST_FAILED", e.to_string(), false))
        })
    }

    pub fn add_watchlist_item(
        &self,
        group_id: &str,
        instrument_id: &str,
    ) -> Result<(), ErrorEnvelope> {
        let gid = group_id.to_string();
        let iid = instrument_id.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .add_watchlist_item(&gid, &iid)
                .map_err(|e| ErrorEnvelope::new("WATCHLIST_FAILED", e.to_string(), false))
        })
    }

    pub fn list_watchlist_groups(&self) -> Result<Vec<WatchlistGroupDto>, ErrorEnvelope> {
        self.execute(|svc| {
            svc.store()
                .list_watchlist_groups()
                .map_err(|e| ErrorEnvelope::new("WATCHLIST_FAILED", e.to_string(), false))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_kline_window_slice(
        &self,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        dataset_version: i64,
        visible_until: Option<DateTime<Utc>>,
        viewport: KlineViewport,
        max_pixel_columns: usize,
        generation: u64,
    ) -> Result<KlineWindowSliceDto, ErrorEnvelope> {
        let iid = instrument_id.to_string();
        let tf = timeframe.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.load_kline_window_slice(
                &iid,
                &tf,
                adjustment,
                dataset_version,
                visible_until,
                &viewport,
                max_pixel_columns,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn load_scoped_kline_window_slice(
        &self,
        scope: AnalysisScope,
        instrument_id: &str,
        timeframe: &str,
        adjustment: AdjustmentKind,
        viewport: KlineViewport,
        max_pixel_columns: usize,
        generation: u64,
    ) -> Result<KlineWindowSliceDto, ErrorEnvelope> {
        let instrument = instrument_id.to_string();
        let timeframe = timeframe.to_string();
        self.execute_with_gen(generation, move |svc| {
            svc.load_scoped_kline_window_slice(
                &scope,
                &instrument,
                &timeframe,
                adjustment,
                &viewport,
                max_pixel_columns,
            )
        })
    }

    pub fn save_chart_context(&self, ctx: ChartContextDto) -> Result<String, ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store_mut()
                .save_chart_context(&ctx)
                .map_err(|e| ErrorEnvelope::new("CHART_CONTEXT_FAILED", e.to_string(), false))
        })
    }

    pub fn get_chart_context(&self, id: &str) -> Result<ChartContextDto, ErrorEnvelope> {
        let id = id.to_string();
        self.execute(move |svc| {
            svc.store()
                .get_chart_context(&id)
                .map_err(|e| ErrorEnvelope::new("CHART_CONTEXT_FAILED", e.to_string(), false))
        })
    }

    pub fn upsert_trade_plan(&self, plan: TradePlanDto) -> Result<(), ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store_mut()
                .upsert_trade_plan(&plan)
                .map_err(|e| ErrorEnvelope::new("TRADE_PLAN_FAILED", e.to_string(), false))
        })
    }

    pub fn link_plan_or_note_fill(
        &self,
        plan_id: Option<&str>,
        note_id: Option<&str>,
        event_id: &str,
        allocated_quantity: Decimal,
        relation: &str,
    ) -> Result<(), ErrorEnvelope> {
        let pid = plan_id.map(str::to_string);
        let nid = note_id.map(str::to_string);
        let eid = event_id.to_string();
        let rel = relation.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .link_plan_or_note_fill(
                    pid.as_deref(),
                    nid.as_deref(),
                    &eid,
                    allocated_quantity,
                    &rel,
                )
                .map_err(|e| ErrorEnvelope::new("LINK_FILL_FAILED", e.to_string(), false))
        })
    }

    pub fn add_note_link(
        &self,
        note_id: &str,
        target_type: &str,
        target_id: &str,
        target_revision: i64,
        allocated_quantity: Option<Decimal>,
        relation: &str,
    ) -> Result<String, ErrorEnvelope> {
        let nid = note_id.to_string();
        let ttype = target_type.to_string();
        let tid = target_id.to_string();
        let rel = relation.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .add_note_link(
                    &nid,
                    &ttype,
                    &tid,
                    target_revision,
                    allocated_quantity,
                    &rel,
                )
                .map_err(|e| ErrorEnvelope::new("NOTE_LINK_FAILED", e.to_string(), false))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_note_revision(
        &self,
        note_id: &str,
        title: &str,
        template_kind: &str,
        allow_model_access: bool,
        body_markdown: &str,
        structured: StructuredNoteFields,
        tags: Vec<String>,
    ) -> Result<i64, ErrorEnvelope> {
        let nid = note_id.to_string();
        let title = title.to_string();
        let tk = template_kind.to_string();
        let body = body_markdown.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .save_note_revision(
                    &nid,
                    &title,
                    &tk,
                    allow_model_access,
                    &body,
                    &structured,
                    &tags,
                )
                .map_err(|e| ErrorEnvelope::new("NOTE_SAVE_FAILED", e.to_string(), true))
        })
    }

    pub fn get_note(&self, note_id: &str) -> Result<NoteDto, ErrorEnvelope> {
        let nid = note_id.to_string();
        self.execute(move |svc| {
            svc.store()
                .get_note(&nid)
                .map_err(|e| ErrorEnvelope::new("NOTE_NOT_FOUND", e.to_string(), false))
        })
    }

    pub fn get_scoped_note(
        &self,
        scope: AnalysisScope,
        note_id: &str,
        generation: u64,
    ) -> Result<NoteDto, ErrorEnvelope> {
        let note_id = note_id.to_string();
        self.execute_with_gen(generation, move |svc| svc.get_scoped_note(&scope, &note_id))
    }

    pub fn list_notes(&self, only_model_accessible: bool) -> Result<Vec<NoteDto>, ErrorEnvelope> {
        self.execute(move |svc| {
            svc.store()
                .list_notes(only_model_accessible)
                .map_err(|e| ErrorEnvelope::new("NOTE_LIST_FAILED", e.to_string(), false))
        })
    }

    pub fn add_note_attachment(
        &self,
        note_id: &str,
        file_name: &str,
        mime_type: &str,
        bytes: Vec<u8>,
    ) -> Result<AttachmentDto, ErrorEnvelope> {
        let nid = note_id.to_string();
        let fname = file_name.to_string();
        let mime = mime_type.to_string();
        self.execute(move |svc| {
            svc.store_mut()
                .add_note_attachment(&nid, &fname, &mime, &bytes)
                .map_err(|e| ErrorEnvelope::new("ATTACHMENT_FAILED", e.to_string(), false))
        })
    }

    pub fn create_backup(&self, backup_dir: PathBuf) -> Result<LibraryManifest, ErrorEnvelope> {
        self.execute(move |svc| svc.create_backup(&backup_dir))
    }

    pub fn export_library(&self, export_dir: PathBuf) -> Result<ExportManifestDto, ErrorEnvelope> {
        self.execute(move |svc| svc.export_library(&export_dir))
    }

    pub fn sync_from_fixtures(
        &self,
        fixtures_root: PathBuf,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
        self.execute(move |svc| svc.sync_from_fixtures(&fixtures_root))
    }
}

/// Seed the canonical AC-02 minimal ledger + market data fixture into `AppService`:
/// - Account `ACC-1`: opening USD `2000`, buy `10` `NASDAQ:AAPL` @ `100` (fee `1`),
///   sell `4` `NASDAQ:AAPL` @ `110` (fee `1`), ending `2026-01-15` close `105`.
/// - Produces cash `1438`, holdings market value `630`, NAV `2068`, realized PnL `38.6`,
///   unrealized PnL `29.4`, total PnL `68`.
pub fn seed_ac02_fixture_in_service(svc: &mut AppService) -> Result<i64, ErrorEnvelope> {
    svc.store_mut()
        .upsert_account(
            "ACC-1",
            "Primary US Equity",
            "broker",
            "futu",
            Some("88881234"),
            "active",
        )
        .map_err(|e| ErrorEnvelope::new("SEED_ACCOUNT_FAILED", e.to_string(), false))?;

    let csv_path = svc.paths().data_dir.join("imports").join("ac02_seed.csv");
    let csv_body = concat!(
        "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n",
        "ACC-1,opening_balance,2026-01-02 14:30:00,UTC,USD,buy,2000,1,USD,0,USD,AC02-OPEN\n",
        "ACC-1,buy,2026-01-05 15:00:00,UTC,NASDAQ:AAPL,buy,10,100,USD,1,USD,AC02-BUY\n",
        "ACC-1,sell,2026-01-10 15:30:00,UTC,NASDAQ:AAPL,sell,4,110,USD,1,USD,AC02-SELL\n",
    );
    std::fs::write(&csv_path, csv_body)
        .map_err(|e| ErrorEnvelope::new("SEED_CSV_WRITE", e.to_string(), false))?;

    let dsv = svc
        .store_mut()
        .create_dataset_version(
            "ac02-fixture",
            "public_open",
            r#"{"equities":["NASDAQ:AAPL"]}"#,
            "exact",
            "exact",
        )
        .map_err(|e| ErrorEnvelope::new("SEED_DSV_FAILED", e.to_string(), false))?;
    let v = dsv.version_num;

    let d_buy = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
    let d_sell = NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
    let d_asof = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

    let mut bars = Vec::new();
    for (d, close_str) in [(d_buy, "100"), (d_sell, "110"), (d_asof, "105")] {
        let close = parse_decimal_lossless(close_str).unwrap();
        bars.push(BarRecord {
            id: format!("BAR:AC02:{d}"),
            instrument_id: "NASDAQ:AAPL".to_string(),
            timeframe: "1d".to_string(),
            session: "regular".to_string(),
            session_date: d,
            open_at: Utc.from_utc_datetime(&d.and_hms_opt(14, 30, 0).unwrap()),
            close_at: Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).unwrap()),
            open: close,
            high: close + Decimal::ONE,
            low: close - Decimal::ONE,
            close,
            volume: Decimal::from(10_000),
            source: "ac02-fixture".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: v,
            valid_to_version: None,
            raw_payload_hash: None,
        });
    }

    svc.store_mut()
        .commit_bars_and_checkpoint(&bars, v, None)
        .map_err(|e| ErrorEnvelope::new("SEED_BARS_FAILED", e.to_string(), false))?;
    svc.store_mut()
        .publish_dataset_version(v)
        .map_err(|e| ErrorEnvelope::new("SEED_PUBLISH_FAILED", e.to_string(), false))?;

    svc.import_statement("ACC-1", StatementPreset::GenericCsv, &csv_path, true)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::AnalysisToolGateway;
    use crate::paths::{resolve_app_paths, BuildChannel};
    use delta_core::{EconomicEventType, FxRateRecord, QualityStatus};
    use serde_json::json;

    #[test]
    fn test_m2_a01_scoped_query_consistency_and_stale_generation_rejection() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_m2_a01_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);

        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let mut scope = AnalysisScope::for_accounts(vec!["ACC-1".to_string()], as_of, "USD");
        scope.dataset_version = dsv;
        scope.ledger_revision = 2;

        let gen1 = host.current_generation();
        // 1. Run the same scoped query twice through AppHost
        let env1 = host
            .query_period_breakdown(scope.clone(), "req-a01-same", gen1)
            .expect("first AppHost query");
        let env2 = host
            .query_period_breakdown(scope.clone(), "req-a01-same", gen1)
            .expect("second AppHost query");

        // 2. And once through the AI tool gateway
        let env3: ResultEnvelope<PeriodBreakdownReport> = host
            .execute_with_gen(gen1, {
                let s = scope.clone();
                move |svc| {
                    let mut gw = AnalysisToolGateway::new("run-a01-test", gen1, s.clone())?;
                    let receipt = gw.execute_tool(
                        svc,
                        "call-a01-1",
                        "report.period_breakdown",
                        &json!({}),
                        "req-a01-same",
                    )?;
                    serde_json::from_value(receipt.envelope_json)
                        .map_err(|e| ErrorEnvelope::new("DESERIALIZE_FAILED", e.to_string(), false))
                }
            })
            .expect("third query via AnalysisToolGateway");

        // Field-for-field identical check (values, quality, evidence_refs, versions, result_id)
        assert_eq!(env1, env2);
        assert_eq!(env1, env3);
        assert_eq!(
            env1.value.ending_net_worth,
            Some(parse_decimal_lossless("2068").unwrap())
        );
        assert_eq!(
            env1.value.investment_pnl,
            Some(parse_decimal_lossless("68").unwrap())
        );
        assert_eq!(
            env1.value.realized_pnl,
            Some(parse_decimal_lossless("38.6").unwrap())
        );
        assert_eq!(
            env1.value.unrealized_pnl,
            Some(parse_decimal_lossless("29.4").unwrap())
        );
        assert!(!env1.evidence_refs.is_empty());

        // Also check OverviewMetricsDto matches AC-02 (cash 1438, NAV 2068, total PnL 68)
        let ov = host
            .query_overview(scope.clone(), "req-a01-ov", gen1)
            .expect("overview query");
        assert_eq!(ov.value.cash_value, parse_decimal_lossless("1438").unwrap());
        assert_eq!(ov.value.net_worth, parse_decimal_lossless("2068").unwrap());
        assert_eq!(
            ov.value.total_pnl,
            Some(parse_decimal_lossless("68").unwrap())
        );

        // 3. Unknown scope rejection
        let bad_scope =
            AnalysisScope::for_accounts(vec!["ACC-NONEXISTENT-999".to_string()], as_of, "USD");
        let err_unknown = host
            .query_period_breakdown(bad_scope, "req-bad-scope", gen1)
            .expect_err("unknown account scope must be rejected");
        assert_eq!(err_unknown.code, "SCOPE_DENIED");

        // 4. Unpublished dataset_version rejection
        let mut unpub_scope = scope.clone();
        unpub_scope.dataset_version = 9999;
        let err_unpub = host
            .query_period_breakdown(unpub_scope, "req-unpub", gen1)
            .expect_err("unpublished dataset version must be rejected");
        assert_eq!(err_unpub.code, "UNPUBLISHED_DATASET");

        // 5. Stale generation rejection
        let gen2 = host.advance_generation();
        assert_eq!(gen2, 2);
        let err_stale = host
            .query_period_breakdown(scope, "req-stale-gen", gen1)
            .expect_err("stale generation must be rejected");
        assert_eq!(err_stale.code, "STALE_GENERATION");

        drop(host);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_m2_a04_accounts_portfolios_and_scoped_period_pnl_breakdown() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_m2_a04_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();

        svc.store_mut()
            .upsert_account("ACC-A", "Account A", "broker", "futu", None, "active")
            .unwrap();
        svc.store_mut()
            .upsert_account("ACC-B", "Account B", "broker", "ibkr", None, "active")
            .unwrap();

        // Overlapping portfolio hierarchy: PF-CHILD1(ACC-A), PF-CHILD2(ACC-A, ACC-B), PF-ROOT(children: PF-CHILD1, PF-CHILD2)
        svc.store_mut()
            .upsert_portfolio("PF-CHILD1", "Child 1", &["ACC-A".to_string()], &[])
            .unwrap();
        svc.store_mut()
            .upsert_portfolio(
                "PF-CHILD2",
                "Child 2",
                &["ACC-A".to_string(), "ACC-B".to_string()],
                &[],
            )
            .unwrap();
        svc.store_mut()
            .upsert_portfolio(
                "PF-ROOT",
                "Root Combined",
                &["ACC-A".to_string()],
                &["PF-CHILD1".to_string(), "PF-CHILD2".to_string()],
            )
            .unwrap();

        // Import events:
        // Day 1 (2026-01-01): ACC-A opening 1002 USD
        // Day 2 (2026-01-02): Internal transfer 1000 USD from ACC-A to ACC-B with fee 2 USD on ACC-A (AC-04)
        // Day 3 (2026-01-03): External deposit 500 USD into ACC-B with unchanged market (AC-07 inflow)
        let csv_a = svc.paths().data_dir.join("imports").join("a04_acca.csv");
        let csv_a_body = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id,counterparty_account\n",
            "ACC-A,opening_balance,2026-01-01 10:00:00,UTC,USD,buy,1002,1,USD,0,USD,A04-OPEN,\n",
            "ACC-A,transfer_out,2026-01-02 10:00:00,UTC,USD,sell,1000,1,USD,2,USD,TG-A04,ACC-B\n",
        );
        std::fs::write(&csv_a, csv_a_body).unwrap();
        svc.import_statement("ACC-A", StatementPreset::GenericCsv, &csv_a, true)
            .unwrap();

        let csv_b = svc.paths().data_dir.join("imports").join("a04_accb.csv");
        let csv_b_body = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id,counterparty_account\n",
            "ACC-B,transfer_in,2026-01-02 10:05:00,UTC,USD,buy,1000,1,USD,0,USD,TG-A04,ACC-A\n",
            "ACC-B,deposit,2026-01-03 10:00:00,UTC,USD,buy,500,1,USD,0,USD,A04-DEP,\n",
        );
        std::fs::write(&csv_b, csv_b_body).unwrap();
        svc.import_statement("ACC-B", StatementPreset::GenericCsv, &csv_b, true)
            .unwrap();

        let host = AppHost::from_service(svc);
        let gen = host.current_generation();
        let d_start = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let d_end = NaiveDate::from_ymd_opt(2026, 1, 3).unwrap();

        let mut pf_scope = AnalysisScope::for_portfolio("PF-ROOT", d_end, "USD");
        pf_scope.start_date = Some(d_start);
        let pf_bd = host
            .query_period_breakdown(pf_scope, "req-a04-pf", gen)
            .unwrap();
        assert_eq!(
            pf_bd.scope.resolved_accounts,
            vec!["ACC-A".to_string(), "ACC-B".to_string()]
        );
        assert_eq!(
            pf_bd.value.opening_net_worth,
            Some(parse_decimal_lossless("1002").unwrap())
        );
        assert_eq!(
            pf_bd.value.ending_net_worth,
            Some(parse_decimal_lossless("1500").unwrap())
        );
        assert_eq!(
            pf_bd.value.net_external_cash_flow,
            Some(parse_decimal_lossless("500").unwrap())
        );
        assert_eq!(
            pf_bd.value.investment_pnl,
            Some(parse_decimal_lossless("-2").unwrap())
        );
        assert_eq!(
            pf_bd.value.explanatory_total_fees,
            parse_decimal_lossless("2").unwrap()
        );
        assert!(!pf_bd.value.drawdown_status.is_empty());

        let mut acc_b_scope = AnalysisScope::for_accounts(vec!["ACC-B".to_string()], d_end, "USD");
        acc_b_scope.start_date = Some(NaiveDate::from_ymd_opt(2026, 1, 3).unwrap());
        let acc_b_bd = host
            .query_period_breakdown(acc_b_scope, "req-a04-accb", gen)
            .unwrap();
        assert_eq!(
            acc_b_bd.value.opening_net_worth,
            Some(parse_decimal_lossless("1000").unwrap())
        );
        assert_eq!(
            acc_b_bd.value.ending_net_worth,
            Some(parse_decimal_lossless("1500").unwrap())
        );
        assert_eq!(
            acc_b_bd.value.net_external_cash_flow,
            Some(parse_decimal_lossless("500").unwrap())
        );
        assert_eq!(acc_b_bd.value.investment_pnl, Some(Decimal::ZERO));

        drop(host);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_m2_a05_missing_price_fx_cost_manual_valuations_and_stale_reports() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_m2_a05_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();

        let csv_path = svc.paths().data_dir.join("imports").join("a05_unknown.csv");
        let csv_body = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,cost_known,source_id\n",
            "ACC-1,opening_balance,2026-01-12 10:00:00,UTC,NYSE:UNPRICED,buy,5,,USD,0,USD,false,A05-UNK-OPEN\n",
        );
        std::fs::write(&csv_path, csv_body).unwrap();
        svc.import_statement("ACC-1", StatementPreset::GenericCsv, &csv_path, true)
            .unwrap();

        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        svc.store_mut()
            .upsert_manual_valuation(&ManualValuation {
                id: "MV-A05-1".to_string(),
                account_id: "ACC-1".to_string(),
                asset_or_liability_id: "PRIVATE:FUND-A".to_string(),
                is_liability: false,
                value: parse_decimal_lossless("500").unwrap(),
                currency: "USD".to_string(),
                as_of,
                method: "quarterly_appraisal".to_string(),
            })
            .unwrap();

        let host = AppHost::from_service(svc);
        let gen = host.current_generation();
        let mut scope = AnalysisScope::for_accounts(vec!["ACC-1".to_string()], as_of, "USD");
        scope.dataset_version = dsv;

        let ov = host
            .query_overview(scope.clone(), "req-a05-ov", gen)
            .unwrap();
        assert_eq!(ov.quality, QualityStatus::Partial);
        assert_eq!(ov.value.total_pnl, None);
        assert_eq!(ov.value.unrealized_pnl, None);
        assert_eq!(
            ov.value.known_partial_net_worth,
            parse_decimal_lossless("2568").unwrap()
        );
        assert!(!ov.value.excluded_items.is_empty());

        let bd = host
            .query_period_breakdown(scope.clone(), "req-a05-bd", gen)
            .unwrap();
        assert_eq!(bd.quality, QualityStatus::Partial);
        assert_eq!(bd.value.investment_pnl, None);
        assert_eq!(bd.value.unrealized_pnl, None);
        assert_eq!(bd.value.manual_valuations.len(), 1);
        assert_eq!(
            bd.value.manual_valuations[0].value,
            parse_decimal_lossless("500").unwrap()
        );
        assert_eq!(bd.value.manual_valuations[0].method, "quarterly_appraisal");
        assert_eq!(bd.value.manual_valuations[0].as_of, as_of);

        let saved_report_id = ov.value.report_id.clone();
        let status_before = host.get_saved_report_status(&saved_report_id).unwrap();
        assert!(!status_before.is_stale);

        let sell_event_id = host
            .execute(|svc| {
                let events = svc.load_all_ledger_events()?;
                Ok(events
                    .into_iter()
                    .find(|e| e.event_type == EconomicEventType::Sell)
                    .unwrap()
                    .event_id)
            })
            .unwrap();
        host.amend_event(
            &sell_event_id,
            parse_decimal_lossless("3").unwrap(),
            Some(parse_decimal_lossless("110").unwrap()),
            Decimal::ONE,
        )
        .unwrap();

        let status_after = host.get_saved_report_status(&saved_report_id).unwrap();
        assert!(status_after.is_stale);
        assert!(status_after.recompute_available);

        drop(host);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn query_notes_isolates_economic_event_links_and_dangling_chart_contexts() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_note_iso_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();
        svc.store_mut()
            .upsert_account(
                "ACC-OTHER",
                "Other Account",
                "broker",
                "ibkr",
                None,
                "active",
            )
            .unwrap();
        let other_csv = svc.paths().data_dir.join("imports").join("other_acc.csv");
        std::fs::write(
            &other_csv,
            concat!(
                "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n",
                "ACC-OTHER,opening_balance,2026-01-03 10:00:00,UTC,USD,buy,500,1,USD,0,USD,OTHER-OPEN\n",
            ),
        )
        .unwrap();
        svc.import_statement("ACC-OTHER", StatementPreset::GenericCsv, &other_csv, true)
            .unwrap();
        let events = svc.load_all_ledger_events().unwrap();
        let acc1_event = events
            .iter()
            .find(|e| e.account_id == "ACC-1")
            .unwrap()
            .event_id
            .clone();
        let other_event = events
            .iter()
            .find(|e| e.account_id == "ACC-OTHER")
            .unwrap()
            .event_id
            .clone();

        // Note linked via `economic_event` to ACC-1 (authorized) AND ACC-OTHER (unauthorized).
        svc.store_mut()
            .save_note_revision(
                "NOTE-CROSS",
                "Cross-account economic_event note",
                "post_trade",
                true,
                "secret cross-account analysis",
                &StructuredNoteFields::default(),
                &[],
            )
            .unwrap();
        svc.store_mut()
            .add_note_link("NOTE-CROSS", "economic_event", &acc1_event, 1, None, "ref")
            .unwrap();
        svc.store_mut()
            .add_note_link("NOTE-CROSS", "economic_event", &other_event, 1, None, "ref")
            .unwrap();

        // Note linked ONLY to ACC-1 via `economic_event` -> must be visible in ACC-1 scope.
        svc.store_mut()
            .save_note_revision(
                "NOTE-OK",
                "In-scope economic_event note",
                "post_trade",
                true,
                "authorized analysis",
                &StructuredNoteFields::default(),
                &[],
            )
            .unwrap();
        svc.store_mut()
            .add_note_link("NOTE-OK", "economic_event", &acc1_event, 1, None, "ref")
            .unwrap();

        // Note with a dangling `chart_context` link must fail closed without aborting search.
        svc.store_mut()
            .save_note_revision(
                "NOTE-DANGLING-CTX",
                "Dangling context note",
                "post_trade",
                true,
                "should be excluded cleanly",
                &StructuredNoteFields::default(),
                &[],
            )
            .unwrap();
        svc.store_mut()
            .add_note_link("NOTE-DANGLING-CTX", "account", "ACC-1", 1, None, "owner")
            .unwrap();
        svc.store_mut()
            .add_note_link(
                "NOTE-DANGLING-CTX",
                "chart_context",
                "CTX-MISSING-999",
                1,
                None,
                "ctx",
            )
            .unwrap();

        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let mut scope = AnalysisScope::for_accounts(vec!["ACC-1".into()], as_of, "USD");
        scope.dataset_version = dsv;
        let res = svc.query_notes(&scope, "", true, "req-iso", 1).unwrap();
        let ids: Vec<&str> = res.value.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["NOTE-OK"]);

        drop(svc);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn kline_window_slice_deduplicates_multi_source_sessions() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_kline_dedup_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = svc
            .store_mut()
            .create_dataset_version("multi", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let d1 = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 1, 6).unwrap();
        let make_bar = |source: &str, d: NaiveDate, close: i64| BarRecord {
            id: format!("BAR:NASDAQ:AAPL:1d:raw:{source}:{d}:{dsv}"),
            instrument_id: "NASDAQ:AAPL".into(),
            timeframe: "1d".into(),
            session: "regular".into(),
            session_date: d,
            open_at: Utc.from_utc_datetime(&d.and_hms_opt(14, 30, 0).unwrap()),
            close_at: Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).unwrap()),
            open: Decimal::from(close),
            high: Decimal::from(close + 1),
            low: Decimal::from(close - 1),
            close: Decimal::from(close),
            volume: Decimal::from(1000),
            source: source.into(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: dsv,
            valid_to_version: None,
            raw_payload_hash: None,
        };
        svc.store_mut()
            .commit_bars_and_checkpoint(
                &[
                    make_bar("yahoo", d1, 100),
                    make_bar("eastmoney", d1, 100),
                    make_bar("yahoo", d2, 105),
                    make_bar("eastmoney", d2, 105),
                ],
                dsv,
                None,
            )
            .unwrap();
        svc.store_mut().publish_dataset_version(dsv).unwrap();

        let vp = KlineViewport::new(0, 50);
        let slice = svc
            .load_kline_window_slice(
                "NASDAQ:AAPL",
                "1d",
                AdjustmentKind::Raw,
                dsv,
                None,
                &vp,
                100,
            )
            .unwrap();
        assert_eq!(
            slice.bars.len(),
            2,
            "duplicate source sessions must be deduplicated"
        );
        assert_eq!(slice.bars[0].session_date, d1);
        assert_eq!(slice.bars[1].session_date, d2);

        drop(svc);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn scoped_queries_exclude_unpublished_fx_and_capture_non_trade_day_drawdown() {
        apply_process_quiet_guards();
        let tmp_dir = std::env::temp_dir().join(format!(
            "delta_fx_dd_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv1 = seed_ac02_fixture_in_service(&mut svc).unwrap();

        // Create an UNPUBLISHED version BEFORE the next published version so `unpub_v < pub_v`.
        // This directly verifies that `ScopedValuationContext` joins `dataset_publication` rather
        // than leaking unpublished FX rows with `dataset_version <= resolved.dataset_version` (`APP-B05`).
        let dsv_unpub = svc
            .store_mut()
            .create_dataset_version("unpub-fx", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        svc.store_mut()
            .upsert_fx_rates(
                &[FxRateRecord {
                    id: format!("FX:EUR:USD:{as_of}:frankfurter:{dsv_unpub}"),
                    base_currency: "EUR".into(),
                    quote_currency: "USD".into(),
                    rate: parse_decimal_lossless("1.10").unwrap(),
                    effective_date: as_of,
                    effective_at: Utc.from_utc_datetime(&as_of.and_hms_opt(16, 0, 0).unwrap()),
                    observed_at: Utc::now(),
                    source: "frankfurter".into(),
                    quality: QualityStatus::Exact,
                    carried_from_date: None,
                    dataset_version: dsv_unpub,
                }],
                None,
            )
            .unwrap();
        svc.store_mut()
            .upsert_manual_valuation(&ManualValuation {
                id: "MV-EUR-UNPUB".into(),
                account_id: "ACC-1".into(),
                asset_or_liability_id: "CASH:EUR-RESERVE".into(),
                is_liability: false,
                value: Decimal::from(100),
                currency: "EUR".into(),
                as_of,
                method: "statement".into(),
            })
            .unwrap();

        // Insert a non-trade-day price crash on 2026-01-07 (AAPL drops from 100 to 50)
        // in a higher PUBLISHED version (`dsv2 > dsv_unpub`).
        let dsv2 = svc
            .store_mut()
            .create_dataset_version("dd-bars", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let d_drop = NaiveDate::from_ymd_opt(2026, 1, 7).unwrap();
        let drop_bar = BarRecord {
            id: format!("BAR:NASDAQ:AAPL:1d:raw:ac02-fixture:{d_drop}:{dsv2}"),
            instrument_id: "NASDAQ:AAPL".into(),
            timeframe: "1d".into(),
            session: "regular".into(),
            session_date: d_drop,
            open_at: Utc.from_utc_datetime(&d_drop.and_hms_opt(14, 30, 0).unwrap()),
            close_at: Utc.from_utc_datetime(&d_drop.and_hms_opt(21, 0, 0).unwrap()),
            open: Decimal::from(50),
            high: Decimal::from(51),
            low: Decimal::from(49),
            close: Decimal::from(50),
            volume: Decimal::from(10_000),
            source: "ac02-fixture".into(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: dsv2,
            valid_to_version: None,
            raw_payload_hash: None,
        };
        svc.store_mut()
            .commit_bars_and_checkpoint(&[drop_bar], dsv2, None)
            .unwrap();
        svc.store_mut().publish_dataset_version(dsv2).unwrap();
        assert!(dsv_unpub < dsv2);

        // At published dsv2, unpublished dsv_unpub (< dsv2) EUR/USD rate must NOT leak!
        let mut scope = AnalysisScope::for_accounts(vec!["ACC-1".into()], as_of, "USD");
        scope.dataset_version = dsv2;
        let ov = svc.query_overview(&scope, "req-unpub-fx", 1).unwrap();
        assert_eq!(ov.quality, QualityStatus::Partial);
        assert!(ov
            .missing_inputs
            .iter()
            .any(|m| m.contains("manual_valuation_fx:CASH:EUR-RESERVE")));

        // Drawdown at dsv2 must capture the non-trade-day Jan 7 drop (AAPL 100 -> 50 => NAV ~1500/2000 = -25%)!
        // Remove the EUR manual valuation first so inputs are complete, then check drawdown vs dsv1.
        svc.store()
            .conn()
            .execute("DELETE FROM manual_valuation WHERE id = 'MV-EUR-UNPUB'", [])
            .unwrap();
        let bd_with_drop = svc
            .query_period_breakdown(&scope, "req-dd-drop", 1)
            .unwrap();
        let mut scope_v1 = scope.clone();
        scope_v1.dataset_version = dsv1;
        let bd_no_drop = svc
            .query_period_breakdown(&scope_v1, "req-dd-base", 1)
            .unwrap();
        let dd_drop = bd_with_drop.value.max_drawdown.expect("drawdown computed");
        let dd_base = bd_no_drop
            .value
            .max_drawdown
            .expect("base drawdown computed");
        assert!(
            dd_drop > dd_base && dd_drop >= parse_decimal_lossless("0.20").unwrap(),
            "non-trade-day crash must appear in max_drawdown: with_drop={dd_drop}, base={dd_base}"
        );

        // Real-account query must reject ScopeTarget::TrainingRun (`APP-B15`).
        let tr_scope = AnalysisScope::for_training_run("SIM-RUN-ANY");
        assert_eq!(
            svc.query_overview(&tr_scope, "req-tr-reject", 1)
                .unwrap_err()
                .code,
            "SCOPE_DENIED"
        );

        drop(svc);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
