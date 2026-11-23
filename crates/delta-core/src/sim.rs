pub use crate::decimal::canon_decimal;
use crate::model::{BarRecord, CorporateActionRecord, CorporateActionType, Instrument};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

mod fees;
mod matching;
mod prng;

pub use fees::*;
pub use matching::*;
pub use prng::*;

/// Frozen simulation engine version identifier for M4 (`S-36`).
pub const SIM_ENGINE_VERSION: &str = "m4-sim-1";
/// M5 engine (`S-51`): `m4-sim-1` plus visible-close quick fills, order modify, single undo
/// within the current bar and intraday `DAY` expiry. Old runs always replay as `m4-sim-1`.
pub const SIM_ENGINE_VERSION_M5: &str = "m5-sim-2";
/// Frozen per run and printed in the report's sample limitations (`S-51` ①).
pub const FILL_RULE_VERSION_M5: &str = "m5-fill-1:quick=visible_close;orders=next_open_path";

/// Market-order cash reservation buffer (`10%` above last visible close, `S-40`).
pub const MARKET_BUY_RESERVATION_BUFFER_RATE: &str = "0.10";

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error_kind", rename_all = "snake_case")]
pub enum SimError {
    #[error("order rejected ({}): {}", .0.code.as_str(), .0.message)]
    OrderRejected(Box<OrderRejection>),
    #[error("invalid fee profile '{profile_id}': {reason}")]
    InvalidFeeProfile { profile_id: String, reason: String },
    #[error(
        "fee profile '{profile_id}' is frozen by an existing run and cannot be mutated in place"
    )]
    ImmutableFeeProfile { profile_id: String },
    #[error("invalid instrument specification for '{instrument_id}': {reason}")]
    InvalidInstrument {
        instrument_id: String,
        reason: String,
    },
    #[error("unknown instrument '{0}'")]
    UnknownInstrument(String),
    #[error("unknown order '{0}'")]
    UnknownOrder(String),
    #[error("order '{order_id}' cannot be cancelled in status '{}'", status.as_str())]
    OrderNotCancellable {
        order_id: String,
        status: OrderStatus,
    },
    #[error(
        "replay clock cannot move backwards or repeat: current {current:?}, incoming {incoming}"
    )]
    ClockNotMonotonic {
        current: Option<DateTime<Utc>>,
        incoming: DateTime<Utc>,
    },
    #[error("simulation already finished; no further commands are accepted")]
    SessionFinished,
    #[error("invalid bar '{bar_id}': {reason}")]
    InvalidBar { bar_id: String, reason: String },
    #[error("missing step bar input for bar advance command")]
    MissingBarInput,
    #[error("invalid corporate action '{action_id}': {reason}")]
    InvalidCorporateAction { action_id: String, reason: String },
    #[error("invalid sampling range: {0}")]
    InvalidSamplingRange(String),
    #[error("decimal arithmetic overflow in '{0}'")]
    ArithmeticOverflow(String),
    #[error("order '{order_id}' cannot be modified in status '{}'", status.as_str())]
    OrderNotModifiable {
        order_id: String,
        status: OrderStatus,
    },
    #[error("nothing to undo: {0}")]
    UndoUnavailable(String),
    #[error("'{feature}' requires engine {required}, run uses {actual}")]
    EngineFeatureUnavailable {
        feature: String,
        required: String,
        actual: String,
    },
}

impl SimError {
    #[inline]
    pub fn rejected(rejection: OrderRejection) -> Self {
        Self::OrderRejected(Box::new(rejection))
    }

    pub fn rejection(&self) -> Option<&OrderRejection> {
        match self {
            Self::OrderRejected(r) => Some(r.as_ref()),
            _ => None,
        }
    }
}

// ============================================================================
// 4. Simulation Orders, State, Corporate Actions & Matching Engine (S-36..S-42)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimInstrumentSpec {
    pub instrument_id: String,
    pub base_asset_id: String,
    pub quote_currency: String,
    pub lot_size: Decimal,
    pub tick_size: Decimal,
}

impl SimInstrumentSpec {
    pub fn from_instrument(inst: &Instrument) -> Result<Self, SimError> {
        if inst.multiplier != Decimal::ONE {
            return Err(SimError::InvalidInstrument {
                instrument_id: inst.id.clone(),
                reason: format!(
                    "spot simulation requires multiplier=1, got {}",
                    inst.multiplier
                ),
            });
        }
        let spec = Self {
            instrument_id: inst.id.clone(),
            base_asset_id: inst.base_asset_id.clone(),
            quote_currency: inst.quote_asset_id.clone(),
            lot_size: canon_decimal(inst.lot_size),
            tick_size: canon_decimal(inst.tick_size),
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), SimError> {
        if self.instrument_id.trim().is_empty()
            || self.base_asset_id.trim().is_empty()
            || self.quote_currency.trim().is_empty()
        {
            return Err(SimError::InvalidInstrument {
                instrument_id: self.instrument_id.clone(),
                reason: "identifiers must be non-empty".into(),
            });
        }
        if self.lot_size <= Decimal::ZERO || self.tick_size <= Decimal::ZERO {
            return Err(SimError::InvalidInstrument {
                instrument_id: self.instrument_id.clone(),
                reason: "lot_size and tick_size must be positive".into(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    Day,
    Gtc,
}

impl TimeInForce {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Gtc => "gtc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SimOrderType {
    Market,
    Limit {
        limit_price: Decimal,
    },
    StopMarket {
        stop_price: Decimal,
    },
    StopLimit {
        stop_price: Decimal,
        limit_price: Decimal,
    },
}

impl SimOrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Market => "market",
            Self::Limit { .. } => "limit",
            Self::StopMarket { .. } => "stop_market",
            Self::StopLimit { .. } => "stop_limit",
        }
    }

    pub fn limit_price(&self) -> Option<Decimal> {
        match self {
            Self::Limit { limit_price } | Self::StopLimit { limit_price, .. } => {
                Some(canon_decimal(*limit_price))
            }
            _ => None,
        }
    }

    pub fn stop_price(&self) -> Option<Decimal> {
        match self {
            Self::StopMarket { stop_price } | Self::StopLimit { stop_price, .. } => {
                Some(canon_decimal(*stop_price))
            }
            _ => None,
        }
    }

    pub fn canonical(self) -> Self {
        match self {
            Self::Market => Self::Market,
            Self::Limit { limit_price } => Self::Limit {
                limit_price: canon_decimal(limit_price),
            },
            Self::StopMarket { stop_price } => Self::StopMarket {
                stop_price: canon_decimal(stop_price),
            },
            Self::StopLimit {
                stop_price,
                limit_price,
            } => Self::StopLimit {
                stop_price: canon_decimal(stop_price),
                limit_price: canon_decimal(limit_price),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderGroupRole {
    Standalone,
    BracketEntry,
    BracketTakeProfit,
    BracketStopLoss,
    OcoTakeProfit,
    OcoStopLoss,
}

impl OrderGroupRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::BracketEntry => "bracket_entry",
            Self::BracketTakeProfit => "bracket_take_profit",
            Self::BracketStopLoss => "bracket_stop_loss",
            Self::OcoTakeProfit => "oco_take_profit",
            Self::OcoStopLoss => "oco_stop_loss",
        }
    }

    pub fn is_planned_exit(self) -> bool {
        matches!(
            self,
            Self::BracketTakeProfit
                | Self::BracketStopLoss
                | Self::OcoTakeProfit
                | Self::OcoStopLoss
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Accepted,
    Active,
    PendingParent,
    Filled,
    Cancelled,
    Expired,
    Rejected,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Active => "active",
            Self::PendingParent => "pending_parent",
            Self::Filled => "filled",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
        }
    }

    pub fn is_open_or_pending(self) -> bool {
        matches!(self, Self::Accepted | Self::Active | Self::PendingParent)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SimRejectCode {
    UnsupportedOrderType,
    InvalidTickSize,
    InvalidLotSize,
    InvalidOrderParams,
    InsufficientFunds,
    InsufficientPosition,
    InsufficientFundsAtFill,
    MissingReferencePrice,
    /// Quick fill on a bar with zero volume or not tradable (`S-51` ①).
    UntradableBar,
}

impl SimRejectCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UntradableBar => "UNTRADABLE_BAR",
            Self::UnsupportedOrderType => "UNSUPPORTED_ORDER_TYPE",
            Self::InvalidTickSize => "INVALID_TICK_SIZE",
            Self::InvalidLotSize => "INVALID_LOT_SIZE",
            Self::InvalidOrderParams => "INVALID_ORDER_PARAMS",
            Self::InsufficientFunds => "INSUFFICIENT_FUNDS",
            Self::InsufficientPosition => "INSUFFICIENT_POSITION",
            Self::InsufficientFundsAtFill => "INSUFFICIENT_FUNDS_AT_FILL",
            Self::MissingReferencePrice => "MISSING_REFERENCE_PRICE",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRejection {
    pub code: SimRejectCode,
    pub message: String,
    pub field: Option<String>,
    pub nearest_lower: Option<Decimal>,
    pub nearest_upper: Option<Decimal>,
    pub required: Option<Decimal>,
    pub available: Option<Decimal>,
}

/// Attached trade-plan bracket (`take_profit` Limit + `stop_loss` StopMarket) on a Buy entry (`S-39`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedBracket {
    pub take_profit_limit: Option<Decimal>,
    pub stop_loss_stop: Option<Decimal>,
    pub tif: TimeInForce,
}

impl AttachedBracket {
    pub fn canonical(self) -> Self {
        Self {
            take_profit_limit: self.take_profit_limit.map(canon_decimal),
            stop_loss_stop: self.stop_loss_stop.map(canon_decimal),
            tif: self.tif,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "submit_kind", rename_all = "snake_case")]
pub enum OrderSubmitKind {
    Single {
        side: OrderSide,
        order_type: SimOrderType,
        attached_bracket: Option<AttachedBracket>,
    },
    /// Spot long exit OCO: Take-Profit `Limit` + Stop-Loss `StopMarket` sharing one position reservation (`S-39`, `S-40`).
    Oco {
        take_profit_limit: Decimal,
        stop_loss_stop: Decimal,
        is_trade_plan: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderSubmitRequest {
    pub instrument_id: String,
    pub quantity: Decimal,
    pub tif: TimeInForce,
    pub priority: i32,
    pub kind: OrderSubmitKind,
    /// Optional explicit next-session open timestamp for `eligible_at`; defaults to `submitted_at`.
    pub eligible_at_hint: Option<DateTime<Utc>>,
    /// `m5-sim-2` only: `visible_close` fills a market order in the same command at the
    /// current visible close; absent/`next_open_path` keeps the M4 path rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill_rule: Option<FillRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillRule {
    NextOpenPath,
    VisibleClose,
}

impl OrderSubmitRequest {
    pub fn single(
        instrument_id: impl Into<String>,
        side: OrderSide,
        order_type: SimOrderType,
        tif: TimeInForce,
        quantity: Decimal,
    ) -> Self {
        Self {
            instrument_id: instrument_id.into(),
            quantity,
            tif,
            priority: 0,
            kind: OrderSubmitKind::Single {
                side,
                order_type,
                attached_bracket: None,
            },
            eligible_at_hint: None,
            fill_rule: None,
        }
    }

    pub fn single_with_bracket(
        instrument_id: impl Into<String>,
        side: OrderSide,
        order_type: SimOrderType,
        tif: TimeInForce,
        quantity: Decimal,
        attached_bracket: AttachedBracket,
    ) -> Self {
        Self {
            instrument_id: instrument_id.into(),
            quantity,
            tif,
            priority: 0,
            kind: OrderSubmitKind::Single {
                side,
                order_type,
                attached_bracket: Some(attached_bracket),
            },
            eligible_at_hint: None,
            fill_rule: None,
        }
    }

    pub fn oco(
        instrument_id: impl Into<String>,
        tif: TimeInForce,
        quantity: Decimal,
        take_profit_limit: Decimal,
        stop_loss_stop: Decimal,
        is_trade_plan: bool,
    ) -> Self {
        Self {
            instrument_id: instrument_id.into(),
            quantity,
            tif,
            priority: 0,
            kind: OrderSubmitKind::Oco {
                take_profit_limit,
                stop_loss_stop,
                is_trade_plan,
            },
            eligible_at_hint: None,
            fill_rule: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderCostPreview {
    pub instrument_id: String,
    pub quantity: Decimal,
    pub reference_price: Option<Decimal>,
    pub estimated_notional: Option<Decimal>,
    pub estimated_fee: Option<Decimal>,
    pub fee_currency: String,
    pub cash_reservation_required: Decimal,
    pub position_reservation_required: Decimal,
    pub available_cash_before: Decimal,
    pub available_position_before: Decimal,
}

/// Monotonic stage along the daily bar path `O -> L -> H -> C` (`S-39`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarPathStage {
    AtOpen = 0,
    OpenToLow = 1,
    LowToHigh = 2,
    HighToClose = 3,
}

impl BarPathStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AtOpen => "at_open",
            Self::OpenToLow => "open_to_low",
            Self::LowToHigh => "low_to_high",
            Self::HighToClose => "high_to_close",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimOrder {
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
    pub submitted_at: DateTime<Utc>,
    pub eligible_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub activated_bar_close_at: Option<DateTime<Utc>>,
    pub activated_stage: Option<BarPathStage>,
    pub activated_path_price: Option<Decimal>,
}

impl SimOrder {
    pub fn normalize_fields(&mut self) {
        self.order_type = self.order_type.canonical();
        self.quantity = canon_decimal(self.quantity);
        self.activated_path_price = self.activated_path_price.map(canon_decimal);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimFill {
    pub fill_id: String,
    pub fill_seq: u64,
    pub order_id: String,
    pub order_seq: u64,
    pub parent_group_id: Option<String>,
    pub group_role: OrderGroupRole,
    pub has_trade_plan: bool,
    pub instrument_id: String,
    pub bar_session_date: NaiveDate,
    pub bar_open_at: DateTime<Utc>,
    pub bar_close_at: DateTime<Utc>,
    pub path_stage: BarPathStage,
    /// Exact trigger/reference price on the raw `O -> L -> H -> C` path before spread/slippage.
    pub raw_path_price: Decimal,
    /// Executed fill price after adverse spread/slippage and adverse `tick_size` rounding.
    pub fill_price: Decimal,
    /// Gross base order quantity executed.
    pub gross_quantity: Decimal,
    /// Net base asset quantity credited (on Buy) or debited (on Sell).
    pub net_base_quantity: Decimal,
    pub side: OrderSide,
    pub fee_amount: Decimal,
    pub fee_currency: String,
    pub fee_quote_value: Decimal,
    pub realized_pnl_delta: Decimal,
    pub ambiguous: bool,
}

impl SimFill {
    pub fn normalize_fields(&mut self) {
        self.raw_path_price = canon_decimal(self.raw_path_price);
        self.fill_price = canon_decimal(self.fill_price);
        self.gross_quantity = canon_decimal(self.gross_quantity);
        self.net_base_quantity = canon_decimal(self.net_base_quantity);
        self.fee_amount = canon_decimal(self.fee_amount);
        self.fee_quote_value = canon_decimal(self.fee_quote_value);
        self.realized_pnl_delta = canon_decimal(self.realized_pnl_delta);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimLot {
    pub lot_id: String,
    pub source_fill_id: String,
    pub instrument_id: String,
    pub acquired_at: DateTime<Utc>,
    pub original_quantity: Decimal,
    pub remaining_quantity: Decimal,
    pub original_cost_quote: Decimal,
    pub remaining_cost_quote: Decimal,
}

impl SimLot {
    pub fn unit_cost(&self) -> Option<Decimal> {
        if self.remaining_quantity <= Decimal::ZERO {
            None
        } else {
            Some(canon_decimal(
                self.remaining_cost_quote / self.remaining_quantity,
            ))
        }
    }

    pub fn normalize_fields(&mut self) {
        self.original_quantity = canon_decimal(self.original_quantity);
        self.remaining_quantity = canon_decimal(self.remaining_quantity);
        self.original_cost_quote = canon_decimal(self.original_cost_quote);
        self.remaining_cost_quote = canon_decimal(self.remaining_cost_quote);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimHolding {
    pub instrument_id: String,
    pub quantity: Decimal,
    pub total_cost_quote: Decimal,
    pub avg_unit_cost: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashReservation {
    pub reservation_id: String,
    pub order_id: String,
    pub instrument_id: String,
    pub amount: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionReservation {
    pub reservation_id: String,
    pub instrument_id: String,
    pub quantity: Decimal,
    pub order_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimBarSnapshot {
    pub step_index: u64,
    pub instrument_id: String,
    pub session_date: NaiveDate,
    pub open_at: DateTime<Utc>,
    pub close_at: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub tradable: bool,
    pub had_buy_fill: bool,
    pub had_sell_fill: bool,
    pub position_qty_before: Decimal,
    pub cash_after: Decimal,
    pub position_qty_after: Decimal,
    pub position_cost_after: Decimal,
    pub equity_at_close: Decimal,
}

impl SimBarSnapshot {
    pub fn normalize_fields(&mut self) {
        self.open = canon_decimal(self.open);
        self.high = canon_decimal(self.high);
        self.low = canon_decimal(self.low);
        self.close = canon_decimal(self.close);
        self.volume = canon_decimal(self.volume);
        self.position_qty_before = canon_decimal(self.position_qty_before);
        self.cash_after = canon_decimal(self.cash_after);
        self.position_qty_after = canon_decimal(self.position_qty_after);
        self.position_cost_after = canon_decimal(self.position_cost_after);
        self.equity_at_close = canon_decimal(self.equity_at_close);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimStepBar {
    pub bar: BarRecord,
    pub halted: bool,
    pub tradable: bool,
}

impl SimStepBar {
    pub fn from_bar(bar: BarRecord) -> Self {
        let tradable = bar.is_final && bar.volume > Decimal::ZERO;
        Self {
            bar,
            halted: false,
            tradable,
        }
    }

    pub fn tradable(bar: BarRecord) -> Self {
        Self::from_bar(bar)
    }

    pub fn halted(bar: BarRecord) -> Self {
        Self {
            bar,
            halted: true,
            tradable: false,
        }
    }

    pub fn untradable(bar: BarRecord) -> Self {
        Self {
            bar,
            halted: false,
            tradable: false,
        }
    }

    pub fn is_effectively_tradable(&self) -> bool {
        self.bar.is_final && !self.halted && self.tradable && self.bar.volume > Decimal::ZERO
    }
}

impl From<BarRecord> for SimStepBar {
    fn from(bar: BarRecord) -> Self {
        Self::from_bar(bar)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum SimCommand {
    SubmitOrder(OrderSubmitRequest),
    CancelOrder {
        order_id: String,
        reason: Option<String>,
    },
    AdvanceBar {
        corporate_actions: Vec<CorporateActionRecord>,
    },
    Finish,
    /// `m5-sim-2`: change price and/or quantity of an open order (atomic cancel + re-place;
    /// the order becomes eligible from the next bar open, group links are kept).
    ModifyOrder {
        order_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quantity: Option<Decimal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit_price: Option<Decimal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_price: Option<Decimal>,
    },
    /// `m5-sim-2`: void the logged command `target_seq` (the last live one since the last bar
    /// advance). Applied by [`SimReplayer`], which restores the state saved before that command
    /// while keeping id/sequence counters moving forward (never reused).
    UndoLast {
        target_seq: u64,
    },
}

impl SimCommand {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::SubmitOrder(r) if r.fill_rule == Some(FillRule::VisibleClose) => "quick_fill",
            Self::SubmitOrder(_) => "submit_order",
            Self::CancelOrder { .. } => "cancel_order",
            Self::AdvanceBar { .. } => "advance_bar",
            Self::Finish => "finish",
            Self::ModifyOrder { .. } => "modify_order",
            Self::UndoLast { .. } => "undo_last",
        }
    }

    /// Commands a user can undo (`S-51` ③): order entry, cancel, modify and quick fills.
    pub fn is_undoable(&self) -> bool {
        matches!(
            self,
            Self::SubmitOrder(_) | Self::CancelOrder { .. } | Self::ModifyOrder { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SimEvent {
    OrderSubmitted {
        order_ids: Vec<String>,
        parent_group_id: Option<String>,
        cash_reserved: Decimal,
        position_reserved: Decimal,
    },
    OrderActivated {
        order_id: String,
        instrument_id: String,
        quantity: Decimal,
        at_stage: BarPathStage,
        at_path_price: Decimal,
    },
    OrderFilled {
        fill: Box<SimFill>,
    },
    OrderCancelled {
        order_id: String,
        instrument_id: String,
        reason: String,
    },
    OrderExpired {
        order_id: String,
        instrument_id: String,
        bar_close_at: DateTime<Utc>,
    },
    OrderRejected {
        order_id: String,
        instrument_id: String,
        rejection: OrderRejection,
    },
    CorporateActionApplied {
        action_id: String,
        instrument_id: String,
        action_type: CorporateActionType,
        ex_date: NaiveDate,
        ratio_or_amount: Decimal,
        position_qty_before: Decimal,
        position_qty_after: Decimal,
        cash_credited: Decimal,
        cancelled_order_ids: Vec<String>,
        assumption_marker: Option<String>,
    },
    BarAdvanced {
        snapshot: Box<SimBarSnapshot>,
    },
    OrderModified {
        order_id: String,
        instrument_id: String,
        quantity: Decimal,
        limit_price: Option<Decimal>,
        stop_price: Option<Decimal>,
        eligible_at: DateTime<Utc>,
    },
    CommandUndone {
        target_seq: u64,
        target_command: String,
    },
    SessionFinished {
        visible_until: Option<DateTime<Utc>>,
        cancelled_order_ids: Vec<String>,
    },
}

/// Deterministic, zero-float, `BTreeMap`-only simulation state (`S-36`, `M4-K02`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimState {
    pub engine_version: String,
    pub quote_currency: String,
    pub initial_cash: Decimal,
    pub cash_balance: Decimal,
    pub realized_pnl: Decimal,
    pub dividend_income: Decimal,
    pub total_fees_quote: Decimal,
    pub visible_until: Option<DateTime<Utc>>,
    pub step_count: u64,
    pub command_seq: u64,
    pub next_order_seq: u64,
    pub next_fill_seq: u64,
    pub next_group_seq: u64,
    pub next_reservation_seq: u64,
    pub ambiguous_fill_count: u64,
    pub ex_date_dividend_assumed: bool,
    pub finished: bool,
    pub fee_profile: FeeProfileVersion,
    pub instruments: BTreeMap<String, SimInstrumentSpec>,
    pub last_close_by_instrument: BTreeMap<String, Decimal>,
    pub last_close_at_by_instrument: BTreeMap<String, DateTime<Utc>>,
    pub lots_by_instrument: BTreeMap<String, Vec<SimLot>>,
    pub orders: BTreeMap<String, SimOrder>,
    pub fills: Vec<SimFill>,
    pub cash_reservations: BTreeMap<String, CashReservation>,
    pub position_reservations: BTreeMap<String, PositionReservation>,
    pub applied_corporate_actions: BTreeSet<(String, NaiveDate, String)>,
    pub bar_snapshots: Vec<SimBarSnapshot>,
    // ---- `m5-sim-2` fields: omitted from JSON when default so `m4-sim-1` hashes never change.
    /// Frozen bar timeframe (`1d`, `4h`, `1h`); `None` = daily (M4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeframe: Option<String>,
    /// Whether the latest visible bar of each instrument could trade (quick fills).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_bar_tradable: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub quick_fill_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub modify_count: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub undo_count: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl SimState {
    pub fn new(
        quote_currency: impl Into<String>,
        initial_cash: Decimal,
        fee_profile: FeeProfileVersion,
        instruments: Vec<SimInstrumentSpec>,
        initial_visible_until: Option<DateTime<Utc>>,
        initial_last_closes: BTreeMap<String, Decimal>,
    ) -> Result<Self, SimError> {
        let quote = quote_currency.into();
        if quote.trim().is_empty() || initial_cash <= Decimal::ZERO {
            return Err(SimError::InvalidInstrument {
                instrument_id: quote,
                reason: "quote_currency must be non-empty and initial_cash > 0".into(),
            });
        }
        let fee_profile = fee_profile.canonical();
        fee_profile.validate()?;
        if instruments.is_empty() {
            return Err(SimError::InvalidInstrument {
                instrument_id: String::new(),
                reason: "at least one instrument specification is required".into(),
            });
        }
        let mut inst_map = BTreeMap::new();
        for spec in instruments {
            spec.validate()?;
            if !spec.quote_currency.eq_ignore_ascii_case(&quote) {
                return Err(SimError::InvalidInstrument {
                    instrument_id: spec.instrument_id,
                    reason: format!(
                        "instrument quote currency {} does not match simulation quote currency {}",
                        spec.quote_currency, quote
                    ),
                });
            }
            inst_map.insert(spec.instrument_id.clone(), spec);
        }
        let mut closes = BTreeMap::new();
        for (inst_id, close) in initial_last_closes {
            if !inst_map.contains_key(&inst_id) || close <= Decimal::ZERO {
                return Err(SimError::InvalidInstrument {
                    instrument_id: inst_id,
                    reason:
                        "initial last close must reference a registered instrument with price > 0"
                            .into(),
                });
            }
            closes.insert(inst_id, canon_decimal(close));
        }

        let mut state = Self {
            engine_version: SIM_ENGINE_VERSION.to_string(),
            quote_currency: quote,
            initial_cash: canon_decimal(initial_cash),
            cash_balance: canon_decimal(initial_cash),
            realized_pnl: Decimal::ZERO,
            dividend_income: Decimal::ZERO,
            total_fees_quote: Decimal::ZERO,
            visible_until: initial_visible_until,
            step_count: 0,
            command_seq: 0,
            next_order_seq: 1,
            next_fill_seq: 1,
            next_group_seq: 1,
            next_reservation_seq: 1,
            ambiguous_fill_count: 0,
            ex_date_dividend_assumed: false,
            finished: false,
            fee_profile,
            instruments: inst_map,
            last_close_by_instrument: closes,
            last_close_at_by_instrument: BTreeMap::new(),
            lots_by_instrument: BTreeMap::new(),
            orders: BTreeMap::new(),
            fills: Vec::new(),
            cash_reservations: BTreeMap::new(),
            position_reservations: BTreeMap::new(),
            applied_corporate_actions: BTreeSet::new(),
            bar_snapshots: Vec::new(),
            timeframe: None,
            last_bar_tradable: BTreeMap::new(),
            quick_fill_count: 0,
            modify_count: 0,
            undo_count: 0,
        };
        state.normalize_fields();
        Ok(state)
    }

    /// `m5-sim-2` initial state: same as [`SimState::new`] plus the frozen timeframe and the
    /// tradability of the last visible history bar per instrument.
    #[allow(clippy::too_many_arguments)]
    pub fn new_m5(
        quote_currency: impl Into<String>,
        initial_cash: Decimal,
        fee_profile: FeeProfileVersion,
        instruments: Vec<SimInstrumentSpec>,
        initial_visible_until: Option<DateTime<Utc>>,
        initial_last_closes: BTreeMap<String, Decimal>,
        timeframe: &str,
        last_bar_tradable: BTreeMap<String, bool>,
    ) -> Result<Self, SimError> {
        let mut state = Self::new(
            quote_currency,
            initial_cash,
            fee_profile,
            instruments,
            initial_visible_until,
            initial_last_closes,
        )?;
        state.engine_version = SIM_ENGINE_VERSION_M5.to_string();
        state.timeframe = Some(timeframe.to_string());
        state.last_bar_tradable = last_bar_tradable;
        Ok(state)
    }

    pub fn is_m5(&self) -> bool {
        self.engine_version == SIM_ENGINE_VERSION_M5
    }

    fn timeframe_duration(&self) -> chrono::Duration {
        match self.timeframe.as_deref() {
            Some("1h") => chrono::Duration::hours(1),
            Some("4h") => chrono::Duration::hours(4),
            _ => chrono::Duration::days(1),
        }
    }

    fn is_intraday(&self) -> bool {
        matches!(self.timeframe.as_deref(), Some("1h") | Some("4h"))
    }

    pub fn normalize_fields(&mut self) {
        self.initial_cash = canon_decimal(self.initial_cash);
        self.cash_balance = canon_decimal(self.cash_balance);
        self.realized_pnl = canon_decimal(self.realized_pnl);
        self.dividend_income = canon_decimal(self.dividend_income);
        self.total_fees_quote = canon_decimal(self.total_fees_quote);
        self.fee_profile = self.fee_profile.clone().canonical();
        for spec in self.instruments.values_mut() {
            spec.lot_size = canon_decimal(spec.lot_size);
            spec.tick_size = canon_decimal(spec.tick_size);
        }
        for price in self.last_close_by_instrument.values_mut() {
            *price = canon_decimal(*price);
        }
        for lots in self.lots_by_instrument.values_mut() {
            for lot in lots.iter_mut() {
                lot.normalize_fields();
            }
            lots.retain(|l| l.remaining_quantity > Decimal::ZERO);
        }
        self.lots_by_instrument.retain(|_, v| !v.is_empty());
        for order in self.orders.values_mut() {
            order.normalize_fields();
        }
        for fill in &mut self.fills {
            fill.normalize_fields();
        }
        for res in self.cash_reservations.values_mut() {
            res.amount = canon_decimal(res.amount);
        }
        for res in self.position_reservations.values_mut() {
            res.quantity = canon_decimal(res.quantity);
            res.order_ids.sort();
            res.order_ids.dedup();
        }
        for snap in &mut self.bar_snapshots {
            snap.normalize_fields();
        }
    }

    /// Canonical UTF-8 JSON representation of `SimState` with normalized `Decimal` fields and
    /// `BTreeMap`/`BTreeSet` deterministic key ordering (`M4-K02`).
    pub fn canonical_json(&self) -> String {
        let mut normalized = self.clone();
        normalized.normalize_fields();
        serde_json::to_string(&normalized).expect("SimState canonical JSON serialization")
    }

    /// Deterministic SHA-256 hex digest of `canonical_json()` (`M4-K02`).
    pub fn state_hash(&self) -> String {
        crate::sha256_hex(self.canonical_json().as_bytes())
    }

    pub fn reserved_cash(&self) -> Decimal {
        canon_decimal(
            self.cash_reservations
                .values()
                .map(|r| r.amount)
                .sum::<Decimal>(),
        )
    }

    pub fn available_cash(&self) -> Decimal {
        canon_decimal((self.cash_balance - self.reserved_cash()).max(Decimal::ZERO))
    }

    pub fn position_quantity(&self, instrument_id: &str) -> Decimal {
        canon_decimal(
            self.lots_by_instrument
                .get(instrument_id)
                .map(|lots| lots.iter().map(|l| l.remaining_quantity).sum())
                .unwrap_or(Decimal::ZERO),
        )
    }

    pub fn position_cost(&self, instrument_id: &str) -> Decimal {
        canon_decimal(
            self.lots_by_instrument
                .get(instrument_id)
                .map(|lots| lots.iter().map(|l| l.remaining_cost_quote).sum())
                .unwrap_or(Decimal::ZERO),
        )
    }

    pub fn holding(&self, instrument_id: &str) -> Option<SimHolding> {
        let qty = self.position_quantity(instrument_id);
        if qty <= Decimal::ZERO {
            return None;
        }
        let cost = self.position_cost(instrument_id);
        Some(SimHolding {
            instrument_id: instrument_id.to_string(),
            quantity: qty,
            total_cost_quote: cost,
            avg_unit_cost: canon_decimal(cost / qty),
        })
    }

    pub fn reserved_position(&self, instrument_id: &str) -> Decimal {
        canon_decimal(
            self.position_reservations
                .values()
                .filter(|r| r.instrument_id == instrument_id)
                .map(|r| r.quantity)
                .sum::<Decimal>(),
        )
    }

    pub fn available_position(&self, instrument_id: &str) -> Decimal {
        canon_decimal(
            (self.position_quantity(instrument_id) - self.reserved_position(instrument_id))
                .max(Decimal::ZERO),
        )
    }

    pub fn total_equity_at_last_close(&self) -> Decimal {
        let mut pos_val = Decimal::ZERO;
        for (inst_id, lots) in &self.lots_by_instrument {
            let qty: Decimal = lots.iter().map(|l| l.remaining_quantity).sum();
            if qty > Decimal::ZERO {
                if let Some(&close) = self.last_close_by_instrument.get(inst_id) {
                    pos_val += qty * close;
                }
            }
        }
        canon_decimal(self.cash_balance + pos_val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::parse_decimal_lossless;
    use crate::model::AdjustmentKind;
    use chrono::TimeZone;

    fn dec(s: &str) -> Decimal {
        parse_decimal_lossless(s).expect("valid decimal literal")
    }

    fn equity_spec(id: &str, base: &str, lot: &str, tick: &str) -> SimInstrumentSpec {
        SimInstrumentSpec {
            instrument_id: id.to_string(),
            base_asset_id: base.to_string(),
            quote_currency: "USD".to_string(),
            lot_size: dec(lot),
            tick_size: dec(tick),
        }
    }

    fn zero_fee_profile() -> FeeProfileVersion {
        builtin_fee_profiles()
            .into_iter()
            .find(|p| p.preset_key == "zero_cost")
            .expect("zero_cost builtin preset")
    }

    #[allow(clippy::too_many_arguments)]
    fn make_bar(
        inst: &str,
        year: i32,
        month: u32,
        day: u32,
        o: &str,
        h: &str,
        l: &str,
        c: &str,
        vol: &str,
    ) -> BarRecord {
        let session_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        let open_at = Utc.with_ymd_and_hms(year, month, day, 14, 30, 0).unwrap();
        let close_at = Utc.with_ymd_and_hms(year, month, day, 21, 0, 0).unwrap();
        BarRecord {
            id: format!("BAR-{inst}-{session_date}"),
            instrument_id: inst.to_string(),
            timeframe: "1d".to_string(),
            session: "regular".to_string(),
            session_date,
            open_at,
            close_at,
            open: dec(o),
            high: dec(h),
            low: dec(l),
            close: dec(c),
            volume: dec(vol),
            source: "fixture".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        }
    }

    fn init_single_equity_state(
        initial_cash: &str,
        fee_profile: FeeProfileVersion,
        lot: &str,
        tick: &str,
        warmup_close: (&str, DateTime<Utc>),
    ) -> SimState {
        let mut closes = BTreeMap::new();
        closes.insert("NASDAQ:AAPL".to_string(), dec(warmup_close.0));
        SimState::new(
            "USD",
            dec(initial_cash),
            fee_profile,
            vec![equity_spec("NASDAQ:AAPL", "AAPL", lot, tick)],
            Some(warmup_close.1),
            closes,
        )
        .expect("valid initial SimState")
    }

    #[test]
    fn thin_slice_market_buy_and_sell_step_transition() {
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        let state0 =
            init_single_equity_state("10000", zero_fee_profile(), "1", "0.01", ("100", t0));
        assert_eq!(state0.engine_version, SIM_ENGINE_VERSION);
        assert_eq!(SIM_ENGINE_VERSION, "m4-sim-1");
        assert_eq!(state0.state_hash().len(), 64);

        let (state1, ev1) = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .expect("submit market buy");
        assert_eq!(ev1.len(), 1);
        assert_eq!(state1.reserved_cash(), dec("1100")); // 10 * 100 * 1.10

        let bar1 = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            1,
            3,
            "102",
            "106",
            "101",
            "105",
            "1000",
        ));
        let (state2, ev2) = step(
            &state1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&bar1),
        )
        .expect("advance bar 1");
        assert!(ev2
            .iter()
            .any(|e| matches!(e, SimEvent::OrderFilled { .. })));
        assert_eq!(state2.position_quantity("NASDAQ:AAPL"), dec("10"));
        assert_eq!(state2.cash_balance, dec("8980"));
        assert_eq!(state2.reserved_cash(), Decimal::ZERO);
        assert_eq!(state2.total_equity_at_last_close(), dec("10030"));

        // Sell 4 shares on next bar open at 110 -> FIFO cost = 4 * 102 = 408, proceeds = 440, realized = +32
        let (state3, _) = step(
            &state2,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("4"),
            )),
            None,
        )
        .expect("submit market sell");
        assert_eq!(state3.reserved_position("NASDAQ:AAPL"), dec("4"));

        let bar2 = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            1,
            6,
            "110",
            "112",
            "108",
            "109",
            "1200",
        ));
        let (state4, _) = step(
            &state3,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&bar2),
        )
        .expect("advance bar 2");
        assert_eq!(state4.position_quantity("NASDAQ:AAPL"), dec("6"));
        assert_eq!(state4.position_cost("NASDAQ:AAPL"), dec("612"));
        assert_eq!(state4.realized_pnl, dec("32"));
        assert_eq!(state4.cash_balance, dec("9420"));
    }

    #[test]
    fn m4_a_02_deterministic_prng_and_rejection_sampling() {
        let seed = b"m4-seed-alpha";
        let w0 = prng_block_u64(seed, "candidate_draw", 0);
        let w0_repeat = prng_block_u64(seed, "candidate_draw", 0);
        let w1 = prng_block_u64(seed, "candidate_draw", 1);
        let w_other_purpose = prng_block_u64(seed, "redraw_1", 0);
        assert_eq!(w0, w0_repeat);
        assert_ne!(w0, w1);
        assert_ne!(w0, w_other_purpose);

        let seq_a = sample_candidate_sequence("m4-seed-alpha:pool-v1", 17, 12).unwrap();
        let seq_b = sample_candidate_sequence("m4-seed-alpha:pool-v1", 17, 12).unwrap();
        let seq_c = sample_candidate_sequence("m4-seed-beta:pool-v1", 17, 12).unwrap();
        assert_eq!(seq_a, seq_b);
        assert_ne!(seq_a, seq_c);
        assert!(seq_a.iter().all(|&idx| idx < 17));
        for pair in seq_a.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }

        let mut prng = DeterministicPrng::new(seed, "edge");
        assert_eq!(prng.next_u64_below(1).unwrap(), 0);
        assert!(matches!(
            prng.next_u64_below(0),
            Err(SimError::InvalidSamplingRange(_))
        ));
    }

    #[test]
    fn m4_a_03_pit_split_adjustment_and_in_session_corporate_actions() {
        let b1 = make_bar(
            "NASDAQ:AAPL",
            2025,
            8,
            28,
            "400",
            "408",
            "396",
            "404",
            "1000",
        );
        let b2 = make_bar(
            "NASDAQ:AAPL",
            2025,
            8,
            29,
            "102",
            "106",
            "100",
            "105",
            "4200",
        ); // 4:1 split ex_date
        let b3 = make_bar(
            "NASDAQ:AAPL",
            2025,
            9,
            2,
            "105",
            "107",
            "104",
            "106",
            "3800",
        ); // $0.50 dividend ex_date
        let future_sentinel_bar = make_bar(
            "NASDAQ:AAPL",
            2025,
            9,
            3,
            "999999",
            "999999",
            "999999",
            "999999",
            "999999",
        );

        let split_ca = CorporateActionRecord {
            id: "CA-SPLIT-4X".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            action_type: CorporateActionType::Split,
            ex_date: NaiveDate::from_ymd_opt(2025, 8, 29).unwrap(),
            pay_date: None,
            ratio_or_amount: dec("4"),
            currency: None,
            announced_at: None,
            source: "yahoo_fixture".to_string(),
            factor_version: 1,
            raw_payload_hash: None,
        };
        let div_ca = CorporateActionRecord {
            id: "CA-DIV-050".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            action_type: CorporateActionType::CashDividend,
            ex_date: NaiveDate::from_ymd_opt(2025, 9, 2).unwrap(),
            pay_date: Some(NaiveDate::from_ymd_opt(2025, 9, 15).unwrap()),
            ratio_or_amount: dec("0.50"),
            currency: Some("USD".to_string()),
            announced_at: None,
            source: "yahoo_fixture".to_string(),
            factor_version: 1,
            raw_payload_hash: None,
        };
        let future_sentinel_split = CorporateActionRecord {
            id: "CA-FUTURE-SENTINEL".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            action_type: CorporateActionType::Split,
            ex_date: NaiveDate::from_ymd_opt(2025, 9, 3).unwrap(),
            pay_date: None,
            ratio_or_amount: dec("1000"),
            currency: None,
            announced_at: None,
            source: "sentinel".to_string(),
            factor_version: 1,
            raw_payload_hash: None,
        };

        let all_bars = vec![b1.clone(), b2.clone(), b3.clone(), future_sentinel_bar];
        let all_cas = vec![split_ca.clone(), div_ca.clone(), future_sentinel_split];

        // 1) Before split ex_date (`visible_until == b1.close_at`): NO future split or sentinel leaks!
        let pit_before =
            derive_point_in_time_split_adjusted_bars(&all_bars, &all_cas, b1.close_at).unwrap();
        assert_eq!(pit_before.len(), 1);
        assert_eq!(pit_before[0].close, dec("404"));
        assert_eq!(pit_before[0].volume, dec("1000"));

        // 2) On split ex_date (`visible_until == b2.close_at`): b1 divided by 4, b2 untouched, sentinel excluded!
        let pit_on_split =
            derive_point_in_time_split_adjusted_bars(&all_bars, &all_cas, b2.close_at).unwrap();
        assert_eq!(pit_on_split.len(), 2);
        assert_eq!(pit_on_split[0].open, dec("100"));
        assert_eq!(pit_on_split[0].high, dec("102"));
        assert_eq!(pit_on_split[0].low, dec("99"));
        assert_eq!(pit_on_split[0].close, dec("101"));
        assert_eq!(pit_on_split[0].volume, dec("4000"));
        assert_eq!(pit_on_split[1].close, dec("105"));

        // 3) On dividend ex_date (`visible_until == b3.close_at`): cash dividend NEVER alters chart prices!
        let pit_on_div =
            derive_point_in_time_split_adjusted_bars(&all_bars, &all_cas, b3.close_at).unwrap();
        assert_eq!(pit_on_div.len(), 3);
        assert_eq!(pit_on_div[1].close, dec("105"));
        assert_eq!(pit_on_div[2].close, dec("106"));

        // 4) In-session split + cash dividend execution
        let t_pre = Utc.with_ymd_and_hms(2025, 8, 27, 21, 0, 0).unwrap();
        let state0 =
            init_single_equity_state("10000", zero_fee_profile(), "1", "0.01", ("395", t_pre));
        let (state1, _) = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        // Fill 10 shares @ 400 on 2025-08-28 -> cost basis = 4000, cash = 6000
        let (state2, _) = step(
            &state1,
            SimCommand::AdvanceBar {
                corporate_actions: all_cas.clone(),
            },
            Some(&SimStepBar::tradable(b1)),
        )
        .unwrap();
        assert_eq!(state2.position_quantity("NASDAQ:AAPL"), dec("10"));
        assert_eq!(state2.position_cost("NASDAQ:AAPL"), dec("4000"));

        // Place an open GTC sell limit @ 450 before split ex_date
        let (state3, _) = step(
            &state2,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Limit {
                    limit_price: dec("450"),
                },
                TimeInForce::Gtc,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        assert_eq!(state3.reserved_position("NASDAQ:AAPL"), dec("5"));

        // Advance onto split ex_date 2025-08-29: qty 10 -> 40, unit cost 400 -> 100, total cost 4000, open order cancelled!
        let (state4, ev_split) = step(
            &state3,
            SimCommand::AdvanceBar {
                corporate_actions: all_cas.clone(),
            },
            Some(&SimStepBar::tradable(b2)),
        )
        .unwrap();
        let holding = state4.holding("NASDAQ:AAPL").expect("holding after split");
        assert_eq!(holding.quantity, dec("40"));
        assert_eq!(holding.total_cost_quote, dec("4000"));
        assert_eq!(holding.avg_unit_cost, dec("100"));
        assert_eq!(state4.reserved_position("NASDAQ:AAPL"), Decimal::ZERO);
        assert!(ev_split.iter().any(|e| matches!(
            e,
            SimEvent::OrderCancelled { reason, .. } if reason == "corporate_action_split"
        )));

        // Advance onto cash dividend ex_date 2025-09-02: 40 shares * $0.50 = $20 credited once, marker set!
        let (state5, ev_div) = step(
            &state4,
            SimCommand::AdvanceBar {
                corporate_actions: all_cas,
            },
            Some(&SimStepBar::tradable(b3.clone())),
        )
        .unwrap();
        assert_eq!(state5.cash_balance, dec("6020"));
        assert_eq!(state5.dividend_income, dec("20"));
        assert!(state5.ex_date_dividend_assumed);
        assert!(ev_div.iter().any(|e| matches!(
            e,
            SimEvent::CorporateActionApplied {
                action_type: CorporateActionType::CashDividend,
                cash_credited,
                assumption_marker: Some(marker),
                ..
            } if *cash_credited == dec("20") && marker == "ex_date_pre_open_cash_credit"
        )));

        // Clock monotonicity refuses repeating the same bar
        assert!(matches!(
            step(
                &state5,
                SimCommand::AdvanceBar {
                    corporate_actions: vec![]
                },
                Some(&SimStepBar::tradable(b3))
            ),
            Err(SimError::ClockNotMonotonic { .. })
        ));
    }

    #[test]
    fn m4_a_04_market_limit_fills_gaps_and_tick_lot_validation() {
        let mut profile = zero_fee_profile();
        profile.id = "test-spread-slip-v1".to_string();
        profile.half_spread_bps = dec("10"); // 0.10%
        profile.slippage = SlippageSpec::Bps(dec("20")); // 0.20%

        let t0 = Utc.with_ymd_and_hms(2025, 2, 3, 21, 0, 0).unwrap();
        // tick_size = 0.05, lot_size = 5
        let state0 = init_single_equity_state("50000", profile, "5", "0.05", ("100", t0));

        // 1) Unsupported StopLimit rejected cleanly
        let err_stop_lim = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::StopLimit {
                    stop_price: dec("102"),
                    limit_price: dec("103"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err_stop_lim.rejection().unwrap().code,
            SimRejectCode::UnsupportedOrderType
        );

        // 2) Invalid lot_size (7 vs lot 5 -> lower 5, upper 10)
        let err_lot = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("7"),
            )),
            None,
        )
        .unwrap_err();
        let rej_lot = err_lot.rejection().unwrap();
        assert_eq!(rej_lot.code, SimRejectCode::InvalidLotSize);
        assert_eq!(rej_lot.nearest_lower, Some(dec("5")));
        assert_eq!(rej_lot.nearest_upper, Some(dec("10")));

        // 3) Invalid tick_size (100.03 vs tick 0.05 -> lower 100, upper 100.05)
        let err_tick = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("100.03"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap_err();
        let rej_tick = err_tick.rejection().unwrap();
        assert_eq!(rej_tick.code, SimRejectCode::InvalidTickSize);
        assert_eq!(rej_tick.nearest_lower, Some(dec("100")));
        assert_eq!(rej_tick.nearest_upper, Some(dec("100.05")));

        // 4) Market buy on bar opening at 101.00:
        //    raw = 101 + 101*0.001 + 101*0.002 = 101.303 -> adverse Buy tick_size(0.05) ceiling = 101.35!
        let (state1, _) = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        let bar1 = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            2,
            4,
            "101",
            "104",
            "100",
            "103",
            "1000",
        ));
        let (state2, _) = step(
            &state1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&bar1),
        )
        .unwrap();
        assert_eq!(state2.fills[0].fill_price, dec("101.35"));

        // 5) Limit Buy @ 100.00 when next bar gaps down to Open = 95.00:
        //    open + slippage = 95 + 95*0.002 = 95.19 -> ceiling tick(0.05) = 95.20 (better than limit 100.00!)
        //    Also test Limit Buy @ 95.10 when Open = 95.00 -> raw 95.19 capped at limit 95.10!
        let (state3, _) = step(
            &state2,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("100.00"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        let (state4, _) = step(
            &state3,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("95.10"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        let bar2_gap_down = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            2,
            5,
            "95.00",
            "99.00",
            "94.00",
            "98.00",
            "1500",
        ));
        let (state5, _) = step(
            &state4,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&bar2_gap_down),
        )
        .unwrap();
        assert_eq!(state5.fills[1].fill_price, dec("95.20"));
        assert_eq!(state5.fills[2].fill_price, dec("95.10")); // Capped not worse than limit!

        // 6) Limit Sell @ 102.00 touched intra-bar (Open = 99 < 102, High = 104 >= 102) -> exact limit 102.00!
        //    And Limit Sell @ 102.00 when next bar gaps UP to Open = 105.00 -> floor_tick(105 - 105*0.002 = 104.79) = 104.75!
        let (state6, _) = step(
            &state5,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Limit {
                    limit_price: dec("102.00"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        let bar3_inside = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            2,
            6,
            "99.00",
            "104.00",
            "98.50",
            "103.00",
            "1000",
        ));
        let (state7, _) = step(
            &state6,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&bar3_inside),
        )
        .unwrap();
        assert_eq!(state7.fills[3].fill_price, dec("102.00"));

        let (state8, _) = step(
            &state7,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Limit {
                    limit_price: dec("102.00"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        let bar4_gap_up = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            2,
            7,
            "105.00",
            "106.00",
            "103.00",
            "105.50",
            "1000",
        ));
        let (state9, _) = step(
            &state8,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&bar4_gap_up),
        )
        .unwrap();
        assert_eq!(state9.fills[4].fill_price, dec("104.75"));
    }

    #[test]
    fn m4_a_05_stop_oco_path_ordering_ambiguous_fills_and_attached_brackets() {
        let mut profile = zero_fee_profile();
        profile.id = "test-slip-ticks-v1".to_string();
        profile.slippage = SlippageSpec::Ticks(2); // 2 ticks = $0.02 adverse slippage

        let t0 = Utc.with_ymd_and_hms(2025, 3, 3, 21, 0, 0).unwrap();
        let state0 = init_single_equity_state("50000", profile, "1", "0.01", ("100", t0));

        // Open 20 shares first
        let (state1, _) = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("20"),
            )),
            None,
        )
        .unwrap();
        let b1 = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            3,
            4,
            "100",
            "101",
            "99",
            "100",
            "1000",
        ));
        let (state2, _) = step(
            &state1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b1),
        )
        .unwrap();

        // 1) Long OCO (10 shares, TP = 108, SL = 95) on wide bar O=100, L=94, H=110, C=105:
        //    Both SL and TP are inside [L, H]! O->L->H->C executes SL first at 95 - 0.02 = 94.98,
        //    increments ambiguous_fill_count to 1, and cancels the TP leg!
        let (state3, _) = step(
            &state2,
            SimCommand::SubmitOrder(OrderSubmitRequest::oco(
                "NASDAQ:AAPL",
                TimeInForce::Gtc,
                dec("10"),
                dec("108"),
                dec("95"),
                true,
            )),
            None,
        )
        .unwrap();
        // Verify OCO reserves 10 shares ONCE (not 20!)
        assert_eq!(state3.reserved_position("NASDAQ:AAPL"), dec("10"));

        let b2_dual = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            3,
            5,
            "100",
            "110",
            "94",
            "105",
            "2000",
        ));
        let (state4, ev_dual) = step(
            &state3,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b2_dual),
        )
        .unwrap();
        assert_eq!(state4.ambiguous_fill_count, 1);
        let oco_fill = state4.fills.last().unwrap();
        assert_eq!(oco_fill.group_role, OrderGroupRole::OcoStopLoss);
        assert_eq!(oco_fill.path_stage, BarPathStage::OpenToLow);
        assert_eq!(oco_fill.fill_price, dec("94.98"));
        assert!(oco_fill.ambiguous);
        assert!(ev_dual.iter().any(|e| matches!(
            e,
            SimEvent::OrderCancelled { reason, .. } if reason == "oco_peer_filled"
        )));
        assert_eq!(state4.position_quantity("NASDAQ:AAPL"), dec("10"));
        assert_eq!(state4.reserved_position("NASDAQ:AAPL"), Decimal::ZERO);

        // 2) Remaining 10 shares with StopMarket @ 100 when next bar gaps down to Open = 92:
        //    Fills at open - slippage = 92 - 0.02 = 91.98 (NOT stop price 100!)
        let (state5, _) = step(
            &state4,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::StopMarket {
                    stop_price: dec("100"),
                },
                TimeInForce::Gtc,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        let b3_gap_stop = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            3,
            6,
            "92",
            "96",
            "90",
            "95",
            "1800",
        ));
        let (state6, _) = step(
            &state5,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b3_gap_stop),
        )
        .unwrap();
        let gap_stop_fill = state6.fills.last().unwrap();
        assert_eq!(gap_stop_fill.path_stage, BarPathStage::AtOpen);
        assert_eq!(gap_stop_fill.fill_price, dec("91.98"));
        assert_eq!(state6.position_quantity("NASDAQ:AAPL"), Decimal::ZERO);

        // 3) Entry Limit Buy @ 93 with attached bracket (TP = 102, SL = 89) on bar O=95, L=91, H=104, C=101:
        //    - Entry fills on OpenToLow at 93.00
        //    - Attached bracket activates AFTER (OpenToLow, 93), continues down to 91 (> 89, so SL not hit),
        //      then on LowToHigh reaches TP = 102.00 in the SAME bar and cancels SL!
        let mut req_entry_bracket = OrderSubmitRequest::single(
            "NASDAQ:AAPL",
            OrderSide::Buy,
            SimOrderType::Limit {
                limit_price: dec("93"),
            },
            TimeInForce::Gtc,
            dec("10"),
        );
        req_entry_bracket.kind = OrderSubmitKind::Single {
            side: OrderSide::Buy,
            order_type: SimOrderType::Limit {
                limit_price: dec("93"),
            },
            attached_bracket: Some(AttachedBracket {
                take_profit_limit: Some(dec("102")),
                stop_loss_stop: Some(dec("89")),
                tif: TimeInForce::Gtc,
            }),
        };
        let (state7, _) = step(&state6, SimCommand::SubmitOrder(req_entry_bracket), None).unwrap();
        let b4_same_bar_tp = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            3,
            7,
            "95",
            "104",
            "91",
            "101",
            "2500",
        ));
        let (state8, _) = step(
            &state7,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b4_same_bar_tp),
        )
        .unwrap();
        assert_eq!(state8.fills.len(), 5);
        assert_eq!(state8.fills[3].group_role, OrderGroupRole::BracketEntry);
        assert_eq!(state8.fills[3].path_stage, BarPathStage::OpenToLow);
        assert_eq!(state8.fills[3].fill_price, dec("93"));
        assert_eq!(
            state8.fills[4].group_role,
            OrderGroupRole::BracketTakeProfit
        );
        assert_eq!(state8.fills[4].path_stage, BarPathStage::LowToHigh);
        assert_eq!(state8.fills[4].fill_price, dec("102"));
        assert_eq!(state8.position_quantity("NASDAQ:AAPL"), Decimal::ZERO);

        // 4) Breakout StopMarket Buy @ 105 with attached SL @ 98 on bar O=100, L=95, H=107, C=106:
        //    `L=95` happened on OpenToLow BEFORE entry filled at 105 on LowToHigh!
        //    Attached SL @ 98 MUST NOT trigger on the already-passed OpenToLow segment!
        let mut req_breakout = OrderSubmitRequest::single(
            "NASDAQ:AAPL",
            OrderSide::Buy,
            SimOrderType::StopMarket {
                stop_price: dec("105"),
            },
            TimeInForce::Gtc,
            dec("10"),
        );
        req_breakout.kind = OrderSubmitKind::Single {
            side: OrderSide::Buy,
            order_type: SimOrderType::StopMarket {
                stop_price: dec("105"),
            },
            attached_bracket: Some(AttachedBracket {
                take_profit_limit: Some(dec("115")),
                stop_loss_stop: Some(dec("98")),
                tif: TimeInForce::Gtc,
            }),
        };
        let (state9, _) = step(&state8, SimCommand::SubmitOrder(req_breakout), None).unwrap();
        let b5_breakout = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            3,
            10,
            "100",
            "107",
            "95",
            "106",
            "2100",
        ));
        let (state10, _) = step(
            &state9,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b5_breakout),
        )
        .unwrap();
        assert_eq!(state10.fills.len(), 6);
        assert_eq!(state10.fills[5].path_stage, BarPathStage::LowToHigh);
        assert_eq!(state10.fills[5].fill_price, dec("105.02"));
        assert_eq!(state10.position_quantity("NASDAQ:AAPL"), dec("10"));
    }

    #[test]
    fn m4_a_06_reservations_insufficient_funds_at_fill_and_zero_volume_bars() {
        let t0 = Utc.with_ymd_and_hms(2025, 4, 1, 21, 0, 0).unwrap();
        // Account has $1,120 cash, last close = $100
        let state0 = init_single_equity_state("1120", zero_fee_profile(), "1", "0.01", ("100", t0));

        // 1) Limit buy exceeding cash -> InsufficientFunds
        let err_cash = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("120"),
                },
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err_cash.rejection().unwrap().code,
            SimRejectCode::InsufficientFunds
        );

        // 2) Sell exceeding position (0 held) -> InsufficientPosition
        let err_pos = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("1"),
            )),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err_pos.rejection().unwrap().code,
            SimRejectCode::InsufficientPosition
        );

        // 3) Market Buy of 10 shares @ ref 100 reserves 10 * 100 * 1.10 = $1,100 (<= $1,120 available -> accepted!)
        //    Next bar gaps UP to Open = $120 -> fill requires $1,200 > $1,120 -> whole-order rejected with
        //    `InsufficientFundsAtFill`, zero partial fill, cash & position stay non-negative!
        let (state1, _) = step(
            &state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        assert_eq!(state1.reserved_cash(), dec("1100"));
        assert_eq!(state1.available_cash(), dec("20"));

        let b1_extreme_gap = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            4,
            2,
            "120",
            "125",
            "118",
            "122",
            "1000",
        ));
        let (state2, ev2) = step(
            &state1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b1_extreme_gap),
        )
        .unwrap();
        assert!(state2.fills.is_empty());
        assert_eq!(state2.cash_balance, dec("1120"));
        assert_eq!(state2.reserved_cash(), Decimal::ZERO);
        assert_eq!(state2.position_quantity("NASDAQ:AAPL"), Decimal::ZERO);
        assert!(ev2.iter().any(|e| matches!(
            e,
            SimEvent::OrderRejected { rejection, .. }
                if rejection.code == SimRejectCode::InsufficientFundsAtFill
        )));

        // 4) Zero-volume bar with one DAY buy and one GTC buy:
        //    Zero fills, DAY order expires & releases reservation, GTC order stays Active & keeps reservation!
        let (state3, _) = step(
            &state2,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("130"),
                },
                TimeInForce::Day,
                dec("2"),
            )),
            None,
        )
        .unwrap();
        let (state4, _) = step(
            &state3,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("130"),
                },
                TimeInForce::Gtc,
                dec("3"),
            )),
            None,
        )
        .unwrap();
        assert_eq!(state4.reserved_cash(), dec("650")); // (2 + 3) * 130

        let b2_zero_vol = SimStepBar::tradable(make_bar(
            "NASDAQ:AAPL",
            2025,
            4,
            3,
            "121",
            "121",
            "121",
            "121",
            "0",
        ));
        let (state5, ev5) = step(
            &state4,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b2_zero_vol),
        )
        .unwrap();
        assert!(state5.fills.is_empty());
        assert!(ev5
            .iter()
            .any(|e| matches!(e, SimEvent::OrderExpired { .. })));
        assert_eq!(state5.reserved_cash(), dec("390")); // 3 * 130 GTC remains reserved
        assert_eq!(state5.orders["SIM-ORD-000002"].status, OrderStatus::Expired);
        assert_eq!(state5.orders["SIM-ORD-000003"].status, OrderStatus::Active);
    }

    #[test]
    fn m4_a_07_and_08_composable_fees_synthetic_partials_received_asset_and_versioning() {
        // 1) Built-in profiles pass validation & platform-preset source audit (`M4-A-08`)
        let builtins = builtin_fee_profiles();
        assert_eq!(builtins.len(), 3);
        let keys: Vec<&str> = builtins.iter().map(|p| p.preset_key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "zero_cost",
                "us_equity_per_share_example",
                "crypto_rate_example"
            ]
        );
        for p in &builtins {
            p.validate().expect("builtin profile valid");
            assert!(!p.is_platform_preset);
        }

        // Unverified platform preset MUST fail validation
        let mut fake_platform = builtins[1].clone();
        fake_platform.is_platform_preset = true;
        assert!(fake_platform.validate().is_err());

        // 2) Per-order minimum ($2.00) across 3 synthetic partial fills (`M4-A-07`):
        //    $0.005/share on fills of 50, 60, 40 shares -> raw = $0.25 + $0.30 + $0.20 = $0.75 < $2.00 min.
        //    Minimum MUST apply ONCE at order end (total = $2.00, NOT $6.00), and per-fill fees sum to $2.00!
        let min_profile = FeeProfileVersion {
            id: "fee-per-share-min2".to_string(),
            preset_key: "custom".to_string(),
            name: "Per Share Min 2".to_string(),
            version: 1,
            market_category: FeeMarketCategory::UsEquity,
            notional_rate: Decimal::ZERO,
            per_unit_fee: dec("0.005"),
            per_order_fixed_fee: Decimal::ZERO,
            min_fee_per_order: Some(dec("2.00")),
            max_fee_per_order: Some(dec("5.00")),
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage: SlippageSpec::Zero,
            half_spread_bps: Decimal::ZERO,
            is_builtin: false,
            is_platform_preset: false,
            official_source_url: None,
            official_retrieved_at: None,
            effective_from: None,
            effective_to: None,
            assumption_note: "Test per-order min/max".to_string(),
        }
        .canonical();

        let partial_fills = vec![
            SyntheticFillInput {
                fill_id: "F1".to_string(),
                quantity: dec("50"),
                fill_price: dec("100"),
            },
            SyntheticFillInput {
                fill_id: "F2".to_string(),
                quantity: dec("60"),
                fill_price: dec("100"),
            },
            SyntheticFillInput {
                fill_id: "F3".to_string(),
                quantity: dec("40"),
                fill_price: dec("100"),
            },
        ];
        let settled_min =
            settle_order_fees(&min_profile, OrderSide::Buy, "AAPL", "USD", &partial_fills).unwrap();
        assert_eq!(settled_min.total_fee_amount, dec("2"));
        assert_eq!(settled_min.min_fee_top_up_applied, dec("1.25"));
        let sum_fills: Decimal = settled_min.fills.iter().map(|f| f.fee_amount).sum();
        assert_eq!(canon_decimal(sum_fills), settled_min.total_fee_amount);
        assert_eq!(settled_min.fills[0].fee_amount, dec("0.25"));
        assert_eq!(settled_min.fills[1].fee_amount, dec("0.30"));
        assert_eq!(settled_min.fills[2].fee_amount, dec("1.45")); // 0.20 + 1.25 top-up on final fill

        // Max cap ($5.00) across large multi-fill order (600 + 600 shares @ $0.005 = $6.00 capped at $5.00)
        let capped = settle_order_fees(
            &min_profile,
            OrderSide::Sell,
            "AAPL",
            "USD",
            &[
                SyntheticFillInput {
                    fill_id: "F1".to_string(),
                    quantity: dec("600"),
                    fill_price: dec("100"),
                },
                SyntheticFillInput {
                    fill_id: "F2".to_string(),
                    quantity: dec("600"),
                    fill_price: dec("100"),
                },
            ],
        )
        .unwrap();
        assert!(capped.max_fee_cap_applied);
        assert_eq!(capped.total_fee_amount, dec("5"));
        assert_eq!(capped.fills[0].fee_amount, dec("3"));
        assert_eq!(capped.fills[1].fee_amount, dec("2"));

        // 3) PerFill vs PerOrder rounding + HalfUp / HalfEven / Up / Down modes
        assert_eq!(
            FeeRoundingMode::HalfUp.round_value(dec("1.005"), 2),
            dec("1.01")
        );
        assert_eq!(
            FeeRoundingMode::HalfEven.round_value(dec("1.005"), 2),
            dec("1")
        );
        assert_eq!(
            FeeRoundingMode::HalfEven.round_value(dec("1.015"), 2),
            dec("1.02")
        );
        assert_eq!(
            FeeRoundingMode::Up.round_value(dec("1.001"), 2),
            dec("1.01")
        );
        assert_eq!(FeeRoundingMode::Down.round_value(dec("1.009"), 2), dec("1"));

        let mut per_fill_prof = min_profile.clone();
        per_fill_prof.min_fee_per_order = None;
        per_fill_prof.per_unit_fee = dec("0.004"); // 1 share = $0.004
        per_fill_prof.charging_tier = FeeChargingTier::PerFill;
        let two_tiny = vec![
            SyntheticFillInput {
                fill_id: "A".to_string(),
                quantity: dec("1"),
                fill_price: dec("10"),
            },
            SyntheticFillInput {
                fill_id: "B".to_string(),
                quantity: dec("1"),
                fill_price: dec("10"),
            },
        ];
        // PerFill rounds each 0.004 -> 0.00, total 0.00; PerOrder sums 0.004+0.004=0.008 -> rounds to 0.01!
        let s_per_fill =
            settle_order_fees(&per_fill_prof, OrderSide::Buy, "AAPL", "USD", &two_tiny).unwrap();
        assert_eq!(s_per_fill.total_fee_amount, Decimal::ZERO);
        let mut per_ord_prof = per_fill_prof.clone();
        per_ord_prof.charging_tier = FeeChargingTier::PerOrder;
        let s_per_ord =
            settle_order_fees(&per_ord_prof, OrderSide::Buy, "AAPL", "USD", &two_tiny).unwrap();
        assert_eq!(s_per_ord.total_fee_amount, dec("0.01"));

        // 4) ReceivedAsset fee currency on crypto Buy (`M4-A-07`):
        //    Buy 1.0 BTC @ $50,000 with 0.10% fee in ReceivedAsset -> fee = 0.001 BTC, net received = 0.999 BTC!
        let crypto_recv_profile = FeeProfileVersion {
            id: "crypto-recv-v1".to_string(),
            preset_key: "custom_crypto".to_string(),
            name: "Crypto Base Fee".to_string(),
            version: 1,
            market_category: FeeMarketCategory::CryptoSpot,
            notional_rate: dec("0.001"),
            per_unit_fee: Decimal::ZERO,
            per_order_fixed_fee: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::ReceivedAsset,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 8,
            charging_tier: FeeChargingTier::PerOrder,
            slippage: SlippageSpec::Zero,
            half_spread_bps: Decimal::ZERO,
            is_builtin: false,
            is_platform_preset: false,
            official_source_url: None,
            official_retrieved_at: None,
            effective_from: None,
            effective_to: None,
            assumption_note: "Base asset fee deduction".to_string(),
        }
        .canonical();

        let t_crypto = Utc.with_ymd_and_hms(2025, 5, 1, 0, 0, 0).unwrap();
        let mut closes = BTreeMap::new();
        closes.insert("OKX:BTC-USDT".to_string(), dec("49000"));
        let c_state0 = SimState::new(
            "USDT",
            dec("60000"),
            crypto_recv_profile.clone(),
            vec![SimInstrumentSpec {
                instrument_id: "OKX:BTC-USDT".to_string(),
                base_asset_id: "BTC".to_string(),
                quote_currency: "USDT".to_string(),
                lot_size: dec("0.001"),
                tick_size: dec("0.1"),
            }],
            Some(t_crypto),
            closes,
        )
        .unwrap();
        let (c_state1, _) = step(
            &c_state0,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "OKX:BTC-USDT",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Gtc,
                dec("1"),
            )),
            None,
        )
        .unwrap();
        let crypto_bar = BarRecord {
            id: "BAR-BTC-1".to_string(),
            instrument_id: "OKX:BTC-USDT".to_string(),
            timeframe: "1d".to_string(),
            session: "24x7".to_string(),
            session_date: NaiveDate::from_ymd_opt(2025, 5, 1).unwrap(),
            open_at: t_crypto, // Note: crypto bar open_at == previous close_at!
            close_at: Utc.with_ymd_and_hms(2025, 5, 2, 0, 0, 0).unwrap(),
            open: dec("50000"),
            high: dec("51000"),
            low: dec("49500"),
            close: dec("50500"),
            volume: dec("100"),
            source: "okx_fixture".to_string(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        };
        let (c_state2, _) = step(
            &c_state1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&SimStepBar::tradable(crypto_bar)),
        )
        .unwrap();
        assert_eq!(c_state2.position_quantity("OKX:BTC-USDT"), dec("0.999"));
        assert_eq!(c_state2.cash_balance, dec("10000")); // 60000 - 50000
        assert_eq!(c_state2.fills[0].fee_currency, "BTC");
        assert_eq!(c_state2.fills[0].fee_amount, dec("0.001"));
        assert_eq!(c_state2.fills[0].fee_quote_value, dec("50"));

        // 5) Versioning & freezing (`M4-A-08`): deriving v2 leaves c_state2's v1 untouched
        let v2 = crypto_recv_profile
            .derive_next_version("crypto-recv-v2", |p| {
                p.notional_rate = dec("0.002");
            })
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(c_state2.fee_profile.id, "crypto-recv-v1");
        assert_eq!(c_state2.fee_profile.version, 1);
        assert_eq!(c_state2.fee_profile.notional_rate, dec("0.001"));
    }

    #[test]
    fn m4_k02_deterministic_state_hash_replay_and_checkpoint_equivalence() {
        let t0 = Utc.with_ymd_and_hms(2025, 6, 2, 21, 0, 0).unwrap();
        let profile = builtin_fee_profiles()
            .into_iter()
            .find(|p| p.preset_key == "us_equity_per_share_example")
            .unwrap();

        let bars = [
            SimStepBar::tradable(make_bar(
                "NASDAQ:AAPL",
                2025,
                6,
                3,
                "200",
                "205",
                "198",
                "203",
                "1000",
            )),
            SimStepBar::tradable(make_bar(
                "NASDAQ:AAPL",
                2025,
                6,
                4,
                "204",
                "210",
                "202",
                "208",
                "1100",
            )),
            SimStepBar::tradable(make_bar(
                "NASDAQ:AAPL",
                2025,
                6,
                5,
                "207",
                "212",
                "205",
                "211",
                "1200",
            )),
        ];

        let run_script = |start: SimState| -> Vec<String> {
            let mut hashes = Vec::new();
            let (s1, _) = step(
                &start,
                SimCommand::SubmitOrder(OrderSubmitRequest::single(
                    "NASDAQ:AAPL",
                    OrderSide::Buy,
                    SimOrderType::Market,
                    TimeInForce::Day,
                    dec("20"),
                )),
                None,
            )
            .unwrap();
            hashes.push(s1.state_hash());

            let (s2, _) = step(
                &s1,
                SimCommand::AdvanceBar {
                    corporate_actions: vec![],
                },
                Some(&bars[0]),
            )
            .unwrap();
            hashes.push(s2.state_hash());

            // Checkpoint roundtrip through canonical JSON in the middle of the run!
            let ckpt_json = s2.canonical_json();
            let s2_restored: SimState = serde_json::from_str(&ckpt_json).unwrap();
            assert_eq!(s2.state_hash(), s2_restored.state_hash());

            let (s3, _) = step(
                &s2_restored,
                SimCommand::SubmitOrder(OrderSubmitRequest::oco(
                    "NASDAQ:AAPL",
                    TimeInForce::Gtc,
                    dec("20"),
                    dec("209"),
                    dec("195"),
                    true,
                )),
                None,
            )
            .unwrap();
            hashes.push(s3.state_hash());

            let (s4, _) = step(
                &s3,
                SimCommand::AdvanceBar {
                    corporate_actions: vec![],
                },
                Some(&bars[1]),
            )
            .unwrap();
            hashes.push(s4.state_hash());

            let (s5, _) = step(
                &s4,
                SimCommand::AdvanceBar {
                    corporate_actions: vec![],
                },
                Some(&bars[2]),
            )
            .unwrap();
            hashes.push(s5.state_hash());

            let (s6, _) = step(&s5, SimCommand::Finish, None).unwrap();
            hashes.push(s6.state_hash());
            hashes
        };

        let init_a = init_single_equity_state("10000", profile.clone(), "1", "0.01", ("199", t0));
        let init_b =
            init_single_equity_state("10000.0000", profile, "1.00", "0.010", ("199.0", t0));
        assert_eq!(init_a.state_hash(), init_b.state_hash());

        let hashes_a = run_script(init_a);
        let hashes_b = run_script(init_b);
        assert_eq!(hashes_a, hashes_b);
    }

    // ------------------------------------------------------------------
    // `m5-sim-2` (`S-51`) hand-checkable fixtures
    // ------------------------------------------------------------------

    fn m5_state(cash: &str, fp: FeeProfileVersion, close: &str, tradable: bool) -> SimState {
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        SimState::new_m5(
            "USD",
            dec(cash),
            fp,
            vec![equity_spec("NASDAQ:AAPL", "AAPL", "1", "0.01")],
            Some(t0),
            BTreeMap::from([("NASDAQ:AAPL".to_string(), dec(close))]),
            "1d",
            BTreeMap::from([("NASDAQ:AAPL".to_string(), tradable)]),
        )
        .unwrap()
    }

    fn quick_req(side: OrderSide, qty: &str) -> SimCommand {
        let mut req = OrderSubmitRequest::single(
            "NASDAQ:AAPL",
            side,
            SimOrderType::Market,
            TimeInForce::Day,
            dec(qty),
        );
        req.fill_rule = Some(FillRule::VisibleClose);
        SimCommand::SubmitOrder(req)
    }

    fn costly_profile() -> FeeProfileVersion {
        let mut fp = zero_fee_profile();
        fp.notional_rate = dec("0.001");
        fp.half_spread_bps = dec("20");
        fp.slippage = SlippageSpec::Ticks(5);
        fp.rounding_mode = FeeRoundingMode::HalfUp;
        fp.rounding_scale = 2;
        fp
    }

    fn reject_code(err: SimError) -> SimRejectCode {
        err.rejection().expect("order rejection").code
    }

    #[test]
    fn m5_quick_fill_at_visible_close_is_hand_checkable() {
        let st = m5_state("10000", costly_profile(), "100", true);
        assert_eq!(st.engine_version, SIM_ENGINE_VERSION_M5);
        // Buy 40 @ close 100: + half spread 100 × 20 bps = 0.20, + 5 ticks = 0.05 -> 100.25.
        // Notional 4010.00; fee 0.1% = 4.01; cash 10000 - 4010 - 4.01 = 5985.99.
        let (st1, ev) = step(&st, quick_req(OrderSide::Buy, "40"), None).unwrap();
        assert_eq!(st1.fills.len(), 1);
        assert_eq!(st1.fills[0].fill_price, dec("100.25"));
        assert_eq!(st1.fills[0].fee_amount, dec("4.01"));
        assert_eq!(st1.cash_balance, dec("5985.99"));
        assert_eq!(st1.quick_fill_count, 1);
        assert_eq!(
            st1.visible_until, st.visible_until,
            "quick fills never move the clock"
        );
        assert!(ev.iter().any(|e| matches!(e, SimEvent::OrderFilled { .. })
            || matches!(e, SimEvent::OrderSubmitted { .. })));
        // Sell 40 @ 100: 100 - 0.20 - 0.05 = 99.75; notional 3990.00; fee 3.99.
        let (st2, _) = step(&st1, quick_req(OrderSide::Sell, "40"), None).unwrap();
        assert_eq!(st2.fills[1].fill_price, dec("99.75"));
        assert_eq!(st2.fills[1].fee_amount, dec("3.99"));
        assert_eq!(st2.cash_balance, dec("5985.99") + dec("3990") - dec("3.99"));
    }

    #[test]
    fn m5_quick_fill_rounds_adversely_and_rejects_without_shrinking() {
        let mut fp = zero_fee_profile();
        fp.half_spread_bps = dec("7");
        let st = m5_state("10000", fp.clone(), "100.01", true);
        // 100.01 × 7 bps = 0.070007 -> buy raw 100.080007 rounds up to 100.09.
        let (st1, _) = step(&st, quick_req(OrderSide::Buy, "1"), None).unwrap();
        assert_eq!(st1.fills[0].fill_price, dec("100.09"));
        // sell raw 99.939993 rounds down to 99.93.
        let (st2, _) = step(&st1, quick_req(OrderSide::Sell, "1"), None).unwrap();
        assert_eq!(st2.fills[1].fill_price, dec("99.93"));
        // Insufficient cash: rejected, never reduced to an affordable quantity.
        let err = step(&st, quick_req(OrderSide::Buy, "100"), None).unwrap_err();
        assert_eq!(reject_code(err), SimRejectCode::InsufficientFunds);
        // Selling more than held: rejected.
        let err = step(&st, quick_req(OrderSide::Sell, "1"), None).unwrap_err();
        assert_eq!(reject_code(err), SimRejectCode::InsufficientPosition);
        // Zero-volume visible bar: rejected.
        let untradable = m5_state("10000", fp, "100", false);
        let err = step(&untradable, quick_req(OrderSide::Buy, "1"), None).unwrap_err();
        assert_eq!(reject_code(err), SimRejectCode::UntradableBar);
        // `m4-sim-1` states never accept M5-only commands.
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        let m4 = init_single_equity_state("10000", zero_fee_profile(), "1", "0.01", ("100", t0));
        assert!(matches!(
            step(&m4, quick_req(OrderSide::Buy, "1"), None).unwrap_err(),
            SimError::EngineFeatureUnavailable { .. }
        ));
    }

    #[test]
    fn m5_modify_and_undo_restore_exact_state_and_never_cross_an_advance() {
        let st = m5_state("10000", zero_fee_profile(), "100", true);
        let mut rp = SimReplayer::new(st.clone());
        let limit = SimCommand::SubmitOrder(OrderSubmitRequest::single(
            "NASDAQ:AAPL",
            OrderSide::Buy,
            SimOrderType::Limit {
                limit_price: dec("95"),
            },
            TimeInForce::Gtc,
            dec("10"),
        ));
        let (s1, _) = rp.apply(&st, 1, limit, None).unwrap();
        let oid = s1.orders.keys().next().unwrap().clone();
        // Modify price and quantity in place (same id); reservation recomputed: 10 × 95 -> 20 × 96.
        let modify = SimCommand::ModifyOrder {
            order_id: oid.clone(),
            quantity: Some(dec("20")),
            limit_price: Some(dec("96")),
            stop_price: None,
        };
        let (s2, _) = rp.apply(&s1, 2, modify, None).unwrap();
        assert_eq!(s2.orders[&oid].quantity, dec("20"));
        assert_eq!(
            s2.orders[&oid].order_type,
            SimOrderType::Limit {
                limit_price: dec("96")
            }
        );
        assert_eq!(s2.modify_count, 1);
        // Undo the modify: the order is back to 10 @ 95 and cash/reservations match s1.
        let (s3, ev) = rp
            .apply(&s2, 3, SimCommand::UndoLast { target_seq: 2 }, None)
            .unwrap();
        assert!(matches!(
            ev[0],
            SimEvent::CommandUndone { target_seq: 2, .. }
        ));
        assert_eq!(s3.orders[&oid], s1.orders[&oid]);
        assert_eq!(s3.cash_balance, s1.cash_balance);
        assert_eq!(s3.undo_count, 1);
        // Undoing the wrong target or past the boundary is refused.
        assert!(matches!(
            rp.apply(&s3, 4, SimCommand::UndoLast { target_seq: 99 }, None)
                .unwrap_err(),
            SimError::UndoUnavailable(_)
        ));
        // Advance a bar that trades through 95: the GTC limit fills at 95.
        let bar = make_bar("NASDAQ:AAPL", 2025, 1, 3, "99", "100", "94", "97", "1000");
        let (s4, _) = rp
            .apply(
                &s3,
                4,
                SimCommand::AdvanceBar {
                    corporate_actions: Vec::new(),
                },
                Some(&SimStepBar::from_bar(bar)),
            )
            .unwrap();
        assert_eq!(s4.fills.len(), 1);
        assert_eq!(s4.fills[0].fill_price, dec("95"));
        assert!(matches!(
            rp.apply(&s4, 5, SimCommand::UndoLast { target_seq: 1 }, None)
                .unwrap_err(),
            SimError::UndoUnavailable(_)
        ));
        // A filled order cannot be modified.
        let err = step(
            &s4,
            SimCommand::ModifyOrder {
                order_id: oid,
                quantity: None,
                limit_price: Some(dec("90")),
                stop_price: None,
            },
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SimError::OrderNotModifiable { .. }));
    }

    #[test]
    fn m5_undo_after_an_earlier_undo_keeps_surviving_ids_exact() {
        let st = m5_state("10000", zero_fee_profile(), "100", true);
        let mut rp = SimReplayer::new(st.clone());
        let (s1, _) = rp
            .apply(&st, 1, quick_req(OrderSide::Buy, "1"), None)
            .unwrap();
        let (s2, _) = rp
            .apply(&s1, 2, SimCommand::UndoLast { target_seq: 1 }, None)
            .unwrap();
        // The voided id is not reused.
        let (s3, _) = rp
            .apply(&s2, 3, quick_req(OrderSide::Buy, "1"), None)
            .unwrap();
        let limit = SimCommand::SubmitOrder(OrderSubmitRequest::single(
            "NASDAQ:AAPL",
            OrderSide::Buy,
            SimOrderType::Limit {
                limit_price: dec("95"),
            },
            TimeInForce::Gtc,
            dec("10"),
        ));
        let (s4, _) = rp.apply(&s3, 4, limit, None).unwrap();
        let ids: Vec<String> = s4.orders.keys().cloned().collect();
        assert!(!ids.contains(&s1.orders.keys().next().unwrap().clone()));
        let limit_id = ids.iter().max().unwrap().clone();
        let modify = SimCommand::ModifyOrder {
            order_id: limit_id.clone(),
            quantity: None,
            limit_price: Some(dec("94")),
            stop_price: None,
        };
        let (s5, _) = rp.apply(&s4, 5, modify, None).unwrap();
        let (s6, _) = rp
            .apply(&s5, 6, SimCommand::UndoLast { target_seq: 5 }, None)
            .unwrap();
        // Same ids, same orders, fills, cash as before the modify; only counters moved on.
        assert_eq!(s6.orders, s4.orders);
        assert_eq!(s6.fills, s4.fills);
        assert_eq!(s6.cash_balance, s4.cash_balance);
        assert_eq!(s6.reserved_cash(), s4.reserved_cash());
        assert_eq!(s6.next_order_seq, s5.next_order_seq);
        assert_eq!(s6.undo_count, 2);
    }

    #[test]
    fn m5_quick_fill_session_date_and_bar_snapshot_refresh() {
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        let st = m5_state("10000", zero_fee_profile(), "100", true);
        let mut rp = SimReplayer::new(st.clone());

        // Step-0 quick fill at 21:00Z must keep 2025-01-02 (not roll back to 2025-01-01).
        let (s1, _) = rp
            .apply(&st, 1, quick_req(OrderSide::Buy, "10"), None)
            .unwrap();
        assert_eq!(
            s1.fills[0].bar_session_date,
            NaiveDate::from_ymd_opt(2025, 1, 2).unwrap()
        );
        assert_eq!(s1.fills[0].bar_close_at, t0);
        assert_eq!(s1.fills[0].bar_open_at, t0 - chrono::Duration::days(1));

        // Advance one bar (2025-01-03 14:30Z..21:00Z, close 105), then quick buy 10 at close.
        let bar = make_bar(
            "NASDAQ:AAPL",
            2025,
            1,
            3,
            "101",
            "106",
            "100",
            "105",
            "1000",
        );
        let (s2, _) = rp
            .apply(
                &s1,
                2,
                SimCommand::AdvanceBar {
                    corporate_actions: Vec::new(),
                },
                Some(&SimStepBar::from_bar(bar.clone())),
            )
            .unwrap();
        let snap_before = s2.bar_snapshots.last().unwrap();
        assert!(!snap_before.had_buy_fill);
        assert_eq!(snap_before.position_qty_after, dec("10"));
        assert_eq!(snap_before.cash_after, dec("9000"));
        assert_eq!(snap_before.equity_at_close, dec("10050"));

        let (s3, _) = rp
            .apply(&s2, 3, quick_req(OrderSide::Buy, "10"), None)
            .unwrap();
        let fill2 = s3.fills.last().unwrap();
        assert_eq!(fill2.bar_open_at, bar.open_at);
        assert_eq!(fill2.bar_close_at, bar.close_at);
        assert_eq!(fill2.bar_session_date, bar.session_date);
        let snap_after = s3.bar_snapshots.last().unwrap();
        assert!(snap_after.had_buy_fill);
        assert_eq!(snap_after.position_qty_after, dec("20"));
        assert_eq!(snap_after.cash_after, dec("7950"));
        assert_eq!(snap_after.equity_at_close, dec("10050"));

        // Undo restores the pre-quick-fill snapshot exactly.
        let (s4, _) = rp
            .apply(&s3, 4, SimCommand::UndoLast { target_seq: 3 }, None)
            .unwrap();
        assert_eq!(s4.bar_snapshots, s2.bar_snapshots);
        assert_eq!(s4.fills, s2.fills);
        assert_eq!(s4.cash_balance, s2.cash_balance);
    }

    #[test]
    fn step_multi_bars_maintains_strict_order_and_consistent_portfolio_equity() {
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        let mut init_closes = BTreeMap::new();
        init_closes.insert("NASDAQ:AAPL".to_string(), dec("100"));
        init_closes.insert("NASDAQ:MSFT".to_string(), dec("100"));
        let mut st = SimState::new(
            "USD",
            dec("100000"),
            zero_fee_profile(),
            vec![
                SimInstrumentSpec {
                    instrument_id: "NASDAQ:AAPL".to_string(),
                    base_asset_id: "AAPL".to_string(),
                    quote_currency: "USD".to_string(),
                    lot_size: dec("1"),
                    tick_size: dec("0.01"),
                },
                SimInstrumentSpec {
                    instrument_id: "NASDAQ:MSFT".to_string(),
                    base_asset_id: "MSFT".to_string(),
                    quote_currency: "USD".to_string(),
                    lot_size: dec("1"),
                    tick_size: dec("0.01"),
                },
            ],
            Some(t0),
            init_closes,
        )
        .unwrap();

        // Establish initial positions of 10 shares in each instrument on bar 0.
        for inst in ["NASDAQ:AAPL", "NASDAQ:MSFT"] {
            (st, _) = step(
                &st,
                SimCommand::SubmitOrder(OrderSubmitRequest::single(
                    inst,
                    OrderSide::Buy,
                    SimOrderType::Market,
                    TimeInForce::Day,
                    dec("10"),
                )),
                None,
            )
            .unwrap();
        }
        let b0_aapl = SimStepBar::from_bar(make_bar(
            "NASDAQ:AAPL",
            2025,
            1,
            3,
            "100",
            "101",
            "99",
            "100",
            "1000",
        ));
        let b0_msft = SimStepBar::from_bar(make_bar(
            "NASDAQ:MSFT",
            2025,
            1,
            3,
            "100",
            "101",
            "99",
            "100",
            "1000",
        ));
        (st, _) = step_multi_bars(&st, &[b0_aapl, b0_msft], &[]).unwrap();

        // Construct the former Condorcet cycle on OpenToLow:
        // B: AAPL Sell Stop @ 90 (dist=10, prio=2), A: AAPL Buy Limit @ 99 (dist=1, prio=3),
        // C: MSFT Sell Stop @ 95 (dist=5, prio=2, higher order_id than B).
        (st, _) = step(
            &st,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::StopMarket {
                    stop_price: dec("90"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        let aapl_stop_id = st.orders.keys().max().unwrap().clone();
        (st, _) = step(
            &st,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("99"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        let aapl_limit_id = st.orders.keys().max().unwrap().clone();
        (st, _) = step(
            &st,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:MSFT",
                OrderSide::Sell,
                SimOrderType::StopMarket {
                    stop_price: dec("95"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();

        let b1_aapl = SimStepBar::from_bar(make_bar(
            "NASDAQ:AAPL",
            2025,
            1,
            6,
            "100",
            "110",
            "85",
            "105",
            "1000",
        ));
        let b1_msft = SimStepBar::from_bar(make_bar(
            "NASDAQ:MSFT",
            2025,
            1,
            6,
            "100",
            "120",
            "90",
            "115",
            "1000",
        ));
        let (st2, _) = step_multi_bars(&st, &[b1_aapl, b1_msft], &[]).unwrap();

        // Within AAPL, closer Buy Limit @ 99 must execute before further Sell Stop @ 90.
        let step1_fills: Vec<&SimFill> = st2
            .fills
            .iter()
            .filter(|f| f.bar_session_date == NaiveDate::from_ymd_opt(2025, 1, 6).unwrap())
            .collect();
        assert_eq!(step1_fills.len(), 3);
        let aapl_limit_pos = step1_fills
            .iter()
            .position(|f| f.order_id == aapl_limit_id)
            .unwrap();
        let aapl_stop_pos = step1_fills
            .iter()
            .position(|f| f.order_id == aapl_stop_id)
            .unwrap();
        assert!(aapl_limit_pos < aapl_stop_pos);

        // Both snapshots from the simultaneous timestamp must share the same end-of-step
        // portfolio equity reflecting BOTH updated closes (AAPL=105, MSFT=115).
        let n = st2.bar_snapshots.len();
        let snap_a = &st2.bar_snapshots[n - 2];
        let snap_b = &st2.bar_snapshots[n - 1];
        assert_eq!(snap_a.equity_at_close, snap_b.equity_at_close);
        assert_eq!(snap_a.equity_at_close, st2.total_equity_at_last_close());
    }

    #[test]
    fn m5_modify_bracket_validation_qty_sync_and_intraday_day_activation() {
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 15, 0, 0).unwrap();
        let mut st = SimState::new_m5(
            "USD",
            dec("50000"),
            zero_fee_profile(),
            vec![SimInstrumentSpec {
                instrument_id: "NASDAQ:AAPL".to_string(),
                base_asset_id: "AAPL".to_string(),
                quote_currency: "USD".to_string(),
                lot_size: dec("1"),
                tick_size: dec("0.01"),
            }],
            Some(t0),
            BTreeMap::from([("NASDAQ:AAPL".to_string(), dec("105"))]),
            "1h",
            BTreeMap::from([("NASDAQ:AAPL".to_string(), true)]),
        )
        .unwrap();

        let (st1, _) = step(
            &st,
            SimCommand::SubmitOrder(OrderSubmitRequest::single_with_bracket(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("100"),
                },
                TimeInForce::Gtc,
                dec("10"),
                AttachedBracket {
                    take_profit_limit: Some(dec("110")),
                    stop_loss_stop: Some(dec("90")),
                    tif: TimeInForce::Day,
                },
            )),
            None,
        )
        .unwrap();
        let parent_id = st1
            .orders
            .values()
            .find(|o| o.group_role == OrderGroupRole::BracketEntry)
            .unwrap()
            .order_id
            .clone();
        let tp_id = st1
            .orders
            .values()
            .find(|o| o.group_role == OrderGroupRole::BracketTakeProfit)
            .unwrap()
            .order_id
            .clone();
        let sl_id = st1
            .orders
            .values()
            .find(|o| o.group_role == OrderGroupRole::BracketStopLoss)
            .unwrap()
            .order_id
            .clone();

        // Crossing parent <-> child prices must be rejected on ModifyOrder.
        for bad_cmd in [
            SimCommand::ModifyOrder {
                order_id: parent_id.clone(),
                quantity: None,
                limit_price: Some(dec("110")),
                stop_price: None,
            },
            SimCommand::ModifyOrder {
                order_id: parent_id.clone(),
                quantity: None,
                limit_price: Some(dec("90")),
                stop_price: None,
            },
            SimCommand::ModifyOrder {
                order_id: tp_id.clone(),
                quantity: None,
                limit_price: Some(dec("100")),
                stop_price: None,
            },
            SimCommand::ModifyOrder {
                order_id: sl_id.clone(),
                quantity: None,
                limit_price: None,
                stop_price: Some(dec("100")),
            },
        ] {
            let err = step(&st1, bad_cmd, None).unwrap_err();
            assert_eq!(reject_code(err), SimRejectCode::InvalidOrderParams);
        }

        // Modifying parent BracketEntry quantity syncs PendingParent TP/SL quantities.
        (st, _) = step(
            &st1,
            SimCommand::ModifyOrder {
                order_id: parent_id,
                quantity: Some(dec("20")),
                limit_price: None,
                stop_price: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(st.orders[&tp_id].quantity, dec("20"));
        assert_eq!(st.orders[&sl_id].quantity, dec("20"));

        // Entry fills on the NEXT UTC day (2025-01-03 15:00Z..16:00Z).
        // Activated DAY children must refresh eligible_at to 2025-01-03T15:00Z so they stay open
        // for the rest of 2025-01-03 and fill on the subsequent intraday bar.
        let make_1h =
            |hour: u32, close_day: u32, close_hour: u32, o: &str, h: &str, l: &str, c: &str| {
                BarRecord {
                    id: format!("BAR-1H-2025-01-03-{hour}"),
                    instrument_id: "NASDAQ:AAPL".to_string(),
                    timeframe: "1h".to_string(),
                    session: "regular".to_string(),
                    session_date: NaiveDate::from_ymd_opt(2025, 1, 3).unwrap(),
                    open_at: Utc.with_ymd_and_hms(2025, 1, 3, hour, 0, 0).unwrap(),
                    close_at: Utc
                        .with_ymd_and_hms(2025, 1, close_day, close_hour, 0, 0)
                        .unwrap(),
                    open: dec(o),
                    high: dec(h),
                    low: dec(l),
                    close: dec(c),
                    volume: dec("500"),
                    source: "fixture".to_string(),
                    adjustment: AdjustmentKind::Raw,
                    is_final: true,
                    valid_from_version: 1,
                    valid_to_version: None,
                    raw_payload_hash: None,
                }
            };

        let bar_fill = make_1h(15, 3, 16, "102", "103", "99", "101");
        let (st_filled, _) = step(
            &st,
            SimCommand::AdvanceBar {
                corporate_actions: Vec::new(),
            },
            Some(&SimStepBar::from_bar(bar_fill.clone())),
        )
        .unwrap();
        assert_eq!(st_filled.orders[&tp_id].status, OrderStatus::Active);
        assert_eq!(st_filled.orders[&sl_id].status, OrderStatus::Active);
        assert_eq!(st_filled.orders[&tp_id].eligible_at, bar_fill.open_at);

        // Branch 1: next intraday bar on same UTC day hits TP @ 110 for all 20 shares.
        let bar_tp = make_1h(16, 3, 17, "101", "112", "100", "111");
        let (st_tp, _) = step(
            &st_filled,
            SimCommand::AdvanceBar {
                corporate_actions: Vec::new(),
            },
            Some(&SimStepBar::from_bar(bar_tp)),
        )
        .unwrap();
        assert_eq!(st_tp.orders[&tp_id].status, OrderStatus::Filled);
        assert_eq!(st_tp.orders[&sl_id].status, OrderStatus::Cancelled);
        assert_eq!(st_tp.position_quantity("NASDAQ:AAPL"), Decimal::ZERO);

        // Branch 2: bar closing at 2025-01-04T00:00Z expires untouched DAY bracket legs.
        let bar_eod = make_1h(23, 4, 0, "101", "105", "95", "102");
        let (st_eod, _) = step(
            &st_filled,
            SimCommand::AdvanceBar {
                corporate_actions: Vec::new(),
            },
            Some(&SimStepBar::from_bar(bar_eod)),
        )
        .unwrap();
        assert_eq!(st_eod.orders[&tp_id].status, OrderStatus::Expired);
        assert_eq!(st_eod.orders[&sl_id].status, OrderStatus::Expired);
    }

    #[test]
    fn sim_guards_zero_step_arithmetic_overflow_and_tiny_lot_cost_rounding() {
        // CORE-B10: zero step size returns structured violation instead of panicking on `%`.
        let err = fees::validate_step_multiple(dec("1"), Decimal::ZERO).unwrap_err();
        assert_eq!(err.step_size, Decimal::ZERO);
        assert_eq!(err.nearest_lower, None);
        assert_eq!(err.nearest_upper, None);

        // CORE-B10: extreme order notional returns ArithmeticOverflow instead of panicking.
        let st = m5_state("10000", zero_fee_profile(), "100", true);
        let huge = OrderSubmitRequest::single(
            "NASDAQ:AAPL",
            OrderSide::Buy,
            SimOrderType::Limit {
                limit_price: dec("1000000"),
            },
            TimeInForce::Day,
            Decimal::MAX,
        );
        assert!(matches!(
            preview_order_submission(&st, &huge),
            Err(SimError::ArithmeticOverflow(_))
        ));

        // CORE-B15: tiny scale-10 crypto lot partial sell clamps allocated cost and never goes negative.
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        let mut st_crypto = SimState::new_m5(
            "USD",
            dec("100"),
            zero_fee_profile(),
            vec![SimInstrumentSpec {
                instrument_id: "CRYPTO:MICRO".to_string(),
                base_asset_id: "MICRO".to_string(),
                quote_currency: "USD".to_string(),
                lot_size: dec("0.000000001"),
                tick_size: dec("0.000000001"),
            }],
            Some(t0),
            BTreeMap::from([("CRYPTO:MICRO".to_string(), dec("0.1"))]),
            "1d",
            BTreeMap::from([("CRYPTO:MICRO".to_string(), true)]),
        )
        .unwrap();
        let mut buy_req = OrderSubmitRequest::single(
            "CRYPTO:MICRO",
            OrderSide::Buy,
            SimOrderType::Market,
            TimeInForce::Day,
            dec("0.000000007"),
        );
        buy_req.fill_rule = Some(FillRule::VisibleClose);
        (st_crypto, _) = step(&st_crypto, SimCommand::SubmitOrder(buy_req), None).unwrap();

        let mut sell_req = OrderSubmitRequest::single(
            "CRYPTO:MICRO",
            OrderSide::Sell,
            SimOrderType::Market,
            TimeInForce::Day,
            dec("0.000000006"),
        );
        sell_req.fill_rule = Some(FillRule::VisibleClose);
        (st_crypto, _) = step(&st_crypto, SimCommand::SubmitOrder(sell_req), None).unwrap();

        let lot = &st_crypto.lots_by_instrument["CRYPTO:MICRO"][0];
        assert_eq!(lot.remaining_quantity, dec("0.000000001"));
        assert!(
            lot.remaining_cost_quote >= Decimal::ZERO,
            "remaining_cost_quote must never be negative: {}",
            lot.remaining_cost_quote
        );
    }
}
