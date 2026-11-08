//! Serial background operations and foreground presentation updates.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use chrono::NaiveDate;
use delta_app::{
    AccountBalanceSnapshotDto, AdjustmentKind, AnalysisScope, AppService, ChartContextDto,
    ErrorEnvelope, JobState, KlineViewport, ManualValuation, ModelConnection, ModelEventKind,
    NoteEditorSession, NoteTemplateKind, StatementPreset, SyncRequest, TransactionFilterDto,
};
use gpui_kit::{Context, EventEmitter, Task, Window};
use rust_decimal::Decimal;

use crate::workbench::{load_or_default_layout, save_layout_config, WorkbenchSharedState};

pub struct WorkbenchUpdated;

#[derive(Clone)]
pub enum UiAction {
    Initialize,
    Refresh,
    SaveLayout,
    SeedExample,
    Settings {
        currency: String,
        timezone: String,
    },
    Account {
        id: String,
        name: String,
        kind: String,
        platform: String,
        status: String,
    },
    Portfolio {
        id: String,
        name: String,
        accounts: Vec<String>,
        portfolios: Vec<String>,
    },
    ImportPreview {
        account: String,
        preset: StatementPreset,
        file: PathBuf,
    },
    ImportCommit {
        account: String,
        preset: StatementPreset,
        file: PathBuf,
    },
    ResolveHeld {
        row: String,
        include: bool,
    },
    Sync(SyncRequest),
    ManualValuation(ManualValuation),
    Reconcile {
        account: String,
        snapshots: Vec<AccountBalanceSnapshotDto>,
    },
    Amend {
        event: String,
        quantity: Decimal,
        price: Option<Decimal>,
        fee: Decimal,
    },
    SearchInstrument(String),
    SelectInstrument(String),
    ChartViewport(KlineViewport),
    ChartJumpDate(NaiveDate),
    ChartAdjustment(AdjustmentKind),
    ReloadChart,
    Indicators(String),
    Watchlist {
        name: String,
        instrument: String,
    },
    ChartContext(String),
    SelectMarker(String),
    SelectTransaction(String),
    PageTransactions(TransactionFilterDto),
    NewNote(NoteTemplateKind),
    OpenNote(String),
    SaveNote(NoteEditorSession),
    LinkFill {
        event: String,
        quantity: String,
    },
    LinkChart(ChartContextDto),
    Attachment(PathBuf),
    SearchNotes(String),
    ConfigureModel {
        connection: ModelConnection,
        secret: Option<String>,
    },
    ProbeModel,
    AiReview {
        session: String,
        question: String,
    },
    CompactSession {
        session: String,
    },
    AiScope {
        pinned: bool,
        include_notes: bool,
    },
    OpenAiReport(String),
    OpenEvidence(delta_app::EvidenceRef),
    Backup(PathBuf),
    Export(PathBuf),
    Restore {
        backup: PathBuf,
        target: PathBuf,
    },
    DeletePreview,
    DeleteLibrary {
        preview: delta_app::DeleteLibraryPreviewDto,
        typed_id: String,
    },
}

/// Workers mutate a private copy, never the mutex used by rendering. Only the foreground
/// executor publishes completed state and notifies panels.
pub struct WorkbenchController {
    pub state: Arc<Mutex<WorkbenchSharedState>>,
    queue: VecDeque<UiAction>,
    task: Option<Task<()>>,
    pub busy: bool,
    pub ai_running: bool,
    cancel: Arc<AtomicBool>,
    pub stream_text: Arc<Mutex<String>>,
    scope_revision: u64,
    navigation_revision: u64,
    pending_scope: Option<AnalysisScope>,
    chart_revision: u64,
    ai_scope_revision: u64,
}

impl EventEmitter<WorkbenchUpdated> for WorkbenchController {}

impl WorkbenchController {
    pub fn new(state: Arc<Mutex<WorkbenchSharedState>>) -> Self {
        Self {
            state,
            queue: VecDeque::new(),
            task: None,
            busy: false,
            ai_running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            stream_text: Arc::new(Mutex::new(String::new())),
            scope_revision: 0,
            navigation_revision: 0,
            pending_scope: None,
            chart_revision: 0,
            ai_scope_revision: 0,
        }
    }

    pub fn dispatch(&mut self, action: UiAction, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(action, UiAction::ConfigureModel { .. }) {
            self.cancel.store(true, Ordering::SeqCst);
            self.ai_scope_revision += 1;
            let mut state = self.state.lock().unwrap();
            state.last_ai_report = None;
            state.opened_ai_evidence = None;
            state.pre_send_scope = None;
            self.stream_text.lock().unwrap().clear();
        }
        if matches!(action, UiAction::DeleteLibrary { .. }) {
            self.cancel.store(true, Ordering::SeqCst);
            self.queue.clear();
        }
        if matches!(action, UiAction::ChartViewport(_)) {
            self.chart_revision += 1;
            self.queue
                .retain(|pending| !matches!(pending, UiAction::ChartViewport(_)));
        }
        if let UiAction::AiScope { include_notes, .. } = &action {
            let mut state = self.state.lock().unwrap();
            if state.scope.include_notes != *include_notes {
                self.cancel.store(true, Ordering::SeqCst);
                self.ai_scope_revision += 1;
                state.last_ai_report = None;
                state.opened_ai_evidence = None;
                state.saved_ai_reports.clear();
                state.pre_send_scope = None;
                self.stream_text.lock().unwrap().clear();
            }
        }
        if let UiAction::SaveNote(editor) = &action {
            self.queue.retain(|pending| !matches!(pending, UiAction::SaveNote(old) if old.note_id == editor.note_id));
            if self.state.lock().unwrap().note_editor.note_id == editor.note_id {
                self.state.lock().unwrap().note_editor.status = delta_app::AutosaveStatus::Saving;
            }
        }
        self.queue.push_back(action);
        self.pump(window, cx);
    }

    pub fn navigate(&mut self, nav: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::i18n::is_in_scope_nav_key(nav) {
            return;
        }
        self.navigation_revision += 1;
        self.state.lock().unwrap().layout.active_nav = nav.to_string();
        cx.emit(WorkbenchUpdated);
        self.dispatch(UiAction::SaveLayout, window, cx);
    }

    pub fn change_scope(
        &mut self,
        scope: AnalysisScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel.store(true, Ordering::SeqCst);
        self.pending_scope = Some(scope);
        self.queue.clear();
        self.state.lock().unwrap().ui_notice = "正在保存编辑内容并切换范围…".into();
        cx.emit(WorkbenchUpdated);
        self.pump(window, cx);
    }

    fn apply_pending_scope(&mut self, mut scope: AnalysisScope, cx: &mut Context<Self>) {
        self.scope_revision += 1;
        scope.versions_frozen = false;
        scope.dataset_version = 0;
        scope.ledger_revision = 0;
        scope.journal_revision.clear();
        let mut state = self.state.lock().unwrap();
        state.host.advance_generation();
        state.scope = scope;
        state.overview = None;
        state.breakdown = None;
        state.holdings_lots = None;
        state.transactions = None;
        state.reconciliation_issues = None;
        state.data_quality = None;
        state.manual_valuations.clear();
        state.transaction_filter = TransactionFilterDto::default();
        state.import_preview = None;
        state.last_import_receipt = None;
        state.notes_results.clear();
        state.notes_result_notice.clear();
        state.note_editor = new_note_editor(NoteTemplateKind::PostTrade);
        state.kline_slice = None;
        state.selected_marker_outcome = None;
        state.last_ai_report = None;
        state.opened_ai_evidence = None;
        state.last_ai_error = None;
        state.saved_ai_reports.clear();
        state.pre_send_scope = None;
        state.runtime.set_pinned_scope(None);
        self.stream_text.lock().unwrap().clear();
        state.ui_notice = "正在读取所选范围…".into();
        drop(state);
        cx.emit(WorkbenchUpdated);
        self.queue.push_front(UiAction::Refresh);
    }

    pub fn cancel_ai(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::SeqCst);
        self.state.lock().unwrap().ui_notice = "正在取消 AI 生成…".into();
        cx.emit(WorkbenchUpdated);
    }

    pub fn cancel_sync(&mut self, cx: &mut Context<Self>) {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.active_sync_job {
            match state.host.cancel_job(job) {
                Ok(snapshot) => {
                    state.last_job_snapshot = Some(snapshot);
                    state.ui_notice = "已接受取消，正在收回后台资源…".into();
                }
                Err(error) => state.ui_error = Some(error),
            }
        }
        cx.emit(WorkbenchUpdated);
    }

    pub fn poll_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut state = self.state.lock().unwrap();
        if let Some(id) = state.active_sync_job {
            match state.host.poll_job_throttled(id) {
                Ok(snapshot) => {
                    let finished = snapshot.worker_finished;
                    let success = matches!(snapshot.state, JobState::Succeeded);
                    state.last_job_snapshot = Some(snapshot);
                    if finished {
                        state.active_sync_job = None;
                        state.ui_notice = if success {
                            "同步已完成"
                        } else {
                            "后台作业已结束，可查看结果或重试"
                        }
                        .into();
                    }
                    drop(state);
                    cx.emit(WorkbenchUpdated);
                    if finished && success {
                        self.dispatch(UiAction::Refresh, window, cx);
                    }
                }
                Err(error) => {
                    state.ui_error = Some(error);
                    drop(state);
                    cx.emit(WorkbenchUpdated);
                }
            }
        }
    }

    fn pump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.pending_scope.is_some() {
            let editor = self.state.lock().unwrap().note_editor.clone();
            if note_is_dirty(&editor) {
                self.queue.clear();
                self.queue.push_front(UiAction::SaveNote(editor));
            } else {
                let scope = self.pending_scope.take().unwrap();
                self.apply_pending_scope(scope, cx);
            }
        }
        let Some(mut action) = self.queue.pop_front() else {
            return;
        };
        let mut working = self.state.lock().unwrap().clone();
        if let UiAction::SaveNote(editor) = &mut action {
            if editor.note_id != working.note_editor.note_id {
                self.pump(window, cx);
                return;
            }
            *editor = working.note_editor.clone();
            self.state.lock().unwrap().note_editor.status = delta_app::AutosaveStatus::Saving;
        }
        let original = working.clone();
        let note_switch = matches!(
            action,
            UiAction::NewNote(_) | UiAction::OpenNote(_) | UiAction::SelectMarker(_)
        );
        let retry_action = note_switch.then(|| action.clone());
        let scope_revision = self.scope_revision;
        let navigation_revision = self.navigation_revision;
        let chart_revision = self.chart_revision;
        let ai_scope_revision = self.ai_scope_revision;
        self.busy = true;
        self.ai_running = matches!(action, UiAction::AiReview { .. });
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel);
        let stream = Arc::clone(&self.stream_text);
        if self.ai_running {
            stream.lock().unwrap().clear();
        }
        working.ui_error = None;
        self.state.lock().unwrap().ui_notice = if self.ai_running {
            "AI 正在生成，结果尚未校验…"
        } else {
            "正在处理…"
        }
        .into();
        cx.emit(WorkbenchUpdated);
        let operation = cx.background_executor().spawn(async move {
            let mut flushed_editor = None;
            let result = (|| {
                if note_switch {
                    flush_dirty_note(&mut working)?;
                    flushed_editor = Some(working.note_editor.clone());
                }
                apply_action(&mut working, action, cancel, stream)
            })();
            (working, result, flushed_editor)
        });
        self.task = Some(cx.spawn_in(window, async move |view, cx| {
            let (mut completed, result, flushed_editor) = operation.await;
            let _ = view.update_in(cx, |controller, window, cx| {
                let mut current = controller.state.lock().unwrap();
                if controller.scope_revision == scope_revision {
                    if controller.navigation_revision != navigation_revision {
                        completed.layout.active_nav = current.layout.active_nav.clone();
                    }
                    preserve_layout_edits(&original.layout, &current.layout, &mut completed.layout);
                    if controller.chart_revision != chart_revision {
                        completed.kline_viewport = current.kline_viewport.clone();
                        completed.kline_slice = current.kline_slice.clone();
                    }
                    if current.kline_viewport.crosshair_index
                        != original.kline_viewport.crosshair_index
                        || current.kline_viewport.crosshair_price
                            != original.kline_viewport.crosshair_price
                    {
                        completed.kline_viewport.crosshair_index =
                            current.kline_viewport.crosshair_index;
                        completed.kline_viewport.crosshair_price =
                            current.kline_viewport.crosshair_price;
                    }
                    if !same_note_content(&original.note_editor, &current.note_editor) {
                        let saved = flushed_editor.as_ref().unwrap_or(&completed.note_editor);
                        let persisted = (saved.note_id == current.note_editor.note_id)
                            .then_some(saved.last_persisted_revision)
                            .flatten();
                        let save_status = saved.status.clone();
                        completed.note_editor = current.note_editor.clone();
                        if persisted.is_some() {
                            completed.note_editor.last_persisted_revision = persisted;
                        }
                        completed.note_editor.status = match (&result, &save_status) {
                            (
                                Err(_),
                                delta_app::AutosaveStatus::SaveFailed { error, retryable },
                            ) => delta_app::AutosaveStatus::SaveFailed {
                                error: error.clone(),
                                retryable: *retryable,
                            },
                            _ => delta_app::AutosaveStatus::Editing,
                        };
                        if result.is_ok() {
                            if let Some(action) = retry_action {
                                controller.queue.push_front(action);
                            }
                        }
                    }
                    if controller.ai_scope_revision != ai_scope_revision {
                        completed.last_ai_report = None;
                        completed.opened_ai_evidence = None;
                        completed.saved_ai_reports.clear();
                        completed.pre_send_scope = None;
                    }
                    if let Err(error) = result {
                        if controller.ai_running {
                            completed.last_ai_error = Some(error.clone());
                        }
                        if controller.pending_scope.is_some()
                            && note_is_dirty(&completed.note_editor)
                        {
                            controller.pending_scope = None;
                        }
                        completed.ui_notice = "操作未完成，请检查原因后重试".into();
                        completed.ui_error = Some(error);
                    }
                    *current = completed;
                }
                drop(current);
                controller.busy = false;
                controller.ai_running = false;
                cx.emit(WorkbenchUpdated);
                if controller.state.lock().unwrap().library_deleted {
                    window.remove_window();
                    cx.quit();
                    return;
                }
                controller.pump(window, cx);
            });
        }));
    }
}

impl Drop for WorkbenchController {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

fn apply_action(
    state: &mut WorkbenchSharedState,
    action: UiAction,
    cancel: Arc<AtomicBool>,
    stream: Arc<Mutex<String>>,
) -> Result<(), ErrorEnvelope> {
    match action {
        UiAction::Initialize => {
            let (layout, fallback) = load_or_default_layout(&state.data_dir);
            state.layout = layout;
            state.scope.report_currency = state.layout.report_currency.clone();
            if let Some(fallback) = fallback {
                state.system_states.push(fallback);
            }
            state.refresh_all()?;
        }
        UiAction::Refresh => state.refresh_all()?,
        UiAction::SaveLayout => {
            save_layout_config(&state.data_dir, &state.layout)?;
            state.ui_notice = "工作台设置已保存".into();
        }
        UiAction::SeedExample => {
            if !state.accounts.is_empty() {
                return Err(ErrorEnvelope::new(
                    "EXAMPLE_REQUIRES_EMPTY_LIBRARY",
                    "离线示例只可载入空资料库，请新建目录",
                    false,
                ));
            }
            state
                .host
                .execute(delta_app::seed_ac02_fixture_in_service)?;
            state.scope.as_of_date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
            state.refresh_all()?;
            state.ui_notice = "已载入离线合成示例，期间切换为 2026-01-15".into();
        }
        UiAction::Settings { currency, timezone } => {
            state.host.execute({
                let timezone = timezone.clone();
                move |svc| svc.validate_display_timezone(&timezone)
            })?;
            let mut scope = state.scope.clone();
            scope.report_currency = currency.trim().to_ascii_uppercase();
            scope.versions_frozen = false;
            let scope = state
                .host
                .query_scope_describe(scope, "ui-settings", state.host.current_generation())?
                .scope;
            let mut layout = state.layout.clone();
            layout.report_currency = scope.report_currency.clone();
            layout.display_timezone = timezone;
            save_layout_config(&state.data_dir, &layout)?;
            state.layout = layout;
            state.scope = scope;
            state.refresh_all()?;
        }
        UiAction::Account {
            id,
            name,
            kind,
            platform,
            status,
        } => {
            state
                .host
                .upsert_account(&id, &name, &kind, &platform, None, &status)?;
            state.refresh_all()?;
        }
        UiAction::Portfolio {
            id,
            name,
            accounts,
            portfolios,
        } => {
            state
                .host
                .upsert_portfolio(&id, &name, accounts, portfolios)?;
            state.refresh_all()?;
        }
        UiAction::ImportPreview {
            account,
            preset,
            file,
        } => {
            let preview = state.preview_statement_import(&account, preset, file)?;
            state.ui_notice = format!(
                "预览：{} 条可用，{} 条错误，{} 条待核对",
                preview.valid_count, preview.error_count, preview.suspected_duplicate_count
            );
        }
        UiAction::ImportCommit {
            account,
            preset,
            file,
        } => {
            let (_, receipt) = state.commit_statement_import(&account, preset, file)?;
            state.ui_notice = if receipt.idempotent_skipped {
                "此账户已导入该批次，未重复入账".into()
            } else {
                format!(
                    "已入账 {} 条，另有 {} 条疑似重复待确认",
                    receipt.committed_events, receipt.suspected_duplicates_held
                )
            };
        }
        UiAction::ResolveHeld { row, include } => {
            state.host.resolve_import_row(&row, include)?;
            state.refresh_all()?;
        }
        UiAction::Sync(request) => {
            let id = state
                .host
                .start_market_sync(request, Duration::from_secs(120))?;
            state.active_sync_job = Some(id);
            state.last_job_snapshot = Some(state.host.poll_job_throttled(id)?);
            state.ui_notice = "已启动公开行情同步，可随时取消".into();
        }
        UiAction::ManualValuation(valuation) => {
            state.host.upsert_manual_valuation(valuation)?;
            state.refresh_all()?;
        }
        UiAction::Reconcile { account, snapshots } => {
            if !state.scope.resolved_accounts.contains(&account) {
                return Err(ErrorEnvelope::new(
                    "SCOPE_REJECTED",
                    "对账账户不在当前范围内",
                    false,
                ));
            }
            state.host.reconcile_account(&account, snapshots)?;
            state.refresh_all()?;
        }
        UiAction::Amend {
            event,
            quantity,
            price,
            fee,
        } => {
            state.host.get_scoped_transaction(
                state.scope.clone(),
                &event,
                state.host.current_generation(),
            )?;
            state.host.amend_event(&event, quantity, price, fee)?;
            state.refresh_all()?;
            state.ui_notice = "更正已保存；旧版报告和关联会显示更正提示".into();
        }
        UiAction::SearchInstrument(query) => {
            state.search_hits = state.host.search_instruments(&query)?;
        }
        UiAction::SelectInstrument(id) => {
            state.active_instrument = id;
            state.kline_viewport = KlineViewport::new(0, 100);
            load_chart(state)?;
        }
        UiAction::ChartViewport(viewport) => {
            state.kline_viewport = viewport;
            load_chart(state)?;
        }
        UiAction::ChartJumpDate(date) => {
            state.kline_viewport.requested_anchor_date = Some(date);
            load_chart(state)?;
        }
        UiAction::ChartAdjustment(adjustment) => {
            state.active_adjustment = adjustment;
            load_chart(state)?;
        }
        UiAction::ReloadChart => load_chart(state)?,
        UiAction::Indicators(json) => {
            let config = delta_app::parse_indicator_config(&json)?;
            state.kline_viewport.indicator_config = config;
            load_chart(state)?;
        }
        UiAction::Watchlist { name, instrument } => {
            if name.trim().is_empty() {
                return Err(ErrorEnvelope::new(
                    "INVALID_ARGUMENT",
                    "请填写自选组名称",
                    false,
                ));
            }
            let id = format!("watchlist:{}", name.trim());
            state.host.upsert_watchlist_group(&id, name.trim(), 0)?;
            state.host.add_watchlist_item(&id, &instrument)?;
            state.watchlist_groups = state.host.list_watchlist_groups()?;
        }
        UiAction::ChartContext(id) => {
            state.jump_to_chart_context(&id)?;
        }
        UiAction::SelectMarker(id) => {
            let outcome = state.select_chart_marker(&id)?;
            if let crate::workbench::MarkerSelectionOutcome::Note(note) = outcome {
                flush_dirty_note(state)?;
                state.note_editor = editor_from_note(note);
                state.layout.active_nav = "nav.journal".into();
            } else {
                state.layout.active_nav = "nav.assets".into();
            }
        }
        UiAction::SelectTransaction(id) => {
            let row = state.host.get_scoped_transaction(
                state.scope.clone(),
                &id,
                state.host.current_generation(),
            )?;
            state.selected_marker_outcome =
                Some(crate::workbench::MarkerSelectionOutcome::TradeFill(row));
        }
        UiAction::PageTransactions(filter) => {
            state.transaction_filter = filter.clone();
            state.transactions = Some(state.host.query_transactions(
                state.scope.clone(),
                filter,
                "ui-page",
                state.host.current_generation(),
            )?);
        }
        UiAction::NewNote(template) => {
            flush_dirty_note(state)?;
            state.note_editor = new_note_editor(template);
        }
        UiAction::OpenNote(id) => {
            flush_dirty_note(state)?;
            let note = state.host.get_scoped_note(
                state.scope.clone(),
                &id,
                state.host.current_generation(),
            )?;
            state.note_editor = editor_from_note(note);
        }
        UiAction::SaveNote(editor) => {
            state.note_editor = editor;
            let host = state.host.clone();
            let mut editor = state.note_editor.clone();
            let scope = state.scope.clone();
            let (editor, result) = host.execute(move |svc| {
                let result = svc.flush_scoped_note_editor_autosave(&scope, &mut editor);
                Ok((editor, result))
            })?;
            state.note_editor = editor;
            result?;
            let mut scope = state.scope.clone();
            scope.versions_frozen = false;
            state.scope = state
                .host
                .query_scope_describe(scope, "ui-note-scope", state.host.current_generation())?
                .scope;
            state.refresh_notes()?;
            state.ui_notice = "笔记已保存".into();
        }
        UiAction::LinkFill { event, quantity } => {
            flush_dirty_note(state)?;
            let quantity = delta_app::parse_decimal_lossless(&quantity)
                .map_err(|e| ErrorEnvelope::new("INVALID_ARGUMENT", e.to_string(), false))?;
            state.host.get_scoped_transaction(
                state.scope.clone(),
                &event,
                state.host.current_generation(),
            )?;
            state.host.link_plan_or_note_fill(
                None,
                Some(&state.note_editor.note_id),
                &event,
                quantity,
                "execution",
            )?;
            state.refresh_all()?;
        }
        UiAction::LinkChart(context) => {
            flush_dirty_note(state)?;
            let id = state.host.save_chart_context(context)?;
            state.host.add_note_link(
                &state.note_editor.note_id,
                "chart_context",
                &id,
                1,
                None,
                "review",
            )?;
            state.refresh_all()?;
        }
        UiAction::Attachment(path) => {
            flush_dirty_note(state)?;
            let metadata = std::fs::metadata(&path)
                .map_err(|e| ErrorEnvelope::new("ATTACHMENT_FAILED", e.to_string(), false))?;
            if metadata.len() > 16 * 1024 * 1024 {
                return Err(ErrorEnvelope::new(
                    "ATTACHMENT_TOO_LARGE",
                    "附件上限为 16 MiB",
                    false,
                ));
            }
            let bytes = std::fs::read(&path)
                .map_err(|e| ErrorEnvelope::new("ATTACHMENT_FAILED", e.to_string(), false))?;
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| ErrorEnvelope::new("ATTACHMENT_FAILED", "附件文件名无效", false))?;
            state.host.add_note_attachment(
                &state.note_editor.note_id,
                name,
                "application/octet-stream",
                bytes,
            )?;
            state.refresh_all()?;
        }
        UiAction::SearchNotes(query) => {
            state.search_notes(&query)?;
        }
        UiAction::ConfigureModel { connection, secret } => {
            state
                .runtime
                .configure_model_connection(connection, secret.as_deref())?;
            state.model_connection_settings = state.runtime.model_connection_settings()?;
            state.last_ai_report = None;
            state.opened_ai_evidence = None;
            state.last_ai_error = None;
            state.pre_send_scope = Some(state.runtime.build_pre_send_summary(&state.scope)?);
            state.ui_notice = "模型连接已保存；可进行连通性检查".into();
        }
        UiAction::ProbeModel => {
            state.runtime.probe_model_connection(cancel)?;
            state.ui_notice = "模型文本流和正常终态已验证；复盘仍使用所选范围".into();
        }
        UiAction::AiReview { session, question } => {
            flush_dirty_note(state)?;
            let report = state.runtime.run_review_with_callback(
                &session,
                &question,
                state.scope.clone(),
                cancel,
                move |event| {
                    if let ModelEventKind::TextDelta { delta } = event.kind {
                        let mut text = stream.lock().unwrap();
                        if text.len() + delta.len() <= 256 * 1024 {
                            text.push_str(&delta);
                        }
                    }
                },
            )?;
            state.last_ai_report = Some(report);
            state.last_ai_error = None;
            state.saved_ai_reports = state
                .runtime
                .list_saved_ai_reports_for_scope(state.scope.clone())?;
            state.ui_notice = "AI 生成结束，请查看宿主校验状态、证据与局限".into();
        }
        UiAction::CompactSession { session } => {
            flush_dirty_note(state)?;
            state
                .runtime
                .compact_session_with_model(&session, state.scope.clone(), cancel)?;
            state.ui_notice = "会话检查点已保存，原始回合与证据保留".into();
        }
        UiAction::AiScope {
            pinned,
            include_notes,
        } => {
            let changed = state.scope.include_notes != include_notes;
            state.scope.include_notes = include_notes;
            state.scope.versions_frozen = false;
            state.scope = state
                .host
                .query_scope_describe(
                    state.scope.clone(),
                    "ai-scope",
                    state.host.current_generation(),
                )?
                .scope;
            state
                .runtime
                .set_pinned_scope(pinned.then(|| state.scope.clone()));
            state.pre_send_scope = Some(state.runtime.build_pre_send_summary(&state.scope)?);
            if changed {
                state.last_ai_report = None;
                state.opened_ai_evidence = None;
                state.last_ai_error = None;
                state.saved_ai_reports = state
                    .runtime
                    .list_saved_ai_reports_for_scope(state.scope.clone())?;
            }
        }
        UiAction::OpenAiReport(run_id) => {
            state.saved_ai_reports = state
                .runtime
                .list_saved_ai_reports_for_scope(state.scope.clone())?;
            let report = state
                .saved_ai_reports
                .iter()
                .find(|report| report.run_id == run_id)
                .cloned()
                .ok_or_else(|| ErrorEnvelope::new("SCOPE_DENIED", "报告不在当前范围内", false))?;
            state.last_ai_report = Some(report);
            state.opened_ai_evidence = None;
        }
        UiAction::OpenEvidence(reference) => {
            let report = state
                .last_ai_report
                .as_ref()
                .ok_or_else(|| ErrorEnvelope::new("EVIDENCE_NOT_FOUND", "请先打开报告", false))?;
            state.opened_ai_evidence = Some(state.runtime.open_report_evidence(
                &report.run_id,
                reference,
                state.scope.clone(),
            )?);
        }
        UiAction::Backup(path) => {
            state.host.create_backup(path.clone())?;
            state.ui_notice = format!("一致性备份已保存：{}", path.display());
        }
        UiAction::Export(path) => {
            state.host.export_library(path.clone())?;
            state.ui_notice = format!("开放格式已导出：{}", path.display());
        }
        UiAction::Restore { backup, target } => {
            AppService::restore_backup(&backup, &target)?;
            state.ui_notice = "已恢复到新目录并校验引用；可用该目录启动资料库".into();
        }
        UiAction::DeletePreview => {
            state.deletion_preview =
                Some(state.host.execute(|svc| svc.preview_library_deletion())?);
        }
        UiAction::DeleteLibrary { preview, typed_id } => {
            if typed_id.trim() != preview.library_id
                || preview.target_data_dir
                    != std::fs::canonicalize(&state.data_dir).map_err(|e| {
                        ErrorEnvelope::new("DELETE_CONFIRMATION", e.to_string(), false)
                    })?
            {
                return Err(ErrorEnvelope::new(
                    "DELETE_CONFIRMATION",
                    "资料库编号或预览目录不一致，未执行删除",
                    false,
                ));
            }
            let paths = state.host.execute(|svc| Ok(svc.paths().clone()))?;
            state.host.shutdown()?;
            match AppService::delete_library_from_preview(&preview, &typed_id) {
                Ok(result) if result.deleted => {
                    state.library_deleted = true;
                    state.ui_notice = "资料库已删除，即将退出".into();
                }
                result => {
                    if !paths.data_dir.join("delta.sqlite").is_file()
                        || !paths.data_dir.join("manifest.json").is_file()
                    {
                        return result.map(|_| ()).and_then(|_| {
                            Err(ErrorEnvelope::new(
                                "DELETE_INCOMPLETE",
                                "删除未完成；请检查文件或从备份恢复",
                                false,
                            ))
                        });
                    }
                    let reopened = AppService::open(paths, "USD", "USD", "UTC", true)?;
                    state.host = delta_app::AppHost::from_service(reopened);
                    state.runtime = delta_app::AgentRuntime::new(state.host.clone())?;
                    result?;
                    return Err(ErrorEnvelope::new(
                        "DELETE_INCOMPLETE",
                        "删除没有完成",
                        false,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn load_chart(state: &mut WorkbenchSharedState) -> Result<(), ErrorEnvelope> {
    let mut scope = state.scope.clone();
    scope.dataset_version = state
        .pinned_chart_version
        .unwrap_or(state.latest_dataset_version);
    let slice = state.host.load_scoped_kline_window_slice(
        scope,
        &state.active_instrument,
        &state.active_timeframe,
        state.active_adjustment,
        state.kline_viewport.clone(),
        1000,
        state.host.current_generation(),
    )?;
    state.kline_viewport.total_bars = slice.total_visible_bars;
    state.kline_viewport.visible_start = slice.visible_start_index;
    state.kline_viewport.visible_end = slice.visible_end_index;
    if let Some(date) = state.kline_viewport.requested_anchor_date.take() {
        if !slice.bars.is_empty() {
            let local = slice
                .bars
                .partition_point(|bar| bar.session_date < date)
                .min(slice.bars.len() - 1);
            state.kline_viewport.crosshair_index = Some(slice.visible_start_index + local);
        }
    }
    state.kline_viewport.requested_window = None;
    state.kline_slice = Some(slice);
    Ok(())
}

fn flush_dirty_note(state: &mut WorkbenchSharedState) -> Result<(), ErrorEnvelope> {
    if note_is_dirty(&state.note_editor) {
        let body = state.note_editor.body_buffer.clone();
        state.edit_and_autosave_note(body)?;
    }
    Ok(())
}

fn new_note_editor(template: NoteTemplateKind) -> NoteEditorSession {
    NoteEditorSession::new(
        format!("note:{}", uuid::Uuid::new_v4()),
        "新建复盘笔记",
        template,
        false,
    )
}

fn note_is_dirty(editor: &NoteEditorSession) -> bool {
    !matches!(editor.status, delta_app::AutosaveStatus::Saved { .. })
        && (editor.last_persisted_revision.is_some()
            || !editor.body_buffer.is_empty()
            || editor.structured_buffer != delta_app::StructuredNoteFields::default()
            || !editor.tags_buffer.is_empty()
            || editor.allow_model_access
            || editor.title_buffer != "新建复盘笔记")
}

fn same_note_content(left: &NoteEditorSession, right: &NoteEditorSession) -> bool {
    left.note_id == right.note_id
        && left.title_buffer == right.title_buffer
        && left.body_buffer == right.body_buffer
        && left.structured_buffer == right.structured_buffer
        && left.tags_buffer == right.tags_buffer
        && left.allow_model_access == right.allow_model_access
        && left.template_kind == right.template_kind
}

fn preserve_layout_edits(
    original: &crate::workbench::WorkbenchLayoutConfig,
    current: &crate::workbench::WorkbenchLayoutConfig,
    completed: &mut crate::workbench::WorkbenchLayoutConfig,
) {
    macro_rules! preserve {
        ($($field:ident),+) => { $(if original.$field != current.$field { completed.$field = current.$field.clone(); })+ };
    }
    preserve!(
        active_nav,
        right_ai_open,
        bottom_tasks_open,
        color_convention,
        locale,
        report_currency,
        display_timezone
    );
}

fn editor_from_note(note: delta_app::NoteDto) -> NoteEditorSession {
    NoteEditorSession {
        note_id: note.id,
        title_buffer: note.title,
        template_kind: note.template_kind,
        body_buffer: note.body_markdown,
        structured_buffer: note.structured,
        tags_buffer: note.tags,
        allow_model_access: note.allow_model_access,
        status: delta_app::AutosaveStatus::Saved {
            saved_at: note.saved_at,
            revision: note.current_revision,
        },
        last_persisted_revision: Some(note.current_revision),
    }
}
