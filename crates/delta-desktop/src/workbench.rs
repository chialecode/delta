//! Longbridge-styled GPUI desktop workbench (`S-15`, `M2-W03`..`M2-W07`, `M2-K01`, `M2-K02`, `M2-K06`).
//!
//! Built on `gpui-kit =0.9.0`:
//! - `DockSkin::new().dock_area("delta-workbench-dock", ...)` + `DockLayout` + `DockPosition` + `Panel`
//! - `DataTable` + `TableDelegate` for 100,000-row virtual scrolling (`TransactionsTableDelegate`)
//! - Self-contained `MetricCard`, `StatusBanner`, and `KlineChartView` (`crate::components`)
//! - Strictly depends only on `delta-app` and `gpui-kit` (`S-12`).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{NaiveDate, Utc};
#[allow(unused_imports)]
use delta_app::{
    apply_process_quiet_guards, count_visible_windows_for_current_process, parse_decimal_lossless,
    resolve_app_paths, run_m2_k02_viewport_bench, run_quiet_command, sanitize_terminal_output,
    seed_ac02_fixture_in_service, set_note_save_fault_injection_for_test, to_canonical_text,
    AccountBalanceSnapshotDto, AccountRecord, AdjustmentKind, AgentRuntime,
    AiReportVerificationStatus, AiReviewReport, AnalysisScope, AppHost, AppService, AttachmentDto,
    AutosaveStatus, BarRecord, BuildChannel, ChartContextDto, ChartMarkerDto, CommitImportReceipt,
    DataQualitySummaryDto, ErrorEnvelope, EvidenceRef, HoldingsAndLotsDto, ImportPreview,
    InstallIdentity, InstrumentSearchHit, JobId, JobSnapshot, JobState, KlineViewport,
    KlineWindowSliceDto, ManualValuation, MarkerKind, ModelConnection, ModelProtocol, NoteDto,
    NoteEditorSession, NoteTemplateKind, OverviewMetricsDto, PeriodBreakdownReport,
    PortfolioRecord, PreSendScopeSummary, QualityStatus, QuietJobObject, ReconciliationIssueRecord,
    ResultEnvelope, SavedReportStatusDto, ScopeTarget, StatementPreset, StructuredNoteFields,
    TransactionFilterDto, TransactionRowDto, WatchlistGroupDto,
};
use gpui_kit::base::TestTextExt as _;
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    dock::{
        BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, Panel, PanelEvent, PanelStyle,
    },
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, px, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    WindowOptions,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::components::{KlineChartView, MetricCard, StatusBanner, WorkbenchSystemState};
use crate::controller::{UiAction, WorkbenchController, WorkbenchUpdated};
use crate::i18n::{
    is_in_scope_nav_key, I18nCatalog, Locale, MarketColorConvention, PRIMARY_NAV_KEYS,
};
use crate::theme::{self, Chrome};
use crate::ui_forms::{FormFields, CENTER_FIELDS};

pub(crate) static RENDERED_TABLE_CELLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

// ============================================================================
// 1. Layout Persistence & Corruption Fallback (`S-15`, `M2-A-07`)
// ============================================================================

pub const LAYOUT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkbenchLayoutConfig {
    pub version: u32,
    pub active_nav: String,
    pub right_ai_open: bool,
    pub bottom_tasks_open: bool,
    pub color_convention: MarketColorConvention,
    pub locale: Locale,
    pub report_currency: String,
    pub display_timezone: String,
}

impl Default for WorkbenchLayoutConfig {
    fn default() -> Self {
        Self {
            version: LAYOUT_SCHEMA_VERSION,
            active_nav: "nav.overview".to_string(),
            right_ai_open: true,
            bottom_tasks_open: true,
            color_convention: MarketColorConvention::RedUpGreenDown,
            locale: Locale::ZhCn,
            report_currency: "USD".to_string(),
            display_timezone: "UTC".to_string(),
        }
    }
}

pub fn layout_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings").join("layout.json")
}

pub fn save_layout_config(
    data_dir: &Path,
    config: &WorkbenchLayoutConfig,
) -> Result<PathBuf, ErrorEnvelope> {
    let path = layout_config_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ErrorEnvelope::new("LAYOUT_SAVE_FAILED", e.to_string(), false))?;
    }
    let bytes = serde_json::to_vec_pretty(config)
        .map_err(|e| ErrorEnvelope::new("LAYOUT_SAVE_FAILED", e.to_string(), false))?;
    delta_app::ensure_channel_data_isolation(&path, BuildChannel::current())
        .map_err(|e| ErrorEnvelope::new("LAYOUT_SAVE_FAILED", e.to_string(), false))?;
    let temporary = path.with_file_name(format!("layout-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(ErrorEnvelope::new(
            "LAYOUT_SAVE_FAILED",
            error.to_string(),
            true,
        ));
    }
    Ok(path)
}

/// Load `<data_dir>/settings/layout.json`, or fall back to the default layout and return
/// `Some(WorkbenchSystemState::LayoutFallback)` if the file is corrupted or has an unsupported version (`S-15`).
pub fn load_or_default_layout(
    data_dir: &Path,
) -> (WorkbenchLayoutConfig, Option<WorkbenchSystemState>) {
    let path = layout_config_path(data_dir);
    if !path.exists() {
        return (WorkbenchLayoutConfig::default(), None);
    }
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return (
                WorkbenchLayoutConfig::default(),
                Some(WorkbenchSystemState::LayoutFallback {
                    reason: format!("读取 layout.json 失败 ({e})，已回退默认布局"),
                }),
            )
        }
    };
    match serde_json::from_str::<WorkbenchLayoutConfig>(&raw) {
        Ok(mut cfg) => {
            if cfg.version != LAYOUT_SCHEMA_VERSION || !is_in_scope_nav_key(&cfg.active_nav) {
                let reason = format!(
                    "layout.json 版本 ({}) 或导航键 ({}) 不受支持，已恢复默认工作台布局",
                    cfg.version, cfg.active_nav
                );
                cfg = WorkbenchLayoutConfig::default();
                (cfg, Some(WorkbenchSystemState::LayoutFallback { reason }))
            } else {
                (cfg, None)
            }
        }
        Err(e) => (
            WorkbenchLayoutConfig::default(),
            Some(WorkbenchSystemState::LayoutFallback {
                reason: format!("layout.json 解析失败 ({e})，已恢复默认工作台布局"),
            }),
        ),
    }
}

/// Verify via `cargo metadata` that `delta-desktop` does not directly depend on
/// `delta-data`, `delta-core`, `delta-model`, `rusqlite`, `tokio`, or `reqwest` (`S-12`, `M2-K01`, `M2-A-03`).
pub fn verify_desktop_dependency_boundary() -> Result<Vec<String>, ErrorEnvelope> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&manifest_dir);
    let res = run_quiet_command(
        OsStr::new("cargo"),
        ["metadata", "--locked", "--format-version", "1"],
        Some(root),
        &BTreeMap::new(),
    )
    .map_err(|e| ErrorEnvelope::new("METADATA_FAILED", e, false))?;
    if res.status_code != Some(0) {
        return Err(ErrorEnvelope::new("METADATA_FAILED", res.stderr, false));
    }
    let v: Value = serde_json::from_str(&res.stdout)
        .map_err(|e| ErrorEnvelope::new("METADATA_PARSE", e.to_string(), false))?;
    let pkg = v["packages"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|p| p["name"].as_str() == Some("delta-desktop"))
        })
        .ok_or_else(|| ErrorEnvelope::new("METADATA_PKG", "delta-desktop not found", false))?;
    let mut deps: Vec<String> = pkg["dependencies"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    deps.sort();
    deps.dedup();
    for forbidden in [
        "delta-data",
        "delta-core",
        "delta-model",
        "rusqlite",
        "tokio",
        "reqwest",
    ] {
        if deps.iter().any(|d| d == forbidden) {
            return Err(ErrorEnvelope::new(
                "BOUNDARY_VIOLATION",
                format!("delta-desktop directly depends on forbidden crate '{forbidden}'"),
                false,
            ));
        }
    }
    Ok(deps)
}

// ============================================================================
// 2. Virtual Scrolling `TransactionsTableDelegate` (`S-15`, `M2-W04`, `M2-K02`)
// ============================================================================

#[derive(Clone)]
pub struct TransactionsTableDelegate {
    columns: Vec<Column>,
    rows: Vec<TransactionRowDto>,
}

impl TransactionsTableDelegate {
    pub fn new(rows: Vec<TransactionRowDto>) -> Self {
        let columns = vec![
            Column::new("occurred_at", "时间 (UTC)").width(px(150.0)),
            Column::new("account_id", "账户").width(px(90.0)),
            Column::new("event_type", "类型").width(px(90.0)),
            Column::new("asset_id", "标的/币种").width(px(120.0)),
            Column::new("quantity", "数量")
                .width(px(110.0))
                .text_right(),
            Column::new("price", "成交价").width(px(110.0)).text_right(),
            Column::new("fee", "费用").width(px(100.0)).text_right(),
            Column::new("source", "来源批次/ID").width(px(140.0)),
        ];
        Self { columns, rows }
    }

    pub fn rows(&self) -> &[TransactionRowDto] {
        &self.rows
    }

    pub fn set_rows(&mut self, rows: Vec<TransactionRowDto>) {
        self.rows = rows;
    }

    pub fn synthetic_100k_rows() -> Vec<TransactionRowDto> {
        let mut out = Vec::with_capacity(100_000);
        for i in 0..100_000usize {
            out.push(TransactionRowDto {
                event_id: format!("EV-SYN-{i:06}"),
                account_id: if i % 2 == 0 {
                    "ACC-1".to_string()
                } else {
                    "ACC-2".to_string()
                },
                event_type: if i % 3 == 0 {
                    "buy".to_string()
                } else if i % 3 == 1 {
                    "sell".to_string()
                } else {
                    "dividend".to_string()
                },
                occurred_at: format!("2026-01-{:02}T15:00:00Z", 1 + (i % 28)),
                asset_id: "NASDAQ:AAPL".to_string(),
                instrument_id: Some("NASDAQ:AAPL".to_string()),
                quantity: Decimal::from((i % 50 + 1) as i64),
                price: Some(Decimal::from(100 + (i % 25) as i64)),
                quote_currency: "USD".to_string(),
                fee_amount: Decimal::ONE,
                fee_currency: "USD".to_string(),
                cost_known: true,
                revision: 1,
                source_ref: Some(format!("SRC-{i:06}")),
                batch_id: Some("BATCH-100K".to_string()),
                raw_payload: None,
            });
        }
        out
    }

    pub fn format_cell(&self, row_ix: usize, col_ix: usize) -> String {
        let Some(r) = self.rows.get(row_ix) else {
            return String::new();
        };
        match col_ix {
            0 => r.occurred_at.clone(),
            1 => r.account_id.clone(),
            2 => r.event_type.clone(),
            3 => r
                .instrument_id
                .clone()
                .unwrap_or_else(|| r.asset_id.clone()),
            4 => to_canonical_text(r.quantity),
            5 => r
                .price
                .map(|p| format!("{} {}", to_canonical_text(p), r.quote_currency))
                .unwrap_or_else(|| "—".to_string()),
            6 => format!("{} {}", to_canonical_text(r.fee_amount), r.fee_currency),
            7 => r.source_ref.clone().unwrap_or_else(|| "—".to_string()),
            _ => String::new(),
        }
    }
}

impl TableDelegate for TransactionsTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        RENDERED_TABLE_CELLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let text = SharedString::from(self.format_cell(row_ix, col_ix));
        h_flex()
            .size_full()
            .px_2()
            .items_center()
            .text_xs()
            .font_family(cx.theme().mono_font_family.clone())
            .text_color(cx.theme().foreground)
            .child(text.clone())
            .test_text(text)
    }
}

// ============================================================================
// 3. Shared Workbench State & Sub-Views (`M2-W03`..`M2-W07`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetsSubTab {
    Holdings,
    FifoLots,
    Flows,
    Reconciliation,
    ManualValuations,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerSelectionOutcome {
    TradeFill(TransactionRowDto),
    Note(NoteDto),
}

#[derive(Clone)]
pub struct WorkbenchSharedState {
    pub host: AppHost,
    pub runtime: AgentRuntime,
    pub data_dir: PathBuf,
    pub layout: WorkbenchLayoutConfig,
    pub scope: AnalysisScope,
    pub transaction_filter: TransactionFilterDto,
    pub assets_sub_tab: AssetsSubTab,
    pub system_states: Vec<WorkbenchSystemState>,
    pub overview: Option<ResultEnvelope<OverviewMetricsDto>>,
    pub breakdown: Option<ResultEnvelope<PeriodBreakdownReport>>,
    pub holdings_lots: Option<ResultEnvelope<HoldingsAndLotsDto>>,
    pub transactions: Option<ResultEnvelope<Vec<TransactionRowDto>>>,
    pub reconciliation_issues: Option<ResultEnvelope<Vec<ReconciliationIssueRecord>>>,
    pub data_quality: Option<ResultEnvelope<DataQualitySummaryDto>>,
    pub accounts: Vec<AccountRecord>,
    pub portfolios: Vec<PortfolioRecord>,
    pub manual_valuations: Vec<ManualValuation>,
    pub import_preview: Option<ImportPreview>,
    pub last_import_receipt: Option<CommitImportReceipt>,
    pub last_import_error: Option<ErrorEnvelope>,
    pub held_import_rows: Vec<String>,
    pub active_sync_job: Option<JobId>,
    pub last_job_snapshot: Option<JobSnapshot>,
    pub active_instrument: String,
    pub active_timeframe: String,
    pub active_adjustment: AdjustmentKind,
    pub kline_viewport: KlineViewport,
    pub kline_slice: Option<KlineWindowSliceDto>,
    pub pinned_chart_version: Option<i64>,
    pub latest_dataset_version: i64,
    pub selected_marker_outcome: Option<MarkerSelectionOutcome>,
    pub watchlist_groups: Vec<WatchlistGroupDto>,
    pub search_hits: Vec<InstrumentSearchHit>,
    pub note_editor: NoteEditorSession,
    pub notes_search_query: String,
    pub notes_search_mode: String,
    pub notes_results: Vec<NoteDto>,
    pub notes_result_notice: String,
    pub pre_send_scope: Option<PreSendScopeSummary>,
    pub model_connection_settings: Option<ModelConnection>,
    pub last_ai_report: Option<AiReviewReport>,
    pub opened_ai_evidence: Option<Value>,
    pub last_ai_error: Option<ErrorEnvelope>,
    pub ui_notice: String,
    pub ui_error: Option<ErrorEnvelope>,
    pub saved_ai_reports: Vec<AiReviewReport>,
    pub deletion_preview: Option<delta_app::DeleteLibraryPreviewDto>,
    pub library_deleted: bool,
    pub training: crate::training_ui::TrainingUiState,
}

impl WorkbenchSharedState {
    pub fn new(
        host: AppHost,
        data_dir: PathBuf,
        as_of_date: NaiveDate,
    ) -> Result<Self, ErrorEnvelope> {
        let mut state = Self::loading(host, data_dir, as_of_date)?;
        let (layout, fallback) = load_or_default_layout(&state.data_dir);
        state.layout = layout;
        state.scope.report_currency = state.layout.report_currency.clone();
        if let Some(fallback) = fallback {
            state.system_states.push(fallback);
        }
        state.refresh_all()?;
        Ok(state)
    }

    /// Only creates presentation state. Disk/database work starts on the GPUI background executor.
    pub fn loading(
        host: AppHost,
        data_dir: PathBuf,
        as_of_date: NaiveDate,
    ) -> Result<Self, ErrorEnvelope> {
        let runtime = AgentRuntime::new(host.clone())?;
        let layout = WorkbenchLayoutConfig::default();
        let system_states = Vec::new();
        let scope = AnalysisScope::all_accounts(as_of_date, layout.report_currency.clone());
        let note_editor = NoteEditorSession::new(
            format!("note:{}", uuid::Uuid::new_v4()),
            "新建复盘笔记",
            NoteTemplateKind::PostTrade,
            false,
        );

        let state = Self {
            host,
            runtime,
            data_dir,
            layout,
            scope,
            transaction_filter: TransactionFilterDto::default(),
            assets_sub_tab: AssetsSubTab::Holdings,
            system_states,
            overview: None,
            breakdown: None,
            holdings_lots: None,
            transactions: None,
            reconciliation_issues: None,
            data_quality: None,
            accounts: Vec::new(),
            portfolios: Vec::new(),
            manual_valuations: Vec::new(),
            import_preview: None,
            last_import_receipt: None,
            last_import_error: None,
            held_import_rows: Vec::new(),
            active_sync_job: None,
            last_job_snapshot: None,
            active_instrument: "NASDAQ:AAPL".to_string(),
            active_timeframe: "1d".to_string(),
            active_adjustment: AdjustmentKind::Raw,
            kline_viewport: KlineViewport::new(0, 100),
            kline_slice: None,
            pinned_chart_version: None,
            latest_dataset_version: 0,
            selected_marker_outcome: None,
            watchlist_groups: Vec::new(),
            search_hits: Vec::new(),
            note_editor,
            notes_search_query: String::new(),
            notes_search_mode: "empty_all".to_string(),
            notes_results: Vec::new(),
            notes_result_notice: String::new(),
            pre_send_scope: None,
            model_connection_settings: None,
            last_ai_report: None,
            opened_ai_evidence: None,
            last_ai_error: None,
            ui_notice: "正在读取本地资料库…".into(),
            ui_error: None,
            saved_ai_reports: Vec::new(),
            deletion_preview: None,
            library_deleted: false,
            training: crate::training_ui::TrainingUiState::default(),
        };
        Ok(state)
    }

    pub fn refresh_all(&mut self) -> Result<(), ErrorEnvelope> {
        let gen = self.host.current_generation();
        self.overview = None;
        self.breakdown = None;
        self.holdings_lots = None;
        self.transactions = None;
        self.reconciliation_issues = None;
        self.data_quality = None;
        self.kline_slice = None;
        self.notes_results.clear();
        self.notes_result_notice.clear();
        self.accounts = self.host.list_accounts()?;
        self.portfolios = self.host.list_portfolios()?;
        self.held_import_rows = self.host.list_held_import_rows()?;
        self.latest_dataset_version = self.host.execute(|svc| Ok(svc.latest_dataset_version()))?;
        self.model_connection_settings = self.runtime.model_connection_settings()?;
        crate::training_ui::refresh_training_lists(self)?;
        let mut requested = self.scope.clone();
        requested.versions_frozen = false;
        requested.dataset_version = 0;
        requested.ledger_revision = 0;
        self.scope = self
            .host
            .query_scope_describe(requested, "ui-scope", gen)?
            .scope;

        self.system_states.retain(|s| {
            matches!(
                s,
                WorkbenchSystemState::LayoutFallback { .. }
                    | WorkbenchSystemState::Cancelled { .. }
                    | WorkbenchSystemState::SaveFailed { .. }
                    | WorkbenchSystemState::RateLimitedOrAuthFailed { .. }
            )
        });

        if self.accounts.is_empty() {
            self.system_states.push(WorkbenchSystemState::EmptyLibrary);
            self.ui_notice = "资料库已创建。请添加账户或载入离线示例。".into();
            return Ok(());
        }

        {
            let ov = self.host.query_overview(self.scope.clone(), "ui-ov", gen)?;
            if ov.quality == QualityStatus::Partial {
                self.system_states
                    .push(WorkbenchSystemState::PartialValuation {
                        missing: ov.missing_inputs.clone(),
                    });
            }
            self.overview = Some(ov);
        }
        {
            let bd = self
                .host
                .query_period_breakdown(self.scope.clone(), "ui-bd", gen)?;
            self.breakdown = Some(bd);
        }
        {
            let hl = self
                .host
                .query_holdings_lots(self.scope.clone(), "ui-hl", gen)?;
            self.holdings_lots = Some(hl);
        }
        {
            let txs = self.host.query_transactions(
                self.scope.clone(),
                self.transaction_filter.clone(),
                "ui-txs",
                gen,
            )?;
            self.transactions = Some(txs);
        }
        {
            let recon =
                self.host
                    .query_reconciliation_issues(self.scope.clone(), "ui-recon", gen)?;
            self.reconciliation_issues = Some(recon);
        }
        {
            let dq = self
                .host
                .query_data_quality(self.scope.clone(), "ui-dq", gen)?;
            self.data_quality = Some(dq);
        }

        self.manual_valuations = self
            .host
            .list_manual_valuations(Some(self.scope.resolved_accounts.clone()))?;
        self.watchlist_groups = self.host.list_watchlist_groups()?;

        let dsv = self
            .pinned_chart_version
            .unwrap_or(self.latest_dataset_version);
        let mut chart_scope = self.scope.clone();
        chart_scope.dataset_version = dsv;
        if let Ok(slice) = self.host.load_scoped_kline_window_slice(
            chart_scope,
            &self.active_instrument,
            &self.active_timeframe,
            self.active_adjustment,
            self.kline_viewport.clone(),
            120,
            gen,
        ) {
            self.kline_viewport.total_bars = slice.total_visible_bars;
            self.kline_viewport.visible_start = slice.visible_start_index;
            self.kline_viewport.visible_end = slice.visible_end_index;
            self.kline_slice = Some(slice);
        }

        self.refresh_notes()?;

        self.pre_send_scope = self.runtime.build_pre_send_summary(&self.scope).ok();
        self.saved_ai_reports = self
            .runtime
            .list_saved_ai_reports_for_scope(self.scope.clone())?;
        self.ui_notice = "本地数据已更新".into();
        Ok(())
    }

    pub fn navigate_to(&mut self, nav_key: &str) -> Result<(), ErrorEnvelope> {
        if !is_in_scope_nav_key(nav_key) {
            return Err(ErrorEnvelope::new(
                "OUT_OF_SCOPE_NAV",
                format!("navigation key '{nav_key}' is not in delivered M2 scope"),
                false,
            ));
        }
        self.layout.active_nav = nav_key.to_string();
        let _ = save_layout_config(&self.data_dir, &self.layout);
        Ok(())
    }

    pub fn drilldown_from_overview_to_flows(
        &mut self,
        instrument_filter: Option<String>,
    ) -> Result<(), ErrorEnvelope> {
        self.layout.active_nav = "nav.assets".to_string();
        self.assets_sub_tab = AssetsSubTab::Flows;
        self.transaction_filter.instrument_id = instrument_filter;
        let gen = self.host.current_generation();
        if let Ok(txs) = self.host.query_transactions(
            self.scope.clone(),
            self.transaction_filter.clone(),
            "ui-drilldown-txs",
            gen,
        ) {
            self.transactions = Some(txs);
        }
        Ok(())
    }

    pub fn set_report_currency(&mut self, report_currency: &str) -> Result<(), ErrorEnvelope> {
        self.layout.report_currency = report_currency.trim().to_ascii_uppercase();
        self.scope.report_currency = self.layout.report_currency.clone();
        self.host.advance_generation();
        let _ = save_layout_config(&self.data_dir, &self.layout);
        self.refresh_all()
    }

    pub fn preview_statement_import(
        &mut self,
        account_id: &str,
        preset: StatementPreset,
        file_path: PathBuf,
    ) -> Result<ImportPreview, ErrorEnvelope> {
        match self
            .host
            .import_statement(account_id, preset, file_path, false)
        {
            Ok((preview, _)) => {
                self.import_preview = Some(preview.clone());
                self.last_import_error = None;
                Ok(preview)
            }
            Err(err) => {
                self.last_import_error = Some(err.clone());
                Err(err)
            }
        }
    }

    pub fn commit_statement_import(
        &mut self,
        account_id: &str,
        preset: StatementPreset,
        file_path: PathBuf,
    ) -> Result<(ImportPreview, CommitImportReceipt), ErrorEnvelope> {
        if let Some(preview) = &self.import_preview {
            let current = self
                .host
                .import_statement(account_id, preset, file_path.clone(), false)?
                .0;
            if preview.account_id != account_id
                || preview.preset != preset
                || preview.file_hash != current.file_hash
            {
                return Err(ErrorEnvelope::new(
                    "IMPORT_PREVIEW_STALE",
                    "文件或映射已改变，请重新预览后提交",
                    false,
                ));
            }
        }
        let (preview, receipt_opt) = self
            .host
            .import_statement(account_id, preset, file_path, true)?;
        let receipt = receipt_opt.ok_or_else(|| {
            ErrorEnvelope::new("IMPORT_NOT_COMMITTED", "missing commit receipt", false)
        })?;
        self.import_preview = Some(preview.clone());
        self.last_import_receipt = Some(receipt.clone());
        self.last_import_error = None;
        self.refresh_all()?;
        Ok((preview, receipt))
    }

    pub(crate) fn start_verification_cancellation_job(
        &mut self,
        duration_ms: u64,
    ) -> Result<JobId, ErrorEnvelope> {
        let job_id = self.host.execute(move |svc| {
            Ok(svc.jobs().spawn_job(
                "verification_cancellation_worker",
                Duration::from_secs(30),
                move |cancel| {
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_millis(duration_ms) {
                        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                            return Ok(());
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(())
                },
            ))
        })?;
        self.active_sync_job = Some(job_id);
        self.system_states
            .retain(|s| !matches!(s, WorkbenchSystemState::Cancelled { .. }));
        self.system_states.push(WorkbenchSystemState::Syncing {
            detail: "受控取消验证作业".to_string(),
        });
        Ok(job_id)
    }

    pub fn cancel_active_sync_job(&mut self) -> Result<JobSnapshot, ErrorEnvelope> {
        let job_id = self.active_sync_job.take().ok_or_else(|| {
            ErrorEnvelope::new("NO_ACTIVE_SYNC", "no active sync job to cancel", false)
        })?;
        let snap = self.host.cancel_job(job_id)?;
        self.last_job_snapshot = Some(snap.clone());
        self.system_states
            .retain(|s| !matches!(s, WorkbenchSystemState::Syncing { .. }));
        self.system_states.push(WorkbenchSystemState::Cancelled {
            stage: snap.name.clone(),
        });
        Ok(snap)
    }

    pub fn select_chart_marker(
        &mut self,
        marker_id: &str,
    ) -> Result<MarkerSelectionOutcome, ErrorEnvelope> {
        let slice = self
            .kline_slice
            .as_ref()
            .ok_or_else(|| ErrorEnvelope::new("NO_KLINE_SLICE", "no active K-line slice", false))?;
        let layout = slice
            .markers
            .iter()
            .find(|m| m.marker.id == marker_id)
            .ok_or_else(|| {
                ErrorEnvelope::new("MARKER_NOT_FOUND", format!("no marker {marker_id}"), false)
            })?;
        let target_id = layout.marker.target_id.clone();
        match layout.marker.kind {
            MarkerKind::BuyFill | MarkerKind::SellFill => {
                let gen = self.host.current_generation();
                let row = self
                    .host
                    .get_scoped_transaction(self.scope.clone(), &target_id, gen)?;
                let out = MarkerSelectionOutcome::TradeFill(row);
                self.selected_marker_outcome = Some(out.clone());
                Ok(out)
            }
            MarkerKind::Note | MarkerKind::Plan => {
                let note = self.host.get_scoped_note(
                    self.scope.clone(),
                    &target_id,
                    self.host.current_generation(),
                )?;
                let out = MarkerSelectionOutcome::Note(note);
                self.selected_marker_outcome = Some(out.clone());
                Ok(out)
            }
        }
    }

    pub fn jump_to_chart_context(
        &mut self,
        ctx_id: &str,
    ) -> Result<ChartContextDto, ErrorEnvelope> {
        let ctx = self.host.get_chart_context(ctx_id)?;
        self.layout.active_nav = "nav.market".to_string();
        self.active_instrument = ctx.instrument_id.clone();
        self.active_timeframe = ctx.timeframe.clone();
        self.active_adjustment = ctx.adjustment;
        self.pinned_chart_version = ctx.dataset_version;
        self.latest_dataset_version = ctx.latest_dataset_version;

        let gen = self.host.current_generation();
        let dsv = ctx.dataset_version.unwrap_or(ctx.latest_dataset_version);
        let mut vp = KlineViewport::new(0, 100);
        vp.indicator_config = delta_app::parse_indicator_config(&ctx.indicators_json)?;
        vp.requested_anchor_date = Some(ctx.anchor_date.unwrap_or(ctx.visible_end_date));
        vp.requested_window = Some((ctx.visible_start_date, ctx.visible_end_date));
        let mut scope = self.scope.clone();
        scope.dataset_version = dsv;
        let windowed = self.host.load_scoped_kline_window_slice(
            scope,
            &self.active_instrument,
            &self.active_timeframe,
            self.active_adjustment,
            vp.clone(),
            1000,
            gen,
        )?;
        vp.total_bars = windowed.total_visible_bars;
        vp.visible_start = windowed.visible_start_index;
        vp.visible_end = windowed.visible_end_index;
        if !windowed.bars.is_empty() {
            let anchor = ctx.anchor_date.unwrap_or(ctx.visible_end_date);
            let local = windowed
                .bars
                .partition_point(|bar| bar.session_date < anchor)
                .min(windowed.bars.len() - 1);
            vp.crosshair_index = Some(vp.visible_start + local);
        }
        vp.requested_anchor_date = None;
        vp.requested_window = None;
        self.kline_viewport = vp;
        self.kline_slice = Some(windowed);
        Ok(ctx)
    }

    pub fn edit_and_autosave_note(&mut self, new_body: String) -> Result<i64, ErrorEnvelope> {
        self.note_editor.begin_edit(new_body);
        let mut editor_clone = self.note_editor.clone();
        let scope = self.scope.clone();
        let res = self.host.execute(move |svc| {
            let r = svc.flush_scoped_note_editor_autosave(&scope, &mut editor_clone);
            Ok((editor_clone, r))
        })?;
        self.note_editor = res.0;
        match res.1 {
            Ok(rev) => {
                self.system_states
                    .retain(|s| !matches!(s, WorkbenchSystemState::SaveFailed { .. }));
                self.refresh_all()?;
                Ok(rev)
            }
            Err(err) => {
                self.system_states.push(WorkbenchSystemState::SaveFailed {
                    detail: err.message.clone(),
                });
                Err(err)
            }
        }
    }

    pub fn search_notes(&mut self, query: &str) -> Result<(Vec<NoteDto>, String), ErrorEnvelope> {
        self.notes_search_query = query.to_string();
        self.refresh_notes()?;
        let mode = if query.chars().count() < 3 {
            "bounded_like_fallback"
        } else {
            "fts5_trigram"
        }
        .to_string();
        self.notes_search_mode = mode.clone();
        Ok((self.notes_results.clone(), mode))
    }

    pub(crate) fn refresh_notes(&mut self) -> Result<(), ErrorEnvelope> {
        let result = self.host.query_notes(
            self.scope.clone(),
            &self.notes_search_query,
            false,
            "ui-notes",
            self.host.current_generation(),
        )?;
        self.notes_result_notice = if result.quality == QualityStatus::Partial {
            result.warnings.join("；")
        } else {
            String::new()
        };
        self.notes_results = result.value;
        Ok(())
    }

    pub fn request_ai_review(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<AiReviewReport, ErrorEnvelope> {
        self.pre_send_scope = self.runtime.build_pre_send_summary(&self.scope).ok();
        match self.runtime.run_review(
            session_id,
            prompt,
            self.scope.clone(),
            Arc::new(AtomicBool::new(false)),
        ) {
            Ok(rep) => {
                self.last_ai_report = Some(rep.clone());
                self.last_ai_error = None;
                self.system_states
                    .retain(|s| !matches!(s, WorkbenchSystemState::ModelUnavailable { .. }));
                Ok(rep)
            }
            Err(err) => {
                self.last_ai_error = Some(err.clone());
                if err.code == "model_not_configured" || err.code == "provider_unavailable" {
                    self.system_states
                        .retain(|s| !matches!(s, WorkbenchSystemState::ModelUnavailable { .. }));
                    self.system_states
                        .push(WorkbenchSystemState::ModelUnavailable {
                            reason: err.message.clone(),
                        });
                }
                Err(err)
            }
        }
    }
}

// ============================================================================
// 4. Longbridge `DockArea` Panels & `DeltaWorkbench` (`S-15`, `M2-K01`, `M2-W03`..`M2-W07`)
// ============================================================================

pub struct CenterWorkspacePanel {
    focus_handle: FocusHandle,
    pub(crate) training_workspace_focus: FocusHandle,
    pub(crate) training_ticket_focus: FocusHandle,
    pub(crate) state: Arc<Mutex<WorkbenchSharedState>>,
    pub(crate) controller: Entity<WorkbenchController>,
    pub(crate) fields: FormFields,
    pub(crate) note_body: Entity<TextareaState>,
    pub(crate) autosave_task: Option<Task<()>>,
    pub(crate) applied_settings: Option<(String, String)>,
    pub(crate) applied_model_settings: Option<ModelConnection>,
    pub(crate) draft_model_connection_id: String,
    _subscriptions: Vec<Subscription>,
    flows_table: Entity<TableState<TransactionsTableDelegate>>,
    plot_bounds: crate::chart_canvas::PlotBounds,
    chart_drag: Option<(gpui_kit::Point<gpui_kit::Pixels>, KlineViewport)>,
    pub(crate) training_plot_bounds: crate::chart_canvas::PlotBounds,
    pub(crate) training_chart_drag: Option<(gpui_kit::Point<gpui_kit::Pixels>, KlineViewport)>,
}

impl CenterWorkspacePanel {
    pub fn new(
        state: Arc<Mutex<WorkbenchSharedState>>,
        controller: Entity<WorkbenchController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_rows = state
            .lock()
            .unwrap()
            .transactions
            .as_ref()
            .map(|e| e.value.clone())
            .unwrap_or_default();
        let delegate = TransactionsTableDelegate::new(initial_rows);
        let flows_table = cx.new(|cx| TableState::new(delegate, window, cx));
        let fields = FormFields::new(CENTER_FIELDS, window, cx);
        let note_body = cx.new(|cx| TextareaState::new(window, cx));
        let mut subscriptions = vec![
            cx.subscribe_in(
                &controller,
                window,
                |panel, _, _: &WorkbenchUpdated, window, cx| {
                    let rows = panel
                        .state
                        .lock()
                        .unwrap()
                        .transactions
                        .as_ref()
                        .map(|result| result.value.clone())
                        .unwrap_or_default();
                    panel.flows_table.update(cx, |table, cx| {
                        table.delegate_mut().set_rows(rows);
                        cx.notify();
                    });
                    panel.sync_inputs(window, cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &flows_table,
                window,
                |panel, table, event: &TableEvent, window, cx| {
                    let index = match event {
                        TableEvent::SelectRow(index) | TableEvent::DoubleClickedRow(index) => {
                            Some(*index)
                        }
                        TableEvent::SelectCell(index, _) => Some(*index),
                        _ => None,
                    };
                    if let Some(id) = index.and_then(|index| {
                        table
                            .read(cx)
                            .delegate()
                            .rows()
                            .get(index)
                            .map(|row| row.event_id.clone())
                    }) {
                        panel.dispatch(Ok(UiAction::SelectTransaction(id)), window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &note_body,
                window,
                |panel, _, event: &InputEvent, window, cx| {
                    panel.note_input_event(event, window, cx)
                },
            ),
        ];
        for key in [
            "form.note_title",
            "form.note_tags",
            "form.thesis",
            "form.invalidation",
            "form.planned_risk",
            "form.execution",
            "form.emotion",
            "form.conclusion",
        ] {
            subscriptions.push(cx.subscribe_in(
                fields.get(key),
                window,
                |panel, _, event: &InputEvent, window, cx| {
                    panel.note_input_event(event, window, cx)
                },
            ));
        }
        Self {
            focus_handle: cx.focus_handle(),
            training_workspace_focus: cx.focus_handle(),
            training_ticket_focus: cx.focus_handle(),
            state,
            controller,
            fields,
            note_body,
            autosave_task: None,
            applied_settings: None,
            applied_model_settings: None,
            draft_model_connection_id: format!("model:{}", uuid::Uuid::new_v4()),
            _subscriptions: subscriptions,
            flows_table,
            plot_bounds: Default::default(),
            chart_drag: None,
            training_plot_bounds: Default::default(),
            training_chart_drag: None,
        }
    }

    pub fn flows_table(&self) -> &Entity<TableState<TransactionsTableDelegate>> {
        &self.flows_table
    }

    pub fn training_workspace_focus(&self) -> &FocusHandle {
        &self.training_workspace_focus
    }

    pub fn training_ticket_focus(&self) -> &FocusHandle {
        &self.training_ticket_focus
    }

    fn chart_pointer(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        click: bool,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = *self.plot_bounds.lock().unwrap() else {
            return;
        };
        let Some((x, y)) = crate::chart_canvas::pointer_ratio(position, bounds) else {
            return;
        };
        let mut state = self.state.lock().unwrap();
        let index = state.kline_slice.as_ref().and_then(|slice| {
            (!slice.bars.is_empty()).then(|| {
                slice.visible_start_index
                    + ((x * slice.bars.len() as f32).floor() as usize).min(slice.bars.len() - 1)
            })
        });
        let price = state.kline_slice.as_ref().and_then(|slice| {
            let min = slice
                .lod_candles
                .iter()
                .map(|candle| candle.low)
                .reduce(f64::min)?;
            let max = slice
                .lod_candles
                .iter()
                .map(|candle| candle.high)
                .reduce(f64::max)?;
            Some(min + (max - min) * f64::from(1. - y))
        });
        if index != state.kline_viewport.crosshair_index
            || price != state.kline_viewport.crosshair_price
        {
            state.kline_viewport.move_crosshair(index, price);
            cx.notify();
        }
        let target = click
            .then(|| {
                state.kline_slice.as_ref().and_then(|slice| {
                    slice
                        .markers
                        .iter()
                        .filter_map(|marker| {
                            let dx = (marker.x_ratio - x) * f32::from(bounds.size.width);
                            let dy = (marker.y_ratio - y) * f32::from(bounds.size.height);
                            let distance = dx.hypot(dy);
                            (distance <= 9.).then_some((distance, marker.marker.id.clone()))
                        })
                        .min_by(|left, right| left.0.total_cmp(&right.0))
                        .map(|(_, id)| id)
                })
            })
            .flatten();
        if click && target.is_none() {
            self.chart_drag = Some((position, state.kline_viewport.clone()));
        }
        drop(state);
        if let (Some(id), Some(window)) = (target, window) {
            self.dispatch(Ok(UiAction::SelectMarker(id)), window, cx);
        }
    }
}

impl BasePanel for CenterWorkspacePanel {
    fn panel_name(&self) -> &'static str {
        "CenterWorkspacePanel"
    }

    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl Panel for CenterWorkspacePanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let nav = self.state.lock().unwrap().layout.active_nav.clone();
        I18nCatalog::tr(&nav)
    }

    fn title_bar(&self, _cx: &App) -> bool {
        false
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for CenterWorkspacePanel {}

impl Focusable for CenterWorkspacePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CenterWorkspacePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let guard = self.state.lock().unwrap();
        let ccy = guard.scope.report_currency.clone();
        let conv = guard.layout.color_convention;
        let active_nav = guard.layout.active_nav.clone();

        // Build Overview MetricCards (`M2-K01`, `M2-A-03`, `M2-W04`)
        let (cash_opt, nav_opt, pnl_opt, inv_opt, qual, cov_note, recon_cnt) =
            if let Some(ov) = &guard.overview {
                let is_partial = ov.quality == QualityStatus::Partial;
                (
                    Some(ov.value.cash_value),
                    if is_partial {
                        Some(ov.value.known_partial_net_worth)
                    } else {
                        Some(ov.value.net_worth)
                    },
                    ov.value.total_pnl,
                    Some(ov.value.net_invested),
                    ov.quality,
                    ov.value.coverage_summary.clone(),
                    ov.value.pending_reconciliation_count,
                )
            } else {
                (
                    None,
                    None,
                    None,
                    None,
                    QualityStatus::Unavailable,
                    "尚未加载".to_string(),
                    0,
                )
            };

        let cash_card = MetricCard::new(
            "card-cash",
            I18nCatalog::tr("card.cash"),
            cash_opt,
            ccy.clone(),
            qual,
        )
        .with_color_convention(conv)
        .with_note("可用现金与外币折算")
        .with_drilldown("assets.flows");

        let nav_card = MetricCard::new(
            "card-nav",
            I18nCatalog::tr("card.nav"),
            nav_opt,
            ccy.clone(),
            qual,
        )
        .with_color_convention(conv)
        .with_note(cov_note.clone())
        .with_drilldown("assets.holdings");

        let pnl_card = MetricCard::new(
            "card-total-pnl",
            I18nCatalog::tr("card.total_pnl"),
            pnl_opt,
            ccy.clone(),
            qual,
        )
        .with_signed_pnl(true)
        .with_color_convention(conv)
        .with_note("已实现 + 未实现损益")
        .with_drilldown("assets.flows");

        let inv_card = MetricCard::new(
            "card-net-invested",
            I18nCatalog::tr("card.net_invested"),
            inv_opt,
            ccy.clone(),
            qual,
        )
        .with_color_convention(conv)
        .with_note(format!("待对账差异: {recon_cnt}"));

        // Period breakdown cards
        let (real_opt, unreal_opt, div_opt, fee_val) = if let Some(bd) = &guard.breakdown {
            (
                bd.value.realized_pnl,
                bd.value.unrealized_pnl,
                bd.value.dividend_and_interest_income,
                Some(bd.value.explanatory_total_fees),
            )
        } else {
            (None, None, None, None)
        };
        let realized_card = MetricCard::new(
            "card-realized-pnl",
            I18nCatalog::tr("card.realized_pnl"),
            real_opt,
            ccy.clone(),
            qual,
        )
        .with_signed_pnl(true)
        .with_color_convention(conv);
        let unrealized_card = MetricCard::new(
            "card-unrealized-pnl",
            I18nCatalog::tr("card.unrealized_pnl"),
            unreal_opt,
            ccy.clone(),
            qual,
        )
        .with_signed_pnl(true)
        .with_color_convention(conv);
        let div_card = MetricCard::new(
            "card-dividend",
            I18nCatalog::tr("card.dividend_interest"),
            div_opt,
            ccy.clone(),
            qual,
        )
        .with_color_convention(conv);
        let fee_card = MetricCard::new(
            "card-fee",
            I18nCatalog::tr("card.fee_total"),
            fee_val,
            ccy.clone(),
            qual,
        )
        .with_color_convention(conv);

        // System state banners
        let banners: Vec<StatusBanner> = guard
            .system_states
            .iter()
            .enumerate()
            .map(|(idx, st)| {
                StatusBanner::new(format!("sys-banner-{idx}"), st.clone()).with_locale(Locale::ZhCn)
            })
            .collect();

        // K-line chart view if available
        let kline_view_opt = guard.kline_slice.as_ref().map(|slice| {
            KlineChartView::new("kline-main-chart", slice.clone(), conv)
                .with_crosshair(guard.kline_viewport.crosshair_index)
                .with_crosshair_price(guard.kline_viewport.crosshair_price)
                .with_dataset_versions(
                    guard.pinned_chart_version,
                    Some(guard.latest_dataset_version),
                )
        });

        // Holdings & FIFO lots summary text for headless assertions
        let lots_summary = guard
            .holdings_lots
            .as_ref()
            .map(|hl| {
                hl.value
                    .open_lots
                    .iter()
                    .map(|l| {
                        format!(
                            "LOT:{}:{}:qty={}:cost={}",
                            l.account_id,
                            l.asset_id,
                            to_canonical_text(l.remaining_quantity),
                            to_canonical_text(l.remaining_cost_book)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();

        let recon_summary = guard
            .reconciliation_issues
            .as_ref()
            .map(|ri| {
                ri.value
                    .iter()
                    .map(|r| {
                        format!(
                            "RECON:{}:{}:exp={}:obs={}:diff={}",
                            r.account_id, r.asset_id, r.expected, r.observed, r.difference
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .unwrap_or_default();

        let autosave_badge = match &guard.note_editor.status {
            AutosaveStatus::Editing => I18nCatalog::tr("autosave.editing").to_string(),
            AutosaveStatus::Saving => I18nCatalog::tr("autosave.saving").to_string(),
            AutosaveStatus::Saved { revision, .. } => {
                format!("{} (v{revision})", I18nCatalog::tr("autosave.saved"))
            }
            AutosaveStatus::SaveFailed { error, .. } => {
                format!("{}: {error}", I18nCatalog::tr("autosave.failed"))
            }
        };

        let lot_rows: Vec<Vec<String>> = guard
            .holdings_lots
            .as_ref()
            .map(|hl| {
                hl.value
                    .open_lots
                    .iter()
                    .map(|l| {
                        vec![
                            l.account_id.clone(),
                            l.asset_id.clone(),
                            to_canonical_text(l.remaining_quantity),
                            to_canonical_text(l.remaining_cost_book),
                        ]
                    })
                    .collect()
            })
            .unwrap_or_default();
        let recon_rows: Vec<Vec<String>> = guard
            .reconciliation_issues
            .as_ref()
            .map(|ri| {
                ri.value
                    .iter()
                    .map(|r| {
                        vec![
                            r.account_id.clone(),
                            r.asset_id.clone(),
                            r.expected.to_string(),
                            r.observed.to_string(),
                            r.difference.to_string(),
                        ]
                    })
                    .collect()
            })
            .unwrap_or_default();
        let hero_nav = MetricCard::new(
            "hero-nav",
            I18nCatalog::tr("overview.hero"),
            nav_opt,
            ccy.clone(),
            qual,
        )
        .with_color_convention(conv);
        let hero_pnl = conv.format_signed_change(pnl_opt, &ccy);
        let (hero_pnl_fg, _) = conv.colors(hero_pnl.direction, cx);
        let scope_caption = format!(
            "{} · {}{} · {}",
            if guard.scope.resolved_accounts.is_empty() {
                "—".to_string()
            } else {
                guard.scope.resolved_accounts.join(", ")
            },
            guard
                .scope
                .start_date
                .map(|date| format!("{date} → "))
                .unwrap_or_default(),
            guard.scope.as_of_date,
            ccy
        );
        let training_state_opt = (active_nav == "nav.training").then(|| guard.clone());

        drop(guard);

        // Chart element with pointer/scroll/key handling (unchanged interaction contract).
        let chart_element = kline_view_opt.map(|chart| {
            chart
                .render_chart_with_bounds(Arc::clone(&self.plot_bounds), cx)
                .track_focus(&self.focus_handle)
                .on_mouse_move(cx.listener(
                    |panel, event: &gpui_kit::MouseMoveEvent, window, cx| {
                        if event.dragging() {
                            if let (Some((start, mut viewport)), Some(bounds)) =
                                (panel.chart_drag.clone(), *panel.plot_bounds.lock().unwrap())
                            {
                                if bounds.size.width > px(0.) {
                                    let bars = -((event.position.x - start.x) / bounds.size.width
                                        * viewport.visible_len() as f32)
                                        .round()
                                        as isize;
                                    viewport.pan(bars);
                                    panel.state.lock().unwrap().kline_viewport = viewport.clone();
                                    panel.dispatch(
                                        Ok(UiAction::ChartViewport(viewport)),
                                        window,
                                        cx,
                                    );
                                }
                            }
                        } else {
                            panel.chart_pointer(event.position, false, None, cx);
                        }
                    },
                ))
                .on_mouse_down(
                    gpui_kit::MouseButton::Left,
                    cx.listener(|panel, event: &gpui_kit::MouseDownEvent, window, cx| {
                        panel.focus_handle.focus(window, cx);
                        panel.chart_pointer(event.position, true, Some(window), cx);
                    }),
                )
                .on_mouse_up(
                    gpui_kit::MouseButton::Left,
                    cx.listener(|panel, _, _, _| panel.chart_drag = None),
                )
                .on_scroll(
                    cx.listener(|panel, event: &gpui_kit::ScrollEvent, window, cx| {
                        let Some(bounds) = *panel.plot_bounds.lock().unwrap() else {
                            return;
                        };
                        let Some((x, _)) =
                            crate::chart_canvas::pointer_ratio(event.position, bounds)
                        else {
                            return;
                        };
                        let delta = event.delta.pixel_delta(px(16.));
                        let mut viewport = panel.state.lock().unwrap().kline_viewport.clone();
                        viewport.zoom(
                            (-f64::from(delta.y) / 600.).exp().clamp(0.5, 2.),
                            f64::from(x),
                        );
                        panel.state.lock().unwrap().kline_viewport = viewport.clone();
                        panel.dispatch(Ok(UiAction::ChartViewport(viewport)), window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_key_down(
                    cx.listener(|panel, event: &gpui_kit::KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "left" => panel.change_viewport(-20, 1., window, cx),
                            "right" => panel.change_viewport(20, 1., window, cx),
                            "+" | "=" => panel.change_viewport(0, 0.7, window, cx),
                            "-" => panel.change_viewport(0, 1.4, window, cx),
                            "escape" => {
                                panel
                                    .state
                                    .lock()
                                    .unwrap()
                                    .kline_viewport
                                    .move_crosshair(None, None);
                                cx.notify();
                            }
                            _ => {}
                        }
                    }),
                )
        });

        let caption = Chrome::caption();
        let section = |title: &'static str, cx: &App| {
            h_flex()
                .w_full()
                .pt_1()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(3.))
                        .h(px(13.))
                        .rounded(px(1.5))
                        .bg(cx.theme().foreground),
                )
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(I18nCatalog::tr(title)),
                )
        };
        let table = |id: &'static str, headers: &[&'static str], rows: &[Vec<String>], cx: &App| {
            let cols = headers.len();
            v_flex()
                .id(id)
                .w_full()
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
                .overflow_hidden()
                .child(
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .bg(cx.theme().table_head)
                        .text_xs()
                        .text_color(caption)
                        .children(headers.iter().enumerate().map(|(i, h)| {
                            div()
                                .flex_1()
                                .when(i >= 2, |cell| cell.text_right())
                                .child(I18nCatalog::tr(h))
                        })),
                )
                .when(rows.is_empty(), |t| {
                    t.child(
                        div()
                            .px_3()
                            .py_3()
                            .text_xs()
                            .text_color(caption)
                            .child(I18nCatalog::tr("assets.none")),
                    )
                })
                .children(rows.iter().map(|row| {
                    h_flex()
                        .px_3()
                        .py_1p5()
                        .border_t_1()
                        .border_color(cx.theme().table_row_border)
                        .text_sm()
                        .children(row.iter().take(cols).enumerate().map(|(i, v)| {
                            div()
                                .flex_1()
                                .when(i >= 2, |cell| {
                                    cell.text_right()
                                        .font_family(cx.theme().mono_font_family.clone())
                                })
                                .child(v.clone())
                        }))
                }))
        };

        v_flex()
            .id("center-workspace-root")
            .test_support()
            .aria_label(format!("ActiveView:{active_nav}"))
            .size_full()
            .bg(cx.theme().background)
            .when(
                !banners.is_empty() && active_nav != "nav.training",
                |root| {
                    root.child(
                        v_flex()
                            .p_2()
                            .gap_2()
                            .children(banners.iter().map(|banner| {
                                let state = banner.state.clone();
                                banner.render_banner_with_action(
                                    cx,
                                    cx.listener(move |panel, _, window, cx| match &state {
                                        WorkbenchSystemState::Syncing { .. } => panel
                                            .controller
                                            .update(cx, |ctl, cx| ctl.cancel_sync(cx)),
                                        WorkbenchSystemState::SaveFailed { .. } => {
                                            panel.save_note(window, cx)
                                        }
                                        WorkbenchSystemState::StaleReport { .. } => {
                                            panel.dispatch(Ok(UiAction::Refresh), window, cx)
                                        }
                                        WorkbenchSystemState::EmptyLibrary
                                        | WorkbenchSystemState::PartialValuation { .. }
                                        | WorkbenchSystemState::Offline { .. }
                                        | WorkbenchSystemState::Cancelled { .. } => {
                                            panel.controller.update(cx, |ctl, cx| {
                                                ctl.navigate("nav.data_center", window, cx)
                                            })
                                        }
                                        _ => panel.controller.update(cx, |ctl, cx| {
                                            ctl.navigate("nav.settings", window, cx)
                                        }),
                                    }),
                                )
                            })),
                    )
                },
            )
            .when(active_nav == "nav.market", |root| {
                root.child(
                    h_flex()
                        .id("market-workspace")
                        .flex_1()
                        .min_h(px(0.))
                        .w_full()
                        .items_start()
                        .child(self.render_market_sidebar(cx))
                        .child(
                            v_flex()
                                .id("market-main")
                                .flex_1()
                                .min_w(px(0.))
                                .h_full()
                                .overflow_y_scroll()
                                .child(self.render_quote_header(cx))
                                .child(self.render_chart_toolbar(cx))
                                .when_some(chart_element, |column, chart| {
                                    column.child(div().px_3().pt_2().child(chart))
                                })
                                .child(self.render_chart_markers(cx)),
                        ),
                )
            })
            .when(active_nav != "nav.market", |root| {
                root.child(
                    v_flex()
                        .id("center-scroll")
                        .flex_1()
                        .min_h(px(0.))
                        .w_full()
                        .overflow_y_scroll()
                        .p_4()
                        .gap_3()
                        .when(active_nav == "nav.overview", |root| {
                            root.child(
                                // Hero: net worth and total P&L, like a quote header.
                                h_flex()
                                    .id("overview-hero")
                                    .w_full()
                                    .pb_3()
                                    .gap_6()
                                    .items_end()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .items_center()
                                                    .child(
                                                        div().text_xs().text_color(caption).child(
                                                            I18nCatalog::tr("overview.hero"),
                                                        ),
                                                    )
                                                    .child(MetricCard::render_quality_tag(
                                                        qual,
                                                        Locale::ZhCn,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(30.))
                                                    .font_semibold()
                                                    .child(hero_nav.formatted_display_line()),
                                            ),
                                    )
                                    .child(
                                        v_flex()
                                            .pb_1()
                                            .gap_0p5()
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(caption)
                                                    .child(I18nCatalog::tr("card.total_pnl")),
                                            )
                                            .child(
                                                div()
                                                    .text_base()
                                                    .font_semibold()
                                                    .text_color(hero_pnl_fg)
                                                    .child(hero_pnl.text),
                                            ),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        v_flex()
                                            .pb_1()
                                            .items_end()
                                            .text_xs()
                                            .text_color(caption)
                                            .child(scope_caption.clone())
                                            .child(cov_note.clone()),
                                    ),
                            )
                            .child(section("overview.assets_section", cx))
                            .child(
                                h_flex()
                                    .id("overview-cards-grid")
                                    .test_support()
                                    .w_full()
                                    .flex_wrap()
                                    .gap_3()
                                    .child(cash_card.render_card(cx).on_click(cx.listener(
                                        |panel, _, window, cx| {
                                            panel.controller.update(cx, |ctl, cx| {
                                                ctl.navigate("nav.assets", window, cx)
                                            })
                                        },
                                    )))
                                    .child(nav_card.render_card(cx).on_click(cx.listener(
                                        |panel, _, window, cx| {
                                            panel.controller.update(cx, |ctl, cx| {
                                                ctl.navigate("nav.assets", window, cx)
                                            })
                                        },
                                    )))
                                    .child(pnl_card.render_card(cx).on_click(cx.listener(
                                        |panel, _, window, cx| {
                                            panel.controller.update(cx, |ctl, cx| {
                                                ctl.navigate("nav.assets", window, cx)
                                            })
                                        },
                                    )))
                                    .child(inv_card.render_card(cx)),
                            )
                            .child(section("overview.breakdown_section", cx))
                            .child(
                                h_flex()
                                    .id("breakdown-cards-grid")
                                    .test_support()
                                    .w_full()
                                    .flex_wrap()
                                    .gap_3()
                                    .child(realized_card.render_card(cx))
                                    .child(unrealized_card.render_card(cx))
                                    .child(div_card.render_card(cx))
                                    .child(fee_card.render_card(cx)),
                            )
                        })
                        .when(active_nav == "nav.data_center", |root| {
                            root.child(self.render_data_center(cx))
                        })
                        .when(active_nav == "nav.journal", |root| {
                            root.child(self.render_journal(cx))
                        })
                        .when(active_nav == "nav.settings", |root| {
                            root.child(self.render_settings(cx))
                        })
                        .when_some(training_state_opt, |root, trn_state| {
                            root.child(self.render_training_page(&trn_state, cx))
                        })
                        .when(active_nav == "nav.assets", |root| {
                            root.child(
                                // Assets FIFO lots, Reconciliation, and Virtual-Scrolling Flows table (`M2-W04`, `M2-A-09`)
                                v_flex()
                                    .id("assets-and-journal-section")
                                    .test_support()
                                    .w_full()
                                    .gap_3()
                                    .child(section("assets.lots_section", cx))
                                    .child(
                                        div()
                                            .id("fifo-lots-summary")
                                            .test_support()
                                            .w_full()
                                            .child(table(
                                                "fifo-lots-table",
                                                &[
                                                    "assets.account",
                                                    "assets.asset",
                                                    "assets.quantity",
                                                    "assets.cost",
                                                ],
                                                &lot_rows,
                                                cx,
                                            ))
                                            .test_text(lots_summary),
                                    )
                                    .child(section("assets.recon_section", cx))
                                    .child(
                                        div()
                                            .id("reconciliation-summary")
                                            .test_support()
                                            .w_full()
                                            .child(table(
                                                "reconciliation-table",
                                                &[
                                                    "assets.account",
                                                    "assets.asset",
                                                    "assets.expected",
                                                    "assets.observed",
                                                    "assets.difference",
                                                ],
                                                &recon_rows,
                                                cx,
                                            ))
                                            .test_text(recon_summary),
                                    )
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .justify_between()
                                            .child(section("assets.flows_section", cx))
                                            .child(
                                                div()
                                                    .id("journal-autosave-status")
                                                    .test_support()
                                                    .text_xs()
                                                    .text_color(caption)
                                                    .child(autosave_badge.clone())
                                                    .test_text(autosave_badge),
                                            ),
                                    )
                                    .child(
                                        div().h(px(240.0)).w_full().child(
                                            DataTable::new(&self.flows_table)
                                                .stripe(true)
                                                .bordered(true),
                                        ),
                                    ),
                            )
                            .child(self.render_asset_controls(cx))
                        }),
                )
            })
    }
}

pub struct RightAiReviewPanel {
    focus_handle: FocusHandle,
    pub(crate) state: Arc<Mutex<WorkbenchSharedState>>,
    pub(crate) controller: Entity<WorkbenchController>,
    pub(crate) question: Entity<TextareaState>,
    pub(crate) session_id: String,
    _subscriptions: Vec<Subscription>,
}

impl RightAiReviewPanel {
    pub fn new(
        state: Arc<Mutex<WorkbenchSharedState>>,
        controller: Entity<WorkbenchController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let question = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(I18nCatalog::tr("form.ai_question"))
        });
        let subscription = cx.subscribe(&controller, |_, _, _: &WorkbenchUpdated, cx| cx.notify());
        Self {
            focus_handle: cx.focus_handle(),
            state,
            controller,
            question,
            session_id: format!("session:{}", uuid::Uuid::new_v4()),
            _subscriptions: vec![subscription],
        }
    }
}

impl BasePanel for RightAiReviewPanel {
    fn panel_name(&self) -> &'static str {
        "RightAiReviewPanel"
    }

    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl Panel for RightAiReviewPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        I18nCatalog::tr("nav.ai_panel")
    }

    fn title_bar(&self, _cx: &App) -> bool {
        false
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for RightAiReviewPanel {}

impl Focusable for RightAiReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RightAiReviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let guard = self.state.lock().unwrap();
        let scope_text = if let Some(pre) = &guard.pre_send_scope {
            format!(
                "模型: {} | 账户: {} | 期间: {} | 含笔记: {} | 锁定范围: {}",
                pre.model_display,
                pre.accounts.join(","),
                pre.period_label,
                pre.include_notes,
                pre.is_scope_pinned
            )
        } else {
            "模型: 未配置模型".to_string()
        };

        let report_summary = if let Some(rep) = &guard.last_ai_report {
            format!(
                "AI状态:{:?} | 摘要:{} | 证据数:{} | 拦截引用:{:?}",
                rep.verification_status,
                rep.summary,
                rep.verified_evidence.len(),
                rep.blocked_citations
            )
        } else if let Some(err) = &guard.last_ai_error {
            format!("AI不可用:{} ({})", err.code, err.message)
        } else {
            "等待发起证据化复盘".to_string()
        };
        let report = guard.last_ai_report.clone();
        let pinned = guard.runtime.pinned_scope().is_some();
        let include_notes = guard.scope.include_notes;
        let saved_reports = guard.saved_ai_reports.clone();
        let opened_evidence = guard.opened_ai_evidence.clone();
        let running = self.controller.read(cx).ai_running;
        let stream = self.controller.read(cx).stream_text.lock().unwrap().clone();
        drop(guard);

        v_flex()
            .id("ai-review-panel-root")
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .px_3()
            .pb_3()
            .gap_2()
            .bg(cx.theme().background)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .h(px(theme::PANEL_HEADER_HEIGHT))
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Icon::new(IconName::Bot)
                            .small()
                            .text_color(Chrome::accent()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(I18nCatalog::tr("ai.header")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(Chrome::caption())
                            .child(I18nCatalog::tr("ai.offline_hint")),
                    ),
            )
            .child(
                div()
                    .id("ai-pre-send-scope-bar")
                    .test_support()
                    .text_xs()
                    .p_2()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(scope_text.clone())
                    .test_text(scope_text),
            )
            .child(
                Textarea::new(&self.question)
                    .h(px(110.))
                    .accessibility_id("ai-question"),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("ai-pin-scope")
                            .small()
                            .label(I18nCatalog::tr(if pinned {
                                "action.unpin_scope"
                            } else {
                                "action.pin_scope"
                            }))
                            .disabled(running)
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.controller.update(cx, |ctl, cx| {
                                    ctl.dispatch(
                                        UiAction::AiScope {
                                            pinned: !pinned,
                                            include_notes,
                                        },
                                        window,
                                        cx,
                                    )
                                })
                            })),
                    )
                    .child(
                        Button::new("ai-include-notes")
                            .small()
                            .label(I18nCatalog::tr(if include_notes {
                                "action.exclude_notes"
                            } else {
                                "action.include_notes"
                            }))
                            .disabled(running)
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.controller.update(cx, |ctl, cx| {
                                    ctl.dispatch(
                                        UiAction::AiScope {
                                            pinned,
                                            include_notes: !include_notes,
                                        },
                                        window,
                                        cx,
                                    )
                                })
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("ai-send")
                            .small()
                            .primary()
                            .label(I18nCatalog::tr("action.ai_send"))
                            .disabled(running)
                            .on_click(cx.listener(|panel, _, window, cx| panel.send(window, cx))),
                    )
                    .child(
                        Button::new("ai-cancel")
                            .small()
                            .label(I18nCatalog::tr("action.ai_cancel"))
                            .disabled(!running)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.controller.update(cx, |ctl, cx| ctl.cancel_ai(cx))
                            })),
                    )
                    .child(
                        Button::new("ai-compact")
                            .small()
                            .label(I18nCatalog::tr("action.compact"))
                            .disabled(running)
                            .on_click(cx.listener(|panel, _, window, cx| {
                                let session = panel.session_id.clone();
                                panel.controller.update(cx, |ctl, cx| {
                                    ctl.dispatch(UiAction::CompactSession { session }, window, cx)
                                });
                            })),
                    ),
            )
            .when(running, |root| {
                root.child(
                    div()
                        .text_xs()
                        .child(I18nCatalog::tr("form.ai_stream_unverified")),
                )
                .child(div().text_sm().child(stream))
            })
            .children(report.iter().flat_map(|report| {
                report.calculations.iter().map(|(label, value, currency)| {
                    div()
                        .text_sm()
                        .child(format!("{label}: {value} {currency}"))
                })
            }))
            .child(
                div()
                    .id("ai-report-summary")
                    .test_support()
                    .text_xs()
                    .child(report_summary.clone())
                    .test_text(report_summary),
            )
            .children(report.iter().map(|report| {
                let status = match report.verification_status {
                    AiReportVerificationStatus::Verified => "ai.verified",
                    AiReportVerificationStatus::NeedsReview => "ai.needs_review",
                };
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(I18nCatalog::tr(status)))
                    .children(
                        report
                            .limitations
                            .iter()
                            .map(|text| div().text_xs().child(text.clone())),
                    )
                    .children(
                        report
                            .newer_data_available_notice
                            .iter()
                            .map(|text| div().text_xs().child(text.clone())),
                    )
                    .child(div().text_sm().child(I18nCatalog::tr("ai.evidence")))
                    .children(report.verified_evidence.iter().map(|reference| {
                        let reference = reference.clone();
                        Button::new(format!("ai-evidence:{}", reference.id))
                            .small()
                            .label(reference.label.clone())
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.controller.update(cx, |ctl, cx| {
                                    ctl.dispatch(
                                        UiAction::OpenEvidence(reference.clone()),
                                        window,
                                        cx,
                                    )
                                })
                            }))
                    }))
            }))
            .children(opened_evidence.iter().map(|evidence| {
                div()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(
                        serde_json::to_string_pretty(&evidence["receipt"]["value"])
                            .unwrap_or_default(),
                    )
            }))
            .child(div().text_sm().child(I18nCatalog::tr("ai.saved_reports")))
            .children(saved_reports.into_iter().map(|report| {
                let run_id = report.run_id.clone();
                Button::new(format!("ai-saved-report:{run_id}"))
                    .small()
                    .label(format!(
                        "{} · {} · {}",
                        report.model_id,
                        report.frozen_scope.as_of_date,
                        report.frozen_scope.resolved_accounts.join(",")
                    ))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.controller.update(cx, |ctl, cx| {
                            ctl.dispatch(UiAction::OpenAiReport(run_id.clone()), window, cx)
                        })
                    }))
            }))
    }
}

pub struct BottomTaskStatusPanel {
    focus_handle: FocusHandle,
    state: Arc<Mutex<WorkbenchSharedState>>,
    controller: Entity<WorkbenchController>,
    _subscription: Subscription,
}

impl BottomTaskStatusPanel {
    pub fn new(
        state: Arc<Mutex<WorkbenchSharedState>>,
        controller: Entity<WorkbenchController>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&controller, |_, _, _: &WorkbenchUpdated, cx| cx.notify());
        Self {
            focus_handle: cx.focus_handle(),
            state,
            controller,
            _subscription: subscription,
        }
    }
}

impl BasePanel for BottomTaskStatusPanel {
    fn panel_name(&self) -> &'static str {
        "BottomTaskStatusPanel"
    }

    fn closable(&self, _cx: &App) -> bool {
        false
    }
}

impl Panel for BottomTaskStatusPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        I18nCatalog::tr("nav.tasks")
    }

    fn title_bar(&self, _cx: &App) -> bool {
        false
    }

    fn inner_padding(&self, _cx: &App) -> bool {
        false
    }
}

impl EventEmitter<PanelEvent> for BottomTaskStatusPanel {}

impl Focusable for BottomTaskStatusPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BottomTaskStatusPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let guard = self.state.lock().unwrap();
        let job_text = if let Some(snap) = &guard.last_job_snapshot {
            let status = match &snap.state {
                JobState::Running => "执行中",
                JobState::Succeeded if snap.worker_finished => "已完成",
                JobState::Succeeded => "已提交，正在收尾",
                JobState::Failed(_) => "失败",
                JobState::Cancelled(_) => "已取消",
                JobState::TimedOut(_) => "超时",
            };
            format!(
                "任务: {} | {} · {} | 已准备 {} 条 · 已发布 {} 条",
                snap.name,
                status,
                snap.progress.phase,
                snap.progress.records_prepared,
                snap.progress.records_published
            )
        } else {
            "后台任务空闲".to_string()
        };
        let notice = guard.ui_notice.clone();
        let error = guard
            .ui_error
            .as_ref()
            .map(|error| format!("{} · {}", error.message, error.trace_id));
        drop(guard);

        let has_error = error.is_some();
        h_flex()
            .id("bottom-task-panel-root")
            .test_support()
            .size_full()
            .px_3()
            .pt_2()
            .gap_3()
            .items_start()
            .text_xs()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .child(Icon::new(IconName::PanelBottom).xsmall())
                    .child(div().font_semibold().child(I18nCatalog::tr("nav.tasks"))),
            )
            .child(div().w(px(1.)).h(px(12.)).bg(cx.theme().border))
            .child(div().text_color(Chrome::caption()).child(job_text.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .when(has_error, |text| text.text_color(cx.theme().danger))
                    .child(error.unwrap_or(notice)),
            )
            .child(
                Button::new("bottom-cancel-sync")
                    .small()
                    .label(I18nCatalog::tr("action.cancel_sync"))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.controller.update(cx, |ctl, cx| ctl.cancel_sync(cx))
                    })),
            )
            .test_text(job_text)
    }
}

pub struct DeltaWorkbench {
    pub(crate) state: Arc<Mutex<WorkbenchSharedState>>,
    dock_area: Entity<DockArea>,
    _dock_skin: Rc<DockSkin>,
    center_panel: Entity<CenterWorkspacePanel>,
    right_ai_panel: Entity<RightAiReviewPanel>,
    _bottom_task_panel: Entity<BottomTaskStatusPanel>,
    pub(crate) controller: Entity<WorkbenchController>,
    scope_fields: FormFields,
    last_scope_descriptor: String,
    last_training_workspace_run: Option<String>,
    _subscription: Subscription,
    _poll_task: Task<()>,
    pending_exit: Option<crate::lifecycle::PendingExit>,
    last_update_status: String,
}

impl DeltaWorkbench {
    pub fn new(
        host: AppHost,
        data_dir: PathBuf,
        as_of_date: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(
            WorkbenchSharedState::loading(host, data_dir, as_of_date)
                .expect("initialize WorkbenchSharedState"),
        ));
        let controller = cx.new(|_| WorkbenchController::new(Arc::clone(&shared)));
        let s1 = Arc::clone(&shared);
        let center_panel =
            cx.new(|cx| CenterWorkspacePanel::new(s1, controller.clone(), window, cx));
        let s2 = Arc::clone(&shared);
        let right_ai_panel =
            cx.new(|cx| RightAiReviewPanel::new(s2, controller.clone(), window, cx));
        let s3 = Arc::clone(&shared);
        let bottom_task_panel = cx.new(|cx| BottomTaskStatusPanel::new(s3, controller.clone(), cx));

        let (dock_area, dock_skin) =
            DockSkin::dock_area("delta-workbench-dock", Some(1), window, cx);
        dock_skin.set_panel_style(PanelStyle::Auto, cx);

        let cp = center_panel.clone();
        let rp = right_ai_panel.clone();
        let bp = bottom_task_panel.clone();
        dock_area.update(cx, |area, cx| {
            area.set_center(
                DockLayout::tabs().panel_view(gpui_kit::component::dock::panel_handle(cp), cx),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Right,
                DockLayout::tabs().panel_view(gpui_kit::component::dock::panel_handle(rp), cx),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Right, px(320.0), window, cx);
            if !area.is_dock_open(DockPlacement::Right) {
                area.toggle_dock(DockPlacement::Right, window, cx);
            }
            area.set_dock(
                DockPlacement::Bottom,
                DockLayout::tabs().panel_view(gpui_kit::component::dock::panel_handle(bp), cx),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Bottom, px(96.0), window, cx);
            if !area.is_dock_open(DockPlacement::Bottom) {
                area.toggle_dock(DockPlacement::Bottom, window, cx);
            }
        });

        let scope_fields = FormFields::new(
            &[
                ("form.scope_target", "all"),
                ("form.scope_start", ""),
                ("form.scope_end", &as_of_date.to_string()),
            ],
            window,
            cx,
        );
        let subscription = cx.subscribe_in(
            &controller,
            window,
            |workbench, _, _: &WorkbenchUpdated, window, cx| {
                let state = workbench.state.lock().unwrap();
                let target = match &state.scope.target {
                    ScopeTarget::AllAccounts => "all".into(),
                    ScopeTarget::Accounts(ids) => ids.join(","),
                    ScopeTarget::Portfolio(id) => format!("portfolio:{id}"),
                    ScopeTarget::TrainingRun(id) => format!("training:{id}"),
                };
                let descriptor = format!(
                    "{target}|{:?}|{}",
                    state.scope.start_date, state.scope.as_of_date
                );
                if descriptor != workbench.last_scope_descriptor {
                    workbench
                        .scope_fields
                        .set("form.scope_target", target, window, cx);
                    workbench.scope_fields.set(
                        "form.scope_start",
                        state
                            .scope
                            .start_date
                            .map(|date| date.to_string())
                            .unwrap_or_default(),
                        window,
                        cx,
                    );
                    workbench.scope_fields.set(
                        "form.scope_end",
                        state.scope.as_of_date.to_string(),
                        window,
                        cx,
                    );
                    workbench.last_scope_descriptor = descriptor;
                }
                let right = state.layout.right_ai_open;
                let bottom = state.layout.bottom_tasks_open;
                let current_ws_run = (state.layout.active_nav == "nav.training"
                    && state.training.mode == crate::training_ui::TrainingViewMode::Workspace)
                    .then(|| state.training.active_run_id.clone())
                    .flatten();
                drop(state);
                if current_ws_run != workbench.last_training_workspace_run {
                    workbench.last_training_workspace_run = current_ws_run.clone();
                    if current_ws_run.is_some() {
                        let (global_focused, trn_focused) =
                            workbench.is_any_non_workspace_or_training_input_focused(window, cx);
                        if !global_focused && !trn_focused {
                            let fh = workbench
                                .center_panel
                                .read(cx)
                                .training_workspace_focus()
                                .clone();
                            fh.focus(window, cx);
                        }
                    }
                }
                workbench.dock_area.update(cx, |area, cx| {
                    if area.is_dock_open(DockPlacement::Right) != right {
                        area.toggle_dock(DockPlacement::Right, window, cx);
                    }
                    if area.is_dock_open(DockPlacement::Bottom) != bottom {
                        area.toggle_dock(DockPlacement::Bottom, window, cx);
                    }
                });
                cx.notify();
            },
        );
        let tick_controller = controller.clone();
        let executor = cx.background_executor().clone();
        let poll_task = cx.spawn_in(window, async move |workbench, cx| loop {
            executor.timer(Duration::from_millis(100)).await;
            if tick_controller
                .update_in(cx, |ctl, window, cx| {
                    ctl.poll_sync(window, cx);
                    if ctl.ai_running {
                        cx.emit(WorkbenchUpdated);
                    }
                })
                .is_err()
                || workbench
                    .update_in(cx, |workbench, window, cx| {
                        workbench.tick_lifecycle(window, cx)
                    })
                    .is_err()
            {
                break;
            }
        });
        controller.update(cx, |ctl, cx| ctl.dispatch(UiAction::Initialize, window, cx));
        Self {
            state: shared,
            dock_area,
            _dock_skin: dock_skin,
            center_panel,
            right_ai_panel,
            _bottom_task_panel: bottom_task_panel,
            controller,
            scope_fields,
            last_scope_descriptor: String::new(),
            last_training_workspace_run: None,
            _subscription: subscription,
            _poll_task: poll_task,
            pending_exit: None,
            last_update_status: String::new(),
        }
    }

    fn is_any_non_workspace_or_training_input_focused(
        &self,
        window: &Window,
        cx: &App,
    ) -> (bool, bool) {
        let global_input_focused = self.scope_fields.is_any_focused(
            &["form.scope_target", "form.scope_start", "form.scope_end"],
            window,
            cx,
        ) || self
            .right_ai_panel
            .read(cx)
            .question
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let training_input_focused = self
            .center_panel
            .read(cx)
            .is_any_training_text_input_focused(window, cx);
        (global_input_focused, training_input_focused)
    }

    pub fn shared_state(&self) -> &Arc<Mutex<WorkbenchSharedState>> {
        &self.state
    }

    pub fn center_panel(&self) -> &Entity<CenterWorkspacePanel> {
        &self.center_panel
    }

    fn apply_scope_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            let target = self.scope_fields.value("form.scope_target", cx);
            let start = self.scope_fields.value("form.scope_start", cx);
            let end = self.scope_fields.value("form.scope_end", cx);
            let date = |value: &str| {
                NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
                    .map_err(|_| ErrorEnvelope::new("INVALID_DATE", "日期请填写 YYYY-MM-DD", false))
            };
            let mut scope = AnalysisScope::all_accounts(
                date(&end)?,
                self.state.lock().unwrap().scope.report_currency.clone(),
            );
            scope.start_date = if start.trim().is_empty() {
                None
            } else {
                Some(date(&start)?)
            };
            if scope
                .start_date
                .is_some_and(|start| start > scope.as_of_date)
            {
                return Err(ErrorEnvelope::new(
                    "INVALID_RANGE",
                    "开始日期须早于或等于截止日期",
                    false,
                ));
            }
            scope.target = if target.trim() == "all" {
                ScopeTarget::AllAccounts
            } else if let Some(id) = target.trim().strip_prefix("portfolio:") {
                ScopeTarget::Portfolio(id.to_string())
            } else {
                ScopeTarget::Accounts(
                    target
                        .split([',', '，'])
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .collect(),
                )
            };
            scope.include_notes = self.state.lock().unwrap().scope.include_notes;
            Ok(scope)
        })();
        match result {
            Ok(scope) => self
                .controller
                .update(cx, |ctl, cx| ctl.change_scope(scope, window, cx)),
            Err(error) => {
                self.state.lock().unwrap().ui_error = Some(error);
                cx.notify();
            }
        }
    }
}

fn nav_icon(key: &str) -> IconName {
    match key {
        "nav.overview" => IconName::LayoutDashboard,
        "nav.market" => IconName::Globe,
        "nav.assets" => IconName::ChartPie,
        "nav.journal" => IconName::BookOpen,
        "nav.training" => IconName::ChartPie,
        "nav.data_center" => IconName::HardDrive,
        "nav.settings" => IconName::Settings,
        "nav.ai_panel" => IconName::Bot,
        _ => IconName::PanelBottom,
    }
}

/// One entry of the left icon rail: icon over a short label, Longbridge-style.
fn rail_item(
    id: SharedString,
    key: &'static str,
    active: bool,
    cx: &App,
) -> crate::components::ObservedStatefulDiv {
    let label = I18nCatalog::tr(key);
    let fg = if active {
        cx.theme().foreground
    } else {
        Chrome::caption()
    };
    v_flex()
        .id(id)
        .test_support()
        .aria_label(label)
        .w(px(theme::NAV_RAIL_WIDTH - 12.))
        .py_1p5()
        .gap_0p5()
        .items_center()
        .rounded(cx.theme().radius_lg)
        .cursor_pointer()
        .when(active, |item| item.bg(Chrome::hover()))
        .hover(|item| item.bg(Chrome::hover()))
        .child(Icon::new(nav_icon(key)).size_5().text_color(fg))
        .child(
            div()
                .text_size(px(11.))
                .text_color(fg)
                .when(active, |label| label.font_semibold())
                .child(label),
        )
}

impl Render for DeltaWorkbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let active_nav = state.layout.active_nav.clone();
        let scope_text = format!(
            "{} · {} · {}{} · {} · 数据 v{} / 账本 r{}",
            I18nCatalog::tr(&active_nav),
            state.scope.resolved_accounts.join(", "),
            state
                .scope
                .start_date
                .map(|date| format!("{date} 至 "))
                .unwrap_or_default(),
            state.scope.as_of_date,
            state.scope.report_currency,
            state.scope.dataset_version,
            state.scope.ledger_revision
        );
        let accounts_text = if state.scope.resolved_accounts.is_empty() {
            "无账户".to_string()
        } else {
            format!("账户 {}", state.scope.resolved_accounts.join(", "))
        };
        let ai_open = state.layout.right_ai_open;
        let tasks_open = state.layout.bottom_tasks_open;
        let update_badge =
            crate::lifecycle::update_state().and_then(|update| match update.status {
                crate::lifecycle::UpdateStatus::Ready { version } => Some((
                    format!("{} {version}", I18nCatalog::tr("update.ready")),
                    false,
                )),
                crate::lifecycle::UpdateStatus::BusyConfirm { .. } => {
                    Some((I18nCatalog::tr("update.busy_confirm").to_string(), true))
                }
                _ => None,
            });
        let toggle = |id: &'static str, icon: IconName, tip: &'static str, on: bool| {
            Button::new(id)
                .icon(icon)
                .ghost()
                .small()
                .tooltip(I18nCatalog::tr(tip))
                .selected(on)
        };
        v_flex()
            .id("delta-workbench-root")
            .test_support()
            .on_key_down(
                cx.listener(|workbench, event: &gpui_kit::KeyDownEvent, window, cx| {
                    let is_training_ws = {
                        let g = workbench.state.lock().unwrap();
                        g.layout.active_nav == "nav.training"
                            && g.training.mode == crate::training_ui::TrainingViewMode::Workspace
                    };
                    if !is_training_ws {
                        return;
                    }
                    let (global_focused, trn_focused) =
                        workbench.is_any_non_workspace_or_training_input_focused(window, cx);
                    if global_focused || trn_focused {
                        return;
                    }
                    workbench.center_panel.update(cx, |panel, cx| {
                        panel.handle_training_workspace_key_down(event, window, cx);
                    });
                }),
            )
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                // Toolbar: brand, active page, compact scope controls and dock toggles.
                h_flex()
                    .id("workbench-top-bar")
                    .w_full()
                    .h(px(theme::TOOLBAR_HEIGHT))
                    .flex_none()
                    .pr_2()
                    .gap_2()
                    .items_center()
                    .bg(Chrome::shell())
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .w(px(theme::NAV_RAIL_WIDTH))
                            .h_full()
                            .flex_none()
                            .justify_center()
                            .child(
                                div()
                                    .size(px(24.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.))
                                    .bg(cx.theme().foreground)
                                    .text_color(cx.theme().background)
                                    .text_sm()
                                    .font_bold()
                                    .child("Δ"),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .flex_none()
                            .child(I18nCatalog::tr(&active_nav)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(Chrome::caption())
                            .truncate()
                            .min_w(px(0.))
                            .child(accounts_text),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(self.scope_fields.compact("form.scope_target", 140.))
                            .child(self.scope_fields.compact("form.scope_start", 100.))
                            .child(div().text_xs().text_color(Chrome::caption()).child("→"))
                            .child(self.scope_fields.compact("form.scope_end", 100.))
                            .child(
                                Button::new("scope-apply")
                                    .small()
                                    .primary()
                                    .label(I18nCatalog::tr("action.apply_scope"))
                                    .on_click(cx.listener(|workbench, _, window, cx| {
                                        workbench.apply_scope_controls(window, cx)
                                    })),
                            ),
                    )
                    .when_some(update_badge, |bar, (label, warning)| {
                        bar.child(
                            Button::new("update-ready-badge")
                                .small()
                                .when(!warning, |button| button.primary())
                                .when(warning, |button| button.warning())
                                .label(label)
                                .on_click(cx.listener(|workbench, _, window, cx| {
                                    workbench.controller.update(cx, |ctl, cx| {
                                        ctl.navigate("nav.settings", window, cx)
                                    })
                                })),
                        )
                    })
                    .child(div().w(px(1.)).h(px(18.)).bg(cx.theme().border))
                    .child(
                        toggle("dock-toggle-ai", IconName::Bot, "nav.ai_panel", ai_open).on_click(
                            cx.listener(|workbench, _, window, cx| {
                                {
                                    let mut state = workbench.state.lock().unwrap();
                                    state.layout.right_ai_open = !state.layout.right_ai_open;
                                }
                                workbench.controller.update(cx, |ctl, cx| {
                                    ctl.dispatch(UiAction::SaveLayout, window, cx)
                                });
                            }),
                        ),
                    )
                    .child(
                        toggle(
                            "dock-toggle-tasks",
                            IconName::PanelBottom,
                            "nav.tasks",
                            tasks_open,
                        )
                        .on_click(cx.listener(
                            |workbench, _, window, cx| {
                                {
                                    let mut state = workbench.state.lock().unwrap();
                                    state.layout.bottom_tasks_open =
                                        !state.layout.bottom_tasks_open;
                                }
                                workbench.controller.update(cx, |ctl, cx| {
                                    ctl.dispatch(UiAction::SaveLayout, window, cx)
                                });
                            },
                        )),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .child(
                        v_flex()
                            .id("workbench-nav-rail")
                            .test_support()
                            .w(px(theme::NAV_RAIL_WIDTH))
                            .flex_none()
                            .h_full()
                            .py_2()
                            .gap_1()
                            .items_center()
                            .bg(Chrome::shell())
                            .border_r_1()
                            .border_color(cx.theme().border)
                            .children(
                                PRIMARY_NAV_KEYS
                                    .iter()
                                    .filter(|key| **key != "nav.settings")
                                    .map(|&key| self.rail_button(key, &active_nav, cx)),
                            )
                            .child(div().flex_1())
                            .child(self.rail_button("nav.settings", &active_nav, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .child(self.dock_area.clone()),
                    ),
            )
            .child(
                // Status bar: scope/version on the left, as-of and currency on the right.
                h_flex()
                    .id("workbench-status-bar")
                    .w_full()
                    .h(px(theme::STATUS_BAR_HEIGHT))
                    .flex_none()
                    .px_3()
                    .gap_3()
                    .items_center()
                    .bg(Chrome::shell())
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(Chrome::caption())
                    .child(
                        div()
                            .id("workbench-scope-badge")
                            .test_support()
                            .truncate()
                            .min_w(px(0.))
                            .child(scope_text.clone())
                            .test_text(scope_text),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(Icon::new(IconName::Calendar).xsmall())
                            .child(format!(
                                "截至 {} · {}",
                                state.scope.as_of_date, state.scope.report_currency
                            )),
                    ),
            )
    }
}

impl DeltaWorkbench {
    /// Drain control-pipe requests and advance a pending save-then-exit (`S-29`, `S-31`).
    pub(crate) fn tick_lifecycle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::lifecycle::{
            emit_event, next_control_request, options, ExitReason, PendingExit, APPLY_REQUESTED,
        };
        use delta_app::{ControlCommand, ControlReply, ControlStatus};
        while let Some((command, reply)) = next_control_request() {
            match command {
                ControlCommand::Focus => {
                    if options().is_some_and(|options| options.allow_focus) {
                        window.activate_window();
                    }
                    emit_event("control-focus", json!({}));
                    let _ = reply.send(ControlReply::new(ControlStatus::Ok, "focused"));
                }
                ControlCommand::QuitForUpdate if self.pending_exit.is_some() => {
                    let _ = reply.send(ControlReply::new(
                        ControlStatus::Busy,
                        "an exit is already in progress",
                    ));
                }
                ControlCommand::QuitForUpdate => {
                    emit_event("control-quit-for-update", json!({}));
                    self.pending_exit = Some(PendingExit::new(ExitReason::Control(reply)));
                }
            }
        }
        if APPLY_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst)
            && self.pending_exit.is_none()
        {
            self.pending_exit = Some(PendingExit::new(ExitReason::ApplyUpdate));
        }
        let status = crate::lifecycle::update_state()
            .map(|state| format!("{:?}", state.status))
            .unwrap_or_default();
        if status != self.last_update_status {
            self.last_update_status = status;
            cx.notify();
        }
        if let Some(mut exit) = self.pending_exit.take() {
            if !self.advance_exit(&mut exit, window, cx) {
                self.pending_exit = Some(exit);
            }
        }
    }

    /// One step of the exit sequence. Returns true when the sequence has ended (exited,
    /// refused as busy, or failed); the app is never terminated with unsaved work.
    fn advance_exit(
        &mut self,
        exit: &mut crate::lifecycle::PendingExit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::lifecycle::{
            emit_event, options, ExitPhase, ExitReason, UpdateStatus, APPLY_CONFIRMED_BUSY,
        };
        use delta_app::{ControlReply, ControlStatus};
        fn refuse(
            exit: &crate::lifecycle::PendingExit,
            status: ControlStatus,
            reason: String,
        ) -> bool {
            crate::lifecycle::emit_event(
                "exit-refused",
                json!({"status": status, "reason": reason}),
            );
            match &exit.reason {
                crate::lifecycle::ExitReason::Control(reply) => {
                    let _ = reply.send(ControlReply::new(status, reason));
                }
                crate::lifecycle::ExitReason::ApplyUpdate => {
                    crate::lifecycle::set_update_failed("UPDATE_BUSY", &reason);
                }
            }
            true
        }
        if exit.timed_out() {
            return refuse(
                exit,
                ControlStatus::Busy,
                "保存未在期限内完成，应用保持运行".into(),
            );
        }
        let busy = {
            let state = self.state.lock().unwrap();
            if state.active_sync_job.is_some() {
                Some("同步任务进行中")
            } else if self.controller.read(cx).ai_running {
                Some("AI 正在生成")
            } else {
                None
            }
        };
        match exit.phase {
            ExitPhase::CheckBusy => {
                if let Some(reason) = busy {
                    if matches!(exit.reason, ExitReason::ApplyUpdate)
                        && APPLY_CONFIRMED_BUSY.swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        // The user explicitly chose to stop running work and update.
                        self.controller.update(cx, |ctl, cx| {
                            ctl.cancel_sync(cx);
                            ctl.cancel_ai(cx);
                        });
                        exit.dispatched = true;
                        return false;
                    }
                    if exit.dispatched {
                        return false; // Waiting for the confirmed cancellation to finish.
                    }
                    if matches!(exit.reason, ExitReason::ApplyUpdate) {
                        let version = crate::lifecycle::update_state()
                            .and_then(|state| state.offer.map(|offer| offer.manifest.version))
                            .unwrap_or_default();
                        emit_event("update-busy-prompt", json!({"detail": reason}));
                        crate::lifecycle::set_update_status(UpdateStatus::BusyConfirm {
                            version,
                            detail: reason.into(),
                        });
                        return true;
                    }
                    return refuse(exit, ControlStatus::Busy, reason.into());
                }
                if !self.controller.read(cx).is_idle() {
                    // An import commit or another queued write is still running: wait for it
                    // to finish instead of exiting mid-write (bounded by the exit deadline).
                    return false;
                }
                exit.phase = ExitPhase::SaveNote;
                exit.dispatched = false;
                false
            }
            ExitPhase::SaveNote => {
                let editor = self.state.lock().unwrap().note_editor.clone();
                if exit.dispatched
                    && matches!(editor.status, delta_app::AutosaveStatus::SaveFailed { .. })
                {
                    return refuse(
                        exit,
                        ControlStatus::SaveFailed,
                        "笔记保存失败，应用保持运行以免丢失编辑".into(),
                    );
                }
                if crate::controller::note_is_dirty(&editor) {
                    if !exit.dispatched {
                        exit.dispatched = true;
                        self.controller.update(cx, |ctl, cx| {
                            ctl.dispatch(UiAction::SaveNote(editor), window, cx)
                        });
                    }
                    return false;
                }
                if !self.controller.read(cx).is_idle() {
                    return false;
                }
                emit_event("exit-note-saved", json!({}));
                exit.phase = ExitPhase::SaveLayout;
                exit.dispatched = false;
                false
            }
            ExitPhase::SaveLayout => {
                if !exit.dispatched {
                    exit.dispatched = true;
                    self.controller
                        .update(cx, |ctl, cx| ctl.dispatch(UiAction::SaveLayout, window, cx));
                    return false;
                }
                if !self.controller.read(cx).is_idle() {
                    return false;
                }
                exit.phase = ExitPhase::Finish;
                false
            }
            ExitPhase::Finish => {
                match &exit.reason {
                    ExitReason::Control(reply) => {
                        let mut answer = ControlReply::new(ControlStatus::Ok, "saved; exiting");
                        answer.relaunch_args = options()
                            .map(|options| options.relaunch_args.clone())
                            .unwrap_or_default();
                        emit_event("exit-for-update", json!({}));
                        let _ = reply.send(answer);
                        cx.quit();
                    }
                    ExitReason::ApplyUpdate => {
                        crate::lifecycle::set_update_status(UpdateStatus::Applying {
                            version: crate::lifecycle::update_state()
                                .and_then(|state| state.offer.map(|offer| offer.manifest.version))
                                .unwrap_or_default(),
                        });
                        let host = self.state.lock().unwrap().host.clone();
                        match crate::lifecycle::backup_and_launch_installer(&host) {
                            Ok(()) => {
                                emit_event("exit-for-update", json!({}));
                                cx.quit();
                            }
                            Err(error) => crate::lifecycle::set_update_failed(
                                error.code(),
                                &error.to_string(),
                            ),
                        }
                    }
                }
                true
            }
        }
    }

    fn rail_button(
        &self,
        key: &'static str,
        active_nav: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        rail_item(
            SharedString::from(format!("nav-btn-{}", key.replace('.', "-"))),
            key,
            active_nav == key,
            cx,
        )
        .on_click(cx.listener(move |workbench, _, window, cx| {
            workbench
                .controller
                .update(cx, |ctl, cx| ctl.navigate(key, window, cx))
        }))
    }
}

// ============================================================================
// 5. Binary Entrypoint, `--ui-smoke`, and `--ui-bench` (`M2-K02`, `M2-K06`, `M2-A-20`)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiBenchReport {
    pub executed_at: String,
    pub hardware_logical_cores: usize,
    pub transaction_rows: usize,
    pub table_scroll_frames: usize,
    pub table_scroll_p50_us: u64,
    pub table_scroll_p95_us: u64,
    pub kline_total_bars: usize,
    pub kline_visible_bars: usize,
    pub kline_frames: usize,
    pub kline_p50_us: u64,
    pub kline_p95_us: u64,
    pub kline_p99_us: u64,
    pub kline_equivalent_fps_at_p95: f64,
    pub cancel_ui_latency_ms: u64,
    pub cancel_backend_latency_ms: u64,
    pub table_scroll_p99_us: u64,
    pub chart_frame_times_us: Vec<u64>,
    pub table_frame_times_us: Vec<u64>,
    pub chart_update_times_us: Vec<u64>,
    pub native_chart_paints: usize,
    pub native_table_cells: usize,
    pub native_window_created: bool,
    pub focus_stolen: bool,
    pub visible_windows: usize,
    pub build_profile: String,
    pub cpu: String,
    pub gpu: String,
    pub window_size: String,
    pub scale_factor: f32,
    pub measurement: String,
    pub first_screen_times_ms: Vec<u64>,
    pub first_screen_p50_ms: u64,
    pub first_screen_p95_ms: u64,
    pub first_screen_p99_ms: u64,
    pub interaction_times_us: Vec<u64>,
    pub interaction_p50_us: u64,
    pub interaction_p95_us: u64,
    pub interaction_p99_us: u64,
    pub workbench_measurement: String,
    /// Windows power-throttling state of the measuring process (`EXECUTION_SPEED_OPT_OUT`).
    #[serde(default)]
    pub power_throttling: String,
}

/// Verification owns a newly created child, never the caller's supplied root.
pub(crate) struct VerificationDirectory {
    pub(crate) path: PathBuf,
    parent: PathBuf,
}

impl VerificationDirectory {
    pub(crate) fn create(base: &Path, mode: &str) -> Result<Self, ErrorEnvelope> {
        delta_app::ensure_channel_data_isolation(base, BuildChannel::Source)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_PATH", e.to_string(), false))?;
        fs::create_dir_all(base)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_PATH", e.to_string(), false))?;
        let parent = fs::canonicalize(base)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_PATH", e.to_string(), false))?;
        let path = parent.join(format!("delta-{mode}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_PATH", e.to_string(), false))?;
        Ok(Self { path, parent })
    }

    pub(crate) fn cleanup(&self) -> Result<(), ErrorEnvelope> {
        if !self.path.exists() {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(&self.path)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_CLEANUP", e.to_string(), false))?;
        #[cfg(windows)]
        let redirected = {
            use std::os::windows::fs::MetadataExt as _;
            metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
        };
        #[cfg(not(windows))]
        let redirected = metadata.file_type().is_symlink();
        let resolved = fs::canonicalize(&self.path)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_CLEANUP", e.to_string(), false))?;
        if redirected || resolved != self.path || resolved.parent() != Some(self.parent.as_path()) {
            return Err(ErrorEnvelope::new(
                "VERIFICATION_CLEANUP",
                "验证目录身份已改变，保留目录供检查",
                false,
            ));
        }
        fs::remove_dir_all(&self.path)
            .map_err(|e| ErrorEnvelope::new("VERIFICATION_CLEANUP", e.to_string(), false))
    }
}

pub fn run_fixed_scale_ui_bench(data_dir: &Path) -> Result<UiBenchReport, ErrorEnvelope> {
    let owned = VerificationDirectory::create(data_dir, "bench")?;
    let result = run_fixed_scale_bench_in_owned_directory(&owned.path);
    owned.cleanup()?;
    result
}

fn run_fixed_scale_bench_in_owned_directory(
    data_dir: &Path,
) -> Result<UiBenchReport, ErrorEnvelope> {
    apply_process_quiet_guards();
    // The hidden benchmark window must not be measured under OS background throttling.
    let power_throttling = delta_app::opt_out_of_execution_speed_throttling();
    let mut report = crate::ui_verification::run_native_benchmark()?;
    report.power_throttling = power_throttling;
    let executable = std::env::current_exe()
        .map_err(|e| ErrorEnvelope::new("BENCH_EXECUTABLE", e.to_string(), false))?;
    for _ in 0..5 {
        let result = run_quiet_command(
            executable.as_os_str(),
            [
                "--ui-smoke",
                "--workbench-bench",
                "--data-dir",
                &data_dir.join("startup-runs").display().to_string(),
            ],
            None,
            &BTreeMap::new(),
        )
        .map_err(|e| ErrorEnvelope::new("BENCH_WORKBENCH", e.to_string(), false))?;
        let sample: Value = serde_json::from_str(&result.stdout).map_err(|_| {
            ErrorEnvelope::new("BENCH_WORKBENCH", "工作台原生测量未返回有效结果", false)
        })?;
        if result.status_code != Some(0)
            || result.timed_out
            || sample["status"] != "passed"
            || sample["native_window_created"] != true
            || sample["visible_windows"] != 0
            || sample["focus_stolen"] != false
            || sample["navigation_actions"] != 60
        {
            return Err(ErrorEnvelope::new(
                "BENCH_WORKBENCH",
                "工作台原生测量没有完成",
                false,
            ));
        }
        report.first_screen_times_ms.push(
            sample["first_screen_ms"]
                .as_u64()
                .ok_or_else(|| ErrorEnvelope::new("BENCH_WORKBENCH", "首屏样本缺失", false))?,
        );
        let navigation = sample["navigation_times_us"]
            .as_array()
            .ok_or_else(|| ErrorEnvelope::new("BENCH_WORKBENCH", "交互样本缺失", false))?;
        if navigation.len() != 60 {
            return Err(ErrorEnvelope::new(
                "BENCH_WORKBENCH",
                "交互样本数量不足",
                false,
            ));
        }
        for time in navigation {
            report.interaction_times_us.push(
                time.as_u64()
                    .ok_or_else(|| ErrorEnvelope::new("BENCH_WORKBENCH", "交互样本无效", false))?,
            );
        }
    }
    (
        report.first_screen_p50_ms,
        report.first_screen_p95_ms,
        report.first_screen_p99_ms,
    ) = crate::ui_verification::percentiles(&report.first_screen_times_ms);
    (
        report.interaction_p50_us,
        report.interaction_p95_us,
        report.interaction_p99_us,
    ) = crate::ui_verification::percentiles(&report.interaction_times_us);
    report.workbench_measurement = "5 fresh processes: AC-02 local library, Rust entry through fixture setup, host load and first native paint (excludes OS executable loader and does not flush disk caches); 300 navigation actions through settings persistence and native paint, including the 20 ms observer interval. 100k table/chart component scale is measured separately.".into();
    let paths = verification_app_paths(data_dir);
    let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
    let _ = seed_ac02_fixture_in_service(&mut svc)?;
    let host = AppHost::from_service(svc);
    let mut shared = WorkbenchSharedState::new(
        host,
        data_dir.to_path_buf(),
        NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
    )?;

    // Controlled worker cancellation measures dispatch separately from actual resource release.
    let job_id = shared.start_verification_cancellation_job(5000)?;
    let cancel_t0 = Instant::now();
    let snap = shared.cancel_active_sync_job()?;
    let cancel_ui_latency_ms = cancel_t0.elapsed().as_millis() as u64;
    let _ = shared
        .host
        .wait_job_finished(job_id, Duration::from_secs(5))?;
    let cancel_backend_latency_ms = cancel_t0.elapsed().as_millis() as u64;
    if !matches!(snap.state, JobState::Cancelled(_)) {
        return Err(ErrorEnvelope::new(
            "BENCH_CANCEL_FAILED",
            "sync job did not transition to Cancelled",
            false,
        ));
    }

    report.cancel_ui_latency_ms = cancel_ui_latency_ms;
    report.cancel_backend_latency_ms = cancel_backend_latency_ms;
    Ok(report)
}

pub fn run_desktop_entrypoint() -> ExitCode {
    let entry_started = Instant::now();
    apply_process_quiet_guards();
    let _job = QuietJobObject::new().inspect(|j| {
        let _ = j.assign_current_process();
    });

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // The installer restarts the app with its previous arguments, hex-encoded (`S-31`).
    if let Some(position) = args.iter().position(|a| a == "--restore-args") {
        match args
            .get(position + 1)
            .and_then(|encoded| delta_app::update::decode_relaunch_args(encoded))
        {
            Some(restored) if !restored.iter().any(|a| a == "--restore-args") => args = restored,
            _ => {
                eprintln!("invalid --restore-args");
                return ExitCode::from(2);
            }
        }
    }
    let has_flag = |f: &str| args.iter().any(|a| a == f);
    let parse_flag = |f: &str| args.windows(2).find(|w| w[0] == f).map(|w| w[1].clone());

    if has_flag("--build-info") {
        println!("{}", delta_app::build_info("delta-desktop"));
        return ExitCode::SUCCESS;
    }

    if has_flag("--version") || has_flag("-V") {
        println!(
            "{} {} ({})",
            InstallIdentity::current()
                .map(InstallIdentity::name)
                .unwrap_or("delta-source"),
            delta_app::app_version(),
            BuildChannel::current().as_str()
        );
        return ExitCode::SUCCESS;
    }
    if has_flag("--help") || has_flag("-h") {
        println!(
            "delta (M4) - GPUI desktop investment review & training workbench\n\
             Flags:\n\
             - --data-dir <dir>\n\
             - --ui-smoke [--data-dir <dir>]\n\
             - --ui-bench [--data-dir <dir>]\n\
             - --m4-ui-smoke [--data-dir <dir>]\n\
             - --m4-ui-bench [--data-dir <dir>]\n\
             - --build-info | --version | --help"
        );
        return ExitCode::SUCCESS;
    }

    let cli_data_dir = parse_flag("--data-dir").map(PathBuf::from);
    if has_flag("--m4-ui-smoke") {
        return match crate::ui_verification::run_m4_ui_smoke(cli_data_dir.as_deref()) {
            Ok(rep) => {
                println!(
                    "{}",
                    sanitize_terminal_output(&serde_json::to_string(&rep).unwrap_or_default())
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", sanitize_terminal_output(&e.message));
                ExitCode::from(1)
            }
        };
    }

    if has_flag("--m4-ui-bench") {
        return match crate::ui_verification::run_m4_k03_next_bar_bench(cli_data_dir.as_deref()) {
            Ok(rep) => {
                println!(
                    "{}",
                    sanitize_terminal_output(&serde_json::to_string(&rep).unwrap_or_default())
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", sanitize_terminal_output(&e.message));
                ExitCode::from(1)
            }
        };
    }
    if has_flag("--ui-smoke") {
        let base = cli_data_dir.unwrap_or_else(std::env::temp_dir);
        let owned = match VerificationDirectory::create(&base, "smoke") {
            Ok(owned) => owned,
            Err(error) => {
                eprintln!("{}", sanitize_terminal_output(&error.message));
                return ExitCode::from(1);
            }
        };
        let tmp_dir = owned.path.clone();
        let res = (|| -> Result<Value, ErrorEnvelope> {
            let paths = verification_app_paths(&tmp_dir);
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            seed_ac02_fixture_in_service(&mut svc)?;
            let host = AppHost::from_service(svc);
            crate::ui_verification::run_native_smoke(
                host,
                tmp_dir.clone(),
                entry_started,
                if has_flag("--workbench-bench") { 10 } else { 1 },
            )
        })();
        if let Err(error) = owned.cleanup() {
            eprintln!("{}", sanitize_terminal_output(&error.message));
            return ExitCode::from(1);
        }
        return match res {
            Ok(mut v) => {
                v["build_info"] = delta_app::build_info("delta-desktop");
                println!("{}", sanitize_terminal_output(&v.to_string()));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", sanitize_terminal_output(&e.message));
                ExitCode::from(1)
            }
        };
    }

    if has_flag("--ui-bench") {
        let base = cli_data_dir.unwrap_or_else(std::env::temp_dir);
        let res = run_fixed_scale_ui_bench(&base);
        return match res {
            Ok(rep) => {
                println!(
                    "{}",
                    sanitize_terminal_output(&serde_json::to_string(&rep).unwrap_or_default())
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{}", sanitize_terminal_output(&e.message));
                ExitCode::from(1)
            }
        };
    }

    // Gate-only hidden resident mode: never available to the user's release/dev identities.
    let resident = has_flag("--verification-resident");
    let identity = InstallIdentity::current();
    if resident && identity.is_some_and(|identity| !identity.is_test()) {
        eprintln!("verification mode is restricted to test identities");
        return ExitCode::from(2);
    }

    // Single instance (`S-29`): a second start only signals the running instance.
    let _instance = match identity.map(delta_app::SingleInstance::acquire) {
        None => None,
        Some(Ok(instance)) => Some(instance),
        Some(Err(delta_app::InstanceError::AlreadyRunning(_))) => {
            let identity = identity.unwrap();
            let reply = delta_app::send_control_command(
                identity,
                delta_app::ControlCommand::Focus,
                Duration::from_secs(10),
            );
            println!(
                "{}",
                json!({"status": "signalled-existing-instance", "identity": identity.name(),
                    "reply": reply.as_ref().ok(), "error": reply.as_ref().err().map(|e| e.code())})
            );
            return ExitCode::SUCCESS;
        }
        Some(Err(error)) => {
            eprintln!("{}", sanitize_terminal_output(&error.to_string()));
            return ExitCode::from(1);
        }
    };
    #[cfg(windows)]
    if let Some(identity) = identity {
        let aumid: Vec<u16> = identity
            .app_user_model_id()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(aumid.as_ptr());
        }
    }
    let (control_sender, control_receiver) = std::sync::mpsc::channel();
    crate::lifecycle::install_control_receiver(control_receiver);
    let _control = identity.and_then(|identity| {
        delta_app::ControlServer::start(identity, crate::lifecycle::bridge_handler(control_sender))
            .map_err(|error| eprintln!("{}", sanitize_terminal_output(&error.to_string())))
            .ok()
    });

    let paths = match resolve_app_paths(None, cli_data_dir.as_deref()) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("{}", sanitize_terminal_output(&error.to_string()));
            return ExitCode::from(1);
        }
    };
    let mut relaunch_args = Vec::new();
    if let Some(dir) = &cli_data_dir {
        relaunch_args.extend(["--data-dir".to_string(), dir.display().to_string()]);
    }
    if resident {
        relaunch_args.push("--verification-resident".into());
        if let Some(seconds) = parse_flag("--verification-max-seconds") {
            relaunch_args.extend(["--verification-max-seconds".into(), seconds]);
        }
    }
    crate::lifecycle::install_options(crate::lifecycle::LifecycleOptions {
        relaunch_args,
        allow_focus: !resident,
        backup_root: paths.default_backup_dir.clone(),
        emit_events: resident,
        event_log: resident.then(|| paths.data_dir.join("verification-events.jsonl")),
    });
    if resident {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::lifecycle::emit_event("resident-panic", json!({"message": info.to_string()}));
            previous(info);
        }));
        crate::lifecycle::emit_event(
            "resident-started",
            json!({"args": std::env::args().skip(1).collect::<Vec<_>>()}),
        );
    }
    if resident {
        let options = crate::ui_verification::ResidentOptions {
            seed: has_flag("--verification-seed"),
            pending_note: parse_flag("--verification-pending-note"),
            busy_sync_ms: parse_flag("--verification-busy-sync-ms").and_then(|v| v.parse().ok()),
            apply_update: has_flag("--verification-apply-update"),
            ignore_control: has_flag("--verification-ignore-control"),
            max_seconds: parse_flag("--verification-max-seconds")
                .and_then(|v| v.parse().ok())
                .unwrap_or(240),
        };
        return match crate::ui_verification::run_native_resident(paths, options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                crate::lifecycle::emit_event(
                    "resident-failed",
                    json!({"code": error.code, "message": error.message}),
                );
                ExitCode::from(1)
            }
        };
    }

    // Default interactive desktop window launch
    let data_dir = paths.data_dir.clone();
    let host = match AppHost::spawn(paths, "USD", "USD", "UTC") {
        Ok(host) => host,
        Err(error) => {
            eprintln!("{}", sanitize_terminal_output(&error.message));
            return ExitCode::from(1);
        }
    };
    crate::lifecycle::initialize_updates(data_dir.clone(), false);
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);
        crate::theme::apply_delta_theme(cx);
        let as_of = Utc::now().date_naive();
        let options = WindowOptions {
            titlebar: Some(gpui_kit::TitlebarOptions {
                title: Some(SharedString::from(
                    identity
                        .map(InstallIdentity::display_name)
                        .unwrap_or("DELTA Dev"),
                )),
                ..Default::default()
            }),
            window_bounds: Some(gpui_kit::WindowBounds::Windowed(
                gpui_kit::Bounds::centered(None, gpui_kit::size(px(1440.), px(900.)), cx),
            )),
            window_min_size: Some(gpui_kit::size(px(960.), px(600.))),
            ..WindowOptions::default()
        };
        cx.open_window(options, move |window, cx| {
            let view = cx.new(|cx| DeltaWorkbench::new(host, data_dir, as_of, window, cx));
            cx.new(|cx| gpui_kit::component::Root::new(view, window, cx))
        })
        .expect("open DELTA desktop window");
    });
    ExitCode::SUCCESS
}

pub(crate) fn verification_app_paths(data_dir: &Path) -> delta_app::ResolvedAppPaths {
    let documents_dir = data_dir.join("documents");
    delta_app::ResolvedAppPaths {
        channel: BuildChannel::Source,
        data_dir: data_dir.to_path_buf(),
        documents_dir: documents_dir.clone(),
        default_export_dir: documents_dir.join("exports"),
        default_backup_dir: data_dir.join("backups"),
        credential_prefix: format!("delta-test-native-{}", uuid::Uuid::new_v4()),
    }
}

// ============================================================================
// 6. Headless `#[gpui_kit::test]` Suite (`M2-K01`, `M2-A-03`, `M2-A-07`..`M2-A-09`, `M2-A-11`, `M2-A-12`, `M2-A-16`)
// ============================================================================

#[cfg(test)]
pub mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::TestAppContext;

    fn temp_test_dir(tag: &str) -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let base = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(
            "delta_m2_desktop_{tag}_{}_{}_{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn click_control(cx: &mut gpui_kit::VisualTestContext, id: &'static str) {
        let position = cx.update(|window, cx| {
            window.render_frame(cx);
            let bounds = window.find(id).bounds();
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "{id} has no rendered bounds"
            );
            bounds.center()
        });
        cx.simulate_click(position, gpui_kit::Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    async fn actual_controls_preserve_input_save_before_scope_switch_and_recover_failure(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("interaction_regression");
        let mut service = AppService::open(
            resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap(),
            "USD",
            "USD",
            "UTC",
            true,
        )
        .unwrap();
        seed_ac02_fixture_in_service(&mut service).unwrap();
        let host = AppHost::from_service(service);
        let mut runtime = AgentRuntime::new(host.clone()).unwrap();
        runtime
            .configure_model_connection(
                ModelConnection::loopback(
                    "saved-library-model",
                    ModelProtocol::OpenAiChatCompletions,
                    "http://127.0.0.1:1/v1",
                    "restored-model",
                ),
                None,
            )
            .unwrap();
        cx.update(gpui_kit::init);
        let (workbench, cx) = cx.add_window_view(|window, cx| {
            DeltaWorkbench::new(
                host.clone(),
                tmp.clone(),
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                window,
                cx,
            )
        });
        let (controller, center, state) = cx.update(|_, cx| {
            let view = workbench.read(cx);
            (
                view.controller.clone(),
                view.center_panel.clone(),
                Arc::clone(view.shared_state()),
            )
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().overview.is_some()
            })
            .await;

        click_control(cx, "nav-btn-nav-journal");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        assert_eq!(state.lock().unwrap().layout.active_nav, "nav.journal");
        cx.update(|window, cx| {
            let body = center.read(cx).note_body.clone();
            let focus = body.read(cx).focus_handle(cx);
            focus.focus(window, cx);
        });
        cx.simulate_input("复核输入");
        assert_eq!(state.lock().unwrap().note_editor.body_buffer, "复核输入");
        cx.executor().advance_clock(Duration::from_millis(1100));
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy
                    && matches!(
                        ctl.state.lock().unwrap().note_editor.status,
                        AutosaveStatus::Saved { .. }
                    )
            })
            .await;
        let note_id = state.lock().unwrap().note_editor.note_id.clone();
        assert_eq!(host.get_note(&note_id).unwrap().body_markdown, "复核输入");

        host.execute(|_| {
            set_note_save_fault_injection_for_test(true);
            Ok(())
        })
        .unwrap();
        cx.simulate_input(" · 保存失败缓冲");
        cx.update(|window, cx| {
            workbench.update(cx, |workbench, cx| {
                workbench
                    .scope_fields
                    .set("form.scope_target", "ACC-1", window, cx)
            });
        });
        click_control(cx, "scope-apply");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().ui_error.is_some()
            })
            .await;
        assert!(matches!(
            state.lock().unwrap().scope.target,
            ScopeTarget::AllAccounts
        ));
        assert_eq!(state.lock().unwrap().note_editor.note_id, note_id);
        assert!(state
            .lock()
            .unwrap()
            .note_editor
            .body_buffer
            .contains("保存失败缓冲"));
        assert!(matches!(
            state.lock().unwrap().note_editor.status,
            AutosaveStatus::SaveFailed { .. }
        ));
        host.execute(|_| {
            set_note_save_fault_injection_for_test(false);
            Ok(())
        })
        .unwrap();
        click_control(cx, "note-save");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy
                    && matches!(
                        ctl.state.lock().unwrap().note_editor.status,
                        AutosaveStatus::Saved { .. }
                    )
            })
            .await;
        assert!(host
            .get_note(&note_id)
            .unwrap()
            .body_markdown
            .contains("保存失败缓冲"));
        click_control(cx, "scope-apply");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy
                    && matches!(
                        ctl.state.lock().unwrap().scope.target,
                        ScopeTarget::Accounts(_)
                    )
            })
            .await;
        assert_eq!(state.lock().unwrap().scope.resolved_accounts, vec!["ACC-1"]);
        assert_ne!(state.lock().unwrap().note_editor.note_id, note_id);

        click_control(cx, "nav-btn-nav-settings");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        cx.update(|_, cx| {
            let panel = center.read(cx);
            assert_eq!(
                panel.fields.value("form.model_protocol", cx),
                "openai-chat-completions"
            );
            assert_eq!(
                panel.fields.value("form.base_url", cx),
                "http://127.0.0.1:1/v1"
            );
            assert_eq!(panel.fields.value("form.model_id", cx), "restored-model");
            assert_eq!(panel.fields.value("form.api_key", cx), "");
        });
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set("form.report_currency", "EUR", window, cx);
                panel
                    .fields
                    .set("form.model_id", "unsaved-model-draft", window, cx);
            });
            controller.update(cx, |ctl, cx| ctl.dispatch(UiAction::Refresh, window, cx));
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        cx.update(|_, cx| {
            assert_eq!(
                center.read(cx).fields.value("form.report_currency", cx),
                "EUR"
            );
            assert_eq!(
                center.read(cx).fields.value("form.model_id", cx),
                "unsaved-model-draft"
            );
        });
        assert_eq!(state.lock().unwrap().layout.report_currency, "USD");
        click_control(cx, "model-configure");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        let saved = runtime.model_connection_settings().unwrap().unwrap();
        assert_eq!(saved.id, "saved-library-model");
        assert_eq!(saved.model_id, "unsaved-model-draft");
        assert_eq!(count_visible_windows_for_current_process(), 0);
        cx.update(|window, _| window.remove_window());
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn actual_first_run_import_preview_commit_and_ai_failure_controls_reach_the_host(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("first_run_controls");
        let host = AppHost::from_service(
            AppService::open(verification_app_paths(&tmp), "USD", "USD", "UTC", true).unwrap(),
        );
        cx.update(gpui_kit::init);
        let (workbench, cx) = cx.add_window_view(|window, cx| {
            DeltaWorkbench::new(
                host.clone(),
                tmp.clone(),
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                window,
                cx,
            )
        });
        cx.simulate_resize(gpui_kit::size(px(1400.), px(1500.)));
        let (controller, center, ai, state) = cx.update(|_, cx| {
            let view = workbench.read(cx);
            (
                view.controller.clone(),
                view.center_panel.clone(),
                view.right_ai_panel.clone(),
                Arc::clone(view.shared_state()),
            )
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        click_control(cx, "nav-btn-nav-data_center");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set("form.account_id", "ACC-UI", window, cx);
                panel
                    .fields
                    .set("form.account_name", "离线控件账户", window, cx);
            })
        });
        click_control(cx, "account-save");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && !ctl.state.lock().unwrap().accounts.is_empty()
            })
            .await;
        assert_eq!(host.list_accounts().unwrap()[0].id, "ACC-UI");
        let statement = tmp.join("synthetic-statement.csv");
        let contents = concat!(
            "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n",
            "ACC-UI,opening_balance,2026-01-02 10:00:00,UTC,USD,buy,2000,1,USD,0,USD,UI-OPEN\n",
            "ACC-UI,buy,2026-01-05 15:00:00,UTC,NASDAQ:AAPL,buy,2,100,USD,1,USD,UI-BUY\n",
        );
        fs::write(&statement, contents).unwrap();
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel
                    .fields
                    .set("form.import_account", "ACC-UI", window, cx);
                panel.fields.set(
                    "form.import_path",
                    statement.display().to_string(),
                    window,
                    cx,
                );
            })
        });
        click_control(cx, "import-preview");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().import_preview.is_some()
            })
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .import_preview
                .as_ref()
                .unwrap()
                .rows
                .len(),
            2
        );
        fs::write(&statement, contents.replace(",2,100,", ",2,101,")).unwrap();
        click_control(cx, "import-commit");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().ui_error.is_some()
            })
            .await;
        assert_eq!(
            state.lock().unwrap().ui_error.as_ref().unwrap().code,
            "IMPORT_PREVIEW_STALE"
        );
        assert!(state
            .lock()
            .unwrap()
            .transactions
            .as_ref()
            .unwrap()
            .value
            .is_empty());
        fs::write(&statement, contents).unwrap();
        click_control(cx, "import-preview");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        click_control(cx, "import-commit");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().last_import_receipt.is_some()
            })
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .last_import_receipt
                .as_ref()
                .unwrap()
                .committed_events,
            2
        );
        click_control(cx, "import-preview");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        assert!(state
            .lock()
            .unwrap()
            .import_preview
            .as_ref()
            .unwrap()
            .already_imported_batch_id
            .is_some());
        click_control(cx, "import-commit");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        assert!(
            state
                .lock()
                .unwrap()
                .last_import_receipt
                .as_ref()
                .unwrap()
                .idempotent_skipped
        );
        assert_eq!(
            state
                .lock()
                .unwrap()
                .transactions
                .as_ref()
                .unwrap()
                .value
                .len(),
            2
        );
        cx.update(|window, cx| {
            let question = ai.read(cx).question.clone();
            question.read(cx).focus_handle(cx).focus(window, cx);
        });
        cx.simulate_input("解释当前范围的复盘结果");
        click_control(cx, "ai-send");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().last_ai_error.is_some()
            })
            .await;
        assert_eq!(
            state.lock().unwrap().last_ai_error.as_ref().unwrap().code,
            "model_not_configured"
        );
        assert!(state.lock().unwrap().overview.is_some());
        assert!(state.lock().unwrap().last_ai_report.is_none());
        assert_eq!(count_visible_windows_for_current_process(), 0);
        cx.update(|window, _| window.remove_window());
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn chart_mouse_wheel_crosshair_escape_and_marker_click_reach_scoped_host(
        cx: &mut TestAppContext,
    ) {
        let tmp = temp_test_dir("chart_pointer");
        let mut service = AppService::open(
            resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap(),
            "USD",
            "USD",
            "UTC",
            true,
        )
        .unwrap();
        seed_ac02_fixture_in_service(&mut service).unwrap();
        let host = AppHost::from_service(service);
        cx.update(gpui_kit::init);
        let (workbench, cx) = cx.add_window_view(|window, cx| {
            DeltaWorkbench::new(
                host.clone(),
                tmp.clone(),
                NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                window,
                cx,
            )
        });
        cx.simulate_resize(gpui_kit::size(px(1400.), px(1100.)));
        let (controller, center, state) = cx.update(|_, cx| {
            let view = workbench.read(cx);
            (
                view.controller.clone(),
                view.center_panel.clone(),
                Arc::clone(view.shared_state()),
            )
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().kline_slice.is_some()
            })
            .await;
        click_control(cx, "nav-btn-nav-market");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        state.lock().unwrap().kline_viewport.min_visible_bars = 1;
        let bounds = cx.update(|window, cx| {
            window.render_frame(cx);
            center.read(cx).plot_bounds.lock().unwrap().unwrap()
        });
        cx.simulate_mouse_move(bounds.center(), None, gpui_kit::Modifiers::default());
        assert!(state
            .lock()
            .unwrap()
            .kline_viewport
            .crosshair_index
            .is_some());
        assert!(state
            .lock()
            .unwrap()
            .kline_viewport
            .crosshair_price
            .is_some());
        let old_len = state.lock().unwrap().kline_viewport.visible_len();
        cx.simulate_event(gpui_kit::ScrollEvent {
            position: bounds.center(),
            delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(240.))),
            modifiers: gpui_kit::Modifiers::default(),
            touch_phase: gpui_kit::TouchPhase::Moved,
            source: gpui_kit::ScrollSource::Pointer,
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| !ctl.busy)
            .await;
        assert!(state.lock().unwrap().kline_viewport.visible_len() < old_len);
        cx.simulate_click(bounds.center(), gpui_kit::Modifiers::default());
        cx.simulate_keystrokes("escape");
        assert!(state
            .lock()
            .unwrap()
            .kline_viewport
            .crosshair_index
            .is_none());
        let (point, target) = cx.update(|window, cx| {
            window.render_frame(cx);
            let bounds = center.read(cx).plot_bounds.lock().unwrap().unwrap();
            let state = state.lock().unwrap();
            let marker = state.kline_slice.as_ref().unwrap().markers.last().unwrap();
            (
                gpui_kit::point(
                    bounds.origin.x + bounds.size.width * marker.x_ratio,
                    bounds.origin.y + bounds.size.height * marker.y_ratio,
                ),
                marker.marker.target_id.clone(),
            )
        });
        cx.simulate_click(point, gpui_kit::Modifiers::default());
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                !ctl.busy && ctl.state.lock().unwrap().selected_marker_outcome.is_some()
            })
            .await;
        assert!(
            matches!(&state.lock().unwrap().selected_marker_outcome, Some(MarkerSelectionOutcome::TradeFill(row)) if row.event_id == target)
        );
        assert_eq!(state.lock().unwrap().layout.active_nav, "nav.assets");
        cx.update(|window, _| window.remove_window());
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn test_m2_k01_and_a03_thin_slice_ac02_overview_and_dependency_boundary(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("k01_a03");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let _dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        let (handle, _wb) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| DeltaWorkbench::new(host.clone(), tmp.clone(), as_of, window, cx))
            })
            .unwrap()
        });
        cx.run_until_parked();

        // 1. Verify headless GPUI bounds and rendered MetricCard values (Cash 1438, NAV 2068, Total P&L 68)
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let root = window.find("delta-workbench-root");
            assert!(root.bounds().size.width > px(0.0));

            let cash_card = window.find("card-cash");
            let nav_card = window.find("card-nav");
            let pnl_card = window.find("card-total-pnl");

            assert!(cash_card.label().unwrap_or_default().contains("1438"));
            assert!(nav_card.label().unwrap_or_default().contains("2068"));
            assert!(pnl_card.label().unwrap_or_default().contains("+68 USD"));
        })
        .unwrap();

        // 2. Verify 0 visible windows during headless GPUI test (`D-25`)
        assert_eq!(count_visible_windows_for_current_process(), 0);

        // 3. Verify `cargo metadata` direct dependency boundary (`S-12`)
        let direct_deps = verify_desktop_dependency_boundary().unwrap();
        assert!(direct_deps.contains(&"delta-app".to_string()));
        assert!(direct_deps.contains(&"gpui-kit".to_string()));
        for forbidden in [
            "delta-data",
            "delta-core",
            "delta-model",
            "rusqlite",
            "tokio",
            "reqwest",
        ] {
            assert!(!direct_deps.contains(&forbidden.to_string()));
        }

        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn test_m2_a07_workbench_navigation_scope_bar_layout_recovery_and_import_preview(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("a07");

        // 1. Write a corrupted `settings/layout.json` before startup -> must fall back to default layout with `LayoutFallback` notice
        let settings_dir = tmp.join("settings");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("layout.json"), b"{corrupted_json!!!").unwrap();

        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let host = AppHost::from_service(svc);
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        let mut shared_opt: Option<Arc<Mutex<WorkbenchSharedState>>> = None;
        let (handle, _wb) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let wb =
                    cx.new(|cx| DeltaWorkbench::new(host.clone(), tmp.clone(), as_of, window, cx));
                shared_opt = Some(Arc::clone(wb.read(cx).shared_state()));
                wb
            })
            .unwrap()
        });
        cx.run_until_parked();
        let shared = shared_opt.unwrap();

        {
            let mut g = shared.lock().unwrap();
            // Corrupted layout recovered to default `nav.overview` and emitted `LayoutFallback` + `EmptyLibrary` states
            assert_eq!(g.layout.active_nav, "nav.overview");
            assert!(g
                .system_states
                .iter()
                .any(|s| matches!(s, WorkbenchSystemState::LayoutFallback { .. })));
            assert!(g
                .system_states
                .iter()
                .any(|s| matches!(s, WorkbenchSystemState::EmptyLibrary)));

            // 2. Navigate across all in-scope pages (including M4 training) and reject out-of-scope strategy (`S-15`, `S-46`)
            for &key in PRIMARY_NAV_KEYS {
                g.navigate_to(key).unwrap();
                assert_eq!(g.layout.active_nav, key);
            }
            assert!(g.navigate_to("nav.training").is_ok());
            assert!(g.navigate_to("nav.strategy").is_err());

            // 3. First-run wizard: add account, preview CSV with a held row (missing source_id), resolve held row, commit, and re-import duplicate
            g.host
                .upsert_account(
                    "ACC-WIZ",
                    "Wizard Account",
                    "broker",
                    "futu",
                    None,
                    "active",
                )
                .unwrap();
            let csv_path = tmp.join("imports").join("wiz_import.csv");
            let csv_body = concat!(
                "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n",
                "ACC-WIZ,opening_balance,2026-01-02 10:00:00,UTC,USD,buy,5000,1,USD,0,USD,WIZ-OPEN-1\n",
                "ACC-WIZ,buy,2026-01-05 15:00:00,UTC,NASDAQ:AAPL,buy,2,100,USD,1,USD,\n",
            );
            fs::write(&csv_path, csv_body).unwrap();

            let preview = g
                .preview_statement_import("ACC-WIZ", StatementPreset::GenericCsv, csv_path.clone())
                .unwrap();
            assert_eq!(preview.rows.len(), 2);
            let (_, receipt) = g
                .commit_statement_import("ACC-WIZ", StatementPreset::GenericCsv, csv_path.clone())
                .unwrap();
            assert_eq!(receipt.committed_events, 2);

            // Second file with a matching no-source_id row -> held as suspected duplicate!
            let csv2_path = tmp.join("imports").join("wiz_import2.csv");
            let csv2_body = concat!(
                "account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n",
                "ACC-WIZ,buy,2026-01-05 15:00:00,UTC,NASDAQ:AAPL,buy,2,100,USD,1,USD,\n",
            );
            fs::write(&csv2_path, csv2_body).unwrap();
            assert_eq!(
                g.commit_statement_import(
                    "ACC-WIZ",
                    StatementPreset::GenericCsv,
                    csv2_path.clone(),
                )
                .unwrap_err()
                .code,
                "IMPORT_PREVIEW_STALE"
            );
            g.preview_statement_import("ACC-WIZ", StatementPreset::GenericCsv, csv2_path.clone())
                .unwrap();
            let (_, receipt2) = g
                .commit_statement_import("ACC-WIZ", StatementPreset::GenericCsv, csv2_path)
                .unwrap();
            assert_eq!(receipt2.suspected_duplicates_held, 1);

            // Re-import the same file -> detects duplicate!
            let dup_preview = g
                .preview_statement_import("ACC-WIZ", StatementPreset::GenericCsv, csv_path)
                .unwrap();
            assert!(dup_preview.already_imported_batch_id.is_some());

            // Switch report currency and verify settings persist without altering raw events
            g.set_report_currency("HKD").unwrap();
            assert_eq!(g.scope.report_currency, "HKD");
            let (reloaded_cfg, fb) = load_or_default_layout(&tmp);
            assert!(fb.is_none());
            assert_eq!(reloaded_cfg.report_currency, "HKD");
        }

        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn test_m2_a08_data_center_sync_progress_cancel_and_failure_states(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("a08");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let _ = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        let mut shared_opt: Option<Arc<Mutex<WorkbenchSharedState>>> = None;
        let (handle, _wb) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let wb =
                    cx.new(|cx| DeltaWorkbench::new(host.clone(), tmp.clone(), as_of, window, cx));
                shared_opt = Some(Arc::clone(wb.read(cx).shared_state()));
                wb
            })
            .unwrap()
        });
        cx.run_until_parked();
        let shared = shared_opt.unwrap();

        {
            let mut g = shared.lock().unwrap();

            // 1. Start a sync job and verify `Syncing` state is active
            let job_id = g.start_verification_cancellation_job(4000).unwrap();
            assert!(g
                .system_states
                .iter()
                .any(|s| matches!(s, WorkbenchSystemState::Syncing { .. })));

            // 2. Cancel mid-run: UI acknowledges within <= 1 s (`< 1000 ms`) and job reaches terminal state within <= 5 s
            let t0 = Instant::now();
            let snap = g.cancel_active_sync_job().unwrap();
            let ui_ms = t0.elapsed().as_millis();
            assert!(
                ui_ms <= 1000,
                "cancel UI acknowledgment took {ui_ms} ms (> 1000 ms)"
            );
            assert!(matches!(snap.state, JobState::Cancelled(_)));
            assert!(g
                .system_states
                .iter()
                .any(|s| matches!(s, WorkbenchSystemState::Cancelled { .. })));

            let term_snap = g
                .host
                .wait_job_finished(job_id, Duration::from_secs(5))
                .unwrap();
            assert!(matches!(term_snap.state, JobState::Cancelled(_)));
        }

        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn test_m2_a09_assets_holdings_fifo_lots_flows_reconciliation_and_drilldown(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("a09");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let _ = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        let mut shared_opt: Option<Arc<Mutex<WorkbenchSharedState>>> = None;
        let (handle, _wb) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let wb =
                    cx.new(|cx| DeltaWorkbench::new(host.clone(), tmp.clone(), as_of, window, cx));
                shared_opt = Some(Arc::clone(wb.read(cx).shared_state()));
                wb
            })
            .unwrap()
        });
        cx.run_until_parked();
        let shared = shared_opt.unwrap();

        {
            let mut g = shared.lock().unwrap();

            // 1. Verify AC-02 FIFO open lot: remaining quantity 6, cost 600.6, realized PnL 38.6
            let hl = g.holdings_lots.as_ref().unwrap();
            assert_eq!(hl.value.open_lots.len(), 1);
            assert_eq!(
                hl.value.open_lots[0].remaining_quantity,
                parse_decimal_lossless("6").unwrap()
            );
            assert_eq!(
                hl.value.open_lots[0].remaining_cost_book,
                parse_decimal_lossless("600.6").unwrap()
            );
            assert_eq!(
                g.breakdown.as_ref().unwrap().value.realized_pnl,
                Some(parse_decimal_lossless("38.6").unwrap())
            );

            // 2. Enter a manual balance snapshot that differs from the ledger (observed 5 shares vs 6 expected)
            let issues = g
                .host
                .reconcile_account(
                    "ACC-1",
                    vec![AccountBalanceSnapshotDto {
                        asset_id: "NASDAQ:AAPL".to_string(),
                        quantity: parse_decimal_lossless("5").unwrap(),
                        as_of: Utc::now(),
                    }],
                )
                .unwrap();
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].expected, "6");
            assert_eq!(issues[0].observed, "5");
            assert_eq!(issues[0].difference, "-1");

            // Holdings must NOT be auto-adjusted by reconciliation!
            g.refresh_all().unwrap();
            let hl_after = g.holdings_lots.as_ref().unwrap();
            assert_eq!(
                hl_after.value.open_lots[0].remaining_quantity,
                parse_decimal_lossless("6").unwrap()
            );

            // 3. Drill-down from Overview P&L to Assets Flows preserving scope and filter
            g.drilldown_from_overview_to_flows(Some("NASDAQ:AAPL".to_string()))
                .unwrap();
            assert_eq!(g.layout.active_nav, "nav.assets");
            assert_eq!(g.assets_sub_tab, AssetsSubTab::Flows);
            assert_eq!(
                g.transaction_filter.instrument_id.as_deref(),
                Some("NASDAQ:AAPL")
            );
            let txs = g.transactions.as_ref().unwrap();
            assert_eq!(txs.value.len(), 2); // AC-02 Buy & Sell of AAPL
        }

        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn test_m2_a11_and_a12_markers_chart_context_roundtrip_and_journal_autosave(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("a11_a12");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        let mut shared_opt: Option<Arc<Mutex<WorkbenchSharedState>>> = None;
        let (handle, _wb) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let wb =
                    cx.new(|cx| DeltaWorkbench::new(host.clone(), tmp.clone(), as_of, window, cx));
                shared_opt = Some(Arc::clone(wb.read(cx).shared_state()));
                wb
            })
            .unwrap()
        });
        cx.run_until_parked();
        let shared = shared_opt.unwrap();

        {
            let mut g = shared.lock().unwrap();

            // 1. Save a ChartContext and a Note linked to both the ChartContext and the Buy trade fill
            let ctx_id = g
                .host
                .save_chart_context(ChartContextDto {
                    id: "CTX-A11-1".to_string(),
                    instrument_id: "NASDAQ:AAPL".to_string(),
                    venue: "NASDAQ".to_string(),
                    timeframe: "1d".to_string(),
                    visible_start_date: NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
                    visible_end_date: NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                    anchor_date: Some(NaiveDate::from_ymd_opt(2026, 1, 10).unwrap()),
                    adjustment: AdjustmentKind::Raw,
                    indicators_json: r#"["SMA5","EMA12"]"#.to_string(),
                    dataset_version: Some(dsv),
                    latest_dataset_version: dsv,
                    has_newer_dataset_version: false,
                })
                .unwrap();

            g.note_editor = NoteEditorSession::new(
                "NOTE-A11-1",
                "苹果交易复盘笔记",
                NoteTemplateKind::PostTrade,
                true,
            );
            let rev1 = g
                .edit_and_autosave_note("苹果突破买点与部分止盈复盘结论".to_string())
                .unwrap();
            assert_eq!(rev1, 1);
            assert!(matches!(
                g.note_editor.status,
                AutosaveStatus::Saved { revision: 1, .. }
            ));

            let buy_event_id = g
                .transactions
                .as_ref()
                .unwrap()
                .value
                .iter()
                .find(|r| r.event_type == "buy")
                .unwrap()
                .event_id
                .clone();
            g.host
                .link_plan_or_note_fill(
                    None,
                    Some("NOTE-A11-1"),
                    &buy_event_id,
                    parse_decimal_lossless("6").unwrap(),
                    "entry_fill",
                )
                .unwrap();
            // Over-linking beyond fill quantity (10) must be rejected (`M2-A-11`)
            assert!(g
                .host
                .link_plan_or_note_fill(
                    None,
                    Some("NOTE-A11-1"),
                    &buy_event_id,
                    parse_decimal_lossless("5").unwrap(),
                    "overflow_fill",
                )
                .is_err());

            g.host
                .add_note_link("NOTE-A11-1", "chart_context", &ctx_id, dsv, None, "anchor")
                .unwrap();
            g.refresh_all().unwrap();

            // 2. Click trade marker and note marker on K-line slice (`M2-A-11`)
            let trade_marker_id = format!("MK:EV:{buy_event_id}");
            let out_trade = g.select_chart_marker(&trade_marker_id).unwrap();
            assert!(
                matches!(out_trade, MarkerSelectionOutcome::TradeFill(ref r) if r.event_id == buy_event_id)
            );

            let out_note = g.select_chart_marker("MK:NOTE:NOTE-A11-1").unwrap();
            assert!(
                matches!(out_note, MarkerSelectionOutcome::Note(ref n) if n.id == "NOTE-A11-1")
            );

            // 3. Jump from note back to chart context (`jump_to_chart_context`)
            let restored_ctx = g.jump_to_chart_context(&ctx_id).unwrap();
            assert_eq!(restored_ctx.instrument_id, "NASDAQ:AAPL");
            assert_eq!(g.layout.active_nav, "nav.market");

            // 4. Inject write failure during autosave (`M2-A-12`): status -> `SaveFailed`, buffer retained, never shows `Saved`
            g.host
                .execute(|_| {
                    set_note_save_fault_injection_for_test(true);
                    Ok(())
                })
                .unwrap();
            let err_save = g
                .edit_and_autosave_note("未保存草稿内容保留测试".to_string())
                .unwrap_err();
            assert_eq!(err_save.code, "autosave_failed");
            assert!(matches!(
                g.note_editor.status,
                AutosaveStatus::SaveFailed {
                    retryable: true,
                    ..
                }
            ));
            assert_eq!(g.note_editor.body_buffer, "未保存草稿内容保留测试");

            // Retry save succeeds -> revision 2
            let rev2 = g
                .edit_and_autosave_note("未保存草稿内容保留测试".to_string())
                .unwrap();
            assert_eq!(rev2, 2);

            // 5. Search 1-, 2-, and 3+-character Chinese queries
            let (hits_1, mode_1) = g.search_notes("苹").unwrap();
            assert_eq!(mode_1, "bounded_like_fallback");
            assert_eq!(hits_1.len(), 1);
            let (hits_2, mode_2) = g.search_notes("复盘").unwrap();
            assert_eq!(mode_2, "bounded_like_fallback");
            assert_eq!(hits_2.len(), 1);
            let (hits_3, mode_3) = g.search_notes("交易复盘").unwrap();
            assert_eq!(mode_3, "fts5_trigram");
            assert_eq!(hits_3.len(), 1);
        }

        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[gpui_kit::test]
    async fn test_m2_a16_local_workbench_without_model_and_unreachable_endpoint(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_test_dir("a16");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        let _ = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = AppHost::from_service(svc);
        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();

        let mut shared_opt: Option<Arc<Mutex<WorkbenchSharedState>>> = None;
        let (handle, _wb) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let wb =
                    cx.new(|cx| DeltaWorkbench::new(host.clone(), tmp.clone(), as_of, window, cx));
                shared_opt = Some(Arc::clone(wb.read(cx).shared_state()));
                wb
            })
            .unwrap()
        });
        cx.run_until_parked();
        let shared = shared_opt.unwrap();

        {
            let mut g = shared.lock().unwrap();

            // 1. All local tabs (Overview, Assets, Market K-Line, Journal) work without any model configured
            assert!(g.overview.is_some());
            assert!(g.holdings_lots.is_some());
            assert!(g.kline_slice.is_some());

            // 2. Request AI review without model -> sets `ModelUnavailable` status banner with retryable error while local reports remain readable
            let err = g
                .request_ai_review("sess-ui-a16", "Explain my portfolio")
                .unwrap_err();
            assert_eq!(err.code, "model_not_configured");
            assert!(err.retryable);
            assert!(g
                .system_states
                .iter()
                .any(|s| matches!(s, WorkbenchSystemState::ModelUnavailable { .. })));

            let report_id = g.overview.as_ref().unwrap().value.report_id.clone();
            let saved = g.host.get_saved_report_status(&report_id).unwrap();
            assert!(!saved.is_stale);
        }

        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }
}
