use crate::credentials::CredentialStore;
use crate::paths::{ensure_writable_data_dir, InstanceLock, ResolvedAppPaths};
use crate::process::apply_process_quiet_guards;
use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{
    compute_atr, compute_bollinger_bands, compute_ema, compute_macd, compute_rsi, compute_sma,
    derive_adjusted_bars, evaluate_portfolio, parse_decimal_lossless,
    replay_ledger_events_with_bindings, resolve_fx_rate_s07, to_canonical_text, AdjustmentKind,
    BarRecord, CorporateActionRecord, DataQualityIssue, FxRateRecord, LedgerEventInput,
    QualityStatus, ValuationReport,
};
use delta_data::{
    amend_committed_event, build_http_client, commit_import_preview,
    cross_validate_bars_and_actions, load_all_ledger_events, load_book_fx_lookup,
    preview_statement_import, reconcile_balances, sha256_file, special_table,
    AccountBalanceSnapshotDto, AlpacaEquityProvider, BinanceSpotProvider, CommitImportReceipt,
    DatasetVersionRecord, EastMoneyUsEquityProvider, FrankfurterFxProvider, ImportPreview,
    LibraryManifest, OkxSpotProvider, ReconciliationIssueRecord, SqliteStore, StablecoinFxProvider,
    StatementPreset, SyncCheckpointRecord, TiingoEquityProvider, YahooChartV8Provider,
};
use rusqlite::{params, OptionalExtension as _};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

thread_local! {
    static ISOLATED_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[path = "sync.rs"]
mod sync;
pub use sync::SyncRequest;

#[path = "export.rs"]
mod export;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub trace_id: String,
}

impl ErrorEnvelope {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            trace_id: format!("TRC-{}", Utc::now().timestamp_micros()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed(ErrorEnvelope),
    Cancelled(ErrorEnvelope),
    TimedOut(ErrorEnvelope),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub id: JobId,
    pub name: String,
    pub state: JobState,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub worker_finished: bool,
    #[serde(default)]
    pub progress: JobProgressDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobProgressDto {
    pub phase: String,
    pub records_prepared: usize,
    pub records_published: usize,
}

struct ManagedJob {
    id: JobId,
    name: String,
    cancel_flag: Arc<AtomicBool>,
    state: Arc<Mutex<JobState>>,
    started_at: Instant,
    finished: Arc<AtomicBool>,
    progress: Arc<Mutex<JobProgressDto>>,
}

/// Background job controller isolating panics, timeouts, and cancellation (S-02 / AC-16).
pub struct JobController {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<u64, ManagedJob>>,
}

impl Default for JobController {
    fn default() -> Self {
        Self::new()
    }
}

impl JobController {
    pub fn new() -> Self {
        static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if !ISOLATED_WORKER.with(|flag| flag.get()) {
                    previous(info);
                }
            }));
        });
        Self {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn spawn_job<F>(&self, name: impl Into<String>, timeout: Duration, task: F) -> JobId
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<(), ErrorEnvelope> + Send + 'static,
    {
        self.spawn_controlled_job(
            name,
            timeout,
            move |cancel, _state, _deadline, _progress| task(cancel),
        )
    }

    fn spawn_controlled_job<F>(&self, name: impl Into<String>, timeout: Duration, task: F) -> JobId
    where
        F: FnOnce(
                Arc<AtomicBool>,
                Arc<Mutex<JobState>>,
                Instant,
                Arc<Mutex<JobProgressDto>>,
            ) -> Result<(), ErrorEnvelope>
            + Send
            + 'static,
    {
        let id_num = self.next_id.fetch_add(1, Ordering::SeqCst);
        let job_id = JobId(id_num);
        let job_name = name.into();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(JobState::Running));
        let started_at = Instant::now();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_worker = Arc::clone(&finished);
        let progress = Arc::new(Mutex::new(JobProgressDto {
            phase: "启动中".into(),
            ..Default::default()
        }));
        let progress_worker = Arc::clone(&progress);

        let cancel_clone = Arc::clone(&cancel_flag);
        let state_clone = Arc::clone(&state);
        let name_clone = job_name.clone();
        let (completion_sender, completion_receiver) = std::sync::mpsc::channel();
        let deadline_state = Arc::clone(&state);
        let deadline_cancel = Arc::clone(&cancel_flag);
        thread::spawn(move || {
            if completion_receiver.recv_timeout(timeout).is_err() {
                let mut state = deadline_state.lock().unwrap();
                if matches!(*state, JobState::Running) {
                    deadline_cancel.store(true, Ordering::SeqCst);
                    *state = JobState::TimedOut(ErrorEnvelope::new(
                        "job_timed_out",
                        "job deadline expired; cancellation requested",
                        true,
                    ));
                }
            }
        });

        thread::spawn(move || {
            apply_process_quiet_guards();
            ISOLATED_WORKER.with(|flag| flag.set(true));
            let result = catch_unwind(AssertUnwindSafe(|| {
                task(
                    Arc::clone(&cancel_clone),
                    Arc::clone(&state_clone),
                    started_at + timeout,
                    progress_worker,
                )
            }));
            ISOLATED_WORKER.with(|flag| flag.set(false));
            let mut guard = state_clone.lock().unwrap();
            if !matches!(*guard, JobState::Running) {
                // Cancellation/deadline may have published a terminal state already.
            } else if cancel_clone.load(Ordering::SeqCst) {
                *guard = JobState::Cancelled(ErrorEnvelope::new(
                    "job_cancelled",
                    format!("background job '{name_clone}' was cancelled"),
                    true,
                ));
            } else if started_at.elapsed() > timeout {
                *guard = JobState::TimedOut(ErrorEnvelope::new(
                    "job_timed_out",
                    format!(
                        "background job '{name_clone}' exceeded timeout of {} ms",
                        timeout.as_millis()
                    ),
                    true,
                ));
            } else {
                match result {
                    Ok(Ok(())) => {
                        *guard = JobState::Succeeded;
                    }
                    Ok(Err(env)) => {
                        *guard = JobState::Failed(env);
                    }
                    Err(_) => {
                        *guard = JobState::Failed(ErrorEnvelope::new(
                            "job_panicked",
                            format!("background job '{name_clone}' panicked; see trace ID"),
                            false,
                        ));
                    }
                }
            }
            // A terminal state and released task resources must precede completion.
            finished_worker.store(true, Ordering::SeqCst);
            drop(guard);
            let _ = completion_sender.send(());
        });

        self.jobs.lock().unwrap().insert(
            id_num,
            ManagedJob {
                id: job_id,
                name: job_name,
                cancel_flag,
                state,
                started_at,
                finished,
                progress,
            },
        );
        job_id
    }

    pub fn cancel_job(&self, id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(&id.0).ok_or_else(|| {
            ErrorEnvelope::new("job_not_found", format!("no job {}", id.0), false)
        })?;
        let mut st = job.state.lock().unwrap();
        if matches!(*st, JobState::Running) {
            job.cancel_flag.store(true, Ordering::SeqCst);
            *st = JobState::Cancelled(ErrorEnvelope::new(
                "job_cancelled",
                format!("job '{}' cancelled by caller", job.name),
                true,
            ));
        }
        let progress = job.progress.lock().unwrap().clone();
        Ok(JobSnapshot {
            id: job.id,
            name: job.name.clone(),
            state: st.clone(),
            elapsed_ms: job.started_at.elapsed().as_millis() as u64,
            worker_finished: job.finished.load(Ordering::SeqCst),
            progress,
        })
    }

    pub fn wait_job(&self, id: JobId, max_wait: Duration) -> Result<JobSnapshot, ErrorEnvelope> {
        let start = Instant::now();
        loop {
            let (name, state_arc, started_at, finished, progress) = {
                let jobs = self.jobs.lock().unwrap();
                let job = jobs.get(&id.0).ok_or_else(|| {
                    ErrorEnvelope::new("job_not_found", format!("no job {}", id.0), false)
                })?;
                (
                    job.name.clone(),
                    Arc::clone(&job.state),
                    job.started_at,
                    Arc::clone(&job.finished),
                    Arc::clone(&job.progress),
                )
            };
            {
                let st = state_arc.lock().unwrap();
                if !matches!(*st, JobState::Running) {
                    return Ok(JobSnapshot {
                        id,
                        name,
                        state: st.clone(),
                        elapsed_ms: started_at.elapsed().as_millis() as u64,
                        worker_finished: finished.load(Ordering::SeqCst),
                        progress: progress.lock().unwrap().clone(),
                    });
                }
                if start.elapsed() > max_wait {
                    return Err(ErrorEnvelope::new(
                        "wait_timeout",
                        format!("waiting for {name} exceeded caller deadline"),
                        true,
                    ));
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn snapshot(&self, id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs
            .get(&id.0)
            .ok_or_else(|| ErrorEnvelope::new("job_not_found", "unknown background job", false))?;
        let state = job.state.lock().unwrap().clone();
        let progress = job.progress.lock().unwrap().clone();
        Ok(JobSnapshot {
            id,
            name: job.name.clone(),
            state,
            elapsed_ms: job.started_at.elapsed().as_millis() as u64,
            worker_finished: job.finished.load(Ordering::SeqCst),
            progress,
        })
    }

    pub fn wait_finished(
        &self,
        id: JobId,
        timeout: Duration,
    ) -> Result<JobSnapshot, ErrorEnvelope> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot(id)?;
            if snapshot.worker_finished {
                return Ok(snapshot);
            }
            if Instant::now() >= deadline {
                return Err(ErrorEnvelope::new(
                    "wait_timeout",
                    "worker has not released its resources yet",
                    true,
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn cancel_all_and_wait(&self, timeout: Duration) -> Result<(), ErrorEnvelope> {
        let ids: Vec<_> = self
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|job| !job.finished.load(Ordering::SeqCst))
            .map(|job| job.id)
            .collect();
        for &id in &ids {
            self.cancel_job(id)?;
        }
        let deadline = Instant::now() + timeout;
        for id in ids {
            self.wait_finished(id, deadline.saturating_duration_since(Instant::now()))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSummaryDto {
    pub dataset_version: DatasetVersionRecord,
    pub bars_committed: usize,
    pub corporate_actions_committed: usize,
    pub fx_rates_committed: usize,
    pub quality_issues: Vec<DataQualityIssue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainSummaryDto {
    pub as_of_date: NaiveDate,
    pub dataset_version: i64,
    pub ledger_revision: i64,
    pub visible_until: Option<DateTime<Utc>>,
    pub calculation_version: String,
    pub book_currency: String,
    pub report_currency: String,
    pub valuation_sources: BTreeMap<String, String>,
    pub valuation: ValuationReport,
    pub twr: Option<Decimal>,
    pub twr_method: String,
    pub xirr_status: String,
    pub max_drawdown: Option<Decimal>,
    pub sample_sma_2: BTreeMap<String, Option<f64>>,
    pub sample_rsi_2: BTreeMap<String, Option<f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportManifestDto {
    pub exported_at: String,
    pub library_id: String,
    pub ledger_revision: i64,
    pub dataset_version: i64,
    pub terms_classes: Vec<String>,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteLibraryResultDto {
    pub dry_run_preview: bool,
    pub deleted: bool,
    pub target_data_dir: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteLibraryPreviewDto {
    pub library_id: String,
    pub target_data_dir: PathBuf,
    pub file_count: usize,
    pub total_bytes: u64,
    pub accounts: i64,
    pub notes: i64,
    pub attachments: i64,
    pub ai_sessions: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveProbeResult {
    pub provider: String,
    pub endpoint: String,
    pub status: String,
    pub http_status: Option<u16>,
    pub detail: String,
}

type SyncControl = (
    Arc<AtomicBool>,
    Instant,
    Arc<Mutex<JobState>>,
    Arc<Mutex<JobProgressDto>>,
);

/// Synchronous application facade (S-02).
/// Holds a private Tokio runtime internally; exposes ZERO async functions or Tokio/Reqwest types.
pub struct AppService {
    paths: ResolvedAppPaths,
    _lock: Option<Arc<InstanceLock>>,
    store: SqliteStore,
    credentials: CredentialStore,
    jobs: Arc<JobController>,
    runtime: tokio::runtime::Runtime,
    sync_control: Option<SyncControl>,
}

/// One immutable input snapshot per report. Daily returns replay in memory rather
/// than downloading every instrument's complete SQL history for every date.
struct HistoricalValuationInputs {
    events: Vec<LedgerEventInput>,
    book_fx: HashMap<(String, NaiveDate), Decimal>,
    bindings: HashMap<(String, String), Decimal>,
    market_prices: HashMap<(String, NaiveDate), Decimal>,
    asset_quotes: HashMap<String, String>,
    fx_rates: HashMap<(String, String, NaiveDate), Decimal>,
    account_scope: Vec<String>,
    book_currency: String,
    report_currency: String,
    ledger_revision: i64,
}

impl HistoricalValuationInputs {
    fn evaluate_at(&self, date: NaiveDate) -> Result<ValuationReport, ErrorEnvelope> {
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.occurred_at.date_naive() <= date)
            .cloned()
            .collect();
        let projection = replay_ledger_events_with_bindings(
            &self.book_currency,
            &events,
            &self.book_fx,
            &self.bindings,
            self.ledger_revision,
        )
        .map_err(|e| ErrorEnvelope::new("historical_ledger", e.to_string(), false))?;
        let mut prices = HashMap::new();
        for asset in projection.holdings.keys().map(|(_, asset)| asset).chain(
            projection
                .in_transit_assets
                .iter()
                .filter(|asset| !asset.is_cash)
                .map(|asset| &asset.asset_id),
        ) {
            let venue = if asset.starts_with("NASDAQ:") {
                "XNAS"
            } else if asset.starts_with("NYSE:") {
                "XNYS"
            } else {
                "CRYPTO_24_7"
            };
            let mut session = date;
            for _ in 0..30 {
                if let Some(price) = self.market_prices.get(&(asset.clone(), session)) {
                    prices.insert((asset.clone(), date), *price);
                    break;
                }
                if delta_data::query_calendar_session(venue, session).kind
                    != delta_core::CalendarSessionKind::Closed
                {
                    break;
                }
                let Some(previous) = session.pred_opt() else {
                    break;
                };
                session = previous;
            }
        }
        // Position transfers in a smaller scope require their boundary price.
        for flow in &projection.external_cash_flows {
            if let Some(asset) = &flow.position_asset_id {
                let day = flow.occurred_at.date_naive();
                if let Some(price) = self.market_prices.get(&(asset.clone(), day)) {
                    prices.insert((asset.clone(), day), *price);
                }
            }
        }
        catch_unwind(AssertUnwindSafe(|| {
            evaluate_portfolio(
                &projection,
                &self.account_scope,
                &self.report_currency,
                date,
                &prices,
                &self.asset_quotes,
                &self.fx_rates,
                &HashMap::new(),
            )
        }))
        .map_err(|_| {
            ErrorEnvelope::new(
                "valuation_overflow",
                "valuation exceeds supported Decimal range",
                false,
            )
        })
    }
}

impl AppService {
    pub(crate) fn query_snapshot(&self) -> Result<Self, ErrorEnvelope> {
        let store = self
            .store
            .query_snapshot()
            .map_err(|e| ErrorEnvelope::new("SNAPSHOT_FAILED", e.to_string(), true))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ErrorEnvelope::new("SNAPSHOT_FAILED", e.to_string(), true))?;
        Ok(Self {
            paths: self.paths.clone(),
            _lock: self._lock.clone(),
            store,
            credentials: CredentialStore::for_paths(&self.paths),
            jobs: Arc::new(JobController::new()),
            runtime,
            sync_control: None,
        })
    }

    pub fn open(
        paths: ResolvedAppPaths,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
        acquire_lock: bool,
    ) -> Result<Self, ErrorEnvelope> {
        if !acquire_lock {
            return Err(ErrorEnvelope::new(
                "lock_required",
                "public application sessions require the exclusive library lock",
                false,
            ));
        }
        Self::open_with_lease(paths, book_currency, report_currency, timezone, None)
    }

    fn open_with_lease(
        paths: ResolvedAppPaths,
        book_currency: &str,
        report_currency: &str,
        timezone: &str,
        lease: Option<Arc<InstanceLock>>,
    ) -> Result<Self, ErrorEnvelope> {
        apply_process_quiet_guards();
        if paths.channel == crate::paths::BuildChannel::Release
            && crate::paths::BuildChannel::current() != crate::paths::BuildChannel::Release
        {
            return Err(ErrorEnvelope::new(
                "channel_isolation",
                "development builds cannot select the release channel",
                false,
            ));
        }
        crate::paths::ensure_channel_data_isolation(
            &paths.data_dir,
            crate::paths::BuildChannel::current(),
        )
        .map_err(|e| ErrorEnvelope::new("channel_isolation", e.to_string(), false))?;
        ensure_writable_data_dir(&paths.data_dir)
            .map_err(|e| ErrorEnvelope::new("data_dir_not_writable", e.to_string(), false))?;
        let lock = match lease {
            Some(lock) => lock,
            None => Arc::new(
                InstanceLock::acquire(&paths.data_dir)
                    .map_err(|e| ErrorEnvelope::new("instance_locked", e.to_string(), true))?,
            ),
        };
        let store =
            SqliteStore::open_or_init(&paths.data_dir, book_currency, report_currency, timezone)
                .map_err(|e| ErrorEnvelope::new("storage_open_failed", e.to_string(), false))?;
        let credentials = CredentialStore::for_paths(&paths);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ErrorEnvelope::new("runtime_init_failed", e.to_string(), false))?;

        Ok(Self {
            paths,
            _lock: Some(lock),
            store,
            credentials,
            jobs: Arc::new(JobController::new()),
            runtime,
            sync_control: None,
        })
    }

    pub fn paths(&self) -> &ResolvedAppPaths {
        &self.paths
    }

    pub fn validate_display_timezone(&self, timezone: &str) -> Result<(), ErrorEnvelope> {
        timezone.parse::<chrono_tz::Tz>().map(|_| ()).map_err(|_| {
            ErrorEnvelope::new(
                "INVALID_TIMEZONE",
                "请选择有效的 IANA 时区，如 UTC 或 Asia/Shanghai",
                false,
            )
        })
    }

    pub fn register_market_instrument(
        &mut self,
        venue: &str,
        symbol: &str,
        etf: bool,
    ) -> Result<String, ErrorEnvelope> {
        self.store
            .register_market_instrument(venue, symbol, etf)
            .map_err(|e| ErrorEnvelope::new("invalid_market_metadata", e.to_string(), false))
    }

    pub fn resolve_market_symbol(&self, symbol: &str) -> Result<String, ErrorEnvelope> {
        self.store
            .load_instrument_registry()
            .map_err(|e| ErrorEnvelope::new("registry_failed", e.to_string(), false))?
            .resolve_symbol(None, symbol)
            .ok_or_else(|| ErrorEnvelope::new("unknown_symbol", symbol, false))
    }

    pub(crate) fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub(crate) fn store_mut(&mut self) -> &mut SqliteStore {
        &mut self.store
    }

    pub fn latest_dataset_version(&self) -> i64 {
        self.store
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version_num),0) FROM dataset_version",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    pub fn list_quality_issues(&self) -> Result<Vec<DataQualityIssue>, ErrorEnvelope> {
        self.store
            .list_quality_issues()
            .map_err(|e| ErrorEnvelope::new("quality_query_failed", e.to_string(), false))
    }

    pub fn load_all_ledger_events(&self) -> Result<Vec<LedgerEventInput>, ErrorEnvelope> {
        load_all_ledger_events(&self.store)
            .map_err(|e| ErrorEnvelope::new("load_events", e.to_string(), false))
    }

    pub fn search_notes(
        &self,
        query: &str,
        only_model_accessible: bool,
    ) -> Result<(Vec<delta_data::NoteDto>, String), ErrorEnvelope> {
        self.store
            .search_notes(query, only_model_accessible)
            .map_err(|e| ErrorEnvelope::new("search_notes_failed", e.to_string(), false))
    }

    pub fn flush_note_editor_autosave(
        &mut self,
        editor: &mut delta_data::NoteEditorSession,
    ) -> Result<i64, ErrorEnvelope> {
        let current: Option<i64> = self
            .store
            .conn()
            .query_row(
                "SELECT current_revision FROM note WHERE id = ?1",
                params![editor.note_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                editor.status = delta_data::AutosaveStatus::SaveFailed {
                    error: error.to_string(),
                    retryable: true,
                };
                ErrorEnvelope::new("NOTE_SAVE_FAILED", error.to_string(), true)
            })?;
        if current != editor.last_persisted_revision {
            let error = ErrorEnvelope::new(
                "REVISION_CONFLICT",
                "笔记已有更新，编辑缓冲已保留；请载入最新版本后比较",
                false,
            );
            editor.status = delta_data::AutosaveStatus::SaveFailed {
                error: error.message.clone(),
                retryable: false,
            };
            return Err(error);
        }
        editor
            .flush_autosave(&mut self.store)
            .map_err(|e| ErrorEnvelope::new("autosave_failed", e.to_string(), true))
    }

    pub fn flush_scoped_note_editor_autosave(
        &mut self,
        scope: &crate::host::AnalysisScope,
        editor: &mut delta_data::NoteEditorSession,
    ) -> Result<i64, ErrorEnvelope> {
        let previous_revision = editor.last_persisted_revision;
        let creating = previous_revision.is_none();
        let resolved = self.resolve_and_validate_scope(scope)?;
        if !creating {
            if let Err(error) = self.get_scoped_note(&resolved, &editor.note_id) {
                editor.status = delta_data::AutosaveStatus::SaveFailed {
                    error: error.message.clone(),
                    retryable: error.retryable,
                };
                return Err(error);
            }
        }
        self.store
            .conn()
            .execute_batch("SAVEPOINT scoped_note_save")
            .map_err(|e| ErrorEnvelope::new("NOTE_SAVE_FAILED", e.to_string(), true))?;
        let mut result: Result<i64, ErrorEnvelope> = (|| {
            let revision = self.flush_note_editor_autosave(editor)?;
            if creating {
                self.store
                    .add_note_link(
                        &editor.note_id,
                        "review_date",
                        &resolved.as_of_date.to_string(),
                        1,
                        None,
                        "scope",
                    )
                    .map_err(|e| ErrorEnvelope::new("NOTE_SAVE_FAILED", e.to_string(), true))?;
                for account in resolved.resolved_accounts {
                    self.store
                        .add_note_link(&editor.note_id, "account", &account, 1, None, "owner")
                        .map_err(|e| ErrorEnvelope::new("NOTE_SAVE_FAILED", e.to_string(), true))?;
                }
            }
            Ok(revision)
        })();
        if result.is_ok() {
            if let Err(error) = self.store.conn().execute_batch("RELEASE scoped_note_save") {
                let _ = self
                    .store
                    .conn()
                    .execute_batch("ROLLBACK TO scoped_note_save; RELEASE scoped_note_save");
                result = Err(ErrorEnvelope::new(
                    "NOTE_SAVE_FAILED",
                    error.to_string(),
                    true,
                ));
            }
        } else if let Err(error) = self
            .store
            .conn()
            .execute_batch("ROLLBACK TO scoped_note_save; RELEASE scoped_note_save")
        {
            result = Err(ErrorEnvelope::new(
                "NOTE_SAVE_FAILED",
                error.to_string(),
                true,
            ));
        }
        if let Err(error) = &result {
            editor.status = delta_data::AutosaveStatus::SaveFailed {
                error: error.message.clone(),
                retryable: error.retryable,
            };
            editor.last_persisted_revision = previous_revision;
        }
        result
    }

    pub fn credentials(&self) -> &CredentialStore {
        &self.credentials
    }

    pub fn jobs(&self) -> &JobController {
        &self.jobs
    }

    pub(crate) fn job_controller(&self) -> Arc<JobController> {
        Arc::clone(&self.jobs)
    }

    pub fn import_statement(
        &mut self,
        account_id: &str,
        preset: StatementPreset,
        file_path: &Path,
        commit: bool,
    ) -> Result<(ImportPreview, Option<CommitImportReceipt>), ErrorEnvelope> {
        self.import_statement_with_sync(account_id, preset, file_path, commit, None)
    }

    /// Missing book FX is acquired before committing any economic event. Tests can
    /// supply loopback endpoints through the same request contract.
    pub fn import_statement_with_sync(
        &mut self,
        account_id: &str,
        preset: StatementPreset,
        file_path: &Path,
        commit: bool,
        sync_request: Option<SyncRequest>,
    ) -> Result<(ImportPreview, Option<CommitImportReceipt>), ErrorEnvelope> {
        let preview = preview_statement_import(&self.store, account_id, preset, file_path)
            .map_err(|e| ErrorEnvelope::new("import_preview_failed", e.to_string(), false))?;
        if commit {
            let mut committed = commit_import_preview(&mut self.store, &preview);
            if matches!(
                committed,
                Err(delta_data::ImportError::LedgerValidation(
                    delta_core::LedgerError::MissingBookFx { .. }
                ))
            ) {
                let dates: Vec<_> = preview
                    .rows
                    .iter()
                    .filter_map(|row| {
                        row.normalized
                            .as_ref()
                            .map(|event| event.occurred_at.date_naive())
                    })
                    .collect();
                let earliest = dates.iter().min().copied().ok_or_else(|| {
                    ErrorEnvelope::new("empty_import", "no event date to synchronize", false)
                })?;
                let latest = dates.iter().max().copied().unwrap();
                let request = sync_request.unwrap_or_else(|| SyncRequest {
                    start: earliest
                        .checked_sub_days(chrono::Days::new(4))
                        .unwrap_or(earliest),
                    end: latest.succ_opt().unwrap_or(latest),
                    instruments: preview.auto_subscriptions.clone(),
                    fx_pairs: preview.auto_fx_pairs.clone(),
                    use_configured_credentials: true,
                    base_urls: BTreeMap::new(),
                });
                self.sync_market_data(&request)?;
                committed = commit_import_preview(&mut self.store, &preview);
            }
            let receipt = committed
                .map_err(|e| ErrorEnvelope::new("import_commit_failed", e.to_string(), false))?;
            let _ = self.store.sync_manifest();
            Ok((preview, Some(receipt)))
        } else {
            Ok((preview, None))
        }
    }

    pub fn amend_event(
        &mut self,
        original_event_id: &str,
        new_quantity: Decimal,
        new_price: Option<Decimal>,
        new_fee: Decimal,
    ) -> Result<i64, ErrorEnvelope> {
        let rev = amend_committed_event(
            &mut self.store,
            original_event_id,
            new_quantity,
            new_price,
            new_fee,
        )
        .map_err(|e| ErrorEnvelope::new("amend_event_failed", e.to_string(), false))?;
        let _ = self.store.sync_manifest();
        Ok(rev)
    }

    pub fn resolve_import_row(
        &mut self,
        row_id: &str,
        include: bool,
    ) -> Result<i64, ErrorEnvelope> {
        delta_data::resolve_held_import_row(&mut self.store, row_id, include)
            .map_err(|e| ErrorEnvelope::new("resolve_import_failed", e.to_string(), false))
    }

    /// Ingest market data from fixtures into `SqliteStore` atomically (S-08, S-09).
    pub fn sync_from_fixtures(
        &mut self,
        fixtures_root: &Path,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
        self.store
            .conn()
            .execute_batch("SAVEPOINT fixture_sync")
            .map_err(|e| ErrorEnvelope::new("sync_begin", e.to_string(), false))?;
        let result = self.sync_fixtures_inner(fixtures_root);
        let sql = if result.is_ok() {
            "RELEASE fixture_sync"
        } else {
            "ROLLBACK TO fixture_sync; RELEASE fixture_sync"
        };
        self.store
            .conn()
            .execute_batch(sql)
            .map_err(|e| ErrorEnvelope::new("sync_finish", e.to_string(), false))?;
        result
    }

    fn sync_fixtures_inner(
        &mut self,
        fixtures_root: &Path,
    ) -> Result<SyncSummaryDto, ErrorEnvelope> {
        let mut dsv = self
            .store
            .create_dataset_version(
                "multi-source-fixture",
                "unofficial_personal,public_open,official_free_personal",
                r#"{"equities":["AAPL","NVDA","SPY"],"crypto":["BTC-USDT"],"fx":["USD/HKD","USD/CNY","USDT/USD","USDC/USD"]}"#,
                "exact",
                "exact",
            )
            .map_err(|e| ErrorEnvelope::new("dataset_version_failed", e.to_string(), false))?;
        let v = dsv.version_num;

        let md_dir = fixtures_root.join("market_data");
        let mut all_bars: Vec<BarRecord> = Vec::new();
        let mut all_actions: Vec<CorporateActionRecord> = Vec::new();
        let mut all_fx: Vec<FxRateRecord> = Vec::new();
        let mut all_issues: Vec<DataQualityIssue> = Vec::new();

        // 1. Yahoo + EastMoney AAPL
        let yahoo_aapl = fs::read_to_string(md_dir.join("yahoo_aapl_2020_split.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let y_hash = self
            .store
            .store_raw_payload("yahoo", "application/json", &yahoo_aapl)
            .map_err(|e| ErrorEnvelope::new("raw_store_failed", e.to_string(), false))?;
        let (y_aapl_bars, y_aapl_actions) = YahooChartV8Provider::parse_chart_v8_json(
            "NASDAQ:AAPL",
            "XNAS",
            &yahoo_aapl,
            Some(y_hash),
            v,
        )
        .map_err(|e| ErrorEnvelope::new("yahoo_parse_failed", e.to_string(), false))?;

        let em_aapl = fs::read_to_string(md_dir.join("eastmoney_aapl_2020_raw.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let em_hash = self
            .store
            .store_raw_payload("eastmoney", "application/json", &em_aapl)
            .map_err(|e| ErrorEnvelope::new("raw_store_failed", e.to_string(), false))?;
        let em_aapl_bars = EastMoneyUsEquityProvider::parse_kline_json(
            "NASDAQ:AAPL",
            "XNAS",
            AdjustmentKind::Raw,
            &em_aapl,
            Some(em_hash),
            v,
        )
        .map_err(|e| ErrorEnvelope::new("eastmoney_parse_failed", e.to_string(), false))?;

        let cv_aapl = cross_validate_bars_and_actions(
            "NASDAQ:AAPL",
            &y_aapl_bars,
            &em_aapl_bars,
            &y_aapl_actions,
            v,
        );
        all_issues.extend(cv_aapl.issues);
        all_bars.extend(y_aapl_bars);
        all_bars.extend(em_aapl_bars);
        all_actions.extend(y_aapl_actions);

        // 2. OKX BTC-USDT
        let okx_btc = fs::read_to_string(md_dir.join("okx_btc_usdt_candles.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let okx_hash = self
            .store
            .store_raw_payload("okx", "application/json", &okx_btc)
            .map_err(|e| ErrorEnvelope::new("raw_store_failed", e.to_string(), false))?;
        let okx_bars =
            OkxSpotProvider::parse_candles_json("OKX:BTC/USDT", &okx_btc, Some(okx_hash), v)
                .map_err(|e| ErrorEnvelope::new("okx_parse_failed", e.to_string(), false))?;
        all_bars.extend(okx_bars);

        // 3. Stablecoin FX (Coinbase USDT/USD + Bitstamp USDC/USD) & Frankfurter Fiat FX
        let cb_usdt = fs::read_to_string(md_dir.join("coinbase_usdt_usd_candles.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let cb_fx = StablecoinFxProvider::parse_coinbase_candles("USDT-USD", &cb_usdt, v)
            .map_err(|e| ErrorEnvelope::new("coinbase_parse_failed", e.to_string(), false))?;
        all_fx.extend(cb_fx);

        let bs_usdc = fs::read_to_string(md_dir.join("bitstamp_usdc_usd_ohlc.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let bs_fx = StablecoinFxProvider::parse_bitstamp_ohlc("USDC", &bs_usdc, v)
            .map_err(|e| ErrorEnvelope::new("bitstamp_parse_failed", e.to_string(), false))?;
        all_fx.extend(bs_fx);

        let ff_fx_json = fs::read_to_string(md_dir.join("frankfurter_usd_fx.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let ff_fx = FrankfurterFxProvider::parse_fx_json(&ff_fx_json, v)
            .map_err(|e| ErrorEnvelope::new("frankfurter_parse_failed", e.to_string(), false))?;
        all_fx.extend(ff_fx);

        let synthetic = fs::read_to_string(md_dir.join("ledger_fx_synthetic.json"))
            .map_err(|e| ErrorEnvelope::new("fixture_read_failed", e.to_string(), false))?;
        let mut rates: Vec<FxRateRecord> = serde_json::from_str(&synthetic)
            .map_err(|e| ErrorEnvelope::new("fixture_parse_failed", e.to_string(), false))?;
        for rate in &mut rates {
            rate.dataset_version = v;
            rate.id = format!("{}:{v}", rate.id);
        }
        all_fx.extend(rates);

        let cp = SyncCheckpointRecord {
            provider: "multi-source-fixture".to_string(),
            resource_kind: "bars_actions_fx".to_string(),
            resource_key: "all".to_string(),
            connection_id: None,
            cursor: format!("DSV-{v:06}"),
            last_committed_at: Utc::now().to_rfc3339(),
            calendar_version: Some(special_table().version_tag.clone()),
            base_url: Some("fixture://local".to_string()),
        };

        let (bars_committed, rev_issues) = self
            .store
            .commit_bars_and_checkpoint(&all_bars, v, Some(&cp))
            .map_err(|e| ErrorEnvelope::new("commit_bars_failed", e.to_string(), false))?;
        all_issues.extend(rev_issues);

        let ca_committed = self
            .store
            .upsert_corporate_actions(&all_actions)
            .map_err(|e| ErrorEnvelope::new("commit_actions_failed", e.to_string(), false))?;
        let fx_committed = self
            .store
            .upsert_fx_rates(&all_fx, None)
            .map_err(|e| ErrorEnvelope::new("commit_fx_failed", e.to_string(), false))?;
        self.store
            .record_quality_issues(&all_issues)
            .map_err(|e| ErrorEnvelope::new("record_issues_failed", e.to_string(), false))?;
        dsv.manifest_hash = self
            .store
            .publish_dataset_version(v)
            .map_err(|e| ErrorEnvelope::new("publish_dataset_failed", e.to_string(), false))?;

        Ok(SyncSummaryDto {
            dataset_version: dsv,
            bars_committed,
            corporate_actions_committed: ca_committed,
            fx_rates_committed: fx_committed,
            quality_issues: all_issues,
        })
    }

    /// Evaluate portfolio holdings, cash, FX impact, TWR/XIRR/drawdown, and technical indicators
    /// at `(as_of_date, dataset_version)` with optional `visible_until` replay truncation (AC-11).
    pub fn evaluate_and_explain(
        &mut self,
        as_of_date: NaiveDate,
        dataset_version: i64,
        visible_until: Option<DateTime<Utc>>,
    ) -> Result<ExplainSummaryDto, ErrorEnvelope> {
        let (mut dto, inputs) = self.evaluate_point(as_of_date, dataset_version, visible_until)?;
        let events = load_all_ledger_events(&self.store)
            .map_err(|e| ErrorEnvelope::new("load_events", e.to_string(), false))?;
        let active: Vec<_> = events
            .iter()
            .filter(|e| {
                e.active_in_correction_group
                    && e.occurred_at.date_naive() <= as_of_date
                    && visible_until.is_none_or(|t| e.occurred_at <= t)
            })
            .collect();
        let mut dates: std::collections::BTreeSet<_> =
            active.iter().map(|e| e.occurred_at.date_naive()).collect();
        // Cash in the reporting currency is constant between events. Start the daily
        // sequence once there is market or currency exposure, avoiding decades of
        // redundant SQL valuations before the first investment.
        let exposed_from = active
            .iter()
            .filter(|e| {
                e.instrument_id.is_some()
                    || !e.quote_currency.eq_ignore_ascii_case(&dto.report_currency)
            })
            .map(|e| e.occurred_at.date_naive())
            .min();
        if let Some(mut date) = exposed_from {
            while date < as_of_date {
                dates.insert(date);
                date = date.succ_opt().unwrap();
            }
        }
        dates.insert(as_of_date);
        let mut previous_nav = None;
        let mut previous_capital = Decimal::ZERO;
        let mut normalized = Decimal::ONE;
        let mut nav_series = vec![normalized];
        let mut cashflows = Vec::new();
        let mut complete = dto.valuation.missing_inputs.is_empty() && !active.is_empty();
        for date in dates {
            let valuation = inputs.evaluate_at(date)?;
            if !valuation.missing_inputs.is_empty() {
                complete = false;
                break;
            }
            let nav = valuation.net_worth;
            let capital = valuation.net_external_cash_flow;
            let flow = capital - previous_capital;
            if !flow.is_zero() {
                cashflows.push((date, -flow));
            }
            if let Some(previous) = previous_nav {
                if previous <= Decimal::ZERO {
                    complete = false;
                    break;
                }
                normalized = normalized
                    .checked_mul((nav - flow) / previous)
                    .ok_or_else(|| {
                        ErrorEnvelope::new(
                            "return_overflow",
                            "return series exceeds Decimal range",
                            false,
                        )
                    })?;
            } else if capital > Decimal::ZERO {
                normalized = nav / capital;
            } else {
                complete = false;
                break;
            }
            nav_series.push(normalized);
            previous_nav = Some(nav);
            previous_capital = capital;
        }
        if complete {
            dto.twr = Some(normalized - Decimal::ONE);
            dto.twr_method = "daily_approximation_flows_at_day_end".into();
            dto.max_drawdown = delta_core::compute_max_drawdown(&nav_series);
            cashflows.push((as_of_date, dto.valuation.net_worth));
            dto.xirr_status = format!("{:?}", delta_core::compute_xirr(&cashflows));
        }
        self.save_explain_report(&dto)?;
        Ok(dto)
    }

    fn evaluate_point(
        &mut self,
        as_of_date: NaiveDate,
        dataset_version: i64,
        visible_until: Option<DateTime<Utc>>,
    ) -> Result<(ExplainSummaryDto, HistoricalValuationInputs), ErrorEnvelope> {
        if dataset_version > 0 {
            let published: bool = self
                .store
                .conn()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM dataset_publication WHERE version_num=?1)",
                    params![dataset_version],
                    |r| r.get(0),
                )
                .map_err(|e| ErrorEnvelope::new("query_version", e.to_string(), false))?;
            if !published {
                return Err(ErrorEnvelope::new(
                    "unpublished_dataset",
                    "report requires a published data snapshot",
                    false,
                ));
            }
        } else if dataset_version < 0 {
            return Err(ErrorEnvelope::new(
                "invalid_version",
                "dataset version must be non-negative",
                false,
            ));
        }
        let events = load_all_ledger_events(&self.store)
            .map_err(|e| ErrorEnvelope::new("load_ledger_failed", e.to_string(), false))?;
        let filtered_events: Vec<LedgerEventInput> = events
            .into_iter()
            .filter(|e| {
                e.occurred_at.date_naive() <= as_of_date
                    && visible_until
                        .map(|cut| e.occurred_at <= cut)
                        .unwrap_or(true)
            })
            .collect();

        let (lib_rev, book_ccy, rep_ccy): (i64, String, String) = self
            .store
            .conn()
            .query_row(
                "SELECT ledger_revision, book_currency, report_currency FROM library LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| ErrorEnvelope::new("query_library_failed", e.to_string(), false))?;

        let book_fx =
            delta_data::load_book_fx_lookup_at_version(&self.store, &book_ccy, dataset_version)
                .map_err(|e| ErrorEnvelope::new("load_book_fx_failed", e.to_string(), false))?;

        let bindings = delta_data::load_event_book_fx(&self.store)
            .map_err(|e| ErrorEnvelope::new("load_event_fx", e.to_string(), false))?;
        let projection = replay_ledger_events_with_bindings(
            &book_ccy,
            &filtered_events,
            &book_fx,
            &bindings,
            lib_rev,
        )
        .map_err(|e| ErrorEnvelope::new("ledger_projection_failed", e.to_string(), false))?;

        let mut market_prices: HashMap<(String, NaiveDate), Decimal> = HashMap::new();
        let mut asset_quote_ccys: HashMap<String, String> = HashMap::new();
        let mut sma_map: BTreeMap<String, Option<f64>> = BTreeMap::new();
        let mut rsi_map: BTreeMap<String, Option<f64>> = BTreeMap::new();
        let mut valuation_sources = BTreeMap::new();

        let reg = self
            .store
            .load_instrument_registry()
            .map_err(|e| ErrorEnvelope::new("registry_failed", e.to_string(), false))?;
        let mut instruments: Vec<_> = reg.instruments.iter().collect();
        instruments.sort_by_key(|(id, _)| *id);
        for (inst_id, inst) in instruments {
            asset_quote_ccys.insert(inst_id.clone(), inst.quote_asset_id.clone());
            asset_quote_ccys.insert(inst.base_asset_id.clone(), inst.quote_asset_id.clone());

            let mut bars = self
                .store
                .query_bars_at_version(
                    inst_id,
                    "1d",
                    AdjustmentKind::Raw,
                    None,
                    dataset_version,
                    visible_until,
                )
                .map_err(|e| ErrorEnvelope::new("query_bars_failed", e.to_string(), false))?;
            let cutoff = visible_until.unwrap_or_else(|| {
                as_of_date
                    .succ_opt()
                    .unwrap_or(as_of_date)
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
            });
            bars.retain(|b| b.session_date <= as_of_date && b.close_at <= cutoff);
            // One deterministic source per session; duplicate sources are cross-check inputs.
            bars.sort_by(|a, b| {
                a.session_date
                    .cmp(&b.session_date)
                    .then_with(|| a.source.cmp(&b.source))
            });
            bars.dedup_by_key(|b| b.session_date);
            if !bars.is_empty() {
                let mut actions = self
                    .store
                    .query_corporate_actions(inst_id, Some(dataset_version))
                    .map_err(|e| {
                        ErrorEnvelope::new("query_actions_failed", e.to_string(), false)
                    })?;
                actions.retain(|a| {
                    a.ex_date <= as_of_date && a.announced_at.is_none_or(|t| t <= cutoff)
                });
                let split_adj =
                    derive_adjusted_bars(&bars, &actions, AdjustmentKind::SplitAdjusted).map_err(
                        |e| ErrorEnvelope::new("adjustment_failed", e.to_string(), false),
                    )?;
                let _tot_ret =
                    derive_adjusted_bars(&bars, &actions, AdjustmentKind::TotalReturnAdjusted)
                        .map_err(|e| {
                            ErrorEnvelope::new("adjustment_failed", e.to_string(), false)
                        })?;
                for b in &bars {
                    market_prices.insert((inst_id.clone(), b.session_date), b.close);
                    market_prices.insert((inst.base_asset_id.clone(), b.session_date), b.close);
                }
                if let Some(last_bar) = bars.iter().rfind(|b| b.session_date <= as_of_date) {
                    valuation_sources.insert(
                        inst_id.clone(),
                        format!("{}:{}", last_bar.source, last_bar.session_date),
                    );
                    let venue = if inst.venue == "NASDAQ" {
                        "XNAS"
                    } else if inst.venue == "NYSE" {
                        "XNYS"
                    } else {
                        "CRYPTO_24_7"
                    };
                    let mut date = last_bar.session_date;
                    let mut fresh = true;
                    while date < as_of_date {
                        date = date.succ_opt().unwrap();
                        if delta_data::query_calendar_session(venue, date).kind
                            != delta_core::CalendarSessionKind::Closed
                        {
                            fresh = false;
                            break;
                        }
                    }
                    if fresh {
                        market_prices.insert((inst_id.clone(), as_of_date), last_bar.close);
                        market_prices
                            .insert((inst.base_asset_id.clone(), as_of_date), last_bar.close);
                    }
                }
                let sma = compute_sma(&split_adj, 2, visible_until);
                let _ema = compute_ema(&split_adj, 2, visible_until);
                let _macd = compute_macd(&split_adj, 2, 3, 2, visible_until);
                let rsi = compute_rsi(&split_adj, 2, visible_until);
                let _atr = compute_atr(&split_adj, 2, visible_until);
                let _bb = compute_bollinger_bands(&split_adj, 2, 2.0, visible_until);
                sma_map.insert(inst_id.clone(), sma.last().and_then(|p| p.value));
                rsi_map.insert(inst_id.clone(), rsi.last().and_then(|p| p.value));
            }
        }

        let fx_records = self.load_fx_rates(dataset_version)?;
        let mut fx_map: HashMap<(String, String, NaiveDate), Decimal> = HashMap::new();
        for r in &fx_records {
            if visible_until.is_some_and(|cut| r.effective_at > cut) {
                continue;
            }
            fx_map.insert(
                (
                    r.base_currency.to_ascii_uppercase(),
                    r.quote_currency.to_ascii_uppercase(),
                    r.effective_date,
                ),
                r.rate,
            );
        }

        let mut account_scope: Vec<String> = projection
            .cash_balances
            .keys()
            .map(|(a, _)| a.clone())
            .chain(projection.holdings.keys().map(|(a, _)| a.clone()))
            .chain(filtered_events.iter().map(|event| event.account_id.clone()))
            .collect();
        account_scope.sort();
        account_scope.dedup();
        // Library membership is explicit. An external bank or wallet named in a
        // transfer does not become an internal account merely by being a destination.
        let mut accounts = self
            .store
            .conn()
            .prepare("SELECT id FROM account WHERE status='active'")
            .map_err(|e| ErrorEnvelope::new("query_accounts", e.to_string(), false))?;
        for account in accounts
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| ErrorEnvelope::new("query_accounts", e.to_string(), false))?
        {
            account_scope.push(
                account.map_err(|e| ErrorEnvelope::new("query_accounts", e.to_string(), false))?,
            );
        }
        account_scope.sort();
        account_scope.dedup();

        let liabilities = HashMap::new();
        let mut valuation = catch_unwind(AssertUnwindSafe(|| {
            evaluate_portfolio(
                &projection,
                &account_scope,
                &rep_ccy,
                as_of_date,
                &market_prices,
                &asset_quote_ccys,
                &fx_map,
                &liabilities,
            )
        }))
        .map_err(|_| {
            ErrorEnvelope::new(
                "valuation_overflow",
                "valuation exceeds supported Decimal range",
                false,
            )
        })?;
        if dataset_version > 0 {
            let quality: String = self
                .store
                .conn()
                .query_row(
                    "SELECT quality FROM dataset_version WHERE version_num=?1",
                    params![dataset_version],
                    |r| r.get(0),
                )
                .map_err(|e| ErrorEnvelope::new("query_quality", e.to_string(), false))?;
            if quality == "warning" {
                if matches!(
                    valuation.quality,
                    QualityStatus::Exact | QualityStatus::CarriedForward
                ) {
                    valuation.quality = QualityStatus::Warning;
                }
                valuation
                    .warnings
                    .push("dataset_source_or_coverage_warning".into());
            }
        }

        // A point valuation cannot supply external-flow boundary valuations.
        // Return explicit unavailable metrics until the actual series is assembled.
        let twr = None;
        let xirr = "unavailable: requires historical cash flows and boundary valuations";
        let mdd = None;

        let inputs = HistoricalValuationInputs {
            events: filtered_events,
            book_fx,
            bindings,
            market_prices,
            asset_quotes: asset_quote_ccys,
            fx_rates: fx_map,
            account_scope: account_scope.clone(),
            book_currency: book_ccy.clone(),
            report_currency: rep_ccy.clone(),
            ledger_revision: lib_rev,
        };
        let dto = ExplainSummaryDto {
            as_of_date,
            dataset_version,
            ledger_revision: lib_rev,
            visible_until,
            calculation_version: "m1-ledger-3".into(),
            book_currency: book_ccy,
            report_currency: rep_ccy,
            valuation_sources,
            valuation,
            twr,
            twr_method: "unavailable_missing_boundary_valuations".into(),
            xirr_status: xirr.to_string(),
            max_drawdown: mdd,
            sample_sma_2: sma_map,
            sample_rsi_2: rsi_map,
        };

        Ok((dto, inputs))
    }

    fn save_explain_report(&self, dto: &ExplainSummaryDto) -> Result<(), ErrorEnvelope> {
        let scope = serde_json::json!({"accounts": dto.valuation.account_scope, "as_of_date": dto.as_of_date, "visible_until": dto.visible_until, "book_currency": dto.book_currency, "report_currency": dto.report_currency, "cost_method": "fifo", "fee_rule": "capitalized_buy_net_sell_third_asset_disposal", "valuation_sources": dto.valuation_sources, "calculation_version": dto.calculation_version});
        let scope_json = scope.to_string();
        use sha2::{Digest, Sha256};
        let report_id = format!(
            "REP:{}:{}:{}",
            dto.ledger_revision,
            dto.dataset_version,
            hex::encode(Sha256::digest(scope_json.as_bytes()))
        );
        let payload = serde_json::to_string(dto)
            .map_err(|e| ErrorEnvelope::new("serialize_report", e.to_string(), false))?;
        self.store
            .conn()
            .execute(
                "INSERT OR REPLACE INTO analysis_report (
                    id, report_type, scope_json, ledger_revision, dataset_version,
                    calculation_version, payload_json, status, created_at
                 ) VALUES (?1, 'explain_summary', ?7, ?2, ?3, ?8, ?4, ?5, ?6)",
                params![
                    report_id,
                    dto.ledger_revision,
                    dto.dataset_version,
                    payload,
                    dto.valuation.quality.as_str(),
                    Utc::now().to_rfc3339(),
                    scope_json,
                    dto.calculation_version,
                ],
            )
            .map_err(|e| ErrorEnvelope::new("save_report_failed", e.to_string(), false))?;

        Ok(())
    }

    fn load_fx_rates(&self, max_version: i64) -> Result<Vec<FxRateRecord>, ErrorEnvelope> {
        let mut stmt = self
            .store
            .conn()
            .prepare(
                "SELECT id, base_currency, quote_currency, rate, effective_date,
                        effective_at, observed_at, source, quality, carried_from_date, dataset_version
                 FROM fx_rate JOIN dataset_publication p ON p.version_num = fx_rate.dataset_version
                 WHERE dataset_version <= ?1 ORDER BY dataset_version ASC, effective_date ASC, CASE WHEN base_currency='USDT' AND source='coinbase' THEN 2 WHEN base_currency='USDC' AND source='bitstamp' THEN 2 WHEN source='frankfurter' THEN 2 ELSE 1 END ASC, source DESC",
            )
            .map_err(|e| ErrorEnvelope::new("query_fx_failed", e.to_string(), false))?;
        let rows = stmt
            .query_map(params![max_version], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, i64>(10)?,
                ))
            })
            .map_err(|e| ErrorEnvelope::new("query_fx_failed", e.to_string(), false))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, b, q, rate_s, ed_s, ea_s, oa_s, src, qual_s, cf_s, dv) =
                row.map_err(|e| ErrorEnvelope::new("query_fx_failed", e.to_string(), false))?;
            let rate = parse_decimal_lossless(&rate_s)
                .map_err(|e| ErrorEnvelope::new("parse_fx_failed", e.to_string(), false))?;
            let effective_date = NaiveDate::parse_from_str(&ed_s, "%Y-%m-%d").map_err(|_| {
                ErrorEnvelope::new("corrupt_fx_date", "invalid stored FX date", false)
            })?;
            let effective_at = DateTime::parse_from_rfc3339(&ea_s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| {
                    ErrorEnvelope::new("corrupt_fx_time", "invalid stored FX time", false)
                })?;
            let observed_at = DateTime::parse_from_rfc3339(&oa_s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|_| {
                    ErrorEnvelope::new("corrupt_fx_time", "invalid stored FX observation", false)
                })?;
            let quality = match qual_s.as_str() {
                "exact" => QualityStatus::Exact,
                "carried_forward" => QualityStatus::CarriedForward,
                "partial" => QualityStatus::Partial,
                "warning" => QualityStatus::Warning,
                _ => QualityStatus::Unavailable,
            };
            let carried_from_date = cf_s
                .map(|s| {
                    NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| {
                        ErrorEnvelope::new("corrupt_fx_date", "invalid FX carry date", false)
                    })
                })
                .transpose()?;
            out.push(FxRateRecord {
                id,
                base_currency: b,
                quote_currency: q,
                rate,
                effective_date,
                effective_at,
                observed_at,
                source: src,
                quality,
                carried_from_date,
                dataset_version: dv,
            });
        }
        Ok(out)
    }

    pub fn reconcile_account(
        &mut self,
        account_id: &str,
        snapshots: &[AccountBalanceSnapshotDto],
    ) -> Result<Vec<ReconciliationIssueRecord>, ErrorEnvelope> {
        let events = load_all_ledger_events(&self.store)
            .map_err(|e| ErrorEnvelope::new("load_ledger_failed", e.to_string(), false))?;
        let (lib_rev, book_ccy): (i64, String) = self
            .store
            .conn()
            .query_row(
                "SELECT ledger_revision, book_currency FROM library LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| ErrorEnvelope::new("query_library_failed", e.to_string(), false))?;
        let book_fx = load_book_fx_lookup(&self.store, &book_ccy)
            .map_err(|e| ErrorEnvelope::new("load_book_fx_failed", e.to_string(), false))?;
        let mut issues = Vec::new();
        let bindings = delta_data::load_event_book_fx(&self.store)
            .map_err(|e| ErrorEnvelope::new("load_event_fx", e.to_string(), false))?;
        for snapshot in snapshots {
            let historical: Vec<_> = events
                .iter()
                .filter(|event| event.occurred_at <= snapshot.as_of)
                .cloned()
                .collect();
            let proj = replay_ledger_events_with_bindings(
                &book_ccy,
                &historical,
                &book_fx,
                &bindings,
                lib_rev,
            )
            .map_err(|e| ErrorEnvelope::new("ledger_projection_failed", e.to_string(), false))?;
            issues.extend(reconcile_balances(
                account_id,
                &proj.cash_balances,
                &proj.holdings,
                std::slice::from_ref(snapshot),
            ));
        }
        let tx = self
            .store
            .conn_mut()
            .transaction()
            .map_err(|e| ErrorEnvelope::new("record_recon_failed", e.to_string(), false))?;
        for issue in &issues {
            tx.execute(
                "INSERT OR REPLACE INTO reconciliation_issue (
                    id, account_id, asset_id, as_of, expected, observed, difference, reason, resolution_event_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    issue.id,
                    issue.account_id,
                    issue.asset_id,
                    issue.as_of,
                    issue.expected,
                    issue.observed,
                    issue.difference,
                    issue.reason,
                    issue.resolution_event_id,
                ],
            )
            .map_err(|e| ErrorEnvelope::new("record_recon_failed", e.to_string(), false))?;
        }
        tx.commit()
            .map_err(|e| ErrorEnvelope::new("record_recon_failed", e.to_string(), false))?;
        Ok(issues)
    }

    pub fn create_backup(&self, backup_dir: &Path) -> Result<LibraryManifest, ErrorEnvelope> {
        crate::paths::ensure_channel_data_isolation(
            backup_dir,
            crate::paths::BuildChannel::current(),
        )
        .map_err(|e| ErrorEnvelope::new("channel_isolation", e.to_string(), false))?;
        self.store
            .create_consistent_backup(backup_dir)
            .map_err(|e| ErrorEnvelope::new("backup_failed", e.to_string(), false))
    }

    pub fn restore_backup(
        backup_dir: &Path,
        target_dir: &Path,
    ) -> Result<LibraryManifest, ErrorEnvelope> {
        crate::paths::ensure_channel_data_isolation(
            target_dir,
            crate::paths::BuildChannel::current(),
        )
        .map_err(|e| ErrorEnvelope::new("channel_isolation", e.to_string(), false))?;
        let restored = SqliteStore::restore_consistent_backup(backup_dir, target_dir)
            .map_err(|e| ErrorEnvelope::new("restore_failed", e.to_string(), false))?;
        // Verify ledger, note, and attachment references on restore (`M2-W08`, `M2-A-17`)
        let _ = load_all_ledger_events(&restored).map_err(|e| {
            ErrorEnvelope::new("restore_ledger_verify_failed", e.to_string(), false)
        })?;
        if let Ok(notes) = restored.list_notes(false) {
            for note in &notes {
                for att in &note.attachments {
                    let disk_path = restored.root_dir().join(&att.relative_path);
                    let valid = att.exists_on_disk
                        && sha256_file(&disk_path)
                            .map(|h| h == att.sha256_hash)
                            .unwrap_or(false);
                    if !valid {
                        return Err(ErrorEnvelope::new(
                            "restore_attachment_corrupted",
                            format!(
                                "restored attachment '{}' for note '{}' failed SHA-256 or existence check",
                                att.id, note.id
                            ),
                            false,
                        ));
                    }
                }
                for link in &note.links {
                    if link.target_type == "economic_event" || link.target_type == "event" {
                        let exists: bool = restored
                            .conn()
                            .query_row(
                                "SELECT EXISTS(SELECT 1 FROM economic_event WHERE id = ?1)",
                                params![link.target_id],
                                |r| r.get(0),
                            )
                            .unwrap_or(false);
                        if !exists {
                            return Err(ErrorEnvelope::new(
                                "restore_dangling_note_link",
                                format!(
                                    "restored note '{}' links to missing economic_event '{}'",
                                    note.id, link.target_id
                                ),
                                false,
                            ));
                        }
                    } else if link.target_type == "chart_context" {
                        let exists: bool = restored
                            .conn()
                            .query_row(
                                "SELECT EXISTS(SELECT 1 FROM chart_context WHERE id = ?1)",
                                params![link.target_id],
                                |r| r.get(0),
                            )
                            .unwrap_or(false);
                        if !exists {
                            return Err(ErrorEnvelope::new(
                                "restore_dangling_chart_context",
                                format!(
                                    "restored note '{}' links to missing chart_context '{}'",
                                    note.id, link.target_id
                                ),
                                false,
                            ));
                        }
                    }
                }
            }
        }
        restored
            .sync_manifest()
            .map_err(|e| ErrorEnvelope::new("manifest_sync_failed", e.to_string(), false))
    }

    /// Write allowlisted diagnostic metadata to `<data_dir>/logs/delta.log`.
    /// Caller messages and secret values are never persisted, including values not known
    /// to a redaction list (`M2-W08`, `M2-A-17`).
    pub fn write_audit_log(
        &self,
        action: &str,
        raw_message: &str,
        extra_secrets: &[&str],
    ) -> Result<PathBuf, ErrorEnvelope> {
        let action = match action {
            "ai_review_and_sync"
            | "sync"
            | "import"
            | "backup"
            | "restore"
            | "export"
            | "delete_library"
            | "model_configuration" => action,
            _ => "unclassified",
        };
        let log_dir = self.paths.data_dir.join("logs");
        fs::create_dir_all(&log_dir)
            .map_err(|e| ErrorEnvelope::new("log_write_failed", e.to_string(), false))?;
        let log_path = log_dir.join("delta.log");
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "action": action,
            "message_bytes": raw_message.len(),
            "supplied_secret_count": extra_secrets.len(),
            "dataset_version": self.latest_dataset_version(),
        });
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| ErrorEnvelope::new("log_write_failed", e.to_string(), false))?;
        file.write_all(line.as_bytes())
            .map_err(|e| ErrorEnvelope::new("log_write_failed", e.to_string(), false))?;
        Ok(log_path)
    }

    /// Scan `<data_dir>/logs` for any forbidden substrings (secrets, full account numbers, note bodies).
    pub fn scan_logs_for_sensitive_leaks(
        &self,
        forbidden_substrings: &[&str],
    ) -> Result<Vec<String>, ErrorEnvelope> {
        let log_dir = self.paths.data_dir.join("logs");
        if !log_dir.exists() {
            return Ok(Vec::new());
        }
        let mut leaks = Vec::new();
        for (file_index, entry) in fs::read_dir(&log_dir)
            .map_err(|e| ErrorEnvelope::new("log_scan_failed", e.to_string(), false))?
            .enumerate()
        {
            let path = entry
                .map_err(|e| ErrorEnvelope::new("log_scan_failed", e.to_string(), false))?
                .path();
            if path.is_file() {
                let text = fs::read_to_string(&path).unwrap_or_default();
                for (needle_index, &needle) in forbidden_substrings.iter().enumerate() {
                    if !needle.is_empty() && text.contains(needle) {
                        leaks.push(format!(
                            "sensitive value #{} in log file #{}",
                            needle_index + 1,
                            file_index + 1,
                        ));
                    }
                }
            }
        }
        Ok(leaks)
    }

    /// Delete library data directory (`delta library delete`), supporting dry-run preview and
    /// refusing non-TTY execution without `--yes` immediately without blocking on stdin (S-10, AC-20 subset).
    pub fn delete_library(
        data_dir: &Path,
        confirm_yes: bool,
        is_interactive_tty: bool,
    ) -> Result<DeleteLibraryResultDto, ErrorEnvelope> {
        Self::delete_library_checked(data_dir, confirm_yes, is_interactive_tty, None)
    }

    pub fn preview_library_deletion(&self) -> Result<DeleteLibraryPreviewDto, ErrorEnvelope> {
        let root = fs::canonicalize(&self.paths.data_dir)
            .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?;
        let count = |table: &str| {
            self.store
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))
        };
        let files = walk_files(&root)
            .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?;
        let mut bytes = 0u64;
        for file in &files {
            bytes = bytes
                .checked_add(
                    fs::metadata(file)
                        .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?
                        .len(),
                )
                .ok_or_else(|| ErrorEnvelope::new("delete_preview", "file size overflow", false))?;
        }
        let library_id = self
            .store
            .conn()
            .query_row("SELECT id FROM library", [], |row| row.get(0))
            .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?;
        Ok(DeleteLibraryPreviewDto {
            library_id,
            target_data_dir: root,
            file_count: files.len(),
            total_bytes: bytes,
            accounts: count("account")?,
            notes: count("note")?,
            attachments: count("attachment")?,
            ai_sessions: count("ai_session")?,
        })
    }

    pub fn delete_library_from_preview(
        preview: &DeleteLibraryPreviewDto,
        typed_id: &str,
    ) -> Result<DeleteLibraryResultDto, ErrorEnvelope> {
        if typed_id.trim() != preview.library_id {
            return Err(ErrorEnvelope::new(
                "DELETE_CONFIRMATION",
                "请输入预览中完整的资料库编号",
                false,
            ));
        }
        Self::delete_library_checked(
            &preview.target_data_dir,
            true,
            false,
            Some(&preview.library_id),
        )
    }

    fn delete_library_checked(
        data_dir: &Path,
        confirm_yes: bool,
        is_interactive_tty: bool,
        expected_id: Option<&str>,
    ) -> Result<DeleteLibraryResultDto, ErrorEnvelope> {
        crate::paths::ensure_channel_data_isolation(
            data_dir,
            crate::paths::BuildChannel::current(),
        )
        .map_err(|e| ErrorEnvelope::new("channel_isolation", e.to_string(), false))?;
        let root = fs::canonicalize(data_dir)
            .map_err(|e| ErrorEnvelope::new("invalid_library", e.to_string(), false))?;
        let metadata = fs::symlink_metadata(data_dir)
            .map_err(|e| ErrorEnvelope::new("invalid_library", e.to_string(), false))?;
        #[cfg(windows)]
        let linked = {
            use std::os::windows::fs::MetadataExt as _;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let linked = metadata.file_type().is_symlink();
        if linked {
            return Err(ErrorEnvelope::new(
                "delete_link",
                "library root is a link; deletion refused",
                false,
            ));
        }
        if !root.join("delta.sqlite").is_file() || !root.join("manifest.json").is_file() {
            return Err(ErrorEnvelope::new(
                "invalid_library",
                "delete requires an initialized delta library",
                false,
            ));
        }
        let lock = InstanceLock::acquire(&root)
            .map_err(|e| ErrorEnvelope::new("instance_locked", e.to_string(), false))?;
        fn reject_links(directory: &Path) -> Result<(), ErrorEnvelope> {
            for entry in fs::read_dir(directory)
                .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?
            {
                let path = entry
                    .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?
                    .path();
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?;
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt as _;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = metadata.file_type().is_symlink();
                if linked
                    || !fs::canonicalize(&path)
                        .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?
                        .starts_with(directory)
                {
                    return Err(ErrorEnvelope::new(
                        "delete_link",
                        "library contains a link; deletion refused",
                        false,
                    ));
                }
                if metadata.is_dir() {
                    reject_links(&path)?;
                }
            }
            Ok(())
        }
        reject_links(&root)?;
        let manifest: LibraryManifest = serde_json::from_slice(
            &fs::read(root.join("manifest.json"))
                .map_err(|e| ErrorEnvelope::new("invalid_library", e.to_string(), false))?,
        )
        .map_err(|e| ErrorEnvelope::new("invalid_library", e.to_string(), false))?;
        let database = rusqlite::Connection::open_with_flags(
            root.join("delta.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| ErrorEnvelope::new("invalid_library", e.to_string(), false))?;
        let identity: (String, i64, String) = database
            .query_row(
                "SELECT id,schema_version,book_currency FROM library",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| ErrorEnvelope::new("invalid_library", e.to_string(), false))?;
        if identity
            != (
                manifest.library_id.clone(),
                manifest.schema_version,
                manifest.book_currency.clone(),
            )
        {
            return Err(ErrorEnvelope::new(
                "invalid_library",
                "manifest and database identity differ",
                false,
            ));
        }
        if expected_id.is_some_and(|expected| identity.0 != expected) {
            return Err(ErrorEnvelope::new(
                "DELETE_PREVIEW_CHANGED",
                "资料库身份与预览不一致，未删除任何文件",
                false,
            ));
        }
        drop(database);

        let mut file_count = 0usize;
        let mut total_bytes = 0u64;
        for entry in walk_files(&root)
            .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?
        {
            let meta = fs::symlink_metadata(&entry)
                .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?;
            if meta.file_type().is_symlink()
                || !fs::canonicalize(&entry)
                    .map_err(|e| ErrorEnvelope::new("delete_preview", e.to_string(), false))?
                    .starts_with(&root)
            {
                return Err(ErrorEnvelope::new(
                    "delete_link",
                    "library contains a link; deletion refused",
                    false,
                ));
            }
            file_count = file_count.checked_add(1).ok_or_else(|| {
                ErrorEnvelope::new("delete_preview", "file count overflow", false)
            })?;
            total_bytes = total_bytes
                .checked_add(meta.len())
                .ok_or_else(|| ErrorEnvelope::new("delete_preview", "file size overflow", false))?;
        }

        if !confirm_yes {
            if !is_interactive_tty {
                return Err(ErrorEnvelope::new(
                    "confirmation_required_non_tty",
                    format!(
                        "destructive operation 'library delete' on '{}' requires explicit --yes in non-TTY mode (preview: {} files, {} bytes)",
                        data_dir.display(),
                        file_count,
                        total_bytes
                    ),
                    false,
                ));
            }
            return Ok(DeleteLibraryResultDto {
                dry_run_preview: true,
                deleted: false,
                target_data_dir: data_dir.display().to_string(),
                file_count,
                total_bytes,
            });
        }

        for entry in fs::read_dir(&root)
            .map_err(|e| ErrorEnvelope::new("library_delete_failed", e.to_string(), false))?
        {
            let path = entry
                .map_err(|e| ErrorEnvelope::new("library_delete_failed", e.to_string(), false))?
                .path();
            if path.file_name().is_some_and(|name| name == "delta.lock") {
                continue;
            }
            if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            }
            .map_err(|e| ErrorEnvelope::new("library_delete_failed", e.to_string(), false))?;
        }
        drop(lock);
        fs::remove_file(root.join("delta.lock"))
            .and_then(|_| fs::remove_dir(&root))
            .map_err(|e| ErrorEnvelope::new("library_delete_failed", e.to_string(), false))?;
        Ok(DeleteLibraryResultDto {
            dry_run_preview: false,
            deleted: true,
            target_data_dir: data_dir.display().to_string(),
            file_count,
            total_bytes,
        })
    }

    /// Execute live read-only network probes when explicitly requested via `--live` (Section 6.2).
    pub fn run_live_network_probes(&self) -> Vec<LiveProbeResult> {
        let client = match build_http_client(8000) {
            Ok(c) => c,
            Err(e) => {
                return vec![LiveProbeResult {
                    provider: "http_client".to_string(),
                    endpoint: "n/a".to_string(),
                    status: "failed".to_string(),
                    http_status: None,
                    detail: e.to_string(),
                }]
            }
        };

        let mut results = Vec::new();

        // 1. OKX public BTC-USDT 1Dutc
        let okx = OkxSpotProvider::default();
        let okx_res = self.runtime.block_on(okx.fetch_daily_candles(
            &client,
            "OKX:BTC/USDT",
            "BTC-USDT",
            None,
            1,
        ));
        results.push(match okx_res {
            Ok((bars, _)) => LiveProbeResult {
                provider: "okx".to_string(),
                endpoint: format!(
                    "{}/api/v5/market/history-candles?instId=BTC-USDT&bar=1Dutc",
                    okx.base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} finalized 1Dutc bars", bars.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "okx".to_string(),
                endpoint: okx.base_url,
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 2. Binance Vision public mirror BTCUSDT 1d
        let binance = BinanceSpotProvider::default();
        let bn_res = self.runtime.block_on(binance.fetch_daily_klines(
            &client,
            "OKX:BTC/USDT",
            "BTCUSDT",
            None,
            Utc::now(),
            1,
        ));
        results.push(match bn_res {
            Ok((bars, _)) => LiveProbeResult {
                provider: "binance-vision".to_string(),
                endpoint: format!(
                    "{}/api/v3/klines?symbol=BTCUSDT&interval=1d",
                    binance.rest_base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} finalized 1d bars", bars.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "binance-vision".to_string(),
                endpoint: binance.rest_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 3. Coinbase USDT-USD
        let stable = StablecoinFxProvider::default();
        let cb_res = self
            .runtime
            .block_on(stable.fetch_coinbase_usdt_usd(&client, 1));
        results.push(match cb_res {
            Ok(rates) => LiveProbeResult {
                provider: "coinbase".to_string(),
                endpoint: format!(
                    "{}/products/USDT-USD/candles?granularity=86400",
                    stable.coinbase_base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} USDT/USD daily rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "coinbase".to_string(),
                endpoint: stable.coinbase_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 4. Bitstamp USDC-USD
        let bs_res = self
            .runtime
            .block_on(stable.fetch_bitstamp_pair(&client, "USDC", "USD", 5, 1));
        results.push(match bs_res {
            Ok(rates) => LiveProbeResult {
                provider: "bitstamp".to_string(),
                endpoint: format!("{}/api/v2/ohlc/usdcusd/", stable.bitstamp_base_url),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} USDC/USD daily rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "bitstamp".to_string(),
                endpoint: stable.bitstamp_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 5. Kraken USDCUSD
        let kr_res = self
            .runtime
            .block_on(stable.fetch_kraken_usdc_usd(&client, 1));
        results.push(match kr_res {
            Ok(rates) => LiveProbeResult {
                provider: "kraken".to_string(),
                endpoint: format!(
                    "{}/0/public/OHLC?pair=USDCUSD&interval=1440",
                    stable.kraken_base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} USDC/USD daily rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "kraken".to_string(),
                endpoint: stable.kraken_base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 6. Frankfurter Fiat FX
        let ff = FrankfurterFxProvider::default();
        let ff_res = self.runtime.block_on(ff.fetch_range(
            &client,
            "USD",
            &["HKD", "CNY", "EUR"],
            NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            NaiveDate::from_ymd_opt(2026, 1, 6).unwrap(),
            1,
        ));
        results.push(match ff_res {
            Ok((rates, _)) => LiveProbeResult {
                provider: "frankfurter".to_string(),
                endpoint: format!(
                    "{}/v1/2026-01-02..2026-01-06?base=USD&symbols=HKD,CNY,EUR",
                    ff.base_url
                ),
                status: "passed".to_string(),
                http_status: Some(200),
                detail: format!("fetched {} fiat FX rates", rates.len()),
            },
            Err(e) => LiveProbeResult {
                provider: "frankfurter".to_string(),
                endpoint: ff.base_url,
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 7. Yahoo Chart v8 AAPL 2020-08-28..2020-09-01
        let yahoo = YahooChartV8Provider::default();
        let y_res = self.runtime.block_on(yahoo.fetch_chart_v8(
            &client,
            "NASDAQ:AAPL",
            "XNAS",
            "AAPL",
            1598486400,
            1599004800,
            1,
        ));
        results.push(match y_res {
            Ok((bars, actions, _)) => {
                let b_0828 = bars
                    .iter()
                    .find(|b| b.session_date == NaiveDate::from_ymd_opt(2020, 8, 28).unwrap())
                    .map(|b| to_canonical_text(b.close))
                    .unwrap_or_else(|| "n/a".to_string());
                LiveProbeResult {
                    provider: "yahoo".to_string(),
                    endpoint: format!(
                        "{}/v8/finance/chart/AAPL?interval=1d&events=div,split",
                        yahoo.base_url
                    ),
                    status: "passed".to_string(),
                    http_status: Some(200),
                    detail: format!(
                        "fetched {} bars, {} actions; 2020-08-28 restored raw close={}",
                        bars.len(),
                        actions.len(),
                        b_0828
                    ),
                }
            }
            Err(e) => LiveProbeResult {
                provider: "yahoo".to_string(),
                endpoint: yahoo.base_url.clone(),
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        // 8. EastMoney push2his AAPL fqt=0
        let em = EastMoneyUsEquityProvider::default();
        let em_res = self.runtime.block_on(em.fetch_kline_raw(
            &client,
            "NASDAQ:AAPL",
            "XNAS",
            "105.AAPL",
            "20200827",
            "20200901",
            1,
        ));
        results.push(match em_res {
            Ok((bars, _)) => {
                let b_0828 = bars
                    .iter()
                    .find(|b| b.session_date == NaiveDate::from_ymd_opt(2020, 8, 28).unwrap())
                    .map(|b| to_canonical_text(b.close))
                    .unwrap_or_else(|| "n/a".to_string());
                LiveProbeResult {
                    provider: "eastmoney".to_string(),
                    endpoint: format!(
                        "{}/api/qt/stock/kline/get?secid=105.AAPL&fqt=0",
                        em.base_url
                    ),
                    status: "passed".to_string(),
                    http_status: Some(200),
                    detail: format!(
                        "fetched {} raw bars; 2020-08-28 close={}",
                        bars.len(),
                        b_0828
                    ),
                }
            }
            Err(e) => LiveProbeResult {
                provider: "eastmoney".to_string(),
                endpoint: em.base_url,
                status: "failed".to_string(),
                http_status: extract_status(&e),
                detail: e.to_string(),
            },
        });

        let archive_month = NaiveDate::from_ymd_opt(2025, 3, 1).unwrap();
        let archive = self.runtime.block_on(binance.fetch_monthly_archive(
            &client,
            "BINANCE:BTC/USDT",
            "BTCUSDT",
            archive_month,
            1,
        ));
        results.push(match archive {
            Ok((bars,_,_))=>LiveProbeResult{provider:"binance-archive".into(),endpoint:format!("{}/data/spot/monthly/klines/BTCUSDT/1d/BTCUSDT-1d-2025-03.zip",binance.archive_base_url),status:if bars.len()==31 {"passed"}else{"failed"}.into(),http_status:Some(200),detail:format!("SHA-256 CHECKSUM verified; {} completed daily bars; microsecond timestamps parsed",bars.len())},
            Err(error)=>LiveProbeResult{provider:"binance-archive".into(),endpoint:binance.archive_base_url.clone(),status:"failed".into(),http_status:extract_status(&error),detail:error.to_string()},
        });
        let usdt_check = self
            .runtime
            .block_on(stable.fetch_bitstamp_pair(&client, "USDT", "USD", 5, 1));
        results.push(match usdt_check {
            Ok(rates) => LiveProbeResult {
                provider: "bitstamp-usdt".into(),
                endpoint: format!("{}/api/v2/ohlc/usdtusd/", stable.bitstamp_base_url),
                status: if rates.is_empty() { "failed" } else { "passed" }.into(),
                http_status: Some(200),
                detail: format!("fetched {} USDT/USD cross-check rates", rates.len()),
            },
            Err(error) => LiveProbeResult {
                provider: "bitstamp-usdt".into(),
                endpoint: stable.bitstamp_base_url.clone(),
                status: "failed".into(),
                http_status: extract_status(&error),
                detail: error.to_string(),
            },
        });
        for (symbol, start, end, split) in [
            (
                "NVDA",
                NaiveDate::from_ymd_opt(2024, 6, 6).unwrap(),
                NaiveDate::from_ymd_opt(2024, 6, 12).unwrap(),
                true,
            ),
            (
                "SPY",
                NaiveDate::from_ymd_opt(2025, 3, 17).unwrap(),
                NaiveDate::from_ymd_opt(2025, 3, 25).unwrap(),
                false,
            ),
        ] {
            let venue = if split { "XNAS" } else { "XNYS" };
            let id = format!("{}:{symbol}", if split { "NASDAQ" } else { "NYSE" });
            let response = self.runtime.block_on(yahoo.fetch_chart_v8(
                &client,
                &id,
                venue,
                symbol,
                start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                end.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
                1,
            ));
            results.push(match response {
                Ok((bars, actions, _)) => {
                    let expected = actions.iter().any(|action| {
                        if split {
                            action.action_type == delta_core::CorporateActionType::Split
                                && action.ratio_or_amount == Decimal::from(10)
                        } else {
                            action.action_type == delta_core::CorporateActionType::CashDividend
                                && action.ratio_or_amount > Decimal::ZERO
                        }
                    });
                    LiveProbeResult {
                        provider: format!("yahoo-{}", symbol.to_ascii_lowercase()),
                        endpoint: format!("{}/v8/finance/chart/{symbol}", yahoo.base_url),
                        status: if expected && !bars.is_empty() {
                            "passed"
                        } else {
                            "failed"
                        }
                        .into(),
                        http_status: Some(200),
                        detail: format!(
                            "{} raw bars; {} actions; expected {} observed={expected}",
                            bars.len(),
                            actions.len(),
                            if split { "10:1 split" } else { "cash dividend" }
                        ),
                    }
                }
                Err(error) => LiveProbeResult {
                    provider: format!("yahoo-{}", symbol.to_ascii_lowercase()),
                    endpoint: yahoo.base_url.clone(),
                    status: "failed".into(),
                    http_status: extract_status(&error),
                    detail: error.to_string(),
                },
            });
        }

        // ACT-03 private-service verification is separate from the public probe.
        let tiingo = TiingoEquityProvider::default();
        results.push(LiveProbeResult {
            provider: "tiingo".to_string(),
            endpoint: format!("{}/tiingo/daily/AAPL/prices", tiingo.base_url),
            status: "not-run".to_string(),
            http_status: None,
            detail:
                "ACT-03 private-service live verification not requested; offline fixture verified"
                    .to_string(),
        });

        // 10. Alpaca (ACT-03: remains not-run unless API key is configured)
        let alpaca = AlpacaEquityProvider::default();
        results.push(LiveProbeResult {
            provider: "alpaca".to_string(),
            endpoint: format!("{}/v1/corporate-actions", alpaca.base_url),
            status: "not-run".to_string(),
            http_status: None,
            detail:
                "ACT-03 private-service live verification not requested; offline fixture verified"
                    .to_string(),
        });
        results.push(LiveProbeResult {
            provider: "longport".into(),
            endpoint: "https://openapi.longportapp.com".into(),
            status: "not-run".into(),
            http_status: None,
            detail: "ACT-03 private-service configuration and SDK adapter deferred as LOOP-03"
                .into(),
        });
        for result in &mut results {
            if result.status == "passed" && result.detail.starts_with("fetched 0 ") {
                result.status = "failed".into();
            }
            if matches!(result.provider.as_str(), "yahoo" | "eastmoney")
                && !result.detail.contains("499.23")
            {
                result.status = "failed".into();
            }
        }

        results
    }
}

fn extract_status(err: &delta_data::ProviderError) -> Option<u16> {
    match err {
        delta_data::ProviderError::Http { status, .. } => *status,
        delta_data::ProviderError::RateLimited { .. } => Some(429),
        delta_data::ProviderError::ForbiddenEndpoint(_) => Some(451),
        _ => None,
    }
}

fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        #[cfg(windows)]
        let linked = {
            use std::os::windows::fs::MetadataExt as _;
            metadata.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let linked = metadata.file_type().is_symlink();
        if linked {
            return Err(std::io::Error::other("library contains a link"));
        }
        if metadata.is_dir() {
            out.extend(walk_files(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}
