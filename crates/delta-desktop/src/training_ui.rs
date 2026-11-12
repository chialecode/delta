//! Simulated trading & historical K-line replay training workspace (`M4-W06`, `S-45`, `S-46`, `M4-A-16`).
//!
//! Enforces:
//! - Permanent `SIMULATED_TRAINING_BADGE` (`"模拟训练"`) visual cue across all training views.
//! - Hidden-mode identity & calendar scrubbing prior to explicit post-finish `"揭示"` (`reveal_training_run`).
//! - Strict anti-misclick shortcut separation (`NFR-09`, `S-46`, `M4-A-16`):
//!   - Order submission requires clicking the submit button OR pressing `Ctrl+Enter`.
//!   - Advancing to the next bar (`"下一根"`) uses `N` or `Right` (`→`) and NEVER submits an order,
//!     even when the order ticket or its inputs hold focus.
//! - Post-finish disclosure sequence: `结束训练` -> `训练报告` + 用户自主评分 + 归档笔记 -> 显式 `揭示` -> `AI 复盘`.

use std::fs;
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

use chrono::{Datelike as _, Days, NaiveDate, TimeZone as _, Utc, Weekday};
use delta_app::{
    parse_decimal_lossless, to_canonical_text, AiReportVerificationStatus, AiReviewReport,
    AnalysisScope, AttachedBracket, BenchmarkStatus, CreateTrainingRunRequest,
    DeleteTrainingRunPreview, ErrorEnvelope, FeeMarketCategory, FeeProfileDraftDto, FeeProfileDto,
    IndicatorConfig, KlineViewport, ModelEventKind, NoteDto, OrderRejection, OrderSide,
    OrderStatus, PlanCompliance, PlanExecutionStatus, QualityStatus, RatioMetric, SimOrderType,
    SimRejectCode, SimRunStatus, SlippageSpec, TimeInForce, TrainingMarketCategory,
    TrainingOrderPreviewDto, TrainingOrderRequestDto, TrainingReportDto, TrainingRevealDto,
    TrainingRunSummaryDto, TrainingStepReceiptDto, TrainingWorkspaceSnapshotDto,
    UpsertTrainingNoteRequest, WinRateStatus, BUILTIN_FEE_US_EQUITY_PER_SHARE_ID,
    SIMULATED_TRAINING_BADGE,
};
use gpui_kit::{
    base::TestTextExt as _,
    component::{
        alert::Alert,
        button::{Button, ButtonVariants as _},
        h_flex,
        separator::Separator,
        tag::Tag,
        v_flex, ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _,
        StyledExt as _,
    },
    div,
    prelude::FluentBuilder as _,
    px, AnyElement, App, Context, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window,
};
use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::{
    components::{KlineChartView, MetricCard},
    controller::UiAction,
    i18n::I18nCatalog,
    theme::Chrome,
    workbench::{CenterWorkspacePanel, WorkbenchSharedState},
};

pub const TRAINING_TEXT_INPUT_KEYS: &[&str] = &[
    "form.trn_instrument",
    "form.trn_start_date",
    "form.trn_end_date",
    "form.trn_history_bars",
    "form.trn_training_bars",
    "form.trn_reveal_bars",
    "form.trn_warmup_bars",
    "form.trn_seed",
    "form.trn_initial_cash",
    "form.trn_fee_key",
    "form.trn_fee_name",
    "form.trn_fee_rate",
    "form.trn_fee_min",
    "form.trn_fee_slippage_bps",
    "form.trn_order_qty",
    "form.trn_order_limit",
    "form.trn_order_stop",
    "form.trn_order_tp",
    "form.trn_order_sl",
    "form.trn_note_rationale",
    "form.trn_note_emotion",
    "form.trn_self_eval_summary",
    "form.trn_ai_question",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrainingViewMode {
    #[default]
    List,
    NewDialog,
    Workspace,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingNewRunDraft {
    pub market_category: TrainingMarketCategory,
    pub fee_profile_version_id: String,
    pub hidden_mode: bool,
}

impl Default for TrainingNewRunDraft {
    fn default() -> Self {
        Self {
            market_category: TrainingMarketCategory::Any,
            fee_profile_version_id: BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string(),
            hidden_mode: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderFormKindMode {
    #[default]
    Market,
    Limit,
    StopMarket,
    Oco,
    EntryWithBracket,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingOrderFormDraft {
    pub kind_mode: OrderFormKindMode,
    pub entry_is_limit: bool,
    pub side: OrderSide,
    pub tif: TimeInForce,
}

impl Default for TrainingOrderFormDraft {
    fn default() -> Self {
        Self {
            kind_mode: OrderFormKindMode::Market,
            entry_is_limit: false,
            side: OrderSide::Buy,
            tif: TimeInForce::Day,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingSelfEvalDraft {
    pub decision_score: u8,
    pub plan_discipline_score: Option<u8>,
}

impl Default for TrainingSelfEvalDraft {
    fn default() -> Self {
        Self {
            decision_score: 4,
            plan_discipline_score: Some(4),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrainingNoteDraftMeta {
    pub editing_note_id: Option<String>,
    pub linked_order_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TrainingUiState {
    pub mode: TrainingViewMode,
    pub runs: Vec<TrainingRunSummaryDto>,
    pub fee_profiles: Vec<FeeProfileDto>,
    pub new_run_draft: TrainingNewRunDraft,
    pub order_draft: TrainingOrderFormDraft,
    pub note_draft: TrainingNoteDraftMeta,
    pub self_eval_draft: TrainingSelfEvalDraft,
    pub pending_prestart_run: Option<TrainingRunSummaryDto>,
    pub active_run_id: Option<String>,
    pub workspace: Option<TrainingWorkspaceSnapshotDto>,
    pub chart_viewport: Option<KlineViewport>,
    pub report: Option<TrainingReportDto>,
    pub reveal: Option<TrainingRevealDto>,
    pub delete_preview: Option<DeleteTrainingRunPreview>,
    pub order_preview: Option<TrainingOrderPreviewDto>,
    pub last_order_rejection: Option<OrderRejection>,
    pub last_step_receipt: Option<TrainingStepReceiptDto>,
    pub last_archived_notes: Vec<NoteDto>,
    pub training_ai_report: Option<AiReviewReport>,
    pub training_ai_error: Option<ErrorEnvelope>,
    pub opened_training_evidence: Option<Value>,
}

#[derive(Clone)]
pub enum TrainingUiAction {
    RefreshList,
    OpenNewDialog,
    OpenList,
    CreateDraftRun(CreateTrainingRunRequest),
    CreateAndStartRun(CreateTrainingRunRequest),
    RedrawPendingRun(String),
    StartRun(String),
    ResumeOrOpenRun(String),
    PauseRun(String),
    AdvanceBar(String),
    UpdateChartViewport {
        run_id: String,
        viewport: KlineViewport,
    },
    PreviewOrder {
        run_id: String,
        request: TrainingOrderRequestDto,
    },
    SubmitOrder {
        run_id: String,
        request: TrainingOrderRequestDto,
    },
    CancelOrder {
        run_id: String,
        order_id: String,
    },
    SaveNote {
        run_id: String,
        request: UpsertTrainingNoteRequest,
    },
    FinishRun(String),
    OpenReport(String),
    SaveSelfEvaluation {
        run_id: String,
        decision_score: u8,
        plan_discipline_score: Option<u8>,
        summary: String,
    },
    ArchiveNotesToJournal {
        run_id: String,
        note_ids: Vec<String>,
    },
    RevealRun(String),
    RetryRun(String),
    PreviewDeleteRun(String),
    CancelDeletePreview,
    ConfirmDeleteRun(String),
    SaveCustomFeeProfile(FeeProfileDraftDto),
    DeleteUnusedFeeProfile(String),
    RequestAiReview {
        run_id: String,
        question: String,
    },
    OpenTrainingEvidence {
        run_id: String,
        evidence_id: String,
    },
}

pub(crate) fn preserve_training_draft_edits(
    original: &TrainingUiState,
    current: &TrainingUiState,
    completed: &mut TrainingUiState,
) {
    if original.new_run_draft != current.new_run_draft {
        completed.new_run_draft = current.new_run_draft.clone();
    }
    if original.order_draft != current.order_draft {
        completed.order_draft = current.order_draft.clone();
    }
    if original.note_draft != current.note_draft {
        completed.note_draft = current.note_draft.clone();
    }
    if original.self_eval_draft != current.self_eval_draft {
        completed.self_eval_draft = current.self_eval_draft.clone();
    }
    if completed.active_run_id.is_some() && completed.active_run_id == current.active_run_id {
        if let (Some(orig_vp), Some(cur_vp), Some(comp_vp)) = (
            original.chart_viewport.as_ref(),
            current.chart_viewport.as_ref(),
            completed.chart_viewport.as_mut(),
        ) {
            if cur_vp.crosshair_index != orig_vp.crosshair_index
                || cur_vp.crosshair_price != orig_vp.crosshair_price
            {
                let clamped_idx = cur_vp.crosshair_index.and_then(|idx| {
                    (comp_vp.total_bars > 0).then(|| idx.min(comp_vp.total_bars - 1))
                });
                comp_vp.move_crosshair(clamped_idx, cur_vp.crosshair_price);
            }
        }
    }
}

fn training_input_row_guard(el: impl IntoElement) -> gpui_kit::Div {
    div()
        .w_full()
        .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(el)
}

pub(crate) fn refresh_training_lists(
    state: &mut WorkbenchSharedState,
) -> Result<(), ErrorEnvelope> {
    state.training.runs = state.host.list_training_runs()?;
    state.training.fee_profiles = state.host.list_fee_profile_versions()?;
    if !state
        .training
        .fee_profiles
        .iter()
        .any(|fp| fp.record.id == state.training.new_run_draft.fee_profile_version_id)
    {
        if let Some(first) = state.training.fee_profiles.first() {
            state.training.new_run_draft.fee_profile_version_id = first.record.id.clone();
        }
    }
    Ok(())
}

fn load_workspace_into_state(
    state: &mut WorkbenchSharedState,
    run_id: &str,
    viewport_override: Option<KlineViewport>,
) -> Result<(), ErrorEnvelope> {
    let vp = viewport_override.or_else(|| {
        state
            .training
            .chart_viewport
            .clone()
            .filter(|_| state.training.active_run_id.as_deref() == Some(run_id))
    });
    let mut effective_vp = vp.unwrap_or_else(|| {
        let mut init = KlineViewport::new(0, 120);
        init.indicator_config = IndicatorConfig::default();
        init
    });
    let snap =
        state
            .host
            .get_training_workspace_snapshot(run_id, Some(effective_vp.clone()), 1200)?;
    effective_vp.total_bars = snap.chart_slice.total_visible_bars;
    effective_vp.visible_start = snap.chart_slice.visible_start_index;
    effective_vp.visible_end = snap.chart_slice.visible_end_index;
    state.training.active_run_id = Some(run_id.to_string());
    state.training.chart_viewport = Some(effective_vp);
    state.training.workspace = Some(snap);
    state.training.mode = TrainingViewMode::Workspace;
    Ok(())
}

pub(crate) fn format_rejection_message(rej: &OrderRejection) -> String {
    let reason = match rej.code {
        SimRejectCode::UnsupportedOrderType => "当前模拟训练不支持该委托类型",
        SimRejectCode::InvalidTickSize => "价格不符合最小报价单位（Tick Size）约束",
        SimRejectCode::InvalidLotSize => "数量不符合最小交易手数（Lot Size）约束",
        SimRejectCode::InvalidOrderParams => "委托参数不合法（数量/价格/止盈止损关系错误）",
        SimRejectCode::InsufficientFunds => "模拟账户可用资金不足（含预估手续费）",
        SimRejectCode::InsufficientPosition => "模拟账户可用持仓不足（不支持裸卖空或超额卖出）",
        SimRejectCode::InsufficientFundsAtFill => "成交结算时模拟账户可用资金不足",
        SimRejectCode::MissingReferencePrice => "缺少当前可见参考价格，无法预估市价单成本",
    };
    let mut parts = vec![reason.to_string()];
    if rej.nearest_lower.is_some() || rej.nearest_upper.is_some() {
        let low = rej
            .nearest_lower
            .map(to_canonical_text)
            .unwrap_or_else(|| "无".to_string());
        let high = rej
            .nearest_upper
            .map(to_canonical_text)
            .unwrap_or_else(|| "无".to_string());
        parts.push(format!("最近合法下界: {low} · 最近合法上界: {high}"));
    }
    if let (Some(req), Some(avail)) = (rej.required, rej.available) {
        parts.push(format!(
            "所需: {} · 当前可用: {}",
            to_canonical_text(req),
            to_canonical_text(avail)
        ));
    }
    parts.join("；")
}

pub(crate) fn apply_training_action(
    state: &mut WorkbenchSharedState,
    action: TrainingUiAction,
    cancel: Arc<AtomicBool>,
    stream: Arc<Mutex<String>>,
) -> Result<(), ErrorEnvelope> {
    match action {
        TrainingUiAction::RefreshList => {
            refresh_training_lists(state)?;
            state.ui_notice = "模拟训练列表已更新".into();
        }
        TrainingUiAction::OpenNewDialog => {
            refresh_training_lists(state)?;
            state.training.pending_prestart_run = None;
            state.training.delete_preview = None;
            state.training.mode = TrainingViewMode::NewDialog;
            state.ui_notice = "已打开新建模拟训练配置".into();
        }
        TrainingUiAction::OpenList => {
            refresh_training_lists(state)?;
            state.training.mode = TrainingViewMode::List;
            state.ui_notice = "已返回模拟训练列表".into();
        }
        TrainingUiAction::CreateDraftRun(mut req) => {
            req.create_and_start = false;
            let summary = state.host.create_training_run(req)?;
            refresh_training_lists(state)?;
            state.training.pending_prestart_run = Some(summary);
            state.training.mode = TrainingViewMode::NewDialog;
            state.ui_notice = "已完成候选段抽样；可在开始前点击「换一个」重新抽样".into();
        }
        TrainingUiAction::CreateAndStartRun(mut req) => {
            req.create_and_start = true;
            let summary = state.host.create_training_run(req)?;
            refresh_training_lists(state)?;
            state.training.pending_prestart_run = None;
            state.training.order_preview = None;
            state.training.last_order_rejection = None;
            state.training.last_step_receipt = None;
            state.training.report = None;
            state.training.reveal = None;
            state.training.training_ai_report = None;
            state.training.training_ai_error = None;
            state.training.opened_training_evidence = None;
            state.training.chart_viewport = None;
            load_workspace_into_state(state, &summary.run_id, None)?;
            state.ui_notice = "模拟训练已启动（独立模拟账户，不计入真实资产）".into();
        }
        TrainingUiAction::RedrawPendingRun(run_id) => {
            let summary = state.host.redraw_training_run(&run_id)?;
            refresh_training_lists(state)?;
            state.training.pending_prestart_run = Some(summary);
            state.ui_notice = "已重新抽取训练标的与历史区间".into();
        }
        TrainingUiAction::StartRun(run_id) => {
            let summary = state.host.start_training_run(&run_id)?;
            refresh_training_lists(state)?;
            state.training.pending_prestart_run = None;
            state.training.order_preview = None;
            state.training.last_order_rejection = None;
            state.training.last_step_receipt = None;
            state.training.report = None;
            state.training.reveal = None;
            state.training.training_ai_report = None;
            state.training.training_ai_error = None;
            state.training.opened_training_evidence = None;
            state.training.chart_viewport = None;
            load_workspace_into_state(state, &summary.run_id, None)?;
            state.ui_notice = "模拟训练已启动".into();
        }
        TrainingUiAction::ResumeOrOpenRun(run_id) => {
            let summary = state.host.get_training_run_summary(&run_id)?;
            match summary.status {
                SimRunStatus::Paused => {
                    let _ = state.host.resume_training_run(&run_id)?;
                }
                SimRunStatus::Preparing => {
                    let _ = state.host.start_training_run(&run_id)?;
                }
                SimRunStatus::Finished => {
                    let rep = state.host.get_training_report(&run_id)?;
                    state.training.reveal = if rep.run.revealed {
                        Some(state.host.reveal_training_run(&run_id)?)
                    } else {
                        None
                    };
                    state.training.active_run_id = Some(run_id);
                    state.training.report = Some(rep);
                    state.training.mode = TrainingViewMode::Report;
                    refresh_training_lists(state)?;
                    state.ui_notice = "已打开模拟训练报告".into();
                    return Ok(());
                }
                _ => {}
            }
            refresh_training_lists(state)?;
            state.training.order_preview = None;
            state.training.last_order_rejection = None;
            load_workspace_into_state(state, &run_id, None)?;
            state.ui_notice = "已恢复模拟训练工作台".into();
        }
        TrainingUiAction::PauseRun(run_id) => {
            let _ = state.host.pause_training_run(&run_id)?;
            refresh_training_lists(state)?;
            state.training.mode = TrainingViewMode::List;
            state.ui_notice = "模拟训练已暂停并保存状态，可随时继续".into();
        }
        TrainingUiAction::AdvanceBar(run_id) => {
            let receipt = state.host.advance_training_bar(&run_id)?;
            let auto_finished = receipt.run.status == SimRunStatus::Finished;
            state.training.last_step_receipt = Some(receipt);
            state.training.order_preview = None;
            if let Some(vp) = state.training.chart_viewport.as_mut() {
                // Keep viewport anchored to the newest bar when stepping forward while preserving user zoom
                let next_total = vp.total_bars.saturating_add(1);
                let min_win = vp.min_visible_bars.min(next_total).max(1);
                let win = vp.visible_len().max(min_win).min(next_total);
                vp.total_bars = next_total;
                vp.visible_end = next_total;
                vp.visible_start = next_total.saturating_sub(win);
            }
            refresh_training_lists(state)?;
            if auto_finished {
                let rep = state.host.get_training_report(&run_id)?;
                if let Some(se) = &rep.self_evaluation {
                    state.training.self_eval_draft.decision_score = se.decision_score;
                    state.training.self_eval_draft.plan_discipline_score = se.plan_discipline_score;
                }
                state.training.report = Some(rep);
                state.training.reveal = None;
                state.training.mode = TrainingViewMode::Report;
                state.ui_notice = "已完成全部训练步数，请先查看训练报告与自主复盘".into();
            } else {
                load_workspace_into_state(state, &run_id, None)?;
                state.ui_notice = "已推进至下一根 K 线".into();
            }
        }
        TrainingUiAction::UpdateChartViewport { run_id, viewport } => {
            load_workspace_into_state(state, &run_id, Some(viewport))?;
        }
        TrainingUiAction::PreviewOrder { run_id, request } => {
            let preview = state.host.preview_training_order(&run_id, request)?;
            state.training.last_order_rejection = preview.rejection.clone();
            let valid = preview.valid;
            let msg = preview
                .rejection
                .as_ref()
                .map(format_rejection_message)
                .unwrap_or_else(|| {
                    "模拟委托预估通过，可使用提交按钮或 Ctrl+Enter 下单".to_string()
                });
            state.training.order_preview = Some(preview);
            if !valid {
                return Err(ErrorEnvelope::new("ORDER_PREVIEW_REJECTED", msg, false));
            }
            state.ui_notice = msg;
        }
        TrainingUiAction::SubmitOrder { run_id, request } => {
            match state.host.submit_training_order(&run_id, request) {
                Ok(receipt) => {
                    state.training.last_step_receipt = Some(receipt);
                    state.training.order_preview = None;
                    state.training.last_order_rejection = None;
                    load_workspace_into_state(state, &run_id, None)?;
                    refresh_training_lists(state)?;
                    state.ui_notice = "模拟委托已提交（将在下一根或触发条件满足时撮合）".into();
                }
                Err(err) => {
                    state.training.order_preview = None;
                    if let Ok(rej) = serde_json::from_str::<OrderRejection>(&err.message) {
                        let human = format_rejection_message(&rej);
                        state.training.last_order_rejection = Some(rej);
                        return Err(ErrorEnvelope::new(err.code, human, false));
                    }
                    return Err(err);
                }
            }
        }
        TrainingUiAction::CancelOrder { run_id, order_id } => {
            let receipt = state.host.cancel_training_order(&run_id, &order_id)?;
            state.training.last_step_receipt = Some(receipt);
            load_workspace_into_state(state, &run_id, None)?;
            state.ui_notice = "模拟挂单已撤销并释放冻结额度".into();
        }
        TrainingUiAction::SaveNote { run_id, request } => {
            let _ = state.host.upsert_training_note(&run_id, request)?;
            state.training.note_draft = TrainingNoteDraftMeta::default();
            load_workspace_into_state(state, &run_id, None)?;
            state.ui_notice = "模拟训练笔记已保存".into();
        }
        TrainingUiAction::FinishRun(run_id) => {
            let receipt = state.host.finish_training_run(&run_id)?;
            state.training.last_step_receipt = Some(receipt);
            let rep = state.host.get_training_report(&run_id)?;
            if let Some(se) = &rep.self_evaluation {
                state.training.self_eval_draft.decision_score = se.decision_score;
                state.training.self_eval_draft.plan_discipline_score = se.plan_discipline_score;
            }
            refresh_training_lists(state)?;
            state.training.active_run_id = Some(run_id);
            state.training.report = Some(rep);
            // Strictly keep hidden identity unrevealed right after finish until explicit Reveal action!
            state.training.reveal = None;
            state.training.training_ai_report = None;
            state.training.training_ai_error = None;
            state.training.opened_training_evidence = None;
            state.training.mode = TrainingViewMode::Report;
            state.ui_notice =
                "模拟训练已结束，请先审阅训练报告与自主评分，再选择是否揭示真实标的".into();
        }
        TrainingUiAction::OpenReport(run_id) => {
            let rep = state.host.get_training_report(&run_id)?;
            if let Some(se) = &rep.self_evaluation {
                state.training.self_eval_draft.decision_score = se.decision_score;
                state.training.self_eval_draft.plan_discipline_score = se.plan_discipline_score;
            }
            state.training.reveal = if rep.run.revealed || !rep.run.hidden_mode {
                state.host.reveal_training_run(&run_id).ok()
            } else {
                None
            };
            state.training.active_run_id = Some(run_id);
            state.training.report = Some(rep);
            state.training.training_ai_report = None;
            state.training.training_ai_error = None;
            state.training.opened_training_evidence = None;
            state.training.mode = TrainingViewMode::Report;
            state.ui_notice = "已打开模拟训练报告".into();
        }
        TrainingUiAction::SaveSelfEvaluation {
            run_id,
            decision_score,
            plan_discipline_score,
            summary,
        } => {
            let saved = state.host.save_training_self_evaluation(
                &run_id,
                decision_score,
                plan_discipline_score,
                summary,
            )?;
            state.training.self_eval_draft.decision_score = saved.decision_score;
            state.training.self_eval_draft.plan_discipline_score = saved.plan_discipline_score;
            state.training.report = Some(state.host.get_training_report(&run_id)?);
            state.ui_notice = "已保存用户自主决策评分（与 AI 复盘评价独立存储）".into();
        }
        TrainingUiAction::ArchiveNotesToJournal { run_id, note_ids } => {
            let res = state
                .host
                .archive_training_notes_to_journal(&run_id, note_ids)?;
            let count = res.archived_notes.len();
            state.training.last_archived_notes = res.archived_notes;
            let _ = state.refresh_notes();
            state.ui_notice = format!(
                "已归档 {count} 条模拟训练笔记至复盘日志（均带「来自模拟训练」标记，不影响真实账本）"
            );
        }
        TrainingUiAction::RevealRun(run_id) => {
            let rev = state.host.reveal_training_run(&run_id)?;
            let rep = state.host.get_training_report(&run_id)?;
            refresh_training_lists(state)?;
            state.training.reveal = Some(rev);
            state.training.report = Some(rep);
            state.ui_notice = "已揭示真实标的名称、真实日历日期与训练后走势".into();
        }
        TrainingUiAction::RetryRun(original_run_id) => {
            let child = state.host.retry_training_run(&original_run_id, None)?;
            let started = state.host.start_training_run(&child.run_id)?;
            refresh_training_lists(state)?;
            state.training.pending_prestart_run = None;
            state.training.order_preview = None;
            state.training.last_order_rejection = None;
            state.training.last_step_receipt = None;
            state.training.report = None;
            state.training.reveal = None;
            state.training.training_ai_report = None;
            state.training.training_ai_error = None;
            state.training.opened_training_evidence = None;
            state.training.chart_viewport = None;
            load_workspace_into_state(state, &started.run_id, None)?;
            state.ui_notice = "已基于相同候选段与种子创建独立重试会话".into();
        }
        TrainingUiAction::PreviewDeleteRun(run_id) => {
            let preview = state.host.preview_delete_training_run(&run_id)?;
            state.training.delete_preview = Some(preview);
            state.ui_notice = "请确认待删除模拟训练的记录范围（已归档日志笔记将保留）".into();
        }
        TrainingUiAction::CancelDeletePreview => {
            state.training.delete_preview = None;
            state.ui_notice = "已取消删除操作".into();
        }
        TrainingUiAction::ConfirmDeleteRun(run_id) => {
            let deleted = state.host.delete_training_run(&run_id)?;
            state.training.delete_preview = None;
            if state.training.active_run_id.as_deref() == Some(run_id.as_str()) {
                state.training.active_run_id = None;
                state.training.workspace = None;
                state.training.report = None;
                state.training.reveal = None;
                state.training.mode = TrainingViewMode::List;
            }
            refresh_training_lists(state)?;
            state.ui_notice = format!(
                "已删除模拟训练记录（清理 {} 笔委托、{} 笔成交、{} 条训练内笔记；已归档日志笔记保留）",
                deleted.order_count, deleted.fill_count, deleted.note_count
            );
        }
        TrainingUiAction::SaveCustomFeeProfile(draft) => {
            let saved = state.host.save_fee_profile_version(draft)?;
            refresh_training_lists(state)?;
            state.training.new_run_draft.fee_profile_version_id = saved.record.id;
            state.ui_notice = "已保存新的模拟费用配置版本".into();
        }
        TrainingUiAction::DeleteUnusedFeeProfile(id) => {
            state.host.delete_unused_fee_profile_version(&id)?;
            refresh_training_lists(state)?;
            state.ui_notice = "已删除未被引用的自定义费用配置版本".into();
        }
        TrainingUiAction::RequestAiReview { run_id, question } => {
            let mut trn_runtime = state.runtime.clone();
            trn_runtime.set_pinned_scope(None);
            let scope = AnalysisScope::for_training_run(&run_id);
            let session_id = format!("trn-ai-{run_id}");
            match trn_runtime.run_review_with_callback(
                &session_id,
                &question,
                scope,
                cancel,
                move |event| {
                    if let ModelEventKind::TextDelta { delta } = event.kind {
                        let mut text = stream.lock().unwrap();
                        if text.len() + delta.len() <= 256 * 1024 {
                            text.push_str(&delta);
                        }
                    }
                },
            ) {
                Ok(report) => {
                    state.training.training_ai_report = Some(report);
                    state.training.training_ai_error = None;
                    state.training.opened_training_evidence = None;
                    state.ui_notice = "模拟训练 AI 复盘生成完成，已通过宿主证据与数值校验".into();
                }
                Err(error) => {
                    state.training.training_ai_error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        TrainingUiAction::OpenTrainingEvidence {
            run_id,
            evidence_id,
        } => {
            let payload = state.host.open_training_evidence(&run_id, &evidence_id)?;
            state.training.opened_training_evidence = Some(payload);
            state.ui_notice = "已打开模拟训练证据记录".into();
        }
    }
    Ok(())
}

// ============================================================================
// Form Parsing, Anti-Misclick Shortcuts, and Rendering on `CenterWorkspacePanel`
// ============================================================================

fn parse_invalid(msg: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new("INVALID_ARGUMENT", msg, false)
}

fn parse_pos_usize(raw: &str, label: &str) -> Result<usize, ErrorEnvelope> {
    raw.trim()
        .parse::<usize>()
        .ok()
        .filter(|&n| n > 0 && n <= 20_000)
        .ok_or_else(|| parse_invalid(format!("{label}须填写 1～20000 的正整数")))
}

fn parse_dec_field(raw: &str, label: &str) -> Result<Decimal, ErrorEnvelope> {
    parse_decimal_lossless(raw.trim()).map_err(|_| parse_invalid(format!("{label}请填写合法数值")))
}

fn status_label(status: SimRunStatus) -> (&'static str, &'static str) {
    match status {
        SimRunStatus::Preparing => ("待启动", "secondary"),
        SimRunStatus::Active => ("进行中", "warning"),
        SimRunStatus::Paused => ("已暂停", "info"),
        SimRunStatus::Finished => ("已完成", "success"),
        SimRunStatus::Damaged => ("状态已损坏", "danger"),
    }
}

fn market_category_label(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "us_equity_etf" => "美股/ETF",
        "crypto_spot" => "加密现货",
        _ => "不限类别",
    }
}

fn order_status_label(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::PendingParent => "待主单成交激活",
        OrderStatus::Accepted => "已受理待进入下根",
        OrderStatus::Active => "挂单生效中",
        OrderStatus::Filled => "已全部成交",
        OrderStatus::Cancelled => "已撤单",
        OrderStatus::Expired => "到期失效",
        OrderStatus::Rejected => "已拒绝",
    }
}

fn order_type_summary(order_type: &SimOrderType) -> String {
    match order_type {
        SimOrderType::Market => "市价单".to_string(),
        SimOrderType::Limit { limit_price } => {
            format!("限价 @ {}", to_canonical_text(*limit_price))
        }
        SimOrderType::StopMarket { stop_price } => {
            format!("止损市价 @ {}", to_canonical_text(*stop_price))
        }
        SimOrderType::StopLimit {
            stop_price,
            limit_price,
        } => format!(
            "止损限价 触发 {} / 限价 {}",
            to_canonical_text(*stop_price),
            to_canonical_text(*limit_price)
        ),
    }
}

fn ratio_metric_text(metric: &RatioMetric) -> String {
    match metric {
        RatioMetric::Computed(v) => to_canonical_text(*v),
        RatioMetric::NoClosedTrades => "—（无已平仓交易）".to_string(),
        RatioMetric::NoLosses => "—（无亏损样本，不虚构有限比值）".to_string(),
        RatioMetric::NoWins => "0（无盈利样本）".to_string(),
    }
}

fn plan_compliance_text(c: &PlanCompliance) -> &'static str {
    match c {
        PlanCompliance::ExitedByPlannedBracket => "按原计划止盈/止损出场",
        PlanCompliance::DeviatedWithRationale { .. } => "调整计划且已附原因说明",
        PlanCompliance::DeviatedWithoutRationale => "偏离计划（未记录原因）",
        PlanCompliance::Unplanned => "无预先止盈止损计划",
    }
}

impl CenterWorkspacePanel {
    pub(crate) fn is_any_training_text_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.fields
            .is_any_focused(TRAINING_TEXT_INPUT_KEYS, window, cx)
    }

    pub(crate) fn dispatch_training(
        &self,
        action: Result<TrainingUiAction, ErrorEnvelope>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(action.map(|a| UiAction::Training(Box::new(a))), window, cx);
    }

    pub(crate) fn parse_new_training_run_request(
        &self,
        create_and_start: bool,
        cx: &App,
    ) -> Result<CreateTrainingRunRequest, ErrorEnvelope> {
        let draft = self.state.lock().unwrap().training.new_run_draft.clone();
        let inst_raw = self.fields.value("form.trn_instrument", cx);
        let instrument_id = {
            let t = inst_raw.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        let start_raw = self.fields.value("form.trn_start_date", cx);
        let start_date = if start_raw.trim().is_empty() {
            None
        } else {
            Some(
                NaiveDate::parse_from_str(start_raw.trim(), "%Y-%m-%d")
                    .map_err(|_| parse_invalid("起始日期请填写 YYYY-MM-DD"))?,
            )
        };
        let end_raw = self.fields.value("form.trn_end_date", cx);
        let end_date = if end_raw.trim().is_empty() {
            None
        } else {
            Some(
                NaiveDate::parse_from_str(end_raw.trim(), "%Y-%m-%d")
                    .map_err(|_| parse_invalid("截止日期请填写 YYYY-MM-DD"))?,
            )
        };
        let history_bars = parse_pos_usize(
            &self.fields.value("form.trn_history_bars", cx),
            "展示历史根数",
        )?;
        let training_bars =
            parse_pos_usize(&self.fields.value("form.trn_training_bars", cx), "训练步数")?;
        let reveal_bars = self
            .fields
            .value("form.trn_reveal_bars", cx)
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n <= 5_000)
            .ok_or_else(|| parse_invalid("结束揭示根数须填写 0～5000 的整数"))?;
        let warmup_bars = self
            .fields
            .value("form.trn_warmup_bars", cx)
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n <= 5_000)
            .ok_or_else(|| parse_invalid("指标预热根数须填写 0～5000 的整数"))?;
        let seed_raw = self.fields.value("form.trn_seed", cx);
        let seed = if seed_raw.trim().is_empty() {
            None
        } else {
            Some(
                seed_raw
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| parse_invalid("随机种子须填写非负整数"))?,
            )
        };
        let initial_cash = parse_dec_field(
            &self.fields.value("form.trn_initial_cash", cx),
            "初始模拟资金",
        )?;
        if initial_cash <= Decimal::ZERO {
            return Err(parse_invalid("初始模拟资金必须大于 0"));
        }
        Ok(CreateTrainingRunRequest {
            run_id: None,
            market_category: draft.market_category,
            instrument_id,
            start_date,
            end_date,
            warmup_bars,
            history_bars,
            training_bars,
            reveal_bars,
            seed,
            dataset_version: None,
            initial_cash,
            fee_profile_version_id: draft.fee_profile_version_id,
            hidden_mode: draft.hidden_mode,
            create_and_start,
        })
    }

    pub(crate) fn parse_training_order_request(
        &self,
        cx: &App,
    ) -> Result<TrainingOrderRequestDto, ErrorEnvelope> {
        let draft = self.state.lock().unwrap().training.order_draft.clone();
        let qty = parse_dec_field(&self.fields.value("form.trn_order_qty", cx), "委托数量")?;
        match draft.kind_mode {
            OrderFormKindMode::Market => Ok(TrainingOrderRequestDto::single(
                draft.side,
                SimOrderType::Market,
                qty,
                draft.tif,
                None,
            )),
            OrderFormKindMode::Limit => {
                let limit_px =
                    parse_dec_field(&self.fields.value("form.trn_order_limit", cx), "限价")?;
                Ok(TrainingOrderRequestDto::single(
                    draft.side,
                    SimOrderType::Limit {
                        limit_price: limit_px,
                    },
                    qty,
                    draft.tif,
                    None,
                ))
            }
            OrderFormKindMode::StopMarket => {
                let stop_px =
                    parse_dec_field(&self.fields.value("form.trn_order_stop", cx), "止损触发价")?;
                Ok(TrainingOrderRequestDto::single(
                    draft.side,
                    SimOrderType::StopMarket {
                        stop_price: stop_px,
                    },
                    qty,
                    draft.tif,
                    None,
                ))
            }
            OrderFormKindMode::Oco => {
                let tp = parse_dec_field(&self.fields.value("form.trn_order_tp", cx), "止盈限价")?;
                let sl =
                    parse_dec_field(&self.fields.value("form.trn_order_sl", cx), "止损触发价")?;
                Ok(TrainingOrderRequestDto::oco(qty, tp, sl, draft.tif, true))
            }
            OrderFormKindMode::EntryWithBracket => {
                let order_type = if draft.entry_is_limit {
                    let limit_px = parse_dec_field(
                        &self.fields.value("form.trn_order_limit", cx),
                        "入场限价",
                    )?;
                    SimOrderType::Limit {
                        limit_price: limit_px,
                    }
                } else {
                    SimOrderType::Market
                };
                let tp_raw = self.fields.value("form.trn_order_tp", cx);
                let sl_raw = self.fields.value("form.trn_order_sl", cx);
                let take_profit_limit = if tp_raw.trim().is_empty() {
                    None
                } else {
                    Some(parse_dec_field(&tp_raw, "附带止盈限价")?)
                };
                let stop_loss_stop = if sl_raw.trim().is_empty() {
                    None
                } else {
                    Some(parse_dec_field(&sl_raw, "附带止损触发价")?)
                };
                if take_profit_limit.is_none() && stop_loss_stop.is_none() {
                    return Err(parse_invalid(
                        "入场附带止盈止损至少需填写一个止盈限价或止损触发价",
                    ));
                }
                Ok(TrainingOrderRequestDto::single(
                    OrderSide::Buy,
                    order_type,
                    qty,
                    draft.tif,
                    Some(AttachedBracket {
                        take_profit_limit,
                        stop_loss_stop,
                        tif: TimeInForce::Gtc,
                    }),
                ))
            }
        }
    }

    pub(crate) fn parse_custom_fee_profile_draft(
        &self,
        cx: &App,
    ) -> Result<FeeProfileDraftDto, ErrorEnvelope> {
        let key = self.fields.value("form.trn_fee_key", cx).trim().to_string();
        let name = self
            .fields
            .value("form.trn_fee_name", cx)
            .trim()
            .to_string();
        if key.is_empty() || name.is_empty() {
            return Err(parse_invalid("自定义费率标识与名称不能为空"));
        }
        let rate = parse_dec_field(&self.fields.value("form.trn_fee_rate", cx), "按成交额费率")?;
        let min_raw = self.fields.value("form.trn_fee_min", cx);
        let min_fee = if min_raw.trim().is_empty() {
            None
        } else {
            Some(parse_dec_field(&min_raw, "单笔最低费用")?)
        };
        let slip_raw = self.fields.value("form.trn_fee_slippage_bps", cx);
        let slippage = if slip_raw.trim().is_empty() {
            SlippageSpec::Zero
        } else {
            let bps = parse_dec_field(&slip_raw, "滑点假设")?;
            if bps.is_zero() {
                SlippageSpec::Zero
            } else {
                SlippageSpec::Bps(bps)
            }
        };
        Ok(FeeProfileDraftDto {
            profile_key: key,
            name,
            market_category: FeeMarketCategory::Any,
            notional_rate: rate,
            min_fee_per_order: min_fee,
            slippage,
            is_scenario_assumption: true,
            assumption_note: "用户自定义模拟训练场景假设费率".to_string(),
            ..FeeProfileDraftDto::default()
        })
    }

    pub(crate) fn clear_training_crosshair_and_focus_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let mut g = self.state.lock().unwrap();
            if let Some(vp) = g.training.chart_viewport.as_mut() {
                vp.move_crosshair(None, None);
            }
        }
        self.training_workspace_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn training_chart_pointer(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        click: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = *self.training_plot_bounds.lock().unwrap() else {
            return;
        };
        let Some((x, y)) = crate::chart_canvas::pointer_ratio(position, bounds) else {
            return;
        };
        let mut g = self.state.lock().unwrap();
        let Some(slice) = g.training.workspace.as_ref().map(|ws| &ws.chart_slice) else {
            return;
        };
        let index = (!slice.bars.is_empty()).then(|| {
            slice.visible_start_index
                + ((x * slice.bars.len() as f32).floor() as usize).min(slice.bars.len() - 1)
        });
        let price = (|| {
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
        })();
        let total_bars = slice.total_visible_bars;
        let vis_start = slice.visible_start_index;
        let vis_end = slice.visible_end_index;
        let vp = g.training.chart_viewport.get_or_insert_with(|| {
            let mut init = KlineViewport::new(total_bars, vis_end.saturating_sub(vis_start).max(1));
            init.visible_start = vis_start;
            init.visible_end = vis_end;
            init
        });
        if index != vp.crosshair_index || price != vp.crosshair_price {
            vp.move_crosshair(index, price);
            cx.notify();
        }
        if click {
            self.training_chart_drag = Some((position, vp.clone()));
        }
    }

    /// Handles keyboard shortcuts in the Active Training Workspace (`S-46`, `NFR-09`, `M4-A-16`):
    /// - `Escape` blurs any focused text input back to workspace stepping and clears the crosshair.
    /// - `Ctrl+Enter` submits the current order draft (explicitly separate from next-bar stepping).
    /// - When no text input holds focus:
    ///   - `N` or `Right` (`→`) advances 1 bar and NEVER submits an order.
    ///   - `P` pauses and saves the active training run.
    ///   - `O` focuses the order ticket quantity input (`form.trn_order_qty`).
    ///   - `J` focuses the training note rationale input (`form.trn_note_rationale`).
    pub(crate) fn handle_training_workspace_key_down(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_run_id = {
            let g = self.state.lock().unwrap();
            if g.layout.active_nav != "nav.training"
                || g.training.mode != TrainingViewMode::Workspace
            {
                return;
            }
            g.training
                .workspace
                .as_ref()
                .filter(|ws| ws.run.status == SimRunStatus::Active)
                .map(|ws| ws.run.run_id.clone())
        };
        let Some(run_id) = active_run_id else {
            return;
        };

        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;

        // 0. `Escape`: exit any focused text input back to workspace stepping and clear crosshair
        if key.eq_ignore_ascii_case("escape")
            && !mods.control
            && !mods.alt
            && !mods.platform
            && !mods.shift
        {
            self.clear_training_crosshair_and_focus_workspace(window, cx);
            cx.stop_propagation();
            return;
        }

        // 1. Explicit Order Submit shortcut: `Ctrl+Enter` ONLY (`NFR-09`, `S-46`)
        if key.eq_ignore_ascii_case("enter")
            && (mods.control || mods.secondary(cx))
            && !mods.alt
            && !mods.shift
        {
            let parsed = self.parse_training_order_request(cx);
            self.training_workspace_focus.focus(window, cx);
            self.dispatch_training(
                parsed.map(|request| TrainingUiAction::SubmitOrder { run_id, request }),
                window,
                cx,
            );
            cx.stop_propagation();
            return;
        }

        // 2. Single-key workspace shortcuts when NO text input holds focus and NO modifiers are held
        if !mods.control
            && !mods.alt
            && !mods.platform
            && !mods.shift
            && !self.is_any_training_text_input_focused(window, cx)
        {
            if key.eq_ignore_ascii_case("n") || key == "right" {
                self.dispatch_training(Ok(TrainingUiAction::AdvanceBar(run_id)), window, cx);
                cx.stop_propagation();
            } else if key.eq_ignore_ascii_case("p") {
                self.dispatch_training(Ok(TrainingUiAction::PauseRun(run_id)), window, cx);
                cx.stop_propagation();
            } else if key.eq_ignore_ascii_case("o") {
                let fh = self
                    .fields
                    .get("form.trn_order_qty")
                    .read(cx)
                    .focus_handle(cx);
                fh.focus(window, cx);
                cx.notify();
                cx.stop_propagation();
            } else if key.eq_ignore_ascii_case("j") {
                let fh = self
                    .fields
                    .get("form.trn_note_rationale")
                    .read(cx)
                    .focus_handle(cx);
                fh.focus(window, cx);
                cx.notify();
                cx.stop_propagation();
            }
        }
    }

    fn render_simulated_training_banner(&self, cx: &App) -> AnyElement {
        let badge_text = SharedString::from(SIMULATED_TRAINING_BADGE);
        let subtitle = SharedString::from(
            "独立模拟账户 · 所有模拟订单、费用与盈亏均与真实资产账本严格隔离，永不混入净值报表",
        );
        h_flex()
            .id("training-simulated-banner")
            .test_support()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .justify_between()
            .border_1()
            .border_color(cx.theme().warning)
            .rounded(cx.theme().radius)
            .bg(cx.theme().secondary)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        div()
                            .id("training-simulated-badge")
                            .test_support()
                            .aria_label(badge_text.clone())
                            .child(Tag::warning().child(badge_text.clone()))
                            .test_text(badge_text),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(cx.theme().foreground)
                            .child(subtitle.clone())
                            .test_text(subtitle),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(Chrome::caption())
                    .child("快捷键：下一根 N / → ｜ 暂停 P ｜ 委托数量 O ｜ 记录想法 J ｜ 退出输入/清光标 Esc ｜ 提交委托 Ctrl+Enter"),
            )
            .into_any_element()
    }

    pub(crate) fn render_training_page(
        &mut self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = match state.training.mode {
            TrainingViewMode::List => self.render_training_list_view(state, cx),
            TrainingViewMode::NewDialog => self.render_training_new_dialog(state, cx),
            TrainingViewMode::Workspace => self.render_training_workspace_view(state, cx),
            TrainingViewMode::Report => self.render_training_report_view(state, cx),
        };
        v_flex()
            .id("training-page-root")
            .test_support()
            .w_full()
            .gap_3()
            .child(self.render_simulated_training_banner(cx))
            .child(content)
            .into_any_element()
    }

    fn render_training_list_view(
        &mut self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let runs = state.training.runs.clone();
        let del_preview = state.training.delete_preview.clone();

        v_flex()
            .id("training-list-view")
            .test_support()
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(format!("模拟训练会话列表（共 {} 条）", runs.len())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("training-refresh-list-btn")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Loader)
                                    .label("刷新列表")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::RefreshList),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-open-new-btn")
                                    .small()
                                    .primary()
                                    .icon(IconName::Plus)
                                    .label("新建模拟训练")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::OpenNewDialog),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            .when_some(del_preview, |col, preview| {
                let run_id_confirm = preview.run_id.clone();
                let preview_summary = SharedString::from(format!(
                    "删除预览：会话 {}（委托 {} 笔 · 成交 {} 笔 · 训练内笔记 {} 条 · 检查点 {} 个 · 事件 {} 条；已归档日志笔记 {} 条将保留）",
                    preview.run_id,
                    preview.order_count,
                    preview.fill_count,
                    preview.note_count,
                    preview.checkpoint_count,
                    preview.event_count,
                    preview.archived_journal_note_count
                ));
                col.child(
                    v_flex()
                        .id("training-delete-preview-card")
                        .test_support()
                        .w_full()
                        .p_3()
                        .gap_2()
                        .border_1()
                        .border_color(cx.theme().danger)
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .text_color(cx.theme().danger)
                                .child(preview_summary.clone())
                                .test_text(preview_summary),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("training-confirm-delete-btn")
                                        .small()
                                        .danger()
                                        .label("确认删除模拟训练")
                                        .on_click(cx.listener(move |panel, _, window, cx| {
                                            panel.dispatch_training(
                                                Ok(TrainingUiAction::ConfirmDeleteRun(
                                                    run_id_confirm.clone(),
                                                )),
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    Button::new("training-cancel-delete-btn")
                                        .small()
                                        .ghost()
                                        .label("取消")
                                        .on_click(cx.listener(|panel, _, window, cx| {
                                            panel.dispatch_training(
                                                Ok(TrainingUiAction::CancelDeletePreview),
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                        ),
                )
            })
            .child(if runs.is_empty() {
                v_flex()
                    .w_full()
                    .p_4()
                    .gap_2()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child("暂无模拟训练会话"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(Chrome::caption())
                            .child("请先在「数据中心」同步美股/ETF 或加密现货日线行情，再点击右上角「新建模拟训练」开始逐根回放练习。"),
                    )
                    .into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .gap_2()
                    .children(runs.into_iter().map(|run| {
                        let (st_text, st_tone) = status_label(run.status);
                        let status_tag = match st_tone {
                            "warning" => Tag::warning().small().child(st_text),
                            "info" => Tag::info().small().child(st_text),
                            "success" => Tag::success().small().child(st_text),
                            "danger" => Tag::danger().small().child(st_text),
                            _ => Tag::secondary().small().child(st_text),
                        };
                        let rid_resume = run.run_id.clone();
                        let rid_report = run.run_id.clone();
                        let rid_retry = run.run_id.clone();
                        let rid_del = run.run_id.clone();
                        let row_summary = SharedString::from(format!(
                            "{} | {} | 进度 {}/{} 步 ({}) | 初始资金 {} {} | {}",
                            run.display_symbol,
                            market_category_label(&run.market_category),
                            run.current_step_index,
                            run.total_steps,
                            run.current_bar_label,
                            to_canonical_text(run.initial_cash),
                            run.quote_currency,
                            run.fee_profile_name
                        ));
                        h_flex()
                            .id(SharedString::from(format!("training-row-{}", run.run_id)))
                            .test_support()
                            .w_full()
                            .p_2p5()
                            .gap_3()
                            .items_center()
                            .justify_between()
                            .flex_wrap()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded(cx.theme().radius)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_wrap()
                                    .child(status_tag)
                                    .when(run.is_identity_masked, |row| {
                                        row.child(Tag::warning().small().outline().child("隐藏模式"))
                                    })
                                    .child(
                                        div()
                                            .font_family(cx.theme().mono_font_family.clone())
                                            .text_xs()
                                            .child(row_summary.clone())
                                            .test_text(row_summary),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .when(
                                        matches!(
                                            run.status,
                                            SimRunStatus::Preparing
                                                | SimRunStatus::Active
                                                | SimRunStatus::Paused
                                        ),
                                        |row| {
                                            row.child(
                                                Button::new(SharedString::from(format!(
                                                    "training-resume-{}",
                                                    run.run_id
                                                )))
                                                .xsmall()
                                                .primary()
                                                .label("继续")
                                                .on_click(cx.listener(move |panel, _, window, cx| {
                                                    panel.dispatch_training(
                                                        Ok(TrainingUiAction::ResumeOrOpenRun(
                                                            rid_resume.clone(),
                                                        )),
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                            )
                                        },
                                    )
                                    .when(run.status == SimRunStatus::Finished, |row| {
                                        row.child(
                                            Button::new(SharedString::from(format!(
                                                "training-report-{}",
                                                run.run_id
                                            )))
                                            .xsmall()
                                            .primary()
                                            .label("查看报告")
                                            .on_click(cx.listener(move |panel, _, window, cx| {
                                                panel.dispatch_training(
                                                    Ok(TrainingUiAction::OpenReport(
                                                        rid_report.clone(),
                                                    )),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                        )
                                    })
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "training-retry-{}",
                                            run.run_id
                                        )))
                                        .xsmall()
                                        .ghost()
                                        .label("重试")
                                        .on_click(cx.listener(move |panel, _, window, cx| {
                                            panel.dispatch_training(
                                                Ok(TrainingUiAction::RetryRun(rid_retry.clone())),
                                                window,
                                                cx,
                                            );
                                        })),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "training-del-preview-{}",
                                            run.run_id
                                        )))
                                        .xsmall()
                                        .ghost()
                                        .label("删除预览")
                                        .on_click(cx.listener(move |panel, _, window, cx| {
                                            panel.dispatch_training(
                                                Ok(TrainingUiAction::PreviewDeleteRun(
                                                    rid_del.clone(),
                                                )),
                                                window,
                                                cx,
                                            );
                                        })),
                                    ),
                            )
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_training_new_dialog(
        &mut self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let draft = state.training.new_run_draft.clone();
        let fee_profiles = state.training.fee_profiles.clone();
        let pending = state.training.pending_prestart_run.clone();

        v_flex()
            .id("training-new-dialog")
            .test_support()
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child("新建模拟训练配置"),
                    )
                    .child(
                        Button::new("training-new-back-btn")
                            .small()
                            .ghost()
                            .label("返回训练列表")
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.dispatch_training(
                                    Ok(TrainingUiAction::OpenList),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_3()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_wrap()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Chrome::caption())
                                    .child("市场类别："),
                            )
                            .children(
                                [
                                    (TrainingMarketCategory::Any, "cat-any", "不限类别"),
                                    (
                                        TrainingMarketCategory::UsEquityEtf,
                                        "cat-eq",
                                        "美股 / ETF",
                                    ),
                                    (TrainingMarketCategory::CryptoSpot, "cat-crypto", "加密现货"),
                                ]
                                .into_iter()
                                .map(|(cat, id, label)| {
                                    Button::new(id)
                                        .small()
                                        .outline()
                                        .selected(draft.market_category == cat)
                                        .label(label)
                                        .on_click(cx.listener(move |panel, _, _, cx| {
                                            panel
                                                .state
                                                .lock()
                                                .unwrap()
                                                .training
                                                .new_run_draft
                                                .market_category = cat;
                                            cx.notify();
                                        }))
                                }),
                            )
                            .child(
                                Button::new("training-hidden-toggle")
                                    .small()
                                    .outline()
                                    .selected(draft.hidden_mode)
                                    .label(if draft.hidden_mode {
                                        "隐藏模式：已开启（隐藏真实代码与日期）"
                                    } else {
                                        "隐藏模式：已关闭（显示真实代码与日期）"
                                    })
                                    .on_click(cx.listener(|panel, _, _, cx| {
                                        let mut g = panel.state.lock().unwrap();
                                        g.training.new_run_draft.hidden_mode =
                                            !g.training.new_run_draft.hidden_mode;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(self.fields.row(&[
                        "form.trn_instrument",
                        "form.trn_start_date",
                        "form.trn_end_date",
                    ]))
                    .child(self.fields.row(&[
                        "form.trn_history_bars",
                        "form.trn_training_bars",
                        "form.trn_reveal_bars",
                        "form.trn_warmup_bars",
                    ]))
                    .child(
                        self.fields
                            .row(&["form.trn_initial_cash", "form.trn_seed"]),
                    )
                    .child(Separator::horizontal())
                    .child(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .child("模拟费用与滑点模板（场景假设版本，按版本冻结）"),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
                                    .children(fee_profiles.iter().map(|fp| {
                                        let fp_id = fp.record.id.clone();
                                        let fp_id_del = fp.record.id.clone();
                                        let is_sel = draft.fee_profile_version_id == fp.record.id;
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .child(
                                                Button::new(SharedString::from(format!(
                                                    "fee-sel-{}",
                                                    fp.record.id
                                                )))
                                                .xsmall()
                                                .outline()
                                                .selected(is_sel)
                                                .label(format!(
                                                    "{}{}",
                                                    fp.record.name,
                                                    if fp.record.is_scenario_assumption {
                                                        " [场景假设]"
                                                    } else {
                                                        ""
                                                    }
                                                ))
                                                .on_click(cx.listener(move |panel, _, _, cx| {
                                                    panel
                                                        .state
                                                        .lock()
                                                        .unwrap()
                                                        .training
                                                        .new_run_draft
                                                        .fee_profile_version_id = fp_id.clone();
                                                    cx.notify();
                                                })),
                                            )
                                            .when(!fp.record.is_builtin, |row| {
                                                row.child(
                                                    Button::new(SharedString::from(format!(
                                                        "fee-del-{}",
                                                        fp.record.id
                                                    )))
                                                    .xsmall()
                                                    .ghost()
                                                    .label("删除")
                                                    .on_click(cx.listener(
                                                        move |panel, _, window, cx| {
                                                            panel.dispatch_training(
                                                                Ok(TrainingUiAction::DeleteUnusedFeeProfile(
                                                                    fp_id_del.clone(),
                                                                )),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                                )
                                            })
                                    })),
                            )
                            .child(self.fields.row(&[
                                "form.trn_fee_key",
                                "form.trn_fee_name",
                                "form.trn_fee_rate",
                                "form.trn_fee_min",
                                "form.trn_fee_slippage_bps",
                            ]))
                            .child(
                                h_flex().child(
                                    Button::new("training-save-fee-btn")
                                        .xsmall()
                                        .ghost()
                                        .label("保存为新费用配置版本")
                                        .on_click(cx.listener(|panel, _, window, cx| {
                                            let parsed = panel.parse_custom_fee_profile_draft(cx);
                                            panel.dispatch_training(
                                                parsed.map(TrainingUiAction::SaveCustomFeeProfile),
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                            ),
                    )
                    .child(Separator::horizontal())
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                Button::new("training-create-draft-btn")
                                    .small()
                                    .outline()
                                    .label("先抽样预览（可换一个）")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        let req = panel.parse_new_training_run_request(false, cx);
                                        panel.dispatch_training(
                                            req.map(TrainingUiAction::CreateDraftRun),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-create-start-btn")
                                    .small()
                                    .primary()
                                    .label("立即创建并开始训练")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        let req = panel.parse_new_training_run_request(true, cx);
                                        panel.dispatch_training(
                                            req.map(TrainingUiAction::CreateAndStartRun),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .when_some(pending, |card, run| {
                        let rid_redraw = run.run_id.clone();
                        let rid_start = run.run_id.clone();
                        let info = SharedString::from(format!(
                            "已抽样待启动：{} · 类别 {} · 种子 {} · 重抽次数 {} 次",
                            run.display_symbol,
                            market_category_label(&run.market_category),
                            run.seed,
                            run.redraw_count
                        ));
                        card.child(
                            v_flex()
                                .id("training-prestart-card")
                                .test_support()
                                .w_full()
                                .p_2p5()
                                .gap_2()
                                .border_1()
                                .border_color(cx.theme().warning)
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().secondary)
                                .child(
                                    div()
                                        .text_xs()
                                        .font_medium()
                                        .child(info.clone())
                                        .test_text(info),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("training-redraw-btn")
                                                .small()
                                                .outline()
                                                .label("换一个")
                                                .on_click(cx.listener(
                                                    move |panel, _, window, cx| {
                                                        panel.dispatch_training(
                                                            Ok(TrainingUiAction::RedrawPendingRun(
                                                                rid_redraw.clone(),
                                                            )),
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                )),
                                        )
                                        .child(
                                            Button::new("training-start-pending-btn")
                                                .small()
                                                .primary()
                                                .label("确认开始训练")
                                                .on_click(cx.listener(
                                                    move |panel, _, window, cx| {
                                                        panel.dispatch_training(
                                                            Ok(TrainingUiAction::StartRun(
                                                                rid_start.clone(),
                                                            )),
                                                            window,
                                                            cx,
                                                        );
                                                    },
                                                )),
                                        ),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_training_workspace_view(
        &mut self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(ws) = state.training.workspace.clone() else {
            return div()
                .text_sm()
                .child("尚未打开模拟训练会话")
                .into_any_element();
        };
        let run_id = ws.run.run_id.clone();
        let rid_next = run_id.clone();
        let rid_pause = run_id.clone();
        let rid_finish = run_id.clone();
        let rid_zoom_in = run_id.clone();
        let rid_zoom_out = run_id.clone();
        let rid_pan_left = run_id.clone();
        let rid_pan_right = run_id.clone();
        let rid_chart_drag = run_id.clone();
        let rid_chart_scroll = run_id.clone();

        let convention = state.layout.color_convention;
        let quote_ccy = ws.account.quote_currency.clone();

        let header_title = SharedString::from(format!(
            "{} · {} · 训练进度 {}/{} 步",
            ws.run.display_symbol,
            ws.run.current_bar_label,
            ws.run.current_step_index,
            ws.run.total_steps
        ));
        let ai_locked_label =
            SharedString::from("AI 复盘：训练结束前已禁用（避免事中干扰独立决策）");

        let active_vp = state.training.chart_viewport.as_ref();
        let mut chart =
            KlineChartView::new("training-kline-chart", ws.chart_slice.clone(), convention)
                .with_dataset_versions(Some(ws.run.dataset_version), Some(ws.run.dataset_version))
                .with_crosshair(active_vp.and_then(|vp| vp.crosshair_index))
                .with_crosshair_price(active_vp.and_then(|vp| vp.crosshair_price));
        if ws.run.is_identity_masked {
            chart = chart
                .with_masked_training_labels(ws.run.display_symbol.clone(), ws.bar_labels.clone());
        }
        let chart_element = chart
            .render_chart_with_bounds(Arc::clone(&self.training_plot_bounds), cx)
            .on_mouse_move(cx.listener(
                move |panel, event: &gpui_kit::MouseMoveEvent, window, cx| {
                    if event.dragging() {
                        if let (Some((start, mut viewport)), Some(bounds)) = (
                            panel.training_chart_drag.clone(),
                            *panel.training_plot_bounds.lock().unwrap(),
                        ) {
                            if bounds.size.width > px(0.) {
                                let bars = -((event.position.x - start.x) / bounds.size.width
                                    * viewport.visible_len() as f32)
                                    .round() as isize;
                                viewport.pan(bars);
                                panel.state.lock().unwrap().training.chart_viewport =
                                    Some(viewport.clone());
                                panel.dispatch_training(
                                    Ok(TrainingUiAction::UpdateChartViewport {
                                        run_id: rid_chart_drag.clone(),
                                        viewport,
                                    }),
                                    window,
                                    cx,
                                );
                            }
                        }
                    } else {
                        panel.training_chart_pointer(event.position, false, cx);
                    }
                },
            ))
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|panel, event: &gpui_kit::MouseDownEvent, window, cx| {
                    panel.training_workspace_focus.focus(window, cx);
                    panel.training_chart_pointer(event.position, true, cx);
                }),
            )
            .on_mouse_up(
                gpui_kit::MouseButton::Left,
                cx.listener(|panel, _, _, _| panel.training_chart_drag = None),
            )
            .on_scroll(
                cx.listener(move |panel, event: &gpui_kit::ScrollEvent, window, cx| {
                    let Some(bounds) = *panel.training_plot_bounds.lock().unwrap() else {
                        return;
                    };
                    let Some((x, _)) = crate::chart_canvas::pointer_ratio(event.position, bounds)
                    else {
                        return;
                    };
                    let delta = event.delta.pixel_delta(px(16.));
                    let viewport = {
                        let mut g = panel.state.lock().unwrap();
                        let fallback_total = g
                            .training
                            .workspace
                            .as_ref()
                            .map(|w| w.chart_slice.total_visible_bars)
                            .unwrap_or(100);
                        let mut vp =
                            g.training.chart_viewport.clone().unwrap_or_else(|| {
                                KlineViewport::new(fallback_total, fallback_total)
                            });
                        vp.zoom(
                            (-f64::from(delta.y) / 600.).exp().clamp(0.5, 2.),
                            f64::from(x),
                        );
                        g.training.chart_viewport = Some(vp.clone());
                        vp
                    };
                    panel.dispatch_training(
                        Ok(TrainingUiAction::UpdateChartViewport {
                            run_id: rid_chart_scroll.clone(),
                            viewport,
                        }),
                        window,
                        cx,
                    );
                    cx.stop_propagation();
                }),
            );

        let order_draft = state.training.order_draft.clone();
        let note_draft = state.training.note_draft.clone();
        let order_preview = state.training.order_preview.clone();
        let last_rejection = state.training.last_order_rejection.clone();
        let step_notices = state
            .training
            .last_step_receipt
            .as_ref()
            .map(|r| r.corporate_action_notices.clone())
            .unwrap_or_default();

        v_flex()
            .id("training-workspace-root")
            .test_support()
            .track_focus(&self.training_workspace_focus)
            .capture_key_down(cx.listener(|panel, event: &gpui_kit::KeyDownEvent, window, cx| {
                let mods = &event.keystroke.modifiers;
                if event.keystroke.key.eq_ignore_ascii_case("escape")
                    && !mods.control
                    && !mods.alt
                    && !mods.platform
                    && !mods.shift
                {
                    panel.clear_training_crosshair_and_focus_workspace(window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|panel, _, window, cx| {
                    panel.training_workspace_focus.focus(window, cx);
                }),
            )
            .on_key_down(cx.listener(|panel, event: &gpui_kit::KeyDownEvent, window, cx| {
                panel.handle_training_workspace_key_down(event, window, cx);
            }))
            .w_full()
            .gap_3()
            .child(
                // Top Control Strip
                h_flex()
                    .w_full()
                    .p_2p5()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_wrap()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(header_title.clone())
                                    .test_text(header_title),
                            )
                            .child(
                                div()
                                    .id("training-ai-disabled-hint")
                                    .test_support()
                                    .child(Tag::secondary().small().outline().child(ai_locked_label.clone()))
                                    .test_text(ai_locked_label),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_wrap()
                            .child(
                                Button::new("training-next-bar-btn")
                                    .small()
                                    .primary()
                                    .label("下一根 (N / →)")
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::AdvanceBar(rid_next.clone())),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-pause-btn")
                                    .small()
                                    .outline()
                                    .label("暂停")
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::PauseRun(rid_pause.clone())),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-finish-btn")
                                    .small()
                                    .warning()
                                    .label("结束训练")
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::FinishRun(rid_finish.clone())),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-back-list-btn")
                                    .small()
                                    .ghost()
                                    .label("返回列表")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::OpenList),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            .when(!step_notices.is_empty() && !ws.run.is_identity_masked, |col| {
                col.child(
                    Alert::info(
                        "training-ca-notice",
                        step_notices.join("；"),
                    )
                    .small(),
                )
            })
            .child(
                // Account Summary MetricCards
                h_flex()
                    .w_full()
                    .gap_2p5()
                    .flex_wrap()
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-card-equity",
                                "模拟账户总权益",
                                Some(ws.account.total_equity),
                                quote_ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_color_convention(convention)
                            .with_note(format!(
                                "累计净盈亏 {} {}",
                                to_canonical_text(ws.account.total_equity - ws.account.initial_cash),
                                quote_ccy
                            ))
                            .render_card(cx),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-card-avail-cash",
                                "可用模拟现金",
                                Some(ws.account.available_cash),
                                quote_ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_color_convention(convention)
                            .render_card(cx),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-card-realized",
                                "累计已实现盈亏",
                                Some(ws.account.realized_pnl),
                                quote_ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_signed_pnl(true)
                            .with_color_convention(convention)
                            .render_card(cx),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-card-fees",
                                "累计模拟费用",
                                Some(ws.account.total_fees_quote),
                                quote_ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_color_convention(convention)
                            .render_card(cx),
                        ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .child(format!(
                        "现金总额 {} {quote_ccy} · 买单预留冻结 {} {quote_ccy} · 分红入账 {} {quote_ccy} · 最小手数 Lot={} · 最小报价 Tick={}",
                        to_canonical_text(ws.account.cash_balance),
                        to_canonical_text(ws.account.reserved_cash),
                        to_canonical_text(ws.account.dividend_income),
                        to_canonical_text(ws.account.lot_size),
                        to_canonical_text(ws.account.tick_size),
                    )),
            )
            .child(
                // Two-column Workspace Body: Left Chart/Positions/Orders + Right Order Ticket & Notes
                h_flex()
                    .w_full()
                    .gap_3()
                    .items_start()
                    .flex_wrap()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(460.))
                            .gap_3()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .child(
                                        Button::new("trn-chart-pan-left")
                                            .xsmall()
                                            .ghost()
                                            .label(I18nCatalog::tr("action.pan_left"))
                                            .on_click(cx.listener(move |panel, _, window, cx| {
                                                let vp = {
                                                    let mut g = panel.state.lock().unwrap();
                                                    let mut v = g
                                                        .training
                                                        .chart_viewport
                                                        .clone()
                                                        .unwrap_or_else(|| KlineViewport::new(100, 100));
                                                    v.pan(-20);
                                                    g.training.chart_viewport = Some(v.clone());
                                                    v
                                                };
                                                panel.dispatch_training(
                                                    Ok(TrainingUiAction::UpdateChartViewport {
                                                        run_id: rid_pan_left.clone(),
                                                        viewport: vp,
                                                    }),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new("trn-chart-pan-right")
                                            .xsmall()
                                            .ghost()
                                            .label(I18nCatalog::tr("action.pan_right"))
                                            .on_click(cx.listener(move |panel, _, window, cx| {
                                                let vp = {
                                                    let mut g = panel.state.lock().unwrap();
                                                    let mut v = g
                                                        .training
                                                        .chart_viewport
                                                        .clone()
                                                        .unwrap_or_else(|| KlineViewport::new(100, 100));
                                                    v.pan(20);
                                                    g.training.chart_viewport = Some(v.clone());
                                                    v
                                                };
                                                panel.dispatch_training(
                                                    Ok(TrainingUiAction::UpdateChartViewport {
                                                        run_id: rid_pan_right.clone(),
                                                        viewport: vp,
                                                    }),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new("trn-chart-zoom-in")
                                            .xsmall()
                                            .ghost()
                                            .label(I18nCatalog::tr("action.zoom_in"))
                                            .on_click(cx.listener(move |panel, _, window, cx| {
                                                let vp = {
                                                    let mut g = panel.state.lock().unwrap();
                                                    let mut v = g
                                                        .training
                                                        .chart_viewport
                                                        .clone()
                                                        .unwrap_or_else(|| KlineViewport::new(100, 100));
                                                    v.zoom(0.7, 1.0);
                                                    g.training.chart_viewport = Some(v.clone());
                                                    v
                                                };
                                                panel.dispatch_training(
                                                    Ok(TrainingUiAction::UpdateChartViewport {
                                                        run_id: rid_zoom_in.clone(),
                                                        viewport: vp,
                                                    }),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new("trn-chart-zoom-out")
                                            .xsmall()
                                            .ghost()
                                            .label(I18nCatalog::tr("action.zoom_out"))
                                            .on_click(cx.listener(move |panel, _, window, cx| {
                                                let vp = {
                                                    let mut g = panel.state.lock().unwrap();
                                                    let mut v = g
                                                        .training
                                                        .chart_viewport
                                                        .clone()
                                                        .unwrap_or_else(|| KlineViewport::new(100, 100));
                                                    v.zoom(1.4, 1.0);
                                                    g.training.chart_viewport = Some(v.clone());
                                                    v
                                                };
                                                panel.dispatch_training(
                                                    Ok(TrainingUiAction::UpdateChartViewport {
                                                        run_id: rid_zoom_out.clone(),
                                                        viewport: vp,
                                                    }),
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    ),
                            )
                            .child(chart_element)
                            .child(self.render_training_positions_and_orders(&ws, cx)),
                    )
                    .child(
                        v_flex()
                            .w(px(360.))
                            .min_w(px(320.))
                            .gap_3()
                            .child(self.render_training_order_ticket(
                                &run_id,
                                &order_draft,
                                order_preview.as_ref(),
                                last_rejection.as_ref(),
                                cx,
                            ))
                            .child(self.render_training_notes_panel(
                                &run_id,
                                &ws,
                                &note_draft,
                                cx,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_training_positions_and_orders(
        &mut self,
        ws: &TrainingWorkspaceSnapshotDto,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let run_id = ws.run.run_id.clone();
        v_flex()
            .w_full()
            .p_3()
            .gap_2p5()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .child("模拟持仓与挂单 / 成交明细"),
            )
            .child(if let Some(pos) = &ws.position {
                let pos_text = SharedString::from(format!(
                    "持仓 {} · 数量 {}（可用 {} / 卖单冻结 {}）· 均价 {} · 最新收盘 {} · 浮动盈亏 {} {}",
                    pos.display_symbol,
                    to_canonical_text(pos.quantity),
                    to_canonical_text(pos.available_quantity),
                    to_canonical_text(pos.reserved_quantity),
                    to_canonical_text(pos.avg_unit_cost),
                    to_canonical_text(pos.mark_close_price),
                    to_canonical_text(pos.unrealized_pnl),
                    ws.account.quote_currency
                ));
                div()
                    .id("training-position-summary")
                    .test_support()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .child(pos_text.clone())
                    .test_text(pos_text)
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(Chrome::caption())
                    .child("当前无模拟持仓")
                    .into_any_element()
            })
            .child(Separator::horizontal())
            .child(
                div()
                    .text_xs()
                    .font_medium()
                    .child(format!("活动模拟挂单（{} 笔）", ws.open_orders.len())),
            )
            .children(ws.open_orders.iter().map(|ord| {
                let rid = run_id.clone();
                let oid = ord.order_id.clone();
                let side_zh = match ord.side {
                    OrderSide::Buy => "买入",
                    OrderSide::Sell => "卖出",
                };
                let tif_zh = match ord.tif {
                    TimeInForce::Day => "当日有效",
                    TimeInForce::Gtc => "撤单前有效",
                };
                let line = SharedString::from(format!(
                    "{} · {side_zh} {} @ {} ({tif_zh}) · {}",
                    ord.order_id,
                    to_canonical_text(ord.quantity),
                    order_type_summary(&ord.order_type),
                    order_status_label(ord.status)
                ));
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .child(line.clone())
                            .test_text(line),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "training-cancel-order-{}",
                            ord.order_id
                        )))
                        .xsmall()
                        .ghost()
                        .label("撤单")
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.dispatch_training(
                                Ok(TrainingUiAction::CancelOrder {
                                    run_id: rid.clone(),
                                    order_id: oid.clone(),
                                }),
                                window,
                                cx,
                            );
                        })),
                    )
            }))
            .when(!ws.fills.is_empty(), |col| {
                col.child(Separator::horizontal())
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .child(format!("最近模拟成交（共 {} 笔）", ws.fills.len())),
                    )
                    .children(ws.fills.iter().rev().take(6).map(|f| {
                        let side_zh = match f.side {
                            OrderSide::Buy => "买入",
                            OrderSide::Sell => "卖出",
                        };
                        let fill_line = SharedString::from(format!(
                            "{} ({}) · {side_zh} {} @ {} · 费用 {} {}{}",
                            f.fill_id,
                            f.bar_label,
                            to_canonical_text(f.gross_quantity),
                            to_canonical_text(f.fill_price),
                            to_canonical_text(f.fee_amount),
                            f.fee_currency,
                            if f.ambiguous { " [同根多条件保守路径]" } else { "" }
                        ));
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(Chrome::caption())
                            .child(fill_line.clone())
                            .test_text(fill_line)
                    }))
            })
            .into_any_element()
    }

    fn render_training_order_ticket(
        &mut self,
        run_id: &str,
        draft: &TrainingOrderFormDraft,
        preview: Option<&TrainingOrderPreviewDto>,
        last_rejection: Option<&OrderRejection>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rid_preview = run_id.to_string();
        let rid_submit = run_id.to_string();
        let rid_ctrl_enter = run_id.to_string();

        v_flex()
            .id("training-order-ticket")
            .test_support()
            .track_focus(&self.training_ticket_focus)
            .capture_key_down(cx.listener(
                move |panel, event: &gpui_kit::KeyDownEvent, window, cx| {
                    let mods = &event.keystroke.modifiers;
                    if event.keystroke.key.eq_ignore_ascii_case("enter")
                        && (mods.control || mods.secondary(cx))
                        && !mods.alt
                        && !mods.shift
                    {
                        let parsed = panel.parse_training_order_request(cx);
                        let r_id = rid_ctrl_enter.clone();
                        panel.training_workspace_focus.focus(window, cx);
                        panel.dispatch_training(
                            parsed.map(|request| TrainingUiAction::SubmitOrder {
                                run_id: r_id,
                                request,
                            }),
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    }
                },
            ))
            .w_full()
            .p_3()
            .gap_2p5()
            .border_1()
            .border_color(cx.theme().warning)
            .rounded(cx.theme().radius)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .child("模拟委托面板（Order Ticket）"),
                    )
                    .child(
                        Tag::warning()
                            .small()
                            .outline()
                            .child(SIMULATED_TRAINING_BADGE),
                    ),
            )
            .child(
                h_flex().gap_1().flex_wrap().children(
                    [
                        (OrderFormKindMode::Market, "ord-kind-mkt", "市价单"),
                        (OrderFormKindMode::Limit, "ord-kind-lmt", "限价单"),
                        (OrderFormKindMode::StopMarket, "ord-kind-stp", "止损单"),
                        (OrderFormKindMode::Oco, "ord-kind-oco", "OCO 止盈止损"),
                        (
                            OrderFormKindMode::EntryWithBracket,
                            "ord-kind-bkt",
                            "入场附带止盈止损",
                        ),
                    ]
                    .into_iter()
                    .map(|(mode, id, label)| {
                        Button::new(id)
                            .xsmall()
                            .outline()
                            .selected(draft.kind_mode == mode)
                            .label(label)
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.training_workspace_focus.focus(window, cx);
                                let mut g = panel.state.lock().unwrap();
                                g.training.order_draft.kind_mode = mode;
                                g.training.order_preview = None;
                                g.training.last_order_rejection = None;
                                cx.notify();
                            }))
                    }),
                ),
            )
            .when(
                !matches!(
                    draft.kind_mode,
                    OrderFormKindMode::Oco | OrderFormKindMode::EntryWithBracket
                ),
                |col| {
                    col.child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Button::new("ord-side-buy")
                                    .xsmall()
                                    .outline()
                                    .selected(draft.side == OrderSide::Buy)
                                    .label("买入 (Buy)")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.training_workspace_focus.focus(window, cx);
                                        panel.state.lock().unwrap().training.order_draft.side =
                                            OrderSide::Buy;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ord-side-sell")
                                    .xsmall()
                                    .outline()
                                    .selected(draft.side == OrderSide::Sell)
                                    .label("卖出 (Sell)")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.training_workspace_focus.focus(window, cx);
                                        panel.state.lock().unwrap().training.order_draft.side =
                                            OrderSide::Sell;
                                        cx.notify();
                                    })),
                            ),
                    )
                },
            )
            .when(
                draft.kind_mode == OrderFormKindMode::EntryWithBracket,
                |col| {
                    col.child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Button::new("ord-bkt-entry-mkt")
                                    .xsmall()
                                    .outline()
                                    .selected(!draft.entry_is_limit)
                                    .label("市价买入入场")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.training_workspace_focus.focus(window, cx);
                                        panel
                                            .state
                                            .lock()
                                            .unwrap()
                                            .training
                                            .order_draft
                                            .entry_is_limit = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ord-bkt-entry-lmt")
                                    .xsmall()
                                    .outline()
                                    .selected(draft.entry_is_limit)
                                    .label("限价买入入场")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.training_workspace_focus.focus(window, cx);
                                        panel
                                            .state
                                            .lock()
                                            .unwrap()
                                            .training
                                            .order_draft
                                            .entry_is_limit = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                },
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Button::new("ord-tif-day")
                            .xsmall()
                            .outline()
                            .selected(draft.tif == TimeInForce::Day)
                            .label("当日有效 (DAY)")
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.training_workspace_focus.focus(window, cx);
                                panel.state.lock().unwrap().training.order_draft.tif =
                                    TimeInForce::Day;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ord-tif-gtc")
                            .xsmall()
                            .outline()
                            .selected(draft.tif == TimeInForce::Gtc)
                            .label("撤单前有效 (GTC)")
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.training_workspace_focus.focus(window, cx);
                                panel.state.lock().unwrap().training.order_draft.tif =
                                    TimeInForce::Gtc;
                                cx.notify();
                            })),
                    ),
            )
            .child(training_input_row_guard(
                self.fields.row(&["form.trn_order_qty"]),
            ))
            .when(
                draft.kind_mode == OrderFormKindMode::Limit
                    || (draft.kind_mode == OrderFormKindMode::EntryWithBracket
                        && draft.entry_is_limit),
                |col| {
                    col.child(training_input_row_guard(
                        self.fields.row(&["form.trn_order_limit"]),
                    ))
                },
            )
            .when(draft.kind_mode == OrderFormKindMode::StopMarket, |col| {
                col.child(training_input_row_guard(
                    self.fields.row(&["form.trn_order_stop"]),
                ))
            })
            .when(
                matches!(
                    draft.kind_mode,
                    OrderFormKindMode::Oco | OrderFormKindMode::EntryWithBracket
                ),
                |col| {
                    col.child(training_input_row_guard(
                        self.fields.row(&["form.trn_order_tp", "form.trn_order_sl"]),
                    ))
                },
            )
            .when_some(preview.and_then(|p| p.preview.as_ref()), |col, cost| {
                let summary = SharedString::from(format!(
                    "预估成交额: {} · 预估费用: {} {} · 需冻结现金: {} · 需冻结持仓: {}",
                    cost.estimated_notional
                        .map(to_canonical_text)
                        .unwrap_or_else(|| "按开盘价结算".into()),
                    cost.estimated_fee
                        .map(to_canonical_text)
                        .unwrap_or_else(|| "0".into()),
                    cost.fee_currency,
                    to_canonical_text(cost.cash_reservation_required),
                    to_canonical_text(cost.position_reservation_required),
                ));
                col.child(
                    div()
                        .id("training-order-preview-box")
                        .test_support()
                        .p_2()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().muted)
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .child(summary.clone())
                        .test_text(summary),
                )
            })
            .when_some(last_rejection, |col, rej| {
                let text = SharedString::from(format_rejection_message(rej));
                col.child(
                    div()
                        .id("training-order-rejection-box")
                        .test_support()
                        .p_2()
                        .border_1()
                        .border_color(cx.theme().danger)
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(text.clone())
                        .test_text(text),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("training-preview-order-btn")
                            .small()
                            .outline()
                            .label("预览费用与占用")
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                let parsed = panel.parse_training_order_request(cx);
                                let r_id = rid_preview.clone();
                                panel.training_workspace_focus.focus(window, cx);
                                panel.dispatch_training(
                                    parsed.map(|request| TrainingUiAction::PreviewOrder {
                                        run_id: r_id,
                                        request,
                                    }),
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("training-submit-order-btn")
                            .small()
                            .primary()
                            .label("提交模拟委托 (Ctrl+Enter)")
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                let parsed = panel.parse_training_order_request(cx);
                                let r_id = rid_submit.clone();
                                panel.training_workspace_focus.focus(window, cx);
                                panel.dispatch_training(
                                    parsed.map(|request| TrainingUiAction::SubmitOrder {
                                        run_id: r_id,
                                        request,
                                    }),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_training_notes_panel(
        &mut self,
        run_id: &str,
        ws: &TrainingWorkspaceSnapshotDto,
        note_draft: &TrainingNoteDraftMeta,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rid_save = run_id.to_string();
        v_flex()
            .id("training-notes-panel")
            .test_support()
            .w_full()
            .p_3()
            .gap_2p5()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .child(div().text_xs().font_semibold().child(format!(
                "模拟训练笔记（{} 条，支持无订单独立记录）",
                ws.notes.len()
            )))
            .child(training_input_row_guard(
                self.fields.row(&["form.trn_note_rationale"]),
            ))
            .child(training_input_row_guard(
                self.fields.row(&["form.trn_note_emotion"]),
            ))
            .child(h_flex().gap_1().flex_wrap().children(
                ["专注", "冷静", "犹豫", "冲动"].into_iter().map(|emo| {
                    Button::new(SharedString::from(format!("trn-emo-{emo}")))
                        .xsmall()
                        .ghost()
                        .label(emo)
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.fields.set("form.trn_note_emotion", emo, window, cx);
                        }))
                }),
            ))
            .when(!ws.all_orders.is_empty(), |col| {
                col.child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .child(
                            Button::new("trn-note-link-none")
                                .xsmall()
                                .outline()
                                .selected(note_draft.linked_order_id.is_none())
                                .label("不关联委托")
                                .on_click(cx.listener(|panel, _, _, cx| {
                                    panel
                                        .state
                                        .lock()
                                        .unwrap()
                                        .training
                                        .note_draft
                                        .linked_order_id = None;
                                    cx.notify();
                                })),
                        )
                        .children(ws.all_orders.iter().rev().take(4).map(|ord| {
                            let oid = ord.order_id.clone();
                            let is_sel = note_draft.linked_order_id.as_deref()
                                == Some(ord.order_id.as_str());
                            Button::new(SharedString::from(format!(
                                "trn-note-link-{}",
                                ord.order_id
                            )))
                            .xsmall()
                            .outline()
                            .selected(is_sel)
                            .label(format!("关联 {}", ord.order_id))
                            .on_click(cx.listener(
                                move |panel, _, _, cx| {
                                    panel
                                        .state
                                        .lock()
                                        .unwrap()
                                        .training
                                        .note_draft
                                        .linked_order_id = Some(oid.clone());
                                    cx.notify();
                                },
                            ))
                        })),
                )
            })
            .child(
                h_flex().gap_2().child(
                    Button::new("training-save-note-btn")
                        .small()
                        .outline()
                        .label(if note_draft.editing_note_id.is_some() {
                            "更新训练笔记"
                        } else {
                            "保存训练笔记"
                        })
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            let rationale = panel.fields.value("form.trn_note_rationale", cx);
                            if rationale.trim().is_empty() {
                                panel.dispatch_training(
                                    Err(parse_invalid("训练笔记内容不能为空")),
                                    window,
                                    cx,
                                );
                                return;
                            }
                            let emotion_raw = panel.fields.value("form.trn_note_emotion", cx);
                            let emotion_tag = {
                                let t = emotion_raw.trim();
                                (!t.is_empty()).then(|| t.to_string())
                            };
                            let meta = panel.state.lock().unwrap().training.note_draft.clone();
                            panel.fields.set("form.trn_note_rationale", "", window, cx);
                            panel.training_workspace_focus.focus(window, cx);
                            panel.dispatch_training(
                                Ok(TrainingUiAction::SaveNote {
                                    run_id: rid_save.clone(),
                                    request: UpsertTrainingNoteRequest {
                                        note_id: meta.editing_note_id,
                                        rationale,
                                        emotion_tag,
                                        linked_order_id: meta.linked_order_id,
                                    },
                                }),
                                window,
                                cx,
                            );
                        })),
                ),
            )
            .children(ws.notes.iter().rev().take(5).map(|n| {
                let nid = n.note_id.clone();
                let rat = n.rationale.clone();
                let emo = n.emotion_tag.clone().unwrap_or_default();
                let lnk = n.linked_order_id.clone();
                let summary = SharedString::from(format!(
                    "{} ({}) [{}] {}",
                    n.note_id,
                    n.bar_label,
                    n.emotion_tag.as_deref().unwrap_or("无标签"),
                    n.rationale
                ));
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_1p5()
                    .child(div().text_xs().child(summary.clone()).test_text(summary))
                    .child(
                        Button::new(SharedString::from(format!("trn-edit-note-{}", n.note_id)))
                            .xsmall()
                            .ghost()
                            .label("编辑")
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.fields.set(
                                    "form.trn_note_rationale",
                                    rat.clone(),
                                    window,
                                    cx,
                                );
                                panel
                                    .fields
                                    .set("form.trn_note_emotion", emo.clone(), window, cx);
                                let mut g = panel.state.lock().unwrap();
                                g.training.note_draft.editing_note_id = Some(nid.clone());
                                g.training.note_draft.linked_order_id = lnk.clone();
                                cx.notify();
                            })),
                    )
            }))
            .into_any_element()
    }

    fn render_training_report_view(
        &mut self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(rep_dto) = state.training.report.clone() else {
            return div()
                .text_sm()
                .child("尚未加载模拟训练报告")
                .into_any_element();
        };
        let rep = &rep_dto.report;
        let run = &rep_dto.run;
        let run_id = run.run_id.clone();
        let rid_retry = run_id.clone();
        let rid_self_eval = run_id.clone();
        let rid_archive = run_id.clone();
        let rid_reveal = run_id.clone();
        let rid_ai = run_id.clone();
        let convention = state.layout.color_convention;
        let ccy = rep.quote_currency.clone();

        let win_rate_str = match rep.win_rate.status {
            WinRateStatus::NoClosedTrades => "—（无已平仓交易）".to_string(),
            WinRateStatus::Computed => format!(
                "{}%（胜 {} / 负 {} / 零盈亏单独统计 {}）",
                rep.win_rate
                    .win_rate_pct
                    .map(to_canonical_text)
                    .unwrap_or_else(|| "0".into()),
                rep.win_rate.winning_count,
                rep.win_rate.losing_count,
                rep.win_rate.zero_pnl_count
            ),
        };
        let expectancy_str = match rep.expectancy.status {
            WinRateStatus::NoClosedTrades => {
                format!("—（样本数 {}）", rep.expectancy.sample_count)
            }
            WinRateStatus::Computed => format!(
                "{} {ccy}（已平仓样本数 {}）",
                rep.expectancy
                    .value
                    .map(to_canonical_text)
                    .unwrap_or_else(|| "0".into()),
                rep.expectancy.sample_count
            ),
        };
        let plan_str = match rep.plan_execution.status {
            PlanExecutionStatus::NoPlannedTrades => format!(
                "—（无带计划已平仓交易；无计划交易 {} 组单独统计）",
                rep.plan_execution.unplanned_closed_group_count
            ),
            PlanExecutionStatus::Computed => format!(
                "{}%（按计划 {} / 附说明调整 {} / 无说明偏离 {}；另无计划 {} 组）",
                rep.plan_execution
                    .execution_rate_pct
                    .map(to_canonical_text)
                    .unwrap_or_else(|| "0".into()),
                rep.plan_execution.exited_by_plan_count,
                rep.plan_execution.deviated_with_rationale_count,
                rep.plan_execution.deviated_without_rationale_count,
                rep.plan_execution.unplanned_closed_group_count
            ),
        };
        let bench_str = match rep.benchmark.status {
            BenchmarkStatus::Executed => format!(
                "期末权益 {} {ccy}（收益率 {}% · 最大回撤 {}%）｜ 相对基准超额收益 {}%",
                to_canonical_text(rep.benchmark.ending_equity),
                to_canonical_text(rep.benchmark.total_return_pct),
                to_canonical_text(rep.benchmark.max_drawdown_pct),
                to_canonical_text(rep.excess_return_vs_benchmark_pct)
            ),
            _ => "同区间买入持有基准不可用（样本或首根资金不足一手）".to_string(),
        };

        let metrics_summary = SharedString::from(format!(
            "总收益率 {}% | 最大回撤 {}% ({} {ccy}) | 仓位暴露 {}% ({}/{} 根) | 胜率 {} | 盈亏比 {} | 获利因子 {} | 单笔期望 {} | 计划执行率 {}",
            to_canonical_text(rep.total_return_pct),
            to_canonical_text(rep.max_drawdown_pct),
            to_canonical_text(rep.max_drawdown_amount),
            to_canonical_text(rep.exposure_pct),
            rep.exposed_bars,
            rep.total_training_bars,
            win_rate_str,
            ratio_metric_text(&rep.payoff_ratio),
            ratio_metric_text(&rep.profit_factor),
            expectancy_str,
            plan_str,
        ));

        let se_draft = state.training.self_eval_draft.clone();
        let saved_se = rep_dto.self_evaluation.clone();
        let archived_notes = state.training.last_archived_notes.clone();
        let reveal_opt = state.training.reveal.clone();
        let ai_rep_opt = state.training.training_ai_report.clone();
        let ai_err_opt = state.training.training_ai_error.clone();
        let opened_ev_opt = state.training.opened_training_evidence.clone();

        v_flex()
            .id("training-report-view")
            .test_support()
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(format!(
                                "模拟训练复盘报告 · {}",
                                run.display_symbol
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("training-report-retry-btn")
                                    .small()
                                    .outline()
                                    .label("用同一抽样段重试")
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::RetryRun(rid_retry.clone())),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-report-back-btn")
                                    .small()
                                    .ghost()
                                    .label("返回训练列表")
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::OpenList),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            // 1. Quantitative Report Summary Cards
            .child(
                h_flex()
                    .w_full()
                    .gap_2p5()
                    .flex_wrap()
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-rep-ending-equity",
                                "期末模拟总权益",
                                Some(rep.ending_equity),
                                ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_color_convention(convention)
                            .with_note(format!(
                                "总盈亏 {} {}",
                                to_canonical_text(rep.total_pnl),
                                ccy
                            ))
                            .render_card(cx),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-rep-realized",
                                "已实现净盈亏",
                                Some(rep.realized_pnl),
                                ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_signed_pnl(true)
                            .with_color_convention(convention)
                            .render_card(cx),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-rep-unrealized",
                                "期末未平仓浮盈亏",
                                Some(rep.unrealized_pnl),
                                ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_signed_pnl(true)
                            .with_color_convention(convention)
                            .render_card(cx),
                        ),
                    )
                    .child(
                        div().flex_1().min_w(px(170.)).child(
                            MetricCard::new(
                                "trn-rep-fees",
                                "累计模拟费用",
                                Some(rep.total_fees_quote),
                                ccy.clone(),
                                QualityStatus::Exact,
                            )
                            .with_color_convention(convention)
                            .render_card(cx),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .id("training-report-metrics-card")
                    .test_support()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .child(metrics_summary.clone())
                            .test_text(metrics_summary),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(Chrome::caption())
                            .child(format!("同区间买入持有基准：{bench_str}")),
                    )
                    .when(!rep.closed_trade_groups.is_empty(), |col| {
                        col.child(Separator::horizontal())
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .child("已平仓交易组与 MAE / MFE（最大不利 / 最大有利偏离）"),
                            )
                            .children(rep.closed_trade_groups.iter().map(|grp| {
                                // Strictly avoid showing real entry/exit session dates while identity is masked!
                                // `equity_bar_labels` is 0-indexed parallel to `stepped_training_bars` (`idx` -> `"第 {history_bars + idx + 1} 根"`),
                                // matching `grp.entry_bar_index` / `grp.exit_bar_index` (`training.rs:1310-1325`).
                                let span_label = if run.is_identity_masked {
                                    let entry_lbl = rep_dto
                                        .equity_bar_labels
                                        .get(grp.entry_bar_index)
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            format!("训练第 {} 步", grp.entry_bar_index + 1)
                                        });
                                    let exit_lbl = rep_dto
                                        .equity_bar_labels
                                        .get(grp.exit_bar_index)
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            format!("训练第 {} 步", grp.exit_bar_index + 1)
                                        });
                                    format!("{entry_lbl} -> {exit_lbl}")
                                } else {
                                    format!("{} -> {}", grp.entry_session_date, grp.exit_session_date)
                                };
                                let grp_line = SharedString::from(format!(
                                    "{} ({span_label}) · 数量 {} · 入场均价 {} -> 出场均价 {} · 已实现 {} {ccy} · MAE {} ({}%) · MFE {} ({}%) · {}",
                                    grp.group_id,
                                    to_canonical_text(grp.total_quantity),
                                    to_canonical_text(grp.weighted_avg_entry_price),
                                    to_canonical_text(grp.weighted_avg_exit_price),
                                    to_canonical_text(grp.realized_pnl),
                                    to_canonical_text(grp.mae_per_unit),
                                    to_canonical_text(grp.mae_pct),
                                    to_canonical_text(grp.mfe_per_unit),
                                    to_canonical_text(grp.mfe_pct),
                                    plan_compliance_text(&grp.plan_compliance),
                                ));
                                div()
                                    .id(SharedString::from(format!(
                                        "training-closed-grp-{}",
                                        grp.group_id
                                    )))
                                    .test_support()
                                    .aria_label(grp_line.clone())
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_xs()
                                    .child(grp_line.clone())
                                    .test_text(grp_line)
                            }))
                    })
                    .child(Separator::horizontal())
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .child("模拟假设与样本局限声明"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(Chrome::caption())
                            .child(format!(
                                "{} · {} · {}",
                                rep.limitations.bar_path_assumption,
                                rep.limitations.liquidity_assumption,
                                rep.limitations.fee_assumption
                            )),
                    )
                    .children(rep.limitations.items.iter().map(|item| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child(format!("• {}", item.message))
                    })),
            )
            // 2. User Autonomous Self-Evaluation & Journal Archive (`S-45`: strictly separate from AI Review)
            .child(
                v_flex()
                    .id("training-self-evaluation-card")
                    .test_support()
                    .w_full()
                    .p_3()
                    .gap_2p5()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .child("用户自主决策评分与笔记归档（与 AI 复盘独立呈现）"),
                            )
                            .when_some(saved_se, |row, se| {
                                let saved_badge = SharedString::from(format!(
                                    "已存自主评分：决策 {}/5 分{}",
                                    se.decision_score,
                                    se.plan_discipline_score
                                        .map(|d| format!(" · 纪律 {d}/5 分"))
                                        .unwrap_or_default()
                                ));
                                row.child(
                                    div()
                                        .id("training-saved-self-score")
                                        .test_support()
                                        .child(Tag::info().small().child(saved_badge.clone()))
                                        .test_text(saved_badge),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .flex_wrap()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Chrome::caption())
                                    .child("自主决策评分 (1–5)："),
                            )
                            .children((1_u8..=5_u8).map(|score| {
                                Button::new(SharedString::from(format!("trn-se-dec-{score}")))
                                    .xsmall()
                                    .outline()
                                    .selected(se_draft.decision_score == score)
                                    .label(format!("{score} 分"))
                                    .on_click(cx.listener(move |panel, _, _, cx| {
                                        panel
                                            .state
                                            .lock()
                                            .unwrap()
                                            .training
                                            .self_eval_draft
                                            .decision_score = score;
                                        cx.notify();
                                    }))
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Chrome::caption())
                                    .child("计划纪律评分："),
                            )
                            .children((1_u8..=5_u8).map(|score| {
                                Button::new(SharedString::from(format!("trn-se-disc-{score}")))
                                    .xsmall()
                                    .outline()
                                    .selected(se_draft.plan_discipline_score == Some(score))
                                    .label(format!("{score} 分"))
                                    .on_click(cx.listener(move |panel, _, _, cx| {
                                        panel
                                            .state
                                            .lock()
                                            .unwrap()
                                            .training
                                            .self_eval_draft
                                            .plan_discipline_score = Some(score);
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(self.fields.row(&["form.trn_self_eval_summary"]))
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .child(
                                Button::new("training-save-self-eval-btn")
                                    .small()
                                    .outline()
                                    .label("保存自主评分")
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        let summary =
                                            panel.fields.value("form.trn_self_eval_summary", cx);
                                        let draft = panel
                                            .state
                                            .lock()
                                            .unwrap()
                                            .training
                                            .self_eval_draft
                                            .clone();
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::SaveSelfEvaluation {
                                                run_id: rid_self_eval.clone(),
                                                decision_score: draft.decision_score,
                                                plan_discipline_score: draft.plan_discipline_score,
                                                summary,
                                            }),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("training-archive-notes-btn")
                                    .small()
                                    .outline()
                                    .label("归档训练笔记到复盘日志（标注「来自模拟训练」）")
                                    .on_click(cx.listener(move |panel, _, window, cx| {
                                        panel.dispatch_training(
                                            Ok(TrainingUiAction::ArchiveNotesToJournal {
                                                run_id: rid_archive.clone(),
                                                note_ids: Vec::new(),
                                            }),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    )
                    .when(!archived_notes.is_empty(), |col| {
                        let txt = SharedString::from(format!(
                            "已归档至复盘日志（共 {} 条）：{}",
                            archived_notes.len(),
                            archived_notes
                                .iter()
                                .map(|n| n.title.as_str())
                                .collect::<Vec<_>>()
                                .join("；")
                        ));
                        col.child(
                            div()
                                .id("training-archived-notes-banner")
                                .test_support()
                                .text_xs()
                                .text_color(cx.theme().success)
                                .child(txt.clone())
                                .test_text(txt),
                        )
                    }),
            )
            // 3. Explicit Reveal Section (`揭示`)
            .child(
                v_flex()
                    .id("training-reveal-section")
                    .test_support()
                    .w_full()
                    .p_3()
                    .gap_2p5()
                    .border_1()
                    .border_color(cx.theme().warning)
                    .rounded(cx.theme().radius)
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .child("揭示真实标的、日历区间与训练后走势"),
                    )
                    .child(if let Some(rev) = reveal_opt {
                        let rev_text = SharedString::from(format!(
                            "已揭晓真实标的：{}（{} · {}）｜ 训练真实日期：{} 至 {} ｜ 训练后跟踪截止：{}（后续 {} 根日线）",
                            rev.real_symbol,
                            rev.real_instrument_id,
                            rev.venue,
                            rev.train_start_date,
                            rev.train_end_date,
                            rev.reveal_end_date,
                            rev.post_finish_bars.len()
                        ));
                        let reveal_chart = KlineChartView::new(
                            "training-reveal-kline-chart",
                            rev.full_chart_slice.clone(),
                            convention,
                        )
                        .with_dataset_versions(Some(run.dataset_version), Some(run.dataset_version));
                        v_flex()
                            .id("training-revealed-content")
                            .test_support()
                            .w_full()
                            .gap_2()
                            .child(
                                div()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_xs()
                                    .font_medium()
                                    .child(rev_text.clone())
                                    .test_text(rev_text),
                            )
                            .child(reveal_chart.render_chart(cx))
                            .into_any_element()
                    } else {
                        v_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Chrome::caption())
                                    .child("隐藏模式保护生效中：点击下方「揭示」按钮前，界面不展示真实标的名称、真实日期及训练结束后的后续行情。"),
                            )
                            .child(
                                h_flex().child(
                                    Button::new("training-reveal-btn")
                                        .small()
                                        .primary()
                                        .label("揭示真实标的与后续行情")
                                        .on_click(cx.listener(move |panel, _, window, cx| {
                                            panel.dispatch_training(
                                                Ok(TrainingUiAction::RevealRun(
                                                    rid_reveal.clone(),
                                                )),
                                                window,
                                                cx,
                                            );
                                        })),
                                ),
                            )
                            .into_any_element()
                    }),
            )
            // 4. Finished-Only AI Review Section (`AI 复盘`, `S-45`, `M4-A-14`, `M4-A-16`)
            .child(
                v_flex()
                    .id("training-ai-review-card")
                    .test_support()
                    .w_full()
                    .p_3()
                    .gap_2p5()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .child("模拟训练证据型 AI 复盘（仅读取本次已完成模拟训练快照，与真实账户隔离）"),
                            )
                            .child(Tag::secondary().small().outline().child("不代替用户自主评分")),
                    )
                    .child(self.fields.row(&["form.trn_ai_question"]))
                    .child(
                        h_flex().child(
                            Button::new("training-ai-review-btn")
                                .small()
                                .primary()
                                .disabled(run.status != SimRunStatus::Finished)
                                .label("生成训练 AI 复盘")
                                .on_click(cx.listener(move |panel, _, window, cx| {
                                    let q = panel.fields.value("form.trn_ai_question", cx);
                                    panel.dispatch_training(
                                        Ok(TrainingUiAction::RequestAiReview {
                                            run_id: rid_ai.clone(),
                                            question: q,
                                        }),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                    )
                    .when_some(ai_err_opt, |col, err| {
                        let msg = SharedString::from(format!(
                            "AI 复盘未完成：{}（本地训练报告与自主评分不受影响，配置模型后可重试）",
                            err.message
                        ));
                        col.child(
                            div()
                                .id("training-ai-error-banner")
                                .test_support()
                                .p_2()
                                .border_1()
                                .border_color(cx.theme().danger)
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().secondary)
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(msg.clone())
                                .test_text(msg),
                        )
                    })
                    .when_some(ai_rep_opt, |col, ai_rep| {
                        let ver_label = match ai_rep.verification_status {
                            AiReportVerificationStatus::Verified => "宿主已核验数值与引用证据",
                            AiReportVerificationStatus::NeedsReview => "存在未核验数值，需人工复核",
                        };
                        let summary_text = SharedString::from(ai_rep.summary.clone());
                        col.child(
                            v_flex()
                                .id("training-ai-report-box")
                                .test_support()
                                .w_full()
                                .p_2p5()
                                .gap_2()
                                .border_1()
                                .border_color(cx.theme().info)
                                .rounded(cx.theme().radius)
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(Tag::info().small().child(ver_label))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(Chrome::caption())
                                                .child(format!("模型：{}", ai_rep.model_id)),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .child(summary_text.clone())
                                        .test_text(summary_text),
                                )
                                .when(!ai_rep.limitations.is_empty(), |box_col| {
                                    box_col.children(ai_rep.limitations.iter().map(|lim| {
                                        div()
                                            .text_xs()
                                            .text_color(Chrome::caption())
                                            .child(format!("局限说明：{lim}"))
                                    }))
                                })
                                .when(!ai_rep.verified_evidence.is_empty(), |box_col| {
                                    box_col
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_medium()
                                                .child("点击查看本次模拟训练引用证据："),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_1p5()
                                                .flex_wrap()
                                                .children(ai_rep.verified_evidence.iter().map(
                                                    |ev| {
                                                        let ev_id = ev.id.clone();
                                                        let r_id = run_id.clone();
                                                        Button::new(SharedString::from(format!(
                                                            "training-evidence-{}",
                                                            ev.id
                                                        )))
                                                        .xsmall()
                                                        .outline()
                                                        .label(format!("{} ({})", ev.label, ev.id))
                                                        .on_click(cx.listener(
                                                            move |panel, _, window, cx| {
                                                                panel.dispatch_training(
                                                                    Ok(TrainingUiAction::OpenTrainingEvidence {
                                                                        run_id: r_id.clone(),
                                                                        evidence_id: ev_id.clone(),
                                                                    }),
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        ))
                                                    },
                                                )),
                                        )
                                }),
                        )
                    })
                    .when_some(opened_ev_opt, |col, ev_val| {
                        let rendered = SharedString::from(format!(
                            "已打开模拟证据明细：{}",
                            serde_json::to_string(&ev_val).unwrap_or_default()
                        ));
                        col.child(
                            div()
                                .id("training-opened-evidence")
                                .test_support()
                                .p_2()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted)
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_xs()
                                .child(rendered.clone())
                                .test_text(rendered),
                        )
                    }),
            )
            .into_any_element()
    }
}

// ============================================================================
// Self-Contained Synthetic Market Fixture Generator (for M4-A-16 & M4-K03)
// ============================================================================

/// Writes a self-contained `market_data/` fixture directory consumable by
/// `AppService::sync_from_fixtures` / `AppHost::sync_from_fixtures`:
/// - `OKX:BTC/USDT`: `okx_days` continuous 24/7 UTC finalized daily candles (`"1"`),
///   supporting 1,600+ bar replay windows (`warmup=200`, `history=1000`, `training=340`, `reveal=60`).
/// - `NASDAQ:AAPL`: up to 62 continuous US regular trading days (`2024-09-03..2024-11-27`,
///   zero XNAS market holidays in window) cross-validated identically across Yahoo + EastMoney.
pub fn write_synthetic_training_fixtures(
    fixtures_root: &Path,
    okx_days: usize,
) -> Result<(), ErrorEnvelope> {
    let md_dir = fixtures_root.join("market_data");
    fs::create_dir_all(&md_dir)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_DIR_FAILED", e.to_string(), false))?;

    // 1. OKX BTC/USDT continuous 24/7 finalized daily bars starting 2020-01-01
    let start_okx = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid date");
    let mut okx_rows = Vec::with_capacity(okx_days);
    let mut prev_close = 28_000.0_f64;
    for idx in 0..okx_days {
        let day = start_okx
            .checked_add_days(Days::new(idx as u64))
            .expect("valid day");
        let ts_ms = Utc
            .from_utc_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
            .timestamp_millis();
        let wave = ((idx as f64) * 0.17).sin() * 240.0 + ((idx as f64) * 0.03).cos() * 410.0;
        let close = (28_000.0 + (idx as f64) * 12.5 + wave).max(1_000.0);
        let open = prev_close;
        let high = open.max(close) + 180.0 + ((idx % 7) as f64) * 15.0;
        let low = (open.min(close) - 160.0 - ((idx % 5) as f64) * 12.0).max(500.0);
        let vol = 1_200.0 + ((idx % 19) as f64) * 85.0;
        let quote_vol = vol * close;
        prev_close = close;
        okx_rows.push(json!([
            ts_ms.to_string(),
            format!("{open:.2}"),
            format!("{high:.2}"),
            format!("{low:.2}"),
            format!("{close:.2}"),
            format!("{vol:.2}"),
            format!("{quote_vol:.2}"),
            format!("{quote_vol:.2}"),
            "1"
        ]));
    }
    // OKX returns newest-first in raw payload (parser sorts chronologically)
    okx_rows.reverse();
    let okx_json = serde_json::to_string(&json!({
        "code": "0",
        "msg": "",
        "data": okx_rows,
    }))
    .unwrap();
    fs::write(md_dir.join("okx_btc_usdt_candles.json"), okx_json)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    // 2. Yahoo + EastMoney NASDAQ:AAPL continuous trading window (2024-09-03..2024-11-27 = 62 XNAS days)
    let mut eq_dates = Vec::new();
    let mut cur = NaiveDate::from_ymd_opt(2024, 9, 3).unwrap();
    let end_eq = NaiveDate::from_ymd_opt(2024, 11, 27).unwrap();
    while cur <= end_eq {
        if !matches!(cur.weekday(), Weekday::Sat | Weekday::Sun) {
            eq_dates.push(cur);
        }
        cur = cur.checked_add_days(Days::new(1)).unwrap();
    }
    let mut timestamps = Vec::with_capacity(eq_dates.len());
    let mut opens = Vec::with_capacity(eq_dates.len());
    let mut highs = Vec::with_capacity(eq_dates.len());
    let mut lows = Vec::with_capacity(eq_dates.len());
    let mut closes = Vec::with_capacity(eq_dates.len());
    let mut volumes = Vec::with_capacity(eq_dates.len());
    let mut em_klines = Vec::with_capacity(eq_dates.len());

    let mut eq_prev = 218.0_f64;
    for (i, d) in eq_dates.iter().enumerate() {
        let ts = Utc
            .from_utc_datetime(&d.and_hms_opt(14, 30, 0).unwrap())
            .timestamp();
        let o = ((eq_prev * 100.0).round()) / 100.0;
        let c = (((218.0 + (i as f64) * 0.45 + ((i as f64) * 0.35).sin() * 3.2) * 100.0).round())
            / 100.0;
        let h = ((o.max(c) + 2.25) * 100.0).round() / 100.0;
        let l = ((o.min(c) - 2.00) * 100.0).round() / 100.0;
        let v = 45_000_000_i64 + (i as i64) * 150_000;
        eq_prev = c;
        timestamps.push(ts);
        opens.push(o);
        highs.push(h);
        lows.push(l);
        closes.push(c);
        volumes.push(v);
        em_klines.push(format!(
            "{d},{o:.2},{c:.2},{h:.2},{l:.2},{v},9800000000.00,2.10,0.50,1.10,0.85"
        ));
    }
    let yahoo_json = serde_json::to_string(&json!({
        "chart": {
            "result": [{
                "meta": {
                    "currency": "USD",
                    "symbol": "AAPL",
                    "exchangeName": "NMS",
                    "instrumentType": "EQUITY"
                },
                "timestamp": timestamps,
                "events": {},
                "indicators": {
                    "quote": [{
                        "open": opens,
                        "high": highs,
                        "low": lows,
                        "close": closes.clone(),
                        "volume": volumes
                    }],
                    "adjclose": [{ "adjclose": closes }]
                }
            }],
            "error": null
        }
    }))
    .unwrap();
    fs::write(md_dir.join("yahoo_aapl_2020_split.json"), yahoo_json)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    let em_json = serde_json::to_string(&json!({
        "rc": 0,
        "rt": 17,
        "svr": 182994511,
        "lt": 1,
        "full": 1,
        "data": {
            "code": "AAPL",
            "market": 105,
            "name": "苹果",
            "decimal": 2,
            "dktotal": em_klines.len(),
            "preKPrice": 218.0,
            "klines": em_klines
        }
    }))
    .unwrap();
    fs::write(md_dir.join("eastmoney_aapl_2020_raw.json"), em_json)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    // 3. Minimal FX fixtures required by `sync_from_fixtures`
    fs::write(
        md_dir.join("coinbase_usdt_usd_candles.json"),
        r#"[[1735689600,0.9998,1.0001,0.9999,1.0000,150000000.0]]"#,
    )
    .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;
    fs::write(
        md_dir.join("bitstamp_usdc_usd_ohlc.json"),
        r#"{"data":{"pair":"USDC/USD","ohlc":[{"timestamp":"1735689600","open":"1.0000","high":"1.0001","low":"0.9999","close":"1.0000","volume":"9500000.00"}]}}"#,
    )
    .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;
    fs::write(
        md_dir.join("frankfurter_usd_fx.json"),
        r#"{"amount":1.0,"base":"USD","start_date":"2025-01-02","end_date":"2025-01-02","rates":{"2025-01-02":{"CNY":7.265,"EUR":0.958,"HKD":7.7812}}}"#,
    )
    .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;
    fs::write(md_dir.join("ledger_fx_synthetic.json"), "[]")
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::PathBuf;

    use delta_app::{
        apply_process_quiet_guards, count_visible_windows_for_current_process, resolve_app_paths,
        AppHost, AppService, BuildChannel, ModelConnection, ModelProtocol,
    };
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::TestAppContext;

    use crate::controller::WorkbenchUpdated;
    use crate::workbench::DeltaWorkbench;

    fn temp_training_test_dir(tag: &str) -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let base = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!(
            "delta_m4_desktop_{tag}_{}_{}_{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn click_id(cx: &mut gpui_kit::VisualTestContext, id: &str) {
        let id_str = gpui_kit::SharedString::from(id.to_string());
        let position = cx.update(move |window, cx| {
            window.render_frame(cx);
            let bounds = window.find(id_str.clone()).bounds();
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "control '{id_str}' has no rendered bounds"
            );
            bounds.center()
        });
        cx.simulate_click(position, gpui_kit::Modifiers::default());
        cx.run_until_parked();
    }

    fn has_rendered_id(cx: &mut gpui_kit::VisualTestContext, id: &str) -> bool {
        let id_str = gpui_kit::SharedString::from(id.to_string());
        cx.update(move |window, cx| {
            window.render_frame(cx);
            let bounds = window.find(id_str.clone()).bounds();
            bounds.size.width > px(0.) && bounds.size.height > px(0.)
        })
    }

    fn chat_tc_sse(call_id: &str, tool_name: &str, args_json: &str) -> String {
        let escaped_args = serde_json::to_string(args_json).unwrap();
        format!(
            concat!(
                "data: {{\"id\":\"chatcmpl_tc\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{call_id}\",\"type\":\"function\",\"function\":{{\"name\":\"{tool_name}\",\"arguments\":{escaped_args}}}}}]}},\"finish_reason\":null}}]}}\n\n",
                "data: {{\"id\":\"chatcmpl_tc\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}],\"usage\":{{\"prompt_tokens\":25,\"completion_tokens\":10,\"total_tokens\":35}}}}\n\n",
                "data: [DONE]\n\n"
            ),
            call_id = call_id,
            tool_name = tool_name,
            escaped_args = escaped_args
        )
    }

    fn chat_final_sse(text_json: &str) -> String {
        let escaped_text = serde_json::to_string(text_json).unwrap();
        format!(
            concat!(
                "data: {{\"id\":\"chatcmpl_txt\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{escaped_text}}},\"finish_reason\":null}}]}}\n\n",
                "data: {{\"id\":\"chatcmpl_txt\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":35,\"completion_tokens\":20,\"total_tokens\":55}}}}\n\n",
                "data: [DONE]\n\n"
            ),
            escaped_text = escaped_text
        )
    }

    fn spawn_scripted_sse_server(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut queue: VecDeque<String> = responses.into();
        std::thread::spawn(move || {
            while let Some(sse_body) = queue.pop_front() {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut req_buf = vec![0u8; 32_768];
                let mut total = 0usize;
                loop {
                    match stream.read(&mut req_buf[total..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            total += n;
                            if total >= req_buf.len() {
                                break;
                            }
                            if let Ok(text) = std::str::from_utf8(&req_buf[..total]) {
                                if let Some(hdr_end) = text.find("\r\n\r\n") {
                                    let content_len = text[..hdr_end]
                                        .lines()
                                        .find_map(|l| {
                                            let lower = l.to_ascii_lowercase();
                                            lower
                                                .strip_prefix("content-length:")
                                                .and_then(|v| v.trim().parse::<usize>().ok())
                                        })
                                        .unwrap_or(0);
                                    if total >= hdr_end + 4 + content_len {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let http_resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                let _ = stream.write_all(http_resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{}/v1", addr.port())
    }

    #[cfg(debug_assertions)]
    #[test]
    fn test_m4_k03_bench_rejects_non_release_build() {
        apply_process_quiet_guards();
        let err = crate::ui_verification::run_m4_k03_next_bar_bench(None).unwrap_err();
        assert_eq!(err.code, "RELEASE_BUILD_REQUIRED");
        assert_eq!(count_visible_windows_for_current_process(), 0);
    }

    #[gpui_kit::test]
    async fn test_m4_a16_headless_training_workflow_shortcuts_hidden_masking_and_ai_review(
        cx: &mut TestAppContext,
    ) {
        apply_process_quiet_guards();
        let tmp = temp_training_test_dir("m4_a16");
        let fix_dir = tmp.join("fixtures");
        write_synthetic_training_fixtures(&fix_dir, 400).unwrap();

        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        svc.sync_from_fixtures(&fix_dir).unwrap();
        let host = AppHost::from_service(svc);

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
        // Tall headless viewport so every training control has non-zero layout bounds.
        cx.simulate_resize(gpui_kit::size(px(1600.), px(3400.)));

        let (controller, center, state) = cx.update(|_, cx| {
            let view = workbench.read(cx);
            (
                view.controller.clone(),
                view.center_panel().clone(),
                Arc::clone(view.shared_state()),
            )
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;

        let initial_ledger_rev = state.lock().unwrap().scope.ledger_revision;
        assert_eq!(initial_ledger_rev, 1);

        // 1. Navigate to `"训练"` (`nav.training`) and open New Training Dialog
        click_id(cx, "nav-btn-nav-training");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(state.lock().unwrap().layout.active_nav, "nav.training");
        assert!(has_rendered_id(cx, "training-simulated-badge"));

        click_id(cx, "training-open-new-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state.lock().unwrap().training.mode,
            TrainingViewMode::NewDialog
        );

        // Save a custom fee profile version and verify 3 built-in + 1 custom exist
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel
                    .fields
                    .set("form.trn_fee_key", "ui_custom_bps", window, cx);
                panel
                    .fields
                    .set("form.trn_fee_name", "UI 自定义万五假设", window, cx);
                panel.fields.set("form.trn_fee_rate", "0.0005", window, cx);
                panel.fields.set("form.trn_fee_min", "1.0", window, cx);
                panel
                    .fields
                    .set("form.trn_fee_slippage_bps", "2", window, cx);
            });
        });
        click_id(cx, "training-save-fee-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(state.lock().unwrap().training.fee_profiles.len(), 4);

        // Select US Equity category (`NASDAQ:AAPL` has 62 trading bars in synthetic fixture)
        click_id(cx, "cat-eq");
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set("form.trn_warmup_bars", "10", window, cx);
                panel.fields.set("form.trn_history_bars", "15", window, cx);
                panel.fields.set("form.trn_training_bars", "12", window, cx);
                panel.fields.set("form.trn_reveal_bars", "10", window, cx);
                panel.fields.set("form.trn_seed", "20260416", window, cx);
            });
        });

        // Draw pre-start candidate (`"抽样预览（可换一个）"`), redraw (`"换一个"`), then start!
        click_id(cx, "training-create-draft-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                ctl.is_idle()
                    && ctl
                        .state
                        .lock()
                        .unwrap()
                        .training
                        .pending_prestart_run
                        .is_some()
            })
            .await;
        {
            let g = state.lock().unwrap();
            let pre = g.training.pending_prestart_run.as_ref().unwrap();
            assert_eq!(pre.status, SimRunStatus::Preparing);
            assert_eq!(pre.redraw_count, 0);
            assert!(!pre.display_symbol.contains("AAPL"));
            assert!(!pre.display_symbol.contains("NASDAQ"));
        }

        click_id(cx, "training-redraw-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .pending_prestart_run
                .as_ref()
                .unwrap()
                .redraw_count,
            1
        );

        click_id(cx, "training-start-pending-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                ctl.is_idle()
                    && ctl.state.lock().unwrap().training.mode == TrainingViewMode::Workspace
            })
            .await;

        // 2. Verify permanent `"模拟训练"` badge, mid-run AI disabled hint, and ZERO real symbol/date leakage!
        assert!(has_rendered_id(cx, "training-simulated-badge"));
        assert!(has_rendered_id(cx, "training-simulated-banner"));
        assert!(has_rendered_id(cx, "training-ai-disabled-hint"));
        assert!(has_rendered_id(cx, "training-kline-chart"));
        assert!(has_rendered_id(cx, "training-order-ticket"));
        let run_id = {
            let g = state.lock().unwrap();
            let ws = g.training.workspace.as_ref().unwrap();
            assert!(ws.run.is_identity_masked);
            assert_eq!(ws.run.current_step_index, 0);
            let ws_json = serde_json::to_string(ws).unwrap();
            assert!(!ws_json.contains("AAPL"));
            assert!(!ws_json.contains("NASDAQ"));
            assert!(!ws_json.contains("2024-09"));
            assert!(!ws_json.contains("2024-10"));
            assert!(!ws_json.contains("2024-11"));

            // Also verify KlineChartView masked badge & crosshair readout never leak raw `HIDDEN:INSTRUMENT_A` or `0001-`
            let chart = KlineChartView::new(
                "chk-chart",
                ws.chart_slice.clone(),
                g.layout.color_convention,
            )
            .with_masked_training_labels(ws.run.display_symbol.clone(), ws.bar_labels.clone())
            .with_crosshair(Some(0));
            let badge = chart.venue_pair_badge_text();
            let readout = chart.crosshair_readout_text();
            assert!(badge.contains(SIMULATED_TRAINING_BADGE));
            assert!(!badge.contains("AAPL"));
            assert!(!badge.contains("HIDDEN:INSTRUMENT_A"));
            assert!(readout.contains("第 1 根"));
            assert!(!readout.contains("0001-"));
            assert!(!readout.contains("2024-"));

            ws.run.run_id.clone()
        };

        // 3. Real-Window Auto-Focus, Chart Pointer/Zoom, Anti-Misclick & Shortcut Separation (`S-46`, `NFR-09`, `M4-A-16`):
        // Right after `"training-start-pending-btn"`, `training_workspace_focus` is auto-focused (NO manual `fh.focus`!):
        // pressing `N` advances step `0 -> 1` naturally.
        cx.simulate_keystrokes("n");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .run
                .current_step_index,
            1
        );

        // Prepare a valid Market Buy order draft (`qty = 10`) and click `"training-preview-order-btn"` inside Order Ticket:
        // focus returns to workspace stepping, so pressing `Right` (`→`) advances `1 -> 2` with ZERO order submitted!
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set("form.trn_order_qty", "10", window, cx);
            });
        });
        click_id(cx, "training-preview-order-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| {
                ctl.is_idle() && ctl.state.lock().unwrap().training.order_preview.is_some()
            })
            .await;
        assert!(state
            .lock()
            .unwrap()
            .training
            .order_preview
            .as_ref()
            .unwrap()
            .rejection
            .is_none());

        cx.simulate_keystrokes("right");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        {
            let g = state.lock().unwrap();
            let ws = g.training.workspace.as_ref().unwrap();
            assert_eq!(ws.run.current_step_index, 2);
            assert!(ws.all_orders.is_empty());
        }

        // Hover pointer + wheel zoom on `"training-kline-chart"`: updates crosshair & zooms viewport while preserving crosshair!
        let chart_bounds = cx.update(|window, cx| {
            window.render_frame(cx);
            center
                .read(cx)
                .training_plot_bounds
                .lock()
                .unwrap()
                .expect("training_plot_bounds must be populated after render_frame")
        });
        cx.simulate_mouse_move(chart_bounds.center(), None, gpui_kit::Modifiers::default());
        let old_vis_len = {
            let g = state.lock().unwrap();
            let vp = g.training.chart_viewport.as_ref().unwrap();
            assert!(vp.crosshair_index.is_some());
            assert!(vp.crosshair_price.is_some());
            vp.visible_len()
        };
        cx.simulate_event(gpui_kit::ScrollEvent {
            position: chart_bounds.center(),
            delta: gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(240.))),
            modifiers: gpui_kit::Modifiers::default(),
            touch_phase: gpui_kit::TouchPhase::Moved,
            source: gpui_kit::ScrollSource::Pointer,
        });
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        {
            let g = state.lock().unwrap();
            let vp = g.training.chart_viewport.as_ref().unwrap();
            assert!(vp.visible_len() < old_vis_len);
            assert!(vp.crosshair_index.is_some());
        }

        // Press `O` shortcut -> focuses `form.trn_order_qty`; while focused, `N`, `Right`, and `Enter` NEVER submit an order or step!
        cx.simulate_keystrokes("o");
        cx.run_until_parked();
        assert!(cx.update(|window, cx| {
            center
                .read(cx)
                .is_any_training_text_input_focused(window, cx)
        }));
        cx.simulate_keystrokes("n");
        cx.run_until_parked();
        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        {
            let g = state.lock().unwrap();
            let ws = g.training.workspace.as_ref().unwrap();
            assert_eq!(ws.run.current_step_index, 2);
            assert!(ws.all_orders.is_empty());
        }
        // Reset `"form.trn_order_qty"` back to `"10"` (since `"n"` went to the focused input rather than stepping)
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set("form.trn_order_qty", "10", window, cx);
            });
        });

        // Press `Escape` while focused in `form.trn_order_qty` -> blurs input back to `training_workspace_focus` AND clears crosshair!
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.update(|window, cx| {
            !center
                .read(cx)
                .is_any_training_text_input_focused(window, cx)
                && center
                    .read(cx)
                    .training_workspace_focus()
                    .is_focused(window)
        }));
        assert!(state
            .lock()
            .unwrap()
            .training
            .chart_viewport
            .as_ref()
            .unwrap()
            .crosshair_index
            .is_none());

        // Press `O` then `Ctrl+Enter` (`ctrl-enter`) while focused in Order Ticket input -> submits order & restores workspace focus!
        cx.simulate_keystrokes("o");
        cx.run_until_parked();
        cx.simulate_keystrokes("ctrl-enter");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .all_orders
                .len(),
            1
        );

        // Immediately press `N` (NO manual `fh.focus`!) -> Market Buy fills on step 3!
        cx.simulate_keystrokes("n");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .fills
                .len(),
            1
        );
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .position
                .as_ref()
                .unwrap()
                .quantity,
            Decimal::from(10)
        );

        // Press `P` shortcut -> pauses active run and returns to Training List; click `"继续"` -> resumes & auto-focuses workspace!
        cx.simulate_keystrokes("p");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(state.lock().unwrap().training.mode, TrainingViewMode::List);
        assert_eq!(
            state.lock().unwrap().training.runs[0].status,
            SimRunStatus::Paused
        );
        click_id(cx, &format!("training-resume-{run_id}"));
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state.lock().unwrap().training.mode,
            TrainingViewMode::Workspace
        );

        // Place Market Sell (`qty = 10`) via submit button and immediately advance with `Right` (NO manual `fh.focus`!) -> closes round-trip trade!
        click_id(cx, "ord-side-sell");
        click_id(cx, "training-submit-order-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .all_orders
                .len(),
            2
        );
        cx.simulate_keystrokes("right");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .fills
                .len(),
            2
        );
        assert!(state
            .lock()
            .unwrap()
            .training
            .workspace
            .as_ref()
            .unwrap()
            .position
            .is_none());

        // 4. Order rejection / failure honesty: off-tick limit price (`100 + tick_size / 10`)
        // shows explicit reason + nearest valid tick bounds and is NOT marked accepted!
        let off_tick_price = {
            let g = state.lock().unwrap();
            let tick = g.training.workspace.as_ref().unwrap().account.tick_size;
            to_canonical_text(Decimal::from(100) + (tick / Decimal::from(10)))
        };
        click_id(cx, "ord-side-buy");
        click_id(cx, "ord-kind-lmt");
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel
                    .fields
                    .set("form.trn_order_limit", off_tick_price, window, cx);
                panel.fields.set("form.trn_order_qty", "10", window, cx);
            });
        });
        click_id(cx, "training-submit-order-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        {
            let g = state.lock().unwrap();
            assert!(g.ui_error.is_some());
            let rej = g.training.last_order_rejection.as_ref().unwrap();
            assert_eq!(rej.code, SimRejectCode::InvalidTickSize);
            assert!(rej.nearest_lower.is_some() && rej.nearest_upper.is_some());
            assert!(g
                .training
                .workspace
                .as_ref()
                .unwrap()
                .open_orders
                .is_empty());
        }
        assert!(has_rendered_id(cx, "training-order-rejection-box"));

        // 5. Add a Training Note using `J` shortcut -> focuses `form.trn_note_rationale`, then saves and restores workspace focus!
        cx.simulate_keystrokes("j");
        cx.run_until_parked();
        assert!(cx.update(|window, cx| {
            center
                .read(cx)
                .fields
                .get("form.trn_note_rationale")
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        }));
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set(
                    "form.trn_note_rationale",
                    "均线顺势试单并按计划平仓离场",
                    window,
                    cx,
                );
                panel
                    .fields
                    .set("form.trn_note_emotion", "冷静", window, cx);
            });
        });
        click_id(cx, "training-save-note-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .workspace
                .as_ref()
                .unwrap()
                .notes
                .len(),
            1
        );

        // 6. Finish Training -> Post-Finish Disclosure Sequence (`训练报告` -> `自主评分 & 归档` -> `揭示` -> `AI 复盘`)
        let (first_order_id, first_fill_id, first_note_id) = {
            let g = state.lock().unwrap();
            let ws = g.training.workspace.as_ref().unwrap();
            (
                ws.all_orders[0].order_id.clone(),
                ws.fills[0].fill_id.clone(),
                ws.notes[0].note_id.clone(),
            )
        };

        click_id(cx, "training-finish-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state.lock().unwrap().training.mode,
            TrainingViewMode::Report
        );
        assert!(has_rendered_id(cx, "training-report-view"));
        assert!(has_rendered_id(cx, "training-report-metrics-card"));
        assert!(has_rendered_id(cx, "training-reveal-btn"));

        // Before `"揭示"`, ZERO real symbol/dates leak in report, and closed trade group span matches chart numbering (`第 18 根 -> 第 19 根`)!
        let (exact_total_pnl, closed_grp_id) = {
            let g = state.lock().unwrap();
            assert!(g.training.reveal.is_none());
            let rep_dto = g.training.report.as_ref().unwrap();
            assert!(rep_dto.run.is_identity_masked);
            assert_eq!(rep_dto.report.closed_trade_groups.len(), 1);
            let grp = &rep_dto.report.closed_trade_groups[0];
            assert_eq!(
                rep_dto
                    .equity_bar_labels
                    .get(grp.entry_bar_index)
                    .map(String::as_str),
                Some("第 18 根")
            );
            assert_eq!(
                rep_dto
                    .equity_bar_labels
                    .get(grp.exit_bar_index)
                    .map(String::as_str),
                Some("第 19 根")
            );
            let rep_json = serde_json::to_string(rep_dto).unwrap();
            assert!(!rep_json.contains("AAPL"));
            assert!(!rep_json.contains("NASDAQ"));
            assert!(!rep_json.contains("2024-"));
            (
                to_canonical_text(rep_dto.report.total_pnl),
                grp.group_id.clone(),
            )
        };
        let rendered_grp_label = cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .find(SharedString::from(format!(
                    "training-closed-grp-{closed_grp_id}"
                )))
                .label()
                .unwrap_or_default()
                .to_string()
        });
        assert!(
            rendered_grp_label.contains("第 18 根 -> 第 19 根"),
            "unexpected closed group label: {rendered_grp_label}"
        );
        assert!(!rendered_grp_label.contains("第 3 根 -> 第 4 根"));

        // Save user autonomous self-evaluation & archive notes to journal (`"来自模拟训练"`)
        cx.update(|window, cx| {
            center.update(cx, |panel, cx| {
                panel.fields.set(
                    "form.trn_self_eval_summary",
                    "按信号入场、按纪律离场，未受盘中噪音干扰",
                    window,
                    cx,
                );
            });
        });
        click_id(cx, "training-save-self-eval-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert!(state
            .lock()
            .unwrap()
            .training
            .report
            .as_ref()
            .unwrap()
            .self_evaluation
            .is_some());
        assert!(has_rendered_id(cx, "training-saved-self-score"));

        click_id(cx, "training-archive-notes-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert!(!state
            .lock()
            .unwrap()
            .training
            .last_archived_notes
            .is_empty());
        {
            let notes = host.list_notes(false).unwrap();
            assert_eq!(notes.len(), 1);
            assert!(
                notes[0].tags.iter().any(|t| t == SIMULATED_TRAINING_BADGE)
                    || notes[0].body_markdown.contains("来自模拟训练")
            );
            // Real portfolio ledger revision remains untouched!
            let scope_desc = host
                .query_scope_describe(
                    AnalysisScope::all_accounts(
                        NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                        "USD",
                    ),
                    "chk-ledger",
                    host.current_generation(),
                )
                .unwrap();
            assert_eq!(scope_desc.scope.ledger_revision, initial_ledger_rev);
        }

        // Trigger background AI failure (`model_not_configured`) -> shows error banner & NOT marked successful!
        click_id(cx, "training-ai-review-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .training_ai_error
                .as_ref()
                .unwrap()
                .code,
            "model_not_configured"
        );
        assert!(state.lock().unwrap().training.training_ai_report.is_none());
        assert!(has_rendered_id(cx, "training-ai-error-banner"));

        // Explicit `"揭示"` (`reveal_training_run`) -> real symbol, real dates & 10 post-finish bars appear!
        click_id(cx, "training-reveal-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        {
            let g = state.lock().unwrap();
            let rev = g.training.reveal.as_ref().unwrap();
            assert_eq!(rev.real_instrument_id, "NASDAQ:AAPL");
            assert_eq!(rev.post_finish_bars.len(), 10);
        }
        assert!(has_rendered_id(cx, "training-revealed-content"));
        assert!(has_rendered_id(cx, "training-reveal-kline-chart"));

        // Configure loopback AI coach, run post-finish `"AI 复盘"`, and open simulated citation!
        let draft_json = json!({
            "summary": format!(
                "本轮模拟训练完成 1 组完整交易，总盈亏 {} USD（证据引用：{}、{}、{}）。",
                exact_total_pnl, first_order_id, first_fill_id, first_note_id
            ),
            "calculations": [
                ["total_pnl", exact_total_pnl, "USD"],
                ["closed_trade_count", "1", ""]
            ],
            "citations": [first_order_id, first_fill_id, first_note_id],
            "limitations": ["日线盘中路径按固定规则近似，单轮样本量有限。"]
        })
        .to_string();
        let base_url = spawn_scripted_sse_server(vec![
            chat_tc_sse("call_1", "scope.describe", "{}"),
            chat_tc_sse("call_2", "training.run_summary", "{}"),
            chat_tc_sse("call_3", "training.orders_and_fills", "{}"),
            chat_tc_sse("call_4", "training.report", "{}"),
            chat_tc_sse("call_5", "training.notes", "{}"),
            chat_tc_sse("call_6", "training.revealed_bars", "{}"),
            chat_final_sse(&draft_json),
        ]);
        {
            let mut g = state.lock().unwrap();
            g.runtime
                .configure_model_connection(
                    ModelConnection::loopback(
                        "conn-trn-ui-a16",
                        ModelProtocol::OpenAiChatCompletions,
                        base_url,
                        "gpt-5-training-ui",
                    ),
                    None,
                )
                .unwrap();
        }

        click_id(cx, "training-ai-review-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        {
            let g = state.lock().unwrap();
            assert!(
                g.training.training_ai_error.is_none(),
                "unexpected AI review error: {:?}",
                g.training.training_ai_error
            );
            let ai_rep = g
                .training
                .training_ai_report
                .as_ref()
                .expect("expected training_ai_report");
            assert_eq!(
                ai_rep.verification_status,
                AiReportVerificationStatus::Verified,
                "blocked={:?}, unmatched={:?}",
                ai_rep.blocked_citations,
                ai_rep.unmatched_numbers
            );
        }
        // User autonomous score and AI review box are both rendered and visually separated!
        assert!(has_rendered_id(cx, "training-saved-self-score"));
        assert!(has_rendered_id(cx, "training-ai-report-box"));

        // Click simulated evidence citation (`SIM-ORD-*`) and verify evidence viewer opens!
        let ev_btn_id = format!("training-evidence-{first_order_id}");
        click_id(cx, &ev_btn_id);
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert!(state
            .lock()
            .unwrap()
            .training
            .opened_training_evidence
            .is_some());
        assert!(has_rendered_id(cx, "training-opened-evidence"));
        assert_eq!(
            state
                .lock()
                .unwrap()
                .training
                .opened_training_evidence
                .as_ref()
                .unwrap()["simulated_training_badge"],
            SIMULATED_TRAINING_BADGE
        );

        // 7. Return to list, inspect Delete Preview counts, and confirm delete (preserving archived journal note!)
        click_id(cx, "training-report-back-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert_eq!(state.lock().unwrap().training.mode, TrainingViewMode::List);
        let del_prev_btn = format!("training-del-preview-{run_id}");
        click_id(cx, &del_prev_btn);
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert!(state.lock().unwrap().training.delete_preview.is_some());
        assert!(has_rendered_id(cx, "training-delete-preview-card"));
        {
            let g = state.lock().unwrap();
            let dp = g.training.delete_preview.as_ref().unwrap();
            assert_eq!(dp.fill_count, 2);
            assert_eq!(dp.note_count, 1);
            assert_eq!(dp.archived_journal_note_count, 1);
        }
        click_id(cx, "training-confirm-delete-btn");
        controller
            .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
            .await;
        assert!(state.lock().unwrap().training.runs.is_empty());
        assert_eq!(host.list_notes(false).unwrap().len(), 1);
        assert_eq!(
            state.lock().unwrap().scope.ledger_revision,
            initial_ledger_rev
        );
        assert_eq!(count_visible_windows_for_current_process(), 0);

        cx.update(|window, _| window.remove_window());
        host.shutdown().unwrap();
        fs::remove_dir_all(&tmp).unwrap();
    }
}
