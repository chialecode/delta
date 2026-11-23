//! Public Training DTOs & Request Contracts (M4-W01..M4-W05, S-37..S-55).

use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{
    AttachedBracket, BarPathStage, BarRecord, FeeChargingTier, FeeCurrencyMode, FeeMarketCategory,
    FeeProfileVersion, FeeRoundingMode, FillRule, OrderCostPreview, OrderGroupRole, OrderRejection,
    OrderSide, OrderStatus, OrderSubmitKind, SimEvent, SimOrderType, SimRejectCode, SlippageSpec,
    TimeInForce, Timeframe, TrainingReport,
};
use delta_data::{FeeProfileVersionRecord, NoteDto, SimRunStatus, BUILTIN_FEE_ZERO_COST_ID};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::kline::{KlineViewport, KlineWindowSliceDto};
use crate::service::ErrorEnvelope;

pub const SIMULATED_TRAINING_BADGE: &str = "模拟训练";
pub const HIDDEN_INSTRUMENT_ID: &str = "HIDDEN:INSTRUMENT_A";
pub const HIDDEN_VENUE_LABEL: &str = "HIDDEN";

// ============================================================================
// 1. Public Training DTOs & Request Contracts (`M4-W01`..`M4-W05`)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrainingMarketCategory {
    #[default]
    Any,
    UsEquityEtf,
    CryptoSpot,
}

impl TrainingMarketCategory {
    pub fn as_pool_str(self) -> &'static str {
        match self {
            Self::Any => "all",
            Self::UsEquityEtf => "us_equity",
            Self::CryptoSpot => "crypto_spot",
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self, ErrorEnvelope> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "all" | "any" => Ok(Self::Any),
            "us_equity" | "us_equity_etf" | "equity" | "etf" => Ok(Self::UsEquityEtf),
            "crypto_spot" | "crypto" => Ok(Self::CryptoSpot),
            other => Err(ErrorEnvelope::new(
                "INVALID_MARKET_CATEGORY",
                format!(
                    "unsupported training market category '{other}' (expected any, us_equity_etf, or crypto_spot)"
                ),
                false,
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTrainingRunRequest {
    pub run_id: Option<String>,
    pub market_category: TrainingMarketCategory,
    pub instrument_id: Option<String>,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub warmup_bars: usize,
    pub history_bars: usize,
    pub training_bars: usize,
    pub reveal_bars: usize,
    pub seed: Option<u64>,
    pub dataset_version: Option<i64>,
    pub initial_cash: Decimal,
    pub fee_profile_version_id: String,
    pub hidden_mode: bool,
    pub create_and_start: bool,
    /// Frozen bar timeframe (`S-50`): crypto `1d`/`4h`/`1h`, US equity/ETF `1d` only.
    #[serde(default)]
    pub timeframe: Timeframe,
    /// Fund the run from the practice account of its quote currency (`S-52`). When false the
    /// run has independent capital (`initial_cash`) like M4 runs.
    #[serde(default)]
    pub use_practice_account: bool,
    /// Allocation from the practice account; `None` = all available funds.
    #[serde(default)]
    pub allocation: Option<Decimal>,
}

impl Default for CreateTrainingRunRequest {
    fn default() -> Self {
        Self {
            run_id: None,
            market_category: TrainingMarketCategory::Any,
            instrument_id: None,
            start_date: None,
            end_date: None,
            warmup_bars: 200,
            history_bars: 120,
            training_bars: 250,
            reveal_bars: 60,
            seed: None,
            dataset_version: None,
            initial_cash: Decimal::from(100_000),
            fee_profile_version_id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
            hidden_mode: true,
            create_and_start: false,
            timeframe: Timeframe::D1,
            use_practice_account: false,
            allocation: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeProfileDraftDto {
    pub profile_key: String,
    pub name: String,
    pub market_category: FeeMarketCategory,
    pub venue: Option<String>,
    pub notional_rate: Decimal,
    pub per_unit_fee: Decimal,
    pub per_order_fixed_fee: Decimal,
    pub min_fee_per_order: Option<Decimal>,
    pub max_fee_per_order: Option<Decimal>,
    pub fee_currency_mode: FeeCurrencyMode,
    pub rounding_mode: FeeRoundingMode,
    pub rounding_scale: u32,
    pub charging_tier: FeeChargingTier,
    pub slippage: SlippageSpec,
    pub half_spread_bps: Decimal,
    pub is_scenario_assumption: bool,
    pub official_source_url: Option<String>,
    pub official_retrieved_at: Option<NaiveDate>,
    pub assumption_note: String,
}

impl Default for FeeProfileDraftDto {
    fn default() -> Self {
        Self {
            profile_key: "custom_profile".to_string(),
            name: "自定义费用模板".to_string(),
            market_category: FeeMarketCategory::Any,
            venue: None,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::ZERO,
            per_order_fixed_fee: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage: SlippageSpec::Zero,
            half_spread_bps: Decimal::ZERO,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note: "用户自定义场景假设费用配置".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeProfileDto {
    pub record: FeeProfileVersionRecord,
    pub engine_profile: FeeProfileVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrubbedRedrawEntryDto {
    pub redraw_index: i64,
    pub previous_instrument_id: Option<String>,
    pub previous_train_start_at: Option<String>,
    pub next_instrument_id: Option<String>,
    pub next_train_start_at: Option<String>,
    pub redrawn_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingRunSummaryDto {
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub status: SimRunStatus,
    pub hidden_mode: bool,
    pub revealed: bool,
    pub is_identity_masked: bool,
    pub simulated_training_badge: String,
    pub market_category: String,
    pub display_symbol: String,
    pub instrument_id: String,
    pub venue: String,
    pub symbol: String,
    pub seed: u64,
    pub candidate_pool_hash: String,
    pub redraw_count: i64,
    pub redraw_history: Vec<ScrubbedRedrawEntryDto>,
    pub dataset_version: i64,
    pub engine_version: String,
    pub fee_profile_version_id: String,
    pub fee_profile_name: String,
    pub quote_currency: String,
    pub initial_cash: Decimal,
    pub current_step_index: i64,
    pub total_steps: i64,
    pub visible_bar_count: usize,
    pub current_bar_label: String,
    pub visible_until: Option<DateTime<Utc>>,
    pub history_start_date: Option<NaiveDate>,
    pub train_start_date: Option<NaiveDate>,
    pub train_end_date: Option<NaiveDate>,
    pub latest_seq: i64,
    pub latest_state_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    /// `S-50`: frozen timeframe of the run.
    #[serde(default = "default_tf_label")]
    pub timeframe: String,
    /// `S-52`: practice account funding the run; `None` = independent capital (M4 runs).
    #[serde(default)]
    pub practice_account_id: Option<String>,
    #[serde(default)]
    pub fill_rule_version: Option<String>,
    /// Quick fills, modifies and undos of the run; filled only by the post-run AI tool
    /// (`S-55`), absent elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_counts: Option<TrainingOperationCountsDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingOperationCountsDto {
    pub quick_fill_count: u64,
    pub modify_count: u64,
    pub undo_count: u64,
}

fn default_tf_label() -> String {
    "1d".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingAccountSummaryDto {
    pub quote_currency: String,
    pub initial_cash: Decimal,
    pub cash_balance: Decimal,
    pub reserved_cash: Decimal,
    pub available_cash: Decimal,
    pub position_market_value: Decimal,
    pub total_equity: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub dividend_income: Decimal,
    pub total_fees_quote: Decimal,
    pub lot_size: Decimal,
    pub tick_size: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingPositionDto {
    pub instrument_id: String,
    pub display_symbol: String,
    pub quantity: Decimal,
    pub available_quantity: Decimal,
    pub reserved_quantity: Decimal,
    pub total_cost_quote: Decimal,
    pub avg_unit_cost: Decimal,
    pub mark_close_price: Decimal,
    pub market_value_quote: Decimal,
    pub unrealized_pnl: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingOrderRequestDto {
    pub kind: OrderSubmitKind,
    pub quantity: Decimal,
    pub tif: TimeInForce,
    #[serde(default)]
    pub priority: Option<i32>,
    /// `visible_close` = quick order filled now at the visible close (`S-51` ①, m5 runs only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_rule: Option<FillRule>,
}

impl TrainingOrderRequestDto {
    /// Quick buy/sell at the current visible close (`S-51` ①).
    pub fn quick(side: OrderSide, quantity: Decimal) -> Self {
        Self {
            kind: OrderSubmitKind::Single {
                side,
                order_type: SimOrderType::Market,
                attached_bracket: None,
            },
            quantity,
            tif: TimeInForce::Day,
            priority: None,
            fill_rule: Some(FillRule::VisibleClose),
        }
    }

    pub fn single(
        side: OrderSide,
        order_type: SimOrderType,
        quantity: Decimal,
        tif: TimeInForce,
        attached_bracket: Option<AttachedBracket>,
    ) -> Self {
        Self {
            kind: OrderSubmitKind::Single {
                side,
                order_type,
                attached_bracket,
            },
            quantity,
            tif,
            priority: None,
            fill_rule: None,
        }
    }

    pub fn oco(
        quantity: Decimal,
        take_profit_limit: Decimal,
        stop_loss_stop: Decimal,
        tif: TimeInForce,
        is_trade_plan: bool,
    ) -> Self {
        Self {
            kind: OrderSubmitKind::Oco {
                take_profit_limit,
                stop_loss_stop,
                is_trade_plan,
            },
            quantity,
            tif,
            priority: None,
            fill_rule: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingOrderPreviewDto {
    pub valid: bool,
    pub preview: Option<OrderCostPreview>,
    pub rejection: Option<OrderRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingOrderDto {
    pub order_id: String,
    pub order_seq: u64,
    pub instrument_id: String,
    pub side: OrderSide,
    pub order_type: SimOrderType,
    pub tif: TimeInForce,
    pub priority: i32,
    pub quantity: Decimal,
    pub parent_group_id: Option<String>,
    pub parent_order_id: Option<String>,
    pub oco_peer_order_id: Option<String>,
    pub reservation_id: Option<String>,
    pub group_role: OrderGroupRole,
    pub has_trade_plan: bool,
    pub status: OrderStatus,
    pub reject_code: Option<SimRejectCode>,
    pub cancel_reason: Option<String>,
    pub submitted_bar_index: usize,
    pub submitted_bar_label: String,
    pub submitted_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingFillDto {
    pub fill_id: String,
    pub fill_seq: u64,
    pub order_id: String,
    pub order_seq: u64,
    pub parent_group_id: Option<String>,
    pub group_role: OrderGroupRole,
    pub has_trade_plan: bool,
    pub instrument_id: String,
    pub bar_index: usize,
    pub bar_label: String,
    pub bar_session_date: Option<NaiveDate>,
    pub bar_close_at: Option<DateTime<Utc>>,
    pub path_stage: BarPathStage,
    pub raw_path_price: Decimal,
    pub fill_price: Decimal,
    pub gross_quantity: Decimal,
    pub net_base_quantity: Decimal,
    pub side: OrderSide,
    pub fee_amount: Decimal,
    pub fee_currency: String,
    pub fee_quote_value: Decimal,
    pub realized_pnl_delta: Decimal,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertTrainingNoteRequest {
    pub note_id: Option<String>,
    pub rationale: String,
    pub emotion_tag: Option<String>,
    pub linked_order_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingNoteDto {
    pub note_id: String,
    pub run_id: String,
    pub bar_index: i64,
    pub display_bar_index: usize,
    pub bar_label: String,
    pub visible_until: Option<DateTime<Utc>>,
    pub rationale: String,
    pub emotion_tag: Option<String>,
    pub linked_order_id: Option<String>,
    pub archived_journal_note_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSelfEvaluationDto {
    pub run_id: String,
    pub decision_score: u8,
    pub plan_discipline_score: Option<u8>,
    pub summary: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingStepReceiptDto {
    pub run: TrainingRunSummaryDto,
    pub seq: i64,
    pub state_hash: String,
    pub events: Vec<SimEvent>,
    pub split_cancelled_order_ids: Vec<String>,
    pub corporate_action_notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingWorkspaceSnapshotDto {
    pub run: TrainingRunSummaryDto,
    pub account: TrainingAccountSummaryDto,
    pub position: Option<TrainingPositionDto>,
    pub open_orders: Vec<TrainingOrderDto>,
    pub all_orders: Vec<TrainingOrderDto>,
    pub fills: Vec<TrainingFillDto>,
    pub notes: Vec<TrainingNoteDto>,
    pub self_evaluation: Option<UserSelfEvaluationDto>,
    pub viewport: KlineViewport,
    pub chart_slice: KlineWindowSliceDto,
    pub bar_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingReportDto {
    pub run: TrainingRunSummaryDto,
    pub report: TrainingReport,
    pub equity_bar_labels: Vec<String>,
    pub self_evaluation: Option<UserSelfEvaluationDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingRevealDto {
    pub run: TrainingRunSummaryDto,
    pub real_instrument_id: String,
    pub venue: String,
    pub real_symbol: String,
    pub train_start_date: NaiveDate,
    pub train_end_date: NaiveDate,
    pub reveal_end_date: NaiveDate,
    pub post_finish_bars: Vec<BarRecord>,
    pub raw_post_finish_bars: Vec<BarRecord>,
    pub full_chart_slice: KlineWindowSliceDto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrainingReplayMode {
    Scratch,
    FromCheckpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingReplayResultDto {
    pub run_id: String,
    pub mode: TrainingReplayMode,
    pub started_from_seq: i64,
    pub initial_or_checkpoint_hash: String,
    pub step_hashes: Vec<(i64, String)>,
    pub final_state_hash: String,
    pub matches_persisted_hashes: bool,
    pub report: TrainingReportDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveTrainingNotesResultDto {
    pub run_id: String,
    pub archived_notes: Vec<NoteDto>,
    pub updated_training_notes: Vec<TrainingNoteDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingOrdersAndFillsAiDto {
    pub run_id: String,
    pub simulated_training_badge: String,
    pub orders: Vec<TrainingOrderDto>,
    pub fills: Vec<TrainingFillDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingRevealedBarsAiDto {
    pub run_id: String,
    pub revealed: bool,
    pub is_identity_masked: bool,
    pub instrument_id: String,
    pub display_symbol: String,
    pub post_finish_bar_count: usize,
    pub post_finish_bars: Vec<BarRecord>,
    pub bar_labels: Vec<String>,
    pub notice: Option<String>,
}
