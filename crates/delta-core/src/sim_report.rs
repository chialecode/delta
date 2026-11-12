use crate::model::CorporateActionRecord;
use crate::sim::{
    canon_decimal, round_price_adverse_tick, settle_order_fees, step_multi_bars, BarPathStage,
    FeeProfileVersion, OrderSide, SimFill, SimInstrumentSpec, SimState, SimStepBar,
    SyntheticFillInput, SIM_ENGINE_VERSION,
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const SIM_REPORT_VERSION: &str = "m4-report-1";

fn round_metric(value: Decimal, scale: u32) -> Decimal {
    canon_decimal(value.round_dp_with_strategy(scale, RoundingStrategy::MidpointAwayFromZero))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingNoteRef {
    pub note_id: String,
    pub visible_until: DateTime<Utc>,
    pub rationale: String,
    pub emotion_tag: Option<String>,
    pub linked_order_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingReportInput {
    pub run_id: String,
    pub dataset_version: i64,
    pub hidden_mode: bool,
    pub state: SimState,
    pub training_bars: Vec<SimStepBar>,
    pub corporate_actions: Vec<CorporateActionRecord>,
    pub notes: Vec<TrainingNoteRef>,
}

impl TrainingReportInput {
    pub fn from_state_and_bars(
        run_id: impl Into<String>,
        dataset_version: i64,
        hidden_mode: bool,
        state: SimState,
        raw_bars: &[crate::model::BarRecord],
        corporate_actions: Vec<CorporateActionRecord>,
        notes: Vec<TrainingNoteRef>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            dataset_version,
            hidden_mode,
            state,
            training_bars: raw_bars.iter().cloned().map(SimStepBar::from_bar).collect(),
            corporate_actions,
            notes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquityCurvePoint {
    pub step_index: usize,
    pub session_date: NaiveDate,
    pub close_at: DateTime<Utc>,
    pub cash: Decimal,
    pub position_quantity: Decimal,
    pub close_price: Decimal,
    pub position_market_value: Decimal,
    pub equity: Decimal,
    pub peak_equity: Decimal,
    pub drawdown: Decimal,
    pub drawdown_pct: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum RatioMetric {
    Computed(Decimal),
    NoClosedTrades,
    NoLosses,
    NoWins,
}

impl RatioMetric {
    pub fn value(&self) -> Option<Decimal> {
        match self {
            Self::Computed(v) => Some(*v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinRateSummary {
    pub status: WinRateStatus,
    pub closed_trade_count: usize,
    pub winning_count: usize,
    pub losing_count: usize,
    pub zero_pnl_count: usize,
    pub win_rate: Option<Decimal>,
    pub win_rate_pct: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WinRateStatus {
    Computed,
    NoClosedTrades,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectancySummary {
    pub status: WinRateStatus,
    pub sample_count: usize,
    pub value: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "compliance", rename_all = "snake_case")]
pub enum PlanCompliance {
    ExitedByPlannedBracket,
    DeviatedWithRationale { note_ids: Vec<String> },
    DeviatedWithoutRationale,
    Unplanned,
}

impl PlanCompliance {
    pub fn is_planned(&self) -> bool {
        !matches!(self, Self::Unplanned)
    }

    pub fn is_compliant(&self) -> bool {
        matches!(
            self,
            Self::ExitedByPlannedBracket | Self::DeviatedWithRationale { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanExecutionStatus {
    Computed,
    NoPlannedTrades,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanExecutionSummary {
    pub status: PlanExecutionStatus,
    pub planned_closed_group_count: usize,
    pub compliant_count: usize,
    pub exited_by_plan_count: usize,
    pub deviated_with_rationale_count: usize,
    pub deviated_without_rationale_count: usize,
    pub unplanned_closed_group_count: usize,
    pub unplanned_group_ids: Vec<String>,
    pub execution_rate: Option<Decimal>,
    pub execution_rate_pct: Option<Decimal>,
}

/// One completed `0 -> position -> 0` trade group supporting scaled entries and scaled exits (`S-44`, `M4-A-12`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedTradeGroup {
    pub group_id: String,
    pub instrument_id: String,
    pub entry_bar_index: usize,
    pub exit_bar_index: usize,
    pub entry_session_date: NaiveDate,
    pub exit_session_date: NaiveDate,
    pub entry_at: DateTime<Utc>,
    pub exit_at: DateTime<Utc>,
    pub entry_fill_ids: Vec<String>,
    pub exit_fill_ids: Vec<String>,
    /// Total split-adjusted base quantity entered across the cycle.
    pub total_quantity: Decimal,
    /// Weighted-average executed entry price in post-split terms (`S-44`).
    pub weighted_avg_entry_price: Decimal,
    /// Weighted-average executed exit price in post-split terms.
    pub weighted_avg_exit_price: Decimal,
    pub gross_pnl: Decimal,
    pub total_fees_quote: Decimal,
    pub realized_pnl: Decimal,
    pub min_path_price: Decimal,
    pub max_path_price: Decimal,
    /// Maximum Adverse Excursion per unit relative to `weighted_avg_entry_price` (`>= 0`).
    pub mae_per_unit: Decimal,
    pub mae_ratio: Decimal,
    pub mae_pct: Decimal,
    /// Maximum Favorable Excursion per unit relative to `weighted_avg_entry_price` (`>= 0`).
    pub mfe_per_unit: Decimal,
    pub mfe_ratio: Decimal,
    pub mfe_pct: Decimal,
    pub has_plan: bool,
    pub plan_compliance: PlanCompliance,
}

/// Summary of an unclosed position remaining at the end of the report window (`S-44`, `M4-A-12`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTradeGroupSummary {
    pub instrument_id: String,
    pub entry_session_date: NaiveDate,
    pub remaining_quantity: Decimal,
    pub carrying_cost_quote: Decimal,
    pub avg_unit_cost: Decimal,
    pub close_valuation_price: Decimal,
    pub market_value_quote: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl_from_partial_exits: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkStatus {
    Executed,
    NoBars,
    NoTradableBar,
    InsufficientCashForOneLot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuyAndHoldBenchmark {
    pub status: BenchmarkStatus,
    pub instrument_id: String,
    pub initial_cash: Decimal,
    pub entry_session_date: Option<NaiveDate>,
    pub entry_fill_price: Option<Decimal>,
    pub initial_quantity_bought: Decimal,
    pub ending_quantity: Decimal,
    pub ending_close_price: Option<Decimal>,
    pub cash_remaining: Decimal,
    pub dividend_income: Decimal,
    pub total_fees_quote: Decimal,
    pub ending_equity: Decimal,
    pub total_pnl: Decimal,
    pub total_return: Decimal,
    pub total_return_pct: Decimal,
    pub max_drawdown: Decimal,
    pub max_drawdown_pct: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleLimitationItem {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleLimitations {
    pub closed_trade_group_count: usize,
    pub small_sample_warning: bool,
    pub ambiguous_fill_count: u64,
    pub bar_path_assumption: String,
    pub liquidity_assumption: String,
    pub fee_profile_id: String,
    pub fee_assumption: String,
    pub hidden_mode_enabled: bool,
    pub hidden_mode_price_level_note: Option<String>,
    pub has_open_position_at_end: bool,
    pub open_position_valuation_note: Option<String>,
    pub ex_date_dividend_assumed: bool,
    pub ex_date_dividend_note: Option<String>,
    pub items: Vec<SampleLimitationItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingReport {
    pub report_version: String,
    pub engine_version: String,
    pub run_id: String,
    pub dataset_version: i64,
    pub fee_profile_version_id: String,
    pub quote_currency: String,
    pub visible_until: Option<DateTime<Utc>>,
    pub total_training_bars: usize,
    pub exposed_bars: usize,
    pub exposure_ratio: Decimal,
    pub exposure_pct: Decimal,
    pub initial_cash: Decimal,
    pub ending_cash: Decimal,
    pub open_position_market_value: Decimal,
    pub ending_equity: Decimal,
    pub total_pnl: Decimal,
    pub total_return: Decimal,
    pub total_return_pct: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub dividend_income: Decimal,
    pub total_fees_quote: Decimal,
    pub max_drawdown: Decimal,
    pub max_drawdown_pct: Decimal,
    pub max_drawdown_amount: Decimal,
    pub equity_curve: Vec<EquityCurvePoint>,
    pub win_rate: WinRateSummary,
    pub gross_profit: Decimal,
    pub gross_loss_abs: Decimal,
    pub average_win: Option<Decimal>,
    pub average_loss_abs: Option<Decimal>,
    pub payoff_ratio: RatioMetric,
    pub profit_factor: RatioMetric,
    pub expectancy: ExpectancySummary,
    pub plan_execution: PlanExecutionSummary,
    pub closed_trade_groups: Vec<ClosedTradeGroup>,
    pub open_positions: Vec<OpenTradeGroupSummary>,
    pub excess_return_vs_benchmark: Decimal,
    pub excess_return_vs_benchmark_pct: Decimal,
    pub benchmark: BuyAndHoldBenchmark,
    pub limitations: SampleLimitations,
}

fn bar_stage_endpoints(bar: &crate::model::BarRecord, stage: BarPathStage) -> (Decimal, Decimal) {
    match stage {
        BarPathStage::AtOpen => (bar.open, bar.open),
        BarPathStage::OpenToLow => (bar.open, bar.low),
        BarPathStage::LowToHigh => (bar.low, bar.high),
        BarPathStage::HighToClose => (bar.high, bar.close),
    }
}

/// Compute `(min_price, max_price)` along `O -> L -> H -> C` strictly between
/// `(start_stage, start_price)` and `(end_stage, end_price)` on `bar` (`S-44`, `M4-A-12`).
pub fn bar_path_slice_extrema(
    bar: &crate::model::BarRecord,
    start_stage: BarPathStage,
    start_price: Decimal,
    end_stage: BarPathStage,
    end_price: Decimal,
) -> (Decimal, Decimal) {
    if start_stage >= end_stage {
        return (
            canon_decimal(start_price.min(end_price)),
            canon_decimal(start_price.max(end_price)),
        );
    }

    let all_stages = [
        BarPathStage::AtOpen,
        BarPathStage::OpenToLow,
        BarPathStage::LowToHigh,
        BarPathStage::HighToClose,
    ];
    let mut min_p = start_price;
    let mut max_p = start_price;
    let mut observe = |p: Decimal| {
        if p < min_p {
            min_p = p;
        }
        if p > max_p {
            max_p = p;
        }
    };

    let (_, s0_end) = bar_stage_endpoints(bar, start_stage);
    observe(s0_end);

    for &st in &all_stages {
        if st > start_stage && st < end_stage {
            let (a, b) = bar_stage_endpoints(bar, st);
            observe(a);
            observe(b);
        }
    }

    let (s1_start, _) = bar_stage_endpoints(bar, end_stage);
    observe(s1_start);
    observe(end_price);

    (canon_decimal(min_p), canon_decimal(max_p))
}

#[derive(Debug, Clone)]
struct ActiveCycle {
    instrument_id: String,
    entry_bar_index: usize,
    entry_session_date: NaiveDate,
    entry_at: DateTime<Utc>,
    entry_bar_close_at: DateTime<Utc>,
    first_entry_stage: BarPathStage,
    first_entry_raw_price: Decimal,
    entry_fills: Vec<SimFill>,
    exit_fills: Vec<SimFill>,
    involved_order_ids: BTreeSet<String>,
    scaled_entry_qty: Decimal,
    scaled_entry_notional: Decimal,
    scaled_exit_qty: Decimal,
    scaled_exit_notional: Decimal,
    total_buy_cost_quote: Decimal,
    total_sell_net_proceeds_quote: Decimal,
    total_fees_quote: Decimal,
    realized_pnl_accum: Decimal,
    min_path_price: Option<Decimal>,
    max_path_price: Option<Decimal>,
}

impl ActiveCycle {
    fn observe_extrema(&mut self, lo: Decimal, hi: Decimal) {
        self.min_path_price = Some(match self.min_path_price {
            Some(cur) => cur.min(lo),
            None => lo,
        });
        self.max_path_price = Some(match self.max_path_price {
            Some(cur) => cur.max(hi),
            None => hi,
        });
    }

    fn apply_split(&mut self, ratio: Decimal) {
        self.scaled_entry_qty = canon_decimal(self.scaled_entry_qty * ratio);
        self.scaled_exit_qty = canon_decimal(self.scaled_exit_qty * ratio);
        if let Some(lo) = self.min_path_price {
            self.min_path_price = Some(round_metric(lo / ratio, 8));
        }
        if let Some(hi) = self.max_path_price {
            self.max_path_price = Some(round_metric(hi / ratio, 8));
        }
    }
}

fn determine_plan_compliance(
    state: &SimState,
    cycle: &ActiveCycle,
    exit_bar_open_at: DateTime<Utc>,
    exit_bar_close_at: DateTime<Utc>,
    notes: &[TrainingNoteRef],
) -> (bool, PlanCompliance) {
    let mut order_ids = cycle.involved_order_ids.clone();

    // Include any planned bracket / OCO orders linked to the entry orders or submitted while this cycle was open
    for ord in state.orders.values() {
        if ord.instrument_id != cycle.instrument_id {
            continue;
        }
        if let Some(parent_id) = &ord.parent_order_id {
            if cycle.involved_order_ids.contains(parent_id) {
                order_ids.insert(ord.order_id.clone());
            }
        }
        if ord.has_trade_plan
            && ord.parent_order_id.is_none()
            && ord.submitted_at >= cycle.entry_bar_close_at
            && ord.submitted_at < exit_bar_close_at
        {
            order_ids.insert(ord.order_id.clone());
        }
        let _ = exit_bar_open_at;
    }

    let has_plan = cycle.entry_fills.iter().any(|f| f.has_trade_plan)
        || cycle.exit_fills.iter().any(|f| f.has_trade_plan)
        || order_ids
            .iter()
            .filter_map(|id| state.orders.get(id))
            .any(|o| o.has_trade_plan);

    if !has_plan {
        return (false, PlanCompliance::Unplanned);
    }

    let all_exits_by_planned_bracket = !cycle.exit_fills.is_empty()
        && cycle
            .exit_fills
            .iter()
            .all(|f| f.has_trade_plan && f.group_role.is_planned_exit());

    if all_exits_by_planned_bracket {
        return (true, PlanCompliance::ExitedByPlannedBracket);
    }

    // Check if the trader recorded a non-empty rationale note explaining the deviation
    let mut matching_notes: Vec<String> = Vec::new();
    for note in notes {
        if note.rationale.trim().is_empty() {
            continue;
        }
        let linked_match = note
            .linked_order_id
            .as_ref()
            .is_some_and(|oid| order_ids.contains(oid));
        let window_match = note.linked_order_id.is_none()
            && note.visible_until >= cycle.entry_bar_close_at
            && note.visible_until <= exit_bar_close_at;
        if linked_match || window_match {
            matching_notes.push(note.note_id.clone());
        }
    }
    matching_notes.sort();
    matching_notes.dedup();

    if !matching_notes.is_empty() {
        (
            true,
            PlanCompliance::DeviatedWithRationale {
                note_ids: matching_notes,
            },
        )
    } else {
        (true, PlanCompliance::DeviatedWithoutRationale)
    }
}

/// Compute Buy-and-Hold benchmark over the exact same window, `initial_cash`, `instrument_spec`,
/// `corporate_actions`, and `fee_profile` (`S-44`, `M4-A-12`).
pub fn compute_buy_and_hold_benchmark(
    initial_cash: Decimal,
    instrument_spec: &SimInstrumentSpec,
    fee_profile: &FeeProfileVersion,
    bars: &[SimStepBar],
    corporate_actions: &[CorporateActionRecord],
) -> BuyAndHoldBenchmark {
    let empty_bench = |status: BenchmarkStatus| BuyAndHoldBenchmark {
        status,
        instrument_id: instrument_spec.instrument_id.clone(),
        initial_cash: canon_decimal(initial_cash),
        entry_session_date: None,
        entry_fill_price: None,
        initial_quantity_bought: Decimal::ZERO,
        ending_quantity: Decimal::ZERO,
        ending_close_price: bars.last().map(|b| canon_decimal(b.bar.close)),
        cash_remaining: canon_decimal(initial_cash),
        dividend_income: Decimal::ZERO,
        total_fees_quote: Decimal::ZERO,
        ending_equity: canon_decimal(initial_cash),
        total_pnl: Decimal::ZERO,
        total_return: Decimal::ZERO,
        total_return_pct: Decimal::ZERO,
        max_drawdown: Decimal::ZERO,
        max_drawdown_pct: Decimal::ZERO,
    };

    if bars.is_empty() {
        return empty_bench(BenchmarkStatus::NoBars);
    }
    let Some(first_tradable_idx) = bars.iter().position(|sb| sb.is_effectively_tradable()) else {
        return empty_bench(BenchmarkStatus::NoTradableBar);
    };

    let Ok(mut bench_state) = SimState::new(
        instrument_spec.quote_currency.clone(),
        initial_cash,
        fee_profile.clone(),
        vec![instrument_spec.clone()],
        None,
        BTreeMap::new(),
    ) else {
        return empty_bench(BenchmarkStatus::NoTradableBar);
    };

    // Advance any leading untradable bars in cash
    for sb in &bars[..first_tradable_idx] {
        if let Ok((next_s, _)) =
            step_multi_bars(&bench_state, std::slice::from_ref(sb), corporate_actions)
        {
            bench_state = next_s;
        }
    }

    let first_bar = &bars[first_tradable_idx].bar;
    let spread_delta = (first_bar.open * fee_profile.half_spread_bps) / Decimal::from(10_000);
    let slip_delta = fee_profile
        .slippage
        .delta_for_price(first_bar.open, instrument_spec.tick_size);
    let exec_open_price = round_price_adverse_tick(
        first_bar.open + spread_delta + slip_delta,
        OrderSide::Buy,
        instrument_spec.tick_size,
    );

    let max_lots_dec = (initial_cash / exec_open_price / instrument_spec.lot_size).floor();
    let max_lots = max_lots_dec.to_u128().unwrap_or(0);
    if max_lots == 0 {
        return empty_bench(BenchmarkStatus::InsufficientCashForOneLot);
    }

    let affordable_settlement = |lots: u128| {
        if lots == 0 {
            return None;
        }
        let qty = canon_decimal(Decimal::from(lots) * instrument_spec.lot_size);
        let settled = settle_order_fees(
            fee_profile,
            OrderSide::Buy,
            &instrument_spec.base_asset_id,
            &instrument_spec.quote_currency,
            &[SyntheticFillInput {
                fill_id: "BENCH-BUY".to_string(),
                quantity: qty,
                fill_price: exec_open_price,
            }],
        )
        .ok()?;
        let cash_needed = -settled.fills[0].net_quote_cash_delta;
        (cash_needed <= initial_cash).then_some((qty, settled))
    };

    // Binary search maximum affordable integer lot count in `1..=max_lots`
    let mut lo: u128 = 1;
    let mut hi: u128 = max_lots;
    let mut best: Option<(Decimal, crate::sim::OrderFeeSettlement)> = None;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        if let Some(res) = affordable_settlement(mid) {
            best = Some(res);
            if mid == u128::MAX {
                break;
            }
            lo = mid + 1;
        } else {
            if mid == 1 {
                break;
            }
            hi = mid - 1;
        }
    }

    let Some((gross_qty, settlement)) = best else {
        return empty_bench(BenchmarkStatus::InsufficientCashForOneLot);
    };
    let settled_fill = &settlement.fills[0];
    let net_qty = settled_fill.net_received_base_quantity;

    // Step `first_tradable_idx` first to apply any pre-open corporate actions on that date cleanly while flat,
    // then record the AtOpen benchmark purchase on `first_tradable_idx`, and step remaining bars!
    if let Ok((mut s_first, _)) = step_multi_bars(
        &bench_state,
        std::slice::from_ref(&bars[first_tradable_idx]),
        corporate_actions,
    ) {
        s_first.cash_balance =
            canon_decimal(s_first.cash_balance + settled_fill.net_quote_cash_delta);
        s_first.total_fees_quote =
            canon_decimal(s_first.total_fees_quote + settled_fill.fee_quote_value);
        let lot_cost = canon_decimal(-settled_fill.net_quote_cash_delta);
        s_first
            .lots_by_instrument
            .entry(instrument_spec.instrument_id.clone())
            .or_default()
            .push(crate::sim::SimLot {
                lot_id: "BENCH-LOT-1".to_string(),
                source_fill_id: "BENCH-FILL-1".to_string(),
                instrument_id: instrument_spec.instrument_id.clone(),
                acquired_at: first_bar.open_at,
                original_quantity: net_qty,
                remaining_quantity: net_qty,
                original_cost_quote: lot_cost,
                remaining_cost_quote: lot_cost,
            });
        if let Some(last_snap) = s_first.bar_snapshots.last_mut() {
            last_snap.had_buy_fill = true;
            last_snap.cash_after = s_first.cash_balance;
            last_snap.position_qty_after = net_qty;
            last_snap.position_cost_after = lot_cost;
            last_snap.equity_at_close =
                canon_decimal(s_first.cash_balance + net_qty * first_bar.close);
        }
        bench_state = s_first;
    }

    for sb in &bars[first_tradable_idx + 1..] {
        if let Ok((next_s, _)) =
            step_multi_bars(&bench_state, std::slice::from_ref(sb), corporate_actions)
        {
            bench_state = next_s;
        }
    }

    let mut peak = initial_cash;
    let mut max_dd = Decimal::ZERO;
    for snap in &bench_state.bar_snapshots {
        if snap.equity_at_close > peak {
            peak = snap.equity_at_close;
        }
        if peak > Decimal::ZERO {
            let dd = (Decimal::ONE - snap.equity_at_close / peak).max(Decimal::ZERO);
            if dd > max_dd {
                max_dd = dd;
            }
        }
    }

    let ending_equity = bench_state.total_equity_at_last_close();
    let total_pnl = canon_decimal(ending_equity - initial_cash);
    let total_return = round_metric(total_pnl / initial_cash, 8);
    let total_return_pct = round_metric(total_pnl * Decimal::from(100) / initial_cash, 6);
    let max_dd_rounded = round_metric(max_dd, 8);
    let max_dd_pct = round_metric(max_dd * Decimal::from(100), 6);
    let _ = gross_qty;

    BuyAndHoldBenchmark {
        status: BenchmarkStatus::Executed,
        instrument_id: instrument_spec.instrument_id.clone(),
        initial_cash: canon_decimal(initial_cash),
        entry_session_date: Some(first_bar.session_date),
        entry_fill_price: Some(exec_open_price),
        initial_quantity_bought: net_qty,
        ending_quantity: bench_state.position_quantity(&instrument_spec.instrument_id),
        ending_close_price: bars.last().map(|b| canon_decimal(b.bar.close)),
        cash_remaining: bench_state.cash_balance,
        dividend_income: bench_state.dividend_income,
        total_fees_quote: bench_state.total_fees_quote,
        ending_equity,
        total_pnl,
        total_return,
        total_return_pct,
        max_drawdown: max_dd_rounded,
        max_drawdown_pct: max_dd_pct,
    }
}

/// Compute the full M4 Training Report purely in `Decimal` (`S-44`, `M4-A-12`).
pub fn compute_training_report(input: &TrainingReportInput) -> TrainingReport {
    let state = &input.state;
    let initial_cash = canon_decimal(state.initial_cash);
    let cutoff_opt = state.visible_until;

    // Resolve visible training bars (never reading past `state.visible_until`)
    let mut visible_bars: Vec<SimStepBar> = if !input.training_bars.is_empty() {
        input
            .training_bars
            .iter()
            .filter(|sb| {
                sb.bar.is_final
                    && match cutoff_opt {
                        Some(cutoff) => sb.bar.close_at <= cutoff,
                        None => true,
                    }
            })
            .cloned()
            .collect()
    } else {
        state
            .bar_snapshots
            .iter()
            .map(|snap| SimStepBar {
                bar: crate::model::BarRecord {
                    id: format!("SNAP-{}", snap.step_index),
                    instrument_id: snap.instrument_id.clone(),
                    timeframe: "1d".to_string(),
                    session: "regular".to_string(),
                    session_date: snap.session_date,
                    open_at: snap.open_at,
                    close_at: snap.close_at,
                    open: snap.open,
                    high: snap.high,
                    low: snap.low,
                    close: snap.close,
                    volume: snap.volume,
                    source: "sim_snapshot".to_string(),
                    adjustment: crate::model::AdjustmentKind::Raw,
                    is_final: true,
                    valid_from_version: input.dataset_version,
                    valid_to_version: None,
                    raw_payload_hash: None,
                },
                halted: false,
                tradable: snap.tradable,
            })
            .collect()
    };
    visible_bars.sort_by(|a, b| {
        a.bar
            .close_at
            .cmp(&b.bar.close_at)
            .then_with(|| a.bar.instrument_id.cmp(&b.bar.instrument_id))
    });

    let max_vis_date = visible_bars.last().map(|b| b.bar.session_date);
    let visible_cas: Vec<CorporateActionRecord> = input
        .corporate_actions
        .iter()
        .filter(|ca| match max_vis_date {
            Some(d) => ca.ex_date <= d,
            None => false,
        })
        .cloned()
        .collect();

    // 1. Close-to-close equity curve, max drawdown, and time-in-market exposure
    let mut equity_curve = Vec::with_capacity(state.bar_snapshots.len());
    let mut peak_equity = initial_cash;
    let mut max_drawdown_ratio = Decimal::ZERO;
    let mut max_drawdown_amount = Decimal::ZERO;
    let mut exposed_bars = 0_usize;

    for (idx, snap) in state.bar_snapshots.iter().enumerate() {
        let pos_mv = canon_decimal(snap.position_qty_after * snap.close);
        let eq = snap.equity_at_close;
        if eq > peak_equity {
            peak_equity = eq;
        }
        let dd_amt = canon_decimal((peak_equity - eq).max(Decimal::ZERO));
        if dd_amt > max_drawdown_amount {
            max_drawdown_amount = dd_amt;
        }
        let dd_ratio = if peak_equity > Decimal::ZERO {
            (dd_amt / peak_equity).max(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };
        if dd_ratio > max_drawdown_ratio {
            max_drawdown_ratio = dd_ratio;
        }

        // Exposed if holding > 0 at bar close OR flat before bar and opened + closed same bar
        if snap.position_qty_after > Decimal::ZERO
            || (snap.position_qty_before == Decimal::ZERO && snap.had_buy_fill)
        {
            exposed_bars += 1;
        }

        equity_curve.push(EquityCurvePoint {
            step_index: idx + 1,
            session_date: snap.session_date,
            close_at: snap.close_at,
            cash: snap.cash_after,
            position_quantity: snap.position_qty_after,
            close_price: snap.close,
            position_market_value: pos_mv,
            equity: eq,
            peak_equity,
            drawdown: round_metric(dd_ratio, 8),
            drawdown_pct: round_metric(dd_ratio * Decimal::from(100), 6),
        });
    }

    let total_training_bars = if !state.bar_snapshots.is_empty() {
        state.bar_snapshots.len()
    } else {
        visible_bars.len()
    };
    let exposure_ratio = if total_training_bars > 0 {
        round_metric(
            Decimal::from(exposed_bars) / Decimal::from(total_training_bars),
            8,
        )
    } else {
        Decimal::ZERO
    };
    let exposure_pct = round_metric(exposure_ratio * Decimal::from(100), 6);

    // 2. Reconstruct closed trade groups (`0 -> pos -> 0`) and open trade groups
    let mut fills_by_bar_close: BTreeMap<(String, DateTime<Utc>), Vec<&SimFill>> = BTreeMap::new();
    for fill in &state.fills {
        fills_by_bar_close
            .entry((fill.instrument_id.clone(), fill.bar_close_at))
            .or_default()
            .push(fill);
    }
    for list in fills_by_bar_close.values_mut() {
        list.sort_by_key(|f| f.fill_seq);
    }

    let mut split_map: BTreeMap<(String, NaiveDate), Decimal> = BTreeMap::new();
    for ca in &visible_cas {
        if ca.action_type == crate::model::CorporateActionType::Split
            && ca.ratio_or_amount > Decimal::ZERO
        {
            split_map.insert((ca.instrument_id.clone(), ca.ex_date), ca.ratio_or_amount);
        }
    }

    let mut closed_trade_groups: Vec<ClosedTradeGroup> = Vec::new();
    let mut active_by_inst: BTreeMap<String, (Decimal, ActiveCycle)> = BTreeMap::new();
    let mut next_group_num = 1_usize;

    for (bar_idx, sb) in visible_bars.iter().enumerate() {
        let bar = &sb.bar;
        let inst_id = &bar.instrument_id;

        // Pre-open split adjustment on any already-open cycle
        if let Some(&ratio) = split_map.get(&(inst_id.clone(), bar.session_date)) {
            if let Some((cur_qty, cycle)) = active_by_inst.get_mut(inst_id) {
                *cur_qty = canon_decimal(*cur_qty * ratio);
                cycle.apply_split(ratio);
            }
        }

        let bar_fills = fills_by_bar_close
            .get(&(inst_id.clone(), bar.close_at))
            .cloned()
            .unwrap_or_default();

        if bar_fills.is_empty() {
            // If a cycle is open across this entire bar, observe `[L, H]`
            if let Some((_, cycle)) = active_by_inst.get_mut(inst_id) {
                cycle.observe_extrema(bar.low, bar.high);
            }
            continue;
        }

        // Track path cursor `(cur_stage, cur_price)` while open on this bar
        let mut open_cursor: Option<(BarPathStage, Decimal)> = active_by_inst
            .contains_key(inst_id)
            .then_some((BarPathStage::AtOpen, bar.open));

        for fill in bar_fills {
            match fill.side {
                OrderSide::Buy => {
                    if let Some((cur_qty, cycle)) = active_by_inst.get_mut(inst_id) {
                        // Scale-in on existing cycle
                        *cur_qty = canon_decimal(*cur_qty + fill.net_base_quantity);
                        cycle.entry_fills.push(fill.clone());
                        cycle.involved_order_ids.insert(fill.order_id.clone());
                        cycle.scaled_entry_qty =
                            canon_decimal(cycle.scaled_entry_qty + fill.net_base_quantity);
                        cycle.scaled_entry_notional = canon_decimal(
                            cycle.scaled_entry_notional + fill.net_base_quantity * fill.fill_price,
                        );
                        let buy_cash_cost = if fill
                            .fee_currency
                            .eq_ignore_ascii_case(&state.quote_currency)
                        {
                            fill.gross_quantity * fill.fill_price + fill.fee_amount
                        } else {
                            fill.gross_quantity * fill.fill_price
                        };
                        cycle.total_buy_cost_quote =
                            canon_decimal(cycle.total_buy_cost_quote + buy_cash_cost);
                        cycle.total_fees_quote =
                            canon_decimal(cycle.total_fees_quote + fill.fee_quote_value);
                    } else {
                        // Open new `0 -> pos` cycle at `(fill.path_stage, fill.raw_path_price)`
                        let mut involved = BTreeSet::new();
                        involved.insert(fill.order_id.clone());
                        let buy_cash_cost = if fill
                            .fee_currency
                            .eq_ignore_ascii_case(&state.quote_currency)
                        {
                            fill.gross_quantity * fill.fill_price + fill.fee_amount
                        } else {
                            fill.gross_quantity * fill.fill_price
                        };
                        let cycle = ActiveCycle {
                            instrument_id: inst_id.clone(),
                            entry_bar_index: bar_idx,
                            entry_session_date: bar.session_date,
                            entry_at: bar.open_at,
                            entry_bar_close_at: bar.close_at,
                            first_entry_stage: fill.path_stage,
                            first_entry_raw_price: fill.raw_path_price,
                            entry_fills: vec![fill.clone()],
                            exit_fills: Vec::new(),
                            involved_order_ids: involved,
                            scaled_entry_qty: fill.net_base_quantity,
                            scaled_entry_notional: canon_decimal(
                                fill.net_base_quantity * fill.fill_price,
                            ),
                            scaled_exit_qty: Decimal::ZERO,
                            scaled_exit_notional: Decimal::ZERO,
                            total_buy_cost_quote: canon_decimal(buy_cash_cost),
                            total_sell_net_proceeds_quote: Decimal::ZERO,
                            total_fees_quote: fill.fee_quote_value,
                            realized_pnl_accum: Decimal::ZERO,
                            min_path_price: None,
                            max_path_price: None,
                        };
                        active_by_inst.insert(inst_id.clone(), (fill.net_base_quantity, cycle));
                        open_cursor = Some((fill.path_stage, fill.raw_path_price));
                    }
                }
                OrderSide::Sell => {
                    let closed_now = if let Some((cur_qty, cycle)) = active_by_inst.get_mut(inst_id)
                    {
                        *cur_qty =
                            canon_decimal((*cur_qty - fill.net_base_quantity).max(Decimal::ZERO));
                        cycle.exit_fills.push(fill.clone());
                        cycle.involved_order_ids.insert(fill.order_id.clone());
                        cycle.scaled_exit_qty =
                            canon_decimal(cycle.scaled_exit_qty + fill.net_base_quantity);
                        cycle.scaled_exit_notional = canon_decimal(
                            cycle.scaled_exit_notional + fill.net_base_quantity * fill.fill_price,
                        );
                        let sell_net =
                            canon_decimal(fill.gross_quantity * fill.fill_price - fill.fee_amount);
                        cycle.total_sell_net_proceeds_quote =
                            canon_decimal(cycle.total_sell_net_proceeds_quote + sell_net);
                        cycle.total_fees_quote =
                            canon_decimal(cycle.total_fees_quote + fill.fee_quote_value);
                        cycle.realized_pnl_accum =
                            canon_decimal(cycle.realized_pnl_accum + fill.realized_pnl_delta);
                        cur_qty.is_zero()
                    } else {
                        false
                    };

                    if closed_now {
                        let (_, mut cycle) = active_by_inst.remove(inst_id).unwrap();
                        if let Some((s_start, p_start)) = open_cursor.take() {
                            let (lo, hi) = bar_path_slice_extrema(
                                bar,
                                s_start,
                                p_start,
                                fill.path_stage,
                                fill.raw_path_price,
                            );
                            cycle.observe_extrema(lo, hi);
                        }

                        let avg_entry =
                            round_metric(cycle.scaled_entry_notional / cycle.scaled_entry_qty, 8);
                        let avg_exit = if cycle.scaled_exit_qty > Decimal::ZERO {
                            round_metric(cycle.scaled_exit_notional / cycle.scaled_exit_qty, 8)
                        } else {
                            fill.fill_price
                        };
                        let min_p = cycle.min_path_price.unwrap_or(avg_entry);
                        let max_p = cycle.max_path_price.unwrap_or(avg_entry);
                        let mae_unit = canon_decimal((avg_entry - min_p).max(Decimal::ZERO));
                        let mfe_unit = canon_decimal((max_p - avg_entry).max(Decimal::ZERO));
                        let mae_ratio = round_metric(mae_unit / avg_entry, 8);
                        let mae_pct = round_metric(mae_unit * Decimal::from(100) / avg_entry, 6);
                        let mfe_ratio = round_metric(mfe_unit / avg_entry, 8);
                        let mfe_pct = round_metric(mfe_unit * Decimal::from(100) / avg_entry, 6);
                        let gross_pnl =
                            canon_decimal(cycle.scaled_exit_notional - cycle.scaled_entry_notional);
                        let (has_plan, compliance) = determine_plan_compliance(
                            state,
                            &cycle,
                            bar.open_at,
                            bar.close_at,
                            &input.notes,
                        );

                        let gid = format!("TRD-GRP-{next_group_num:04}");
                        next_group_num += 1;
                        let _ = (
                            cycle.first_entry_stage,
                            cycle.first_entry_raw_price,
                            cycle.total_buy_cost_quote,
                            cycle.total_sell_net_proceeds_quote,
                        );

                        closed_trade_groups.push(ClosedTradeGroup {
                            group_id: gid,
                            instrument_id: cycle.instrument_id,
                            entry_bar_index: cycle.entry_bar_index,
                            exit_bar_index: bar_idx,
                            entry_session_date: cycle.entry_session_date,
                            exit_session_date: bar.session_date,
                            entry_at: cycle.entry_at,
                            exit_at: bar.close_at,
                            entry_fill_ids: cycle
                                .entry_fills
                                .iter()
                                .map(|f| f.fill_id.clone())
                                .collect(),
                            exit_fill_ids: cycle
                                .exit_fills
                                .iter()
                                .map(|f| f.fill_id.clone())
                                .collect(),
                            total_quantity: cycle.scaled_entry_qty,
                            weighted_avg_entry_price: avg_entry,
                            weighted_avg_exit_price: avg_exit,
                            gross_pnl,
                            total_fees_quote: cycle.total_fees_quote,
                            realized_pnl: cycle.realized_pnl_accum,
                            min_path_price: min_p,
                            max_path_price: max_p,
                            mae_per_unit: mae_unit,
                            mae_ratio,
                            mae_pct,
                            mfe_per_unit: mfe_unit,
                            mfe_ratio,
                            mfe_pct,
                            has_plan,
                            plan_compliance: compliance,
                        });
                    }
                }
            }
        }

        // If a cycle is still open at the close of `bar`, observe remaining path from `open_cursor` to `(HighToClose, bar.close)`
        if let (Some((s_start, p_start)), Some((_, cycle))) =
            (open_cursor, active_by_inst.get_mut(inst_id))
        {
            let (lo, hi) =
                bar_path_slice_extrema(bar, s_start, p_start, BarPathStage::HighToClose, bar.close);
            cycle.observe_extrema(lo, hi);
        }
    }

    // 3. Open positions remaining at the end of the training window
    let mut open_positions = Vec::new();
    let mut open_position_market_value = Decimal::ZERO;
    let mut total_carrying_cost_open = Decimal::ZERO;

    for inst_id in state.instruments.keys() {
        if let Some(holding) = state.holding(inst_id) {
            let close_price = state
                .last_close_by_instrument
                .get(inst_id)
                .copied()
                .unwrap_or(holding.avg_unit_cost);
            let mv = canon_decimal(holding.quantity * close_price);
            let unrea = canon_decimal(mv - holding.total_cost_quote);
            open_position_market_value = canon_decimal(open_position_market_value + mv);
            total_carrying_cost_open =
                canon_decimal(total_carrying_cost_open + holding.total_cost_quote);
            let (entry_date, partial_realized) = active_by_inst
                .get(inst_id)
                .map(|(_, c)| (c.entry_session_date, c.realized_pnl_accum))
                .unwrap_or_else(|| {
                    let fallback_date = visible_bars
                        .first()
                        .map(|b| b.bar.session_date)
                        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
                    (fallback_date, Decimal::ZERO)
                });
            open_positions.push(OpenTradeGroupSummary {
                instrument_id: inst_id.clone(),
                entry_session_date: entry_date,
                remaining_quantity: holding.quantity,
                carrying_cost_quote: holding.total_cost_quote,
                avg_unit_cost: round_metric(holding.avg_unit_cost, 8),
                close_valuation_price: close_price,
                market_value_quote: mv,
                unrealized_pnl: unrea,
                realized_pnl_from_partial_exits: partial_realized,
            });
        }
    }

    let ending_cash = state.cash_balance;
    let ending_equity = canon_decimal(ending_cash + open_position_market_value);
    let unrealized_pnl = canon_decimal(open_position_market_value - total_carrying_cost_open);
    let realized_pnl = state.realized_pnl;
    let dividend_income = state.dividend_income;
    let total_fees_quote = state.total_fees_quote;
    let total_pnl = canon_decimal(ending_equity - initial_cash);
    let total_return = round_metric(total_pnl / initial_cash, 8);
    let total_return_pct = round_metric(total_pnl * Decimal::from(100) / initial_cash, 6);

    // 4. Trade group statistics: Win Rate, Payoff Ratio, Profit Factor, Expectancy
    let closed_count = closed_trade_groups.len();
    let mut winning_count = 0_usize;
    let mut losing_count = 0_usize;
    let mut zero_pnl_count = 0_usize;
    let mut gross_profit = Decimal::ZERO;
    let mut gross_loss_abs = Decimal::ZERO;

    for g in &closed_trade_groups {
        if g.realized_pnl > Decimal::ZERO {
            winning_count += 1;
            gross_profit = canon_decimal(gross_profit + g.realized_pnl);
        } else if g.realized_pnl < Decimal::ZERO {
            losing_count += 1;
            gross_loss_abs = canon_decimal(gross_loss_abs + g.realized_pnl.abs());
        } else {
            zero_pnl_count += 1;
        }
    }

    let win_rate = if closed_count == 0 {
        WinRateSummary {
            status: WinRateStatus::NoClosedTrades,
            closed_trade_count: 0,
            winning_count: 0,
            losing_count: 0,
            zero_pnl_count: 0,
            win_rate: None,
            win_rate_pct: None,
        }
    } else {
        let wr = round_metric(
            Decimal::from(winning_count) / Decimal::from(closed_count),
            8,
        );
        let wr_pct = round_metric(
            Decimal::from(winning_count) * Decimal::from(100) / Decimal::from(closed_count),
            6,
        );
        WinRateSummary {
            status: WinRateStatus::Computed,
            closed_trade_count: closed_count,
            winning_count,
            losing_count,
            zero_pnl_count,
            win_rate: Some(wr),
            win_rate_pct: Some(wr_pct),
        }
    };

    let average_win =
        (winning_count > 0).then(|| round_metric(gross_profit / Decimal::from(winning_count), 8));
    let average_loss_abs =
        (losing_count > 0).then(|| round_metric(gross_loss_abs / Decimal::from(losing_count), 8));

    let profit_factor = if closed_count == 0 {
        RatioMetric::NoClosedTrades
    } else if losing_count == 0 || gross_loss_abs.is_zero() {
        RatioMetric::NoLosses
    } else {
        RatioMetric::Computed(round_metric(gross_profit / gross_loss_abs, 8))
    };

    let payoff_ratio = if closed_count == 0 {
        RatioMetric::NoClosedTrades
    } else if losing_count == 0 {
        RatioMetric::NoLosses
    } else if winning_count == 0 {
        RatioMetric::NoWins
    } else {
        let avg_w = gross_profit / Decimal::from(winning_count);
        let avg_l = gross_loss_abs / Decimal::from(losing_count);
        RatioMetric::Computed(round_metric(avg_w / avg_l, 8))
    };

    let expectancy = if closed_count == 0 {
        ExpectancySummary {
            status: WinRateStatus::NoClosedTrades,
            sample_count: 0,
            value: None,
        }
    } else {
        let net_closed_pnl = canon_decimal(gross_profit - gross_loss_abs);
        ExpectancySummary {
            status: WinRateStatus::Computed,
            sample_count: closed_count,
            value: Some(round_metric(
                net_closed_pnl / Decimal::from(closed_count),
                8,
            )),
        }
    };

    // 5. Plan execution rate
    let mut planned_count = 0_usize;
    let mut exited_by_plan_count = 0_usize;
    let mut deviated_with_note_count = 0_usize;
    let mut deviated_without_note_count = 0_usize;
    let mut unplanned_group_ids = Vec::new();

    for g in &closed_trade_groups {
        match &g.plan_compliance {
            PlanCompliance::ExitedByPlannedBracket => {
                planned_count += 1;
                exited_by_plan_count += 1;
            }
            PlanCompliance::DeviatedWithRationale { .. } => {
                planned_count += 1;
                deviated_with_note_count += 1;
            }
            PlanCompliance::DeviatedWithoutRationale => {
                planned_count += 1;
                deviated_without_note_count += 1;
            }
            PlanCompliance::Unplanned => {
                unplanned_group_ids.push(g.group_id.clone());
            }
        }
    }
    let compliant_count = exited_by_plan_count + deviated_with_note_count;
    let plan_execution = if planned_count == 0 {
        PlanExecutionSummary {
            status: PlanExecutionStatus::NoPlannedTrades,
            planned_closed_group_count: 0,
            compliant_count: 0,
            exited_by_plan_count: 0,
            deviated_with_rationale_count: 0,
            deviated_without_rationale_count: 0,
            unplanned_closed_group_count: unplanned_group_ids.len(),
            unplanned_group_ids,
            execution_rate: None,
            execution_rate_pct: None,
        }
    } else {
        let rate = round_metric(
            Decimal::from(compliant_count) / Decimal::from(planned_count),
            8,
        );
        let rate_pct = round_metric(
            Decimal::from(compliant_count) * Decimal::from(100) / Decimal::from(planned_count),
            6,
        );
        PlanExecutionSummary {
            status: PlanExecutionStatus::Computed,
            planned_closed_group_count: planned_count,
            compliant_count,
            exited_by_plan_count,
            deviated_with_rationale_count: deviated_with_note_count,
            deviated_without_rationale_count: deviated_without_note_count,
            unplanned_closed_group_count: unplanned_group_ids.len(),
            unplanned_group_ids,
            execution_rate: Some(rate),
            execution_rate_pct: Some(rate_pct),
        }
    };

    // 6. Buy-and-Hold Benchmark (primary instrument in training session)
    let primary_spec = state
        .instruments
        .values()
        .next()
        .cloned()
        .expect("SimState always has at least one instrument");
    let primary_bars: Vec<SimStepBar> = visible_bars
        .iter()
        .filter(|b| b.bar.instrument_id == primary_spec.instrument_id)
        .cloned()
        .collect();
    let benchmark = compute_buy_and_hold_benchmark(
        initial_cash,
        &primary_spec,
        &state.fee_profile,
        &primary_bars,
        &visible_cas,
    );
    let excess_return = canon_decimal(total_return - benchmark.total_return);
    let excess_return_pct = canon_decimal(total_return_pct - benchmark.total_return_pct);

    // 7. Structured sample limitations (`S-44`, `M4-A-12`)
    let small_sample_warning = closed_count < 30;
    let has_open_pos = !open_positions.is_empty();
    let mut items = vec![
        SampleLimitationItem {
            code: "SAMPLE_SIZE".to_string(),
            message: format!(
                "Closed trade groups: {closed_count}{}",
                if small_sample_warning {
                    " (small sample size; statistical ratios have wide uncertainty)"
                } else {
                    ""
                }
            ),
        },
        SampleLimitationItem {
            code: "BAR_PATH_ASSUMPTION".to_string(),
            message: "Daily OHLC bars are traversed in fixed Open -> Low -> High -> Close order; MAE/MFE are daily-bar path estimates".to_string(),
        },
        SampleLimitationItem {
            code: "LIQUIDITY_ASSUMPTION".to_string(),
            message: "Whole-order fill model (m4-sim-1): market impact and order-book participation limits are not simulated".to_string(),
        },
        SampleLimitationItem {
            code: "AMBIGUOUS_FILLS".to_string(),
            message: format!(
                "Same-bar dual-touch ambiguous fills resolved by O->L->H->C order: {}",
                state.ambiguous_fill_count
            ),
        },
        SampleLimitationItem {
            code: "FEE_ASSUMPTION".to_string(),
            message: format!(
                "Fee profile '{}' (v{}): {}",
                state.fee_profile.name, state.fee_profile.version, state.fee_profile.assumption_note
            ),
        },
    ];

    let hidden_note = if input.hidden_mode {
        let msg = "Hidden mode masks instrument symbol and session dates during training, but raw price levels are untransformed and may hint at the instrument".to_string();
        items.push(SampleLimitationItem {
            code: "HIDDEN_MODE_PRICE_LEVEL".to_string(),
            message: msg.clone(),
        });
        Some(msg)
    } else {
        None
    };

    let open_pos_note = if has_open_pos {
        let msg = "Ending open position is marked to the final visible bar raw close price without simulated exit fee or slippage".to_string();
        items.push(SampleLimitationItem {
            code: "OPEN_POSITION_CLOSE_VALUATION".to_string(),
            message: msg.clone(),
        });
        Some(msg)
    } else {
        None
    };

    let div_note = if state.ex_date_dividend_assumed {
        let msg = "Cash dividends in simulation are credited to cash at ex_date pre-open (differs from real ledger pay_date accounting)".to_string();
        items.push(SampleLimitationItem {
            code: "EX_DATE_DIVIDEND_ASSUMPTION".to_string(),
            message: msg.clone(),
        });
        Some(msg)
    } else {
        None
    };

    let limitations = SampleLimitations {
        closed_trade_group_count: closed_count,
        small_sample_warning,
        ambiguous_fill_count: state.ambiguous_fill_count,
        bar_path_assumption: "O->L->H->C".to_string(),
        liquidity_assumption: "whole_order_fill_no_liquidity_cap".to_string(),
        fee_profile_id: state.fee_profile.id.clone(),
        fee_assumption: state.fee_profile.assumption_note.clone(),
        hidden_mode_enabled: input.hidden_mode,
        hidden_mode_price_level_note: hidden_note,
        has_open_position_at_end: has_open_pos,
        open_position_valuation_note: open_pos_note,
        ex_date_dividend_assumed: state.ex_date_dividend_assumed,
        ex_date_dividend_note: div_note,
        items,
    };

    TrainingReport {
        report_version: SIM_REPORT_VERSION.to_string(),
        engine_version: SIM_ENGINE_VERSION.to_string(),
        run_id: input.run_id.clone(),
        dataset_version: input.dataset_version,
        fee_profile_version_id: state.fee_profile.id.clone(),
        quote_currency: state.quote_currency.clone(),
        visible_until: state.visible_until,
        total_training_bars,
        exposed_bars,
        exposure_ratio,
        exposure_pct,
        initial_cash,
        ending_cash,
        open_position_market_value,
        ending_equity,
        total_pnl,
        total_return,
        total_return_pct,
        realized_pnl,
        unrealized_pnl,
        dividend_income,
        total_fees_quote,
        max_drawdown: round_metric(max_drawdown_ratio, 8),
        max_drawdown_pct: round_metric(max_drawdown_ratio * Decimal::from(100), 6),
        max_drawdown_amount,
        equity_curve,
        win_rate,
        gross_profit,
        gross_loss_abs,
        average_win,
        average_loss_abs,
        payoff_ratio,
        profit_factor,
        expectancy,
        plan_execution,
        closed_trade_groups,
        open_positions,
        excess_return_vs_benchmark: excess_return,
        excess_return_vs_benchmark_pct: excess_return_pct,
        benchmark,
        limitations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::parse_decimal_lossless;
    use crate::model::{AdjustmentKind, BarRecord, CorporateActionType};
    use crate::sim::{
        builtin_fee_profiles, step, AttachedBracket, FeeChargingTier, FeeCurrencyMode,
        FeeMarketCategory, FeeProfileVersion, FeeRoundingMode, OrderSide, OrderSubmitRequest,
        SimCommand, SimInstrumentSpec, SimOrderType, SlippageSpec, TimeInForce,
    };
    use chrono::TimeZone;

    fn dec(s: &str) -> Decimal {
        parse_decimal_lossless(s).expect("valid decimal literal")
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
    ) -> SimStepBar {
        let session_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        let open_at = Utc.with_ymd_and_hms(year, month, day, 14, 30, 0).unwrap();
        let close_at = Utc.with_ymd_and_hms(year, month, day, 21, 0, 0).unwrap();
        SimStepBar::from_bar(BarRecord {
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
            valid_from_version: 7,
            valid_to_version: None,
            raw_payload_hash: None,
        })
    }

    fn fixed_one_dollar_fee_profile() -> FeeProfileVersion {
        FeeProfileVersion {
            id: "fee-fixed-1usd-v1".to_string(),
            preset_key: "custom_fixed_1".to_string(),
            name: "Fixed $1 Per Order".to_string(),
            version: 1,
            market_category: FeeMarketCategory::UsEquity,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::ZERO,
            per_order_fixed_fee: Decimal::ONE,
            min_fee_per_order: None,
            max_fee_per_order: None,
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
            assumption_note: "Scenario assumption: $1.00 fixed fee per order, zero spread/slippage"
                .to_string(),
        }
        .canonical()
    }

    #[test]
    fn m4_a_12_hand_calculated_report_scaled_entries_mae_mfe_plan_rate_and_open_pos() {
        // Hand-calculated 7-bar training fixture on NASDAQ:AAPL:
        // Initial cash: $10,000.00, Fee: $1.00 fixed per filled order, tick_size = 0.01, lot_size = 1.
        let t0 = Utc.with_ymd_and_hms(2025, 1, 2, 21, 0, 0).unwrap();
        let mut init_closes = BTreeMap::new();
        init_closes.insert("NASDAQ:AAPL".to_string(), dec("100"));
        let mut state = SimState::new(
            "USD",
            dec("10000"),
            fixed_one_dollar_fee_profile(),
            vec![SimInstrumentSpec {
                instrument_id: "NASDAQ:AAPL".to_string(),
                base_asset_id: "AAPL".to_string(),
                quote_currency: "USD".to_string(),
                lot_size: dec("1"),
                tick_size: dec("0.01"),
            }],
            Some(t0),
            init_closes,
        )
        .unwrap();

        let bars = vec![
            // Bar 0 (2025-01-03): Group 1 Entry #1 Limit Buy 10 @ 98 (OpenToLow), post-entry path: 98->95->104->102
            make_bar("NASDAQ:AAPL", 2025, 1, 3, "100", "104", "95", "102", "1000"),
            // Bar 1 (2025-01-06): Group 1 Scale-in Market Buy 10 @ 102 (AtOpen) -> 20 shs, avg entry = 100.00!
            make_bar("NASDAQ:AAPL", 2025, 1, 6, "102", "105", "99", "103", "1000"),
            // Bar 2 (2025-01-07): Group 1 Partial Sell 10 @ 106 (AtOpen)
            make_bar(
                "NASDAQ:AAPL",
                2025,
                1,
                7,
                "106",
                "112",
                "104",
                "110",
                "1000",
            ),
            // Bar 3 (2025-01-08): Group 1 Final Limit Sell 10 @ 114 (LowToHigh); High=125 is AFTER exit and excluded!
            make_bar(
                "NASDAQ:AAPL",
                2025,
                1,
                8,
                "110",
                "125",
                "101",
                "120",
                "1000",
            ),
            // Bar 4 (2025-01-09): Group 2 Market Buy 10 @ 115 + attached SL @ 109 hit same bar (Low=105 & High=118 excluded!)
            make_bar(
                "NASDAQ:AAPL",
                2025,
                1,
                9,
                "115",
                "118",
                "105",
                "110",
                "1000",
            ),
            // Bar 5 (2025-01-10): Group 3 Unplanned Market Buy 10 @ 100 (cost = 1001)
            make_bar(
                "NASDAQ:AAPL",
                2025,
                1,
                10,
                "100",
                "101",
                "99",
                "100",
                "1000",
            ),
            // Bar 6 (2025-01-13): Group 3 Unplanned Limit Sell 10 @ 100.20 (net = 1001 -> exact zero PnL!)
            make_bar(
                "NASDAQ:AAPL",
                2025,
                1,
                13,
                "100",
                "103",
                "99",
                "102",
                "1000",
            ),
            // Bar 7 (2025-01-14): Group 4 Planned Buy Limit 10 @ 99.50 (later deviated without rationale)
            make_bar(
                "NASDAQ:AAPL",
                2025,
                1,
                14,
                "100",
                "103",
                "99",
                "102",
                "1000",
            ),
            // Bar 8 (2025-01-15): Group 4 manual Market Sell 10 @ 98 (loss), plus Open Position Buy 5 @ 97 held to end (Close = 101)
            make_bar("NASDAQ:AAPL", 2025, 1, 15, "98", "102", "96", "101", "1000"),
        ];

        let dividend_on_bar8 = CorporateActionRecord {
            id: "DIV-BAR8".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            action_type: CorporateActionType::CashDividend,
            ex_date: NaiveDate::from_ymd_opt(2025, 1, 15).unwrap(),
            pay_date: None,
            ratio_or_amount: dec("0.50"),
            currency: Some("USD".to_string()),
            announced_at: None,
            source: "fixture".to_string(),
            factor_version: 1,
            raw_payload_hash: None,
        };
        let cas = vec![dividend_on_bar8];

        // --- Bar 0: Submit Group 1 Entry #1 Limit Buy 10 @ 98 with attached bracket (TP 130, SL 85) ---
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single_with_bracket(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("98"),
                },
                TimeInForce::Gtc,
                dec("10"),
                AttachedBracket {
                    take_profit_limit: Some(dec("130")),
                    stop_loss_stop: Some(dec("85")),
                    tif: TimeInForce::Gtc,
                },
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[0]),
        )
        .unwrap();

        // --- Bar 1: Scale-in Market Buy 10 @ 102 ---
        (state, _) = step(
            &state,
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
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[1]),
        )
        .unwrap();

        // Cancel Group 1's attached bracket (cancelling leg SIM-ORD-000002 also cancels coupled OCO peer SIM-ORD-000003)
        (state, _) = step(
            &state,
            SimCommand::CancelOrder {
                order_id: "SIM-ORD-000002".to_string(),
                reason: Some("manual_scale_out".to_string()),
            },
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[2]),
        )
        .unwrap();

        // --- Bar 3: Final Limit Sell 10 @ 114 closing Group 1 ---
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Limit {
                    limit_price: dec("114"),
                },
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[3]),
        )
        .unwrap();

        // --- Bar 4: Group 2 Market Buy 10 @ 115 with attached bracket (TP 130, SL 109) -> hits SL 109 same bar! ---
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single_with_bracket(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
                AttachedBracket {
                    take_profit_limit: Some(dec("130")),
                    stop_loss_stop: Some(dec("109")),
                    tif: TimeInForce::Gtc,
                },
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[4]),
        )
        .unwrap();

        // --- Bar 5: Group 3 Unplanned exact zero-PnL roundtrip (Market Buy 10 @ 100 + Limit Sell 10 @ 100.20) ---
        // Wait: Sell cannot be submitted before holding in spot long-only, so let's buy on Bar 5 and sell @ 100.20 on Bar 6?
        // Even simpler: let's check Group 3 across Bar 5 (Buy 10 @ 100) and Bar 6 (Limit Sell 10 @ 100.20),
        // OR keep Group 3 + Group 4 crisp across Bars 5..7!
        // Let's trace:
        // Bar 5: Unplanned Market Buy 10 @ 100 (cost = 1001)
        (state, _) = step(
            &state,
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
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[5]),
        )
        .unwrap();

        // Bar 6: Unplanned Limit Sell 10 @ 100.20 (proceeds = 1002 - 1 = 1001 -> realized PnL = 0!)
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Limit {
                    limit_price: dec("100.20"),
                },
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[6]),
        )
        .unwrap();
        assert_eq!(state.position_quantity("NASDAQ:AAPL"), Decimal::ZERO);

        // Bar 7: Planned Buy Limit 10 @ 99.50 with attached bracket (TP 115, SL 90) for Group 4
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single_with_bracket(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("99.50"),
                },
                TimeInForce::Day,
                dec("10"),
                AttachedBracket {
                    take_profit_limit: Some(dec("115")),
                    stop_loss_stop: Some(dec("90")),
                    tif: TimeInForce::Gtc,
                },
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[7]),
        )
        .unwrap();
        assert_eq!(state.position_quantity("NASDAQ:AAPL"), dec("10"));

        // Bar 8 (ex-dividend $0.50 on 10 shares -> +$5.00 dividend!):
        // Cancel Group 4's attached bracket WITHOUT rationale note, manual Market Sell 10 @ 98 (loss),
        // AND Limit Buy 5 @ 97 held open at end (marked to close 101)!
        let first_open_bracket_id = state
            .orders
            .values()
            .find(|o| o.status.is_open_or_pending())
            .map(|o| o.order_id.clone())
            .unwrap();
        (state, _) = step(
            &state,
            SimCommand::CancelOrder {
                order_id: first_open_bracket_id,
                reason: None,
            },
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: dec("97"),
                },
                TimeInForce::Day,
                dec("5"),
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&bars[8]),
        )
        .unwrap();
        (state, _) = step(&state, SimCommand::Finish, None).unwrap();

        // Rationale note ONLY for Group 1's manual exit (`SIM-ORD-000005`), none for Group 4!
        let notes = vec![TrainingNoteRef {
            note_id: "NOTE-DEV-1".to_string(),
            visible_until: bars[2].bar.close_at,
            emotion_tag: Some("disciplined".to_string()),
            linked_order_id: Some("SIM-ORD-000005".to_string()),
            rationale: "Scaled out manually ahead of prior swing resistance".to_string(),
        }];

        let report = compute_training_report(&TrainingReportInput {
            run_id: "RUN-M4-A-12".to_string(),
            dataset_version: 7,
            hidden_mode: true,
            state,
            training_bars: bars,
            corporate_actions: cas,
            notes,
        });

        // --- Hand-calculation assertions ---
        // Closed Trade Groups (4 total):
        // 1) Group 1 (Win, Planned, DeviatedWithRationale):
        //    Buy 10@98 ($981) + Buy 10@102 ($1021) = $2002; Sell 10@106 ($1059) + Sell 10@114 ($1139) = $2198.
        //    Realized PnL = +$196.00, Fees = $4.00, Avg Entry = 100.00, Avg Exit = 110.00.
        //    MAE: min_path = 95 -> 5/unit (5%); MFE: max_path = 114 (High=125 excluded!) -> 14/unit (14%).
        // 2) Group 2 (Loss, Planned, ExitedByPlannedBracket):
        //    Buy 10@115 ($1151), SL Sell 10@109 ($1089) -> Realized PnL = -$62.00, Fees = $2.00.
        //    MAE: min_path = 109 (Low=105 excluded!) -> 6/unit; MFE: max_path = 115 (High=118 excluded!) -> 0/unit.
        // 3) Group 3 (Zero-PnL, Unplanned):
        //    Buy 10@100 ($1001), Sell 10@100.20 ($1001) -> Realized PnL = $0.00, Fees = $2.00.
        // 4) Group 4 (Loss, Planned, DeviatedWithoutRationale):
        //    Buy 10@99.50 ($996), Sell 10@98 ($979) -> Realized PnL = -$17.00, Fees = $2.00.
        assert_eq!(report.closed_trade_groups.len(), 4);

        let g1 = &report.closed_trade_groups[0];
        assert_eq!(g1.total_quantity, dec("20"));
        assert_eq!(g1.weighted_avg_entry_price, dec("100"));
        assert_eq!(g1.weighted_avg_exit_price, dec("110"));
        assert_eq!(g1.gross_pnl, dec("200"));
        assert_eq!(g1.total_fees_quote, dec("4"));
        assert_eq!(g1.realized_pnl, dec("196"));
        assert_eq!(g1.min_path_price, dec("95"));
        assert_eq!(g1.max_path_price, dec("114"));
        assert_eq!(g1.mae_per_unit, dec("5"));
        assert_eq!(g1.mae_pct, dec("5"));
        assert_eq!(g1.mfe_per_unit, dec("14"));
        assert_eq!(g1.mfe_pct, dec("14"));
        assert_eq!(
            g1.plan_compliance,
            PlanCompliance::DeviatedWithRationale {
                note_ids: vec!["NOTE-DEV-1".to_string()]
            }
        );

        let g2 = &report.closed_trade_groups[1];
        assert_eq!(g2.realized_pnl, dec("-62"));
        assert_eq!(g2.min_path_price, dec("109"));
        assert_eq!(g2.max_path_price, dec("115"));
        assert_eq!(g2.mae_per_unit, dec("6"));
        assert_eq!(g2.mfe_per_unit, Decimal::ZERO);
        assert_eq!(g2.plan_compliance, PlanCompliance::ExitedByPlannedBracket);

        let g3 = &report.closed_trade_groups[2];
        assert_eq!(g3.realized_pnl, Decimal::ZERO);
        assert_eq!(g3.plan_compliance, PlanCompliance::Unplanned);

        let g4 = &report.closed_trade_groups[3];
        assert_eq!(g4.realized_pnl, dec("-17"));
        assert_eq!(g4.plan_compliance, PlanCompliance::DeviatedWithoutRationale);

        // Open position at end: 5 shares bought @ 97 ($485 + $1 fee = $486 carrying cost), close = 101 -> MV = $505, unrealized = +$19
        assert_eq!(report.open_positions.len(), 1);
        assert_eq!(report.open_positions[0].remaining_quantity, dec("5"));
        assert_eq!(report.open_positions[0].carrying_cost_quote, dec("486"));
        assert_eq!(report.open_positions[0].market_value_quote, dec("505"));
        assert_eq!(report.open_positions[0].unrealized_pnl, dec("19"));

        // Portfolio totals:
        // Realized PnL = 196 - 62 + 0 - 17 = +117
        // Dividend income = 10 * 0.50 = +5
        // Unrealized PnL = +19
        // Total PnL = 117 + 5 + 19 = +141 -> Ending Equity = $10,141.00 (Total return = +1.41%)
        // Total Fees = 4 + 2 + 2 + 2 + 1 (open buy) = $11.00
        assert_eq!(report.realized_pnl, dec("117"));
        assert_eq!(report.dividend_income, dec("5"));
        assert_eq!(report.unrealized_pnl, dec("19"));
        assert_eq!(report.total_fees_quote, dec("11"));
        assert_eq!(report.ending_equity, dec("10141"));
        assert_eq!(report.total_pnl, dec("141"));
        assert_eq!(report.total_return, dec("0.0141"));
        assert_eq!(report.total_return_pct, dec("1.41"));

        // Exposure: 7 of 9 bars exposed (Bars 0,1,2,4,5,7,8; Bars 3 & 6 closed flat after exiting prior day's position) -> 7/9!
        assert_eq!(report.total_training_bars, 9);
        assert_eq!(report.exposed_bars, 7);
        assert_eq!(report.exposure_ratio, dec("0.77777778"));
        assert_eq!(report.exposure_pct, dec("77.777778"));

        // Win rate (zero-PnL separate!): 4 closed -> 1 win, 2 losses, 1 zero-PnL -> win_rate = 1/4 = 0.25 (25%)
        assert_eq!(report.win_rate.status, WinRateStatus::Computed);
        assert_eq!(report.win_rate.closed_trade_count, 4);
        assert_eq!(report.win_rate.winning_count, 1);
        assert_eq!(report.win_rate.losing_count, 2);
        assert_eq!(report.win_rate.zero_pnl_count, 1);
        assert_eq!(report.win_rate.win_rate, Some(dec("0.25")));
        assert_eq!(report.win_rate.win_rate_pct, Some(dec("25")));

        // Profit Factor = 196 / (62 + 17) = 196 / 79 = 2.48101266
        // Payoff Ratio = 196 / (79 / 2) = 196 / 39.5 = 4.96202532
        // Expectancy = (196 - 79) / 4 = 117 / 4 = 29.25 (sample_count = 4)
        assert_eq!(report.gross_profit, dec("196"));
        assert_eq!(report.gross_loss_abs, dec("79"));
        assert_eq!(
            report.profit_factor,
            RatioMetric::Computed(dec("2.48101266"))
        );
        assert_eq!(
            report.payoff_ratio,
            RatioMetric::Computed(dec("4.96202532"))
        );
        assert_eq!(report.expectancy.sample_count, 4);
        assert_eq!(report.expectancy.value, Some(dec("29.25")));

        // Plan execution rate: 3 planned closed groups (G1 compliant via note, G2 compliant via bracket, G4 non-compliant)
        // -> 2 / 3 = 0.66666667 (66.666667%), and G3 listed separately in `unplanned_group_ids`!
        assert_eq!(report.plan_execution.status, PlanExecutionStatus::Computed);
        assert_eq!(report.plan_execution.planned_closed_group_count, 3);
        assert_eq!(report.plan_execution.compliant_count, 2);
        assert_eq!(report.plan_execution.exited_by_plan_count, 1);
        assert_eq!(report.plan_execution.deviated_with_rationale_count, 1);
        assert_eq!(report.plan_execution.deviated_without_rationale_count, 1);
        assert_eq!(report.plan_execution.unplanned_closed_group_count, 1);
        assert_eq!(
            report.plan_execution.unplanned_group_ids,
            vec!["TRD-GRP-0003".to_string()]
        );
        assert_eq!(
            report.plan_execution.execution_rate,
            Some(dec("0.66666667"))
        );

        // Buy-and-Hold Benchmark over same $10,000, $1 fee, Bar 0 Open = $100 -> buys 99 shares ($9,900 + $1 fee = $9,901, $99 cash left)
        // Bar 7 dividend = 99 * $0.50 = $49.50 -> cash = $148.50; Bar 7 Close = $101 -> MV = 99 * 101 = $9,999.00
        // Ending Benchmark Equity = 148.50 + 9999 = $10,147.50 (+1.475%)
        assert_eq!(report.benchmark.status, BenchmarkStatus::Executed);
        assert_eq!(report.benchmark.initial_quantity_bought, dec("99"));
        assert_eq!(report.benchmark.dividend_income, dec("49.5"));
        assert_eq!(report.benchmark.ending_equity, dec("10147.5"));
        assert_eq!(report.benchmark.total_return_pct, dec("1.475"));

        // Structured Sample Limitations
        assert!(report.limitations.small_sample_warning);
        assert!(report.limitations.hidden_mode_enabled);
        assert!(report.limitations.hidden_mode_price_level_note.is_some());
        assert!(report.limitations.has_open_position_at_end);
        assert!(report.limitations.open_position_valuation_note.is_some());
        assert!(report.limitations.ex_date_dividend_assumed);
        assert!(report.limitations.ex_date_dividend_note.is_some());
    }

    #[test]
    fn m4_a_12_zero_losses_and_no_trades_explicit_ratio_states() {
        let zero_fee = builtin_fee_profiles()
            .into_iter()
            .find(|p| p.preset_key == "zero_cost")
            .unwrap();
        let t0 = Utc.with_ymd_and_hms(2025, 2, 3, 21, 0, 0).unwrap();
        let mut init_closes = BTreeMap::new();
        init_closes.insert("NASDAQ:AAPL".to_string(), dec("100"));
        let base_state = SimState::new(
            "USD",
            dec("10000"),
            zero_fee,
            vec![SimInstrumentSpec {
                instrument_id: "NASDAQ:AAPL".to_string(),
                base_asset_id: "AAPL".to_string(),
                quote_currency: "USD".to_string(),
                lot_size: dec("1"),
                tick_size: dec("0.01"),
            }],
            Some(t0),
            init_closes,
        )
        .unwrap();

        let b1 = make_bar("NASDAQ:AAPL", 2025, 2, 4, "100", "106", "99", "105", "1000");
        let b2 = make_bar(
            "NASDAQ:AAPL",
            2025,
            2,
            5,
            "110",
            "112",
            "108",
            "110",
            "1000",
        );

        // 1) No-trades run: explicit NoClosedTrades / NoPlannedTrades states
        let (nt1, _) = step(
            &base_state,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b1),
        )
        .unwrap();
        let (nt2, _) = step(
            &nt1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b2),
        )
        .unwrap();
        let (nt_fin, _) = step(&nt2, SimCommand::Finish, None).unwrap();

        let rep_no_trades = compute_training_report(&TrainingReportInput {
            run_id: "RUN-NO-TRADES".to_string(),
            dataset_version: 7,
            hidden_mode: false,
            state: nt_fin,
            training_bars: vec![b1.clone(), b2.clone()],
            corporate_actions: vec![],
            notes: vec![],
        });
        assert_eq!(rep_no_trades.ending_equity, dec("10000"));
        assert_eq!(rep_no_trades.exposure_ratio, Decimal::ZERO);
        assert_eq!(rep_no_trades.win_rate.status, WinRateStatus::NoClosedTrades);
        assert_eq!(rep_no_trades.profit_factor, RatioMetric::NoClosedTrades);
        assert_eq!(rep_no_trades.payoff_ratio, RatioMetric::NoClosedTrades);
        assert_eq!(
            rep_no_trades.expectancy.status,
            WinRateStatus::NoClosedTrades
        );
        assert_eq!(
            rep_no_trades.plan_execution.status,
            PlanExecutionStatus::NoPlannedTrades
        );
        // Benchmark still computes 100 shares @ 100 -> 110 (+10%)!
        assert_eq!(rep_no_trades.benchmark.ending_equity, dec("11000"));
        assert_eq!(rep_no_trades.excess_return_vs_benchmark_pct, dec("-10"));

        // 2) Zero-losses run (1 winning closed trade, 0 losing trades): explicit `RatioMetric::NoLosses`
        let (wl1, _) = step(
            &base_state,
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
        let (wl2, _) = step(
            &wl1,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b1),
        )
        .unwrap();
        let (wl3, _) = step(
            &wl2,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("10"),
            )),
            None,
        )
        .unwrap();
        let (wl4, _) = step(
            &wl3,
            SimCommand::AdvanceBar {
                corporate_actions: vec![],
            },
            Some(&b2),
        )
        .unwrap();
        let (wl_fin, _) = step(&wl4, SimCommand::Finish, None).unwrap();

        let rep_zero_loss = compute_training_report(&TrainingReportInput {
            run_id: "RUN-ZERO-LOSS".to_string(),
            dataset_version: 7,
            hidden_mode: false,
            state: wl_fin,
            training_bars: vec![b1, b2],
            corporate_actions: vec![],
            notes: vec![],
        });
        assert_eq!(rep_zero_loss.win_rate.winning_count, 1);
        assert_eq!(rep_zero_loss.win_rate.losing_count, 0);
        assert_eq!(rep_zero_loss.win_rate.win_rate_pct, Some(dec("100")));
        assert_eq!(rep_zero_loss.profit_factor, RatioMetric::NoLosses);
        assert_eq!(rep_zero_loss.payoff_ratio, RatioMetric::NoLosses);
        assert_eq!(rep_zero_loss.expectancy.value, Some(dec("100")));
    }

    #[test]
    fn m4_a_12_in_trade_split_drawdown_and_zero_wins_metrics() {
        let zero_fee = builtin_fee_profiles()
            .into_iter()
            .find(|p| p.preset_key == "zero_cost")
            .unwrap();
        let t0 = Utc.with_ymd_and_hms(2025, 3, 3, 21, 0, 0).unwrap();
        let mut init_closes = BTreeMap::new();
        init_closes.insert("NASDAQ:AAPL".to_string(), dec("400"));
        let mut state = SimState::new(
            "USD",
            dec("10000"),
            zero_fee,
            vec![SimInstrumentSpec {
                instrument_id: "NASDAQ:AAPL".to_string(),
                base_asset_id: "AAPL".to_string(),
                quote_currency: "USD".to_string(),
                lot_size: dec("1"),
                tick_size: dec("0.01"),
            }],
            Some(t0),
            init_closes,
        )
        .unwrap();

        // Buy 10 shares @ 400 ($4,000), Close = 440 -> Equity = $10,400 (new peak!)
        // Next day 4:1 split -> 40 shares (post-split avg entry = 100), Close drops to 88 -> Equity = 6000 + 3520 = $9,520
        //   Drawdown from peak $10,400 = $880 -> 880 / 10400 = 0.08461538 (8.461538%)
        // Next day sell all 40 shares @ 90 -> realized loss = 3600 - 4000 = -$400 (0 wins, 1 loss -> NoWins payoff ratio!)
        let b1 = make_bar(
            "NASDAQ:AAPL",
            2025,
            3,
            4,
            "400",
            "448",
            "392",
            "440",
            "1000",
        );
        let b2 = make_bar("NASDAQ:AAPL", 2025, 3, 5, "105", "106", "86", "88", "4000");
        let b3 = make_bar("NASDAQ:AAPL", 2025, 3, 6, "90", "95", "85", "92", "4000");
        let cas = vec![CorporateActionRecord {
            id: "CA-SPLIT-4".to_string(),
            instrument_id: "NASDAQ:AAPL".to_string(),
            action_type: CorporateActionType::Split,
            ex_date: NaiveDate::from_ymd_opt(2025, 3, 5).unwrap(),
            pay_date: None,
            ratio_or_amount: dec("4"),
            currency: None,
            announced_at: None,
            source: "fixture".to_string(),
            factor_version: 1,
            raw_payload_hash: None,
        }];

        (state, _) = step(
            &state,
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
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&b1),
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&b2),
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::SubmitOrder(OrderSubmitRequest::single(
                "NASDAQ:AAPL",
                OrderSide::Sell,
                SimOrderType::Market,
                TimeInForce::Day,
                dec("40"),
            )),
            None,
        )
        .unwrap();
        (state, _) = step(
            &state,
            SimCommand::AdvanceBar {
                corporate_actions: cas.clone(),
            },
            Some(&b3),
        )
        .unwrap();
        (state, _) = step(&state, SimCommand::Finish, None).unwrap();

        let rep = compute_training_report(&TrainingReportInput {
            run_id: "RUN-SPLIT-DD".to_string(),
            dataset_version: 7,
            hidden_mode: false,
            state,
            training_bars: vec![b1, b2, b3],
            corporate_actions: cas,
            notes: vec![],
        });

        assert_eq!(rep.closed_trade_groups.len(), 1);
        let g = &rep.closed_trade_groups[0];
        assert_eq!(g.total_quantity, dec("40"));
        assert_eq!(g.weighted_avg_entry_price, dec("100"));
        assert_eq!(g.weighted_avg_exit_price, dec("90"));
        assert_eq!(g.realized_pnl, dec("-400"));
        // Post-split extrema: b1 [392/4=98, 448/4=112], b2 [86, 106], b3 AtOpen [90] -> min=86 (MAE 14), max=112 (MFE 12)
        assert_eq!(g.mae_per_unit, dec("14"));
        assert_eq!(g.mfe_per_unit, dec("12"));

        assert_eq!(rep.max_drawdown_amount, dec("880"));
        assert_eq!(rep.max_drawdown, dec("0.08461538"));
        assert_eq!(rep.max_drawdown_pct, dec("8.461538"));
        assert_eq!(rep.payoff_ratio, RatioMetric::NoWins);
        assert_eq!(rep.profit_factor, RatioMetric::Computed(Decimal::ZERO));
    }
}
