//! Domain <-> Storage type mappings, hidden-mode scrubbing, and training chart slicing.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Days, NaiveDate, TimeZone, Utc};
use delta_core::{
    compute_ema, compute_macd, compute_rsi, compute_sma, derive_point_in_time_split_adjusted_bars,
    AdjustmentKind, BarRecord, CorporateActionRecord, CorporateActionType, FeeChargingTier,
    FeeCurrencyMode, FeeMarketCategory, FeeProfileVersion, FeeRoundingMode, IndicatorPoint,
    MacdPoint, OrderSide, OrderStatus, SimError, SimEvent, SimFill, SimOrder, SimOrderType,
    SimState, SlippageSpec, TimeInForce, TrainingReport,
};
use delta_data::{
    FeeChargingTier as DataFeeChargingTier, FeeCurrencyMode as DataFeeCurrencyMode,
    FeeProfileVersionRecord, FeeRoundingMode as DataFeeRoundingMode, SimFillRecord, SimOrderRecord,
    SimOrderStatus, SimulationRunRecord, SlippageMode as DataSlippageMode, StorageError,
    TrainingNoteRecord, TrainingSessionRecord,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::json;

use super::dto::*;
use crate::kline::{
    ChartMarkerDto, ChartMarkerLayout, IndicatorConfig, KlineViewport, KlineWindowSliceDto,
    LodCandleDto, MarkerKind,
};
use crate::service::ErrorEnvelope;

// ============================================================================
// 2. Domain <-> Storage Type Mappings & Validation
// ============================================================================

pub fn record_to_core_fee_profile(
    rec: &FeeProfileVersionRecord,
) -> Result<FeeProfileVersion, ErrorEnvelope> {
    let invalid = |msg: String| ErrorEnvelope::new("INVALID_FEE_PROFILE", msg, false);
    let version = u32::try_from(rec.version_num).map_err(|_| {
        invalid(format!(
            "invalid fee_profile version_num {}",
            rec.version_num
        ))
    })?;
    let market_category = match rec.market_category.trim().to_ascii_lowercase().as_str() {
        "all" | "any" => FeeMarketCategory::Any,
        "us_equity" => FeeMarketCategory::UsEquity,
        "crypto_spot" => FeeMarketCategory::CryptoSpot,
        other => {
            return Err(invalid(format!(
                "unknown fee_profile market_category '{other}'"
            )))
        }
    };
    let fee_currency_mode = match rec.fee_currency_mode {
        DataFeeCurrencyMode::Quote => FeeCurrencyMode::Quote,
        DataFeeCurrencyMode::ReceivedAsset => FeeCurrencyMode::ReceivedAsset,
    };
    let rounding_mode = match rec.rounding_mode {
        DataFeeRoundingMode::HalfUp => FeeRoundingMode::HalfUp,
        DataFeeRoundingMode::HalfEven => FeeRoundingMode::HalfEven,
        DataFeeRoundingMode::Up => FeeRoundingMode::Up,
        DataFeeRoundingMode::Down => FeeRoundingMode::Down,
    };
    let charging_tier = match rec.charging_tier {
        DataFeeChargingTier::PerOrder => FeeChargingTier::PerOrder,
        DataFeeChargingTier::PerFill => FeeChargingTier::PerFill,
    };
    let slippage = match rec.slippage_mode {
        DataSlippageMode::None => SlippageSpec::Zero,
        DataSlippageMode::Bps => SlippageSpec::Bps(rec.slippage_value),
        DataSlippageMode::Ticks => {
            if rec.slippage_value < Decimal::ZERO || !rec.slippage_value.fract().is_zero() {
                return Err(invalid(format!(
                    "ticks slippage requires a non-negative integer tick count, got {}",
                    rec.slippage_value
                )));
            }
            let n = rec.slippage_value.to_u32().ok_or_else(|| {
                invalid(format!(
                    "ticks slippage value {} overflows u32",
                    rec.slippage_value
                ))
            })?;
            SlippageSpec::Ticks(n)
        }
    };
    let official_retrieved_at = match rec.official_retrieved_at.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(
            NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
                .map_err(|e| invalid(format!("invalid official_retrieved_at '{s}': {e}")))?,
        ),
        _ => None,
    };

    let fp = FeeProfileVersion {
        id: rec.id.clone(),
        preset_key: rec.profile_key.clone(),
        name: rec.name.clone(),
        version,
        market_category,
        notional_rate: rec.notional_rate,
        per_unit_fee: rec.per_unit_fee,
        per_order_fixed_fee: rec.fixed_fee_per_order,
        min_fee_per_order: rec.min_fee_per_order,
        max_fee_per_order: rec.max_fee_per_order,
        fee_currency_mode,
        rounding_mode,
        rounding_scale: rec.rounding_scale,
        charging_tier,
        slippage,
        half_spread_bps: rec.half_spread_bps,
        is_builtin: rec.is_builtin,
        is_platform_preset: !rec.is_scenario_assumption,
        official_source_url: rec.official_source_url.clone(),
        official_retrieved_at,
        effective_from: None,
        effective_to: None,
        assumption_note: rec.assumption_note.clone(),
    }
    .canonical();
    fp.validate().map_err(|e| invalid(e.to_string()))?;
    Ok(fp)
}

pub(super) fn draft_to_fee_profile_record(
    draft: &FeeProfileDraftDto,
) -> Result<FeeProfileVersionRecord, ErrorEnvelope> {
    let market_category = match draft.market_category {
        FeeMarketCategory::Any => "all",
        FeeMarketCategory::UsEquity => "us_equity",
        FeeMarketCategory::CryptoSpot => "crypto_spot",
    }
    .to_string();
    let fee_currency_mode = match draft.fee_currency_mode {
        FeeCurrencyMode::Quote => DataFeeCurrencyMode::Quote,
        FeeCurrencyMode::ReceivedAsset => DataFeeCurrencyMode::ReceivedAsset,
    };
    let rounding_mode = match draft.rounding_mode {
        FeeRoundingMode::HalfUp => DataFeeRoundingMode::HalfUp,
        FeeRoundingMode::HalfEven => DataFeeRoundingMode::HalfEven,
        FeeRoundingMode::Up => DataFeeRoundingMode::Up,
        FeeRoundingMode::Down => DataFeeRoundingMode::Down,
    };
    let charging_tier = match draft.charging_tier {
        FeeChargingTier::PerOrder => DataFeeChargingTier::PerOrder,
        FeeChargingTier::PerFill => DataFeeChargingTier::PerFill,
    };
    let (slippage_mode, slippage_value) = match draft.slippage.canonical() {
        SlippageSpec::Zero => (DataSlippageMode::None, Decimal::ZERO),
        SlippageSpec::Bps(bps) => (DataSlippageMode::Bps, bps),
        SlippageSpec::Ticks(n) => (DataSlippageMode::Ticks, Decimal::from(n)),
    };
    let rec = FeeProfileVersionRecord {
        id: format!("FPV:{}:v1", draft.profile_key.trim().to_ascii_uppercase()),
        profile_key: draft.profile_key.trim().to_string(),
        version_num: 1,
        name: draft.name.trim().to_string(),
        market_category,
        venue: draft
            .venue
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        notional_rate: draft.notional_rate,
        per_unit_fee: draft.per_unit_fee,
        fixed_fee_per_order: draft.per_order_fixed_fee,
        min_fee_per_order: draft.min_fee_per_order,
        max_fee_per_order: draft.max_fee_per_order,
        fee_currency_mode,
        rounding_mode,
        rounding_scale: draft.rounding_scale,
        charging_tier,
        slippage_mode,
        slippage_value,
        half_spread_bps: draft.half_spread_bps,
        rules_json: json!({
            "profile_key": draft.profile_key.trim(),
            "scenario_assumption": draft.is_scenario_assumption,
        })
        .to_string(),
        is_builtin: false,
        is_scenario_assumption: draft.is_scenario_assumption,
        official_source_url: draft.official_source_url.clone(),
        official_retrieved_at: draft.official_retrieved_at.map(|d| d.to_string()),
        assumption_note: draft.assumption_note.trim().to_string(),
        created_at: Utc::now().to_rfc3339(),
    };
    let _ = record_to_core_fee_profile(&rec)?;
    Ok(rec)
}

fn map_order_status_to_storage(status: OrderStatus) -> SimOrderStatus {
    match status {
        OrderStatus::PendingParent => SimOrderStatus::Created,
        OrderStatus::Accepted => SimOrderStatus::Accepted,
        OrderStatus::Active => SimOrderStatus::Active,
        OrderStatus::Filled => SimOrderStatus::Filled,
        OrderStatus::Cancelled => SimOrderStatus::Cancelled,
        OrderStatus::Expired => SimOrderStatus::Expired,
        OrderStatus::Rejected => SimOrderStatus::Rejected,
    }
}

pub(crate) fn sim_order_to_record(run_id: &str, order: &SimOrder) -> SimOrderRecord {
    let (order_type_str, limit_price, stop_price) = match order.order_type {
        SimOrderType::Market => ("market".to_string(), None, None),
        SimOrderType::Limit { limit_price } => ("limit".to_string(), Some(limit_price), None),
        SimOrderType::StopMarket { stop_price } => {
            ("stop_market".to_string(), None, Some(stop_price))
        }
        SimOrderType::StopLimit {
            stop_price,
            limit_price,
        } => (
            "stop_limit".to_string(),
            Some(limit_price),
            Some(stop_price),
        ),
    };
    let side_str = match order.side {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    }
    .to_string();
    let tif_str = match order.tif {
        TimeInForce::Day => "day",
        TimeInForce::Gtc => "gtc",
    }
    .to_string();
    let role_str = serde_json::to_value(order.group_role)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string));

    SimOrderRecord {
        order_id: order.order_id.clone(),
        run_id: run_id.to_string(),
        instrument_id: order.instrument_id.clone(),
        order_seq: i64::try_from(order.order_seq).unwrap_or(i64::MAX),
        order_type: order_type_str,
        side: side_str,
        tif: tif_str,
        qty: order.quantity,
        limit_price,
        stop_price,
        parent_group_id: order.parent_group_id.clone(),
        group_role: role_str,
        status: map_order_status_to_storage(order.status),
        reject_code: order.reject_code.map(|c| c.as_str().to_string()),
        cancel_reason: order.cancel_reason.clone(),
        submitted_at: order.submitted_at,
        eligible_at: Some(order.eligible_at),
        updated_at: order.updated_at,
        payload_json: serde_json::to_string(order).unwrap_or_else(|_| "{}".to_string()),
    }
}

pub(crate) fn sim_fill_to_record(run_id: &str, fill: &SimFill) -> SimFillRecord {
    let side_str = match fill.side {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    }
    .to_string();
    SimFillRecord {
        fill_id: fill.fill_id.clone(),
        run_id: run_id.to_string(),
        order_id: fill.order_id.clone(),
        instrument_id: fill.instrument_id.clone(),
        bar_close_at: fill.bar_close_at,
        side: side_str,
        qty: fill.gross_quantity,
        raw_price: fill.raw_path_price,
        fill_price: fill.fill_price,
        fee_amount: fill.fee_amount,
        fee_currency: fill.fee_currency.clone(),
        ambiguous: fill.ambiguous,
        payload_json: serde_json::to_string(fill).unwrap_or_else(|_| "{}".to_string()),
    }
}

pub(crate) fn map_storage_err(code: &'static str, err: StorageError) -> ErrorEnvelope {
    ErrorEnvelope::new(code, err.to_string(), false)
}

/// Why a recovery/replay attempt failed (`S-43`). Only `Integrity` (hash, command, bar or
/// checkpoint mismatch) is evidence that the run is corrupted and may mark it `damaged`;
/// a `Storage` read failure is surfaced unchanged and leaves the run as it was.
pub(crate) enum ReplayFailure {
    Integrity(String),
    Storage(ErrorEnvelope),
}

impl From<String> for ReplayFailure {
    fn from(reason: String) -> Self {
        Self::Integrity(reason)
    }
}

impl ReplayFailure {
    pub(crate) fn into_envelope(self, integrity_code: &'static str) -> ErrorEnvelope {
        match self {
            Self::Integrity(reason) => ErrorEnvelope::new(integrity_code, reason, false),
            Self::Storage(err) => err,
        }
    }
}

pub(crate) fn map_sim_err(err: SimError) -> ErrorEnvelope {
    match err {
        SimError::OrderNotModifiable { .. } => {
            ErrorEnvelope::new("ORDER_NOT_MODIFIABLE", err.to_string(), false)
        }
        SimError::UndoUnavailable(_) => {
            ErrorEnvelope::new("UNDO_UNAVAILABLE", err.to_string(), false)
        }
        SimError::EngineFeatureUnavailable { .. } => {
            ErrorEnvelope::new("ENGINE_FEATURE_UNAVAILABLE", err.to_string(), false)
        }
        SimError::OrderRejected(rej) => ErrorEnvelope::new(
            rej.code.as_str(),
            serde_json::to_string(&rej).unwrap_or(rej.message),
            false,
        ),
        SimError::SessionFinished => ErrorEnvelope::new("SESSION_FINISHED", err.to_string(), false),
        SimError::ClockNotMonotonic { .. } => {
            ErrorEnvelope::new("CLOCK_NOT_MONOTONIC", err.to_string(), false)
        }
        SimError::UnknownOrder(_) | SimError::OrderNotCancellable { .. } => {
            ErrorEnvelope::new("ORDER_NOT_CANCELLABLE", err.to_string(), false)
        }
        other => ErrorEnvelope::new("SIMULATION_ERROR", other.to_string(), false),
    }
}

pub(crate) fn map_sim_err_scrubbed(
    err: SimError,
    masked: bool,
    real_instrument_id: &str,
) -> ErrorEnvelope {
    match err {
        SimError::OrderRejected(mut rej) => {
            if masked {
                rej.message = rej
                    .message
                    .replace(real_instrument_id, HIDDEN_INSTRUMENT_ID);
            }
            map_sim_err(SimError::OrderRejected(rej))
        }
        other => map_sim_err(other),
    }
}

/// Orders created or mutated between `prev` and `next` states (`TRN-Q03`).
pub(crate) fn changed_orders(
    run_id: &str,
    prev: &SimState,
    next: &SimState,
) -> Vec<SimOrderRecord> {
    next.orders
        .iter()
        .filter(|(id, order)| prev.orders.get(*id) != Some(*order))
        .map(|(_, order)| sim_order_to_record(run_id, order))
        .collect()
}

// ============================================================================
// 3. Hidden Mode Scrubbing & PIT Chart/Indicator Slice Cache (`S-37`, `S-38`, `M4-K03`)
// ============================================================================

#[inline]
pub fn is_run_identity_masked(run: &SimulationRunRecord, session: &TrainingSessionRecord) -> bool {
    session.hidden_mode && !(run.status.is_ended() && session.revealed)
}

pub fn hidden_display_symbol_for_category(market_category: &str) -> String {
    if market_category.eq_ignore_ascii_case("crypto_spot") {
        "标的 A（加密现货）".to_string()
    } else {
        "标的 A（美股/ETF）".to_string()
    }
}

#[inline]
pub fn bar_label_for_one_based(one_based_index: usize) -> String {
    format!("第 {} 根", one_based_index.max(1))
}

#[inline]
pub(super) fn synthetic_date_for_index(idx: usize) -> NaiveDate {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .expect("epoch 0001-01-01")
        .checked_add_days(Days::new(idx as u64))
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
}

#[inline]
pub(super) fn synthetic_open_close_for_index(idx: usize) -> (DateTime<Utc>, DateTime<Utc>) {
    let d = synthetic_date_for_index(idx);
    let open_at = Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap());
    let close_at = Utc.from_utc_datetime(&d.and_hms_opt(23, 59, 59).unwrap());
    (open_at, close_at)
}

pub(super) fn scrub_bar_record(bar: &BarRecord, display_idx: usize) -> BarRecord {
    let syn_date = synthetic_date_for_index(display_idx);
    let (open_at, close_at) = synthetic_open_close_for_index(display_idx);
    BarRecord {
        id: format!("BAR:HIDDEN:{}", display_idx + 1),
        instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
        timeframe: bar.timeframe.clone(),
        session: "training".to_string(),
        session_date: syn_date,
        open_at,
        close_at,
        open: bar.open,
        high: bar.high,
        low: bar.low,
        close: bar.close,
        volume: bar.volume,
        source: "training_port".to_string(),
        adjustment: bar.adjustment,
        is_final: bar.is_final,
        valid_from_version: bar.valid_from_version,
        valid_to_version: None,
        raw_payload_hash: None,
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TrainingBarFingerprint {
    len: usize,
    first: Option<(String, DateTime<Utc>, Decimal)>,
    last: Option<(String, DateTime<Utc>, Decimal)>,
}

impl TrainingBarFingerprint {
    fn from_bars(bars: &[BarRecord]) -> Self {
        Self {
            len: bars.len(),
            first: bars.first().map(|b| (b.id.clone(), b.close_at, b.close)),
            last: bars.last().map(|b| (b.id.clone(), b.close_at, b.close)),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TrainingCaFingerprint {
    len: usize,
    first: Option<(String, NaiveDate, Decimal)>,
    last: Option<(String, NaiveDate, Decimal)>,
}

impl TrainingCaFingerprint {
    fn from_cas(cas: &[CorporateActionRecord]) -> Self {
        Self {
            len: cas.len(),
            first: cas
                .first()
                .map(|c| (c.id.clone(), c.ex_date, c.ratio_or_amount)),
            last: cas
                .last()
                .map(|c| (c.id.clone(), c.ex_date, c.ratio_or_amount)),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TrainingIndicatorCacheKey {
    run_id: String,
    source_instrument_id: String,
    dataset_version: i64,
    history_start_at: DateTime<Utc>,
    visible_until: DateTime<Utc>,
    masked: bool,
    bars_fp: TrainingBarFingerprint,
    cas_fp: TrainingCaFingerprint,
    config: IndicatorConfig,
}

struct CachedTrainingChartBase {
    key: TrainingIndicatorCacheKey,
    display_bars: Vec<BarRecord>,
    pit_price_scales: Vec<Decimal>,
    real_close_to_display_idx: BTreeMap<DateTime<Utc>, usize>,
    real_date_to_display_idx: BTreeMap<NaiveDate, usize>,
    sma_display: Vec<(usize, Vec<IndicatorPoint>)>,
    ema_display: Vec<(usize, Vec<IndicatorPoint>)>,
    macd_display: Vec<MacdPoint>,
    rsi_display: Vec<IndicatorPoint>,
}

thread_local! {
    static TRAINING_SLICE_CACHE: RefCell<Option<CachedTrainingChartBase>> = const { RefCell::new(None) };
}

/// Evict any cached chart slice belonging to `run_id` (including its revealed slice).
pub(super) fn evict_training_slice_cache(run_id: &str) {
    let revealed_key = format!("{run_id}:revealed");
    TRAINING_SLICE_CACHE.with(|cell| {
        if cell
            .borrow()
            .as_ref()
            .is_some_and(|c| c.key.run_id == run_id || c.key.run_id == revealed_key)
        {
            *cell.borrow_mut() = None;
        }
    });
}

fn rebase_points_with_synthetic_clock<T>(
    full: Vec<T>,
    warmup_count: usize,
    masked: bool,
    mut stamp: impl FnMut(&mut T, NaiveDate, DateTime<Utc>),
) -> Vec<T> {
    full.into_iter()
        .skip(warmup_count)
        .enumerate()
        .map(|(disp_idx, mut pt)| {
            if masked {
                let (_, close_at) = synthetic_open_close_for_index(disp_idx);
                stamp(&mut pt, synthetic_date_for_index(disp_idx), close_at);
            }
            pt
        })
        .collect()
}

fn rebase_indicator_points(
    full: Vec<IndicatorPoint>,
    warmup_count: usize,
    masked: bool,
) -> Vec<IndicatorPoint> {
    rebase_points_with_synthetic_clock(full, warmup_count, masked, |pt, date, close_at| {
        pt.session_date = date;
        pt.close_at = close_at;
    })
}

fn rebase_macd_points(full: Vec<MacdPoint>, warmup_count: usize, masked: bool) -> Vec<MacdPoint> {
    rebase_points_with_synthetic_clock(full, warmup_count, masked, |pt, date, close_at| {
        pt.session_date = date;
        pt.close_at = close_at;
    })
}

pub(super) type VisibleCloseIndexMap = BTreeMap<DateTime<Utc>, usize>;
pub(super) type VisibleDateIndexMap = BTreeMap<NaiveDate, usize>;

pub(super) fn build_visible_index_maps(
    vis_bars: &[BarRecord],
) -> (VisibleCloseIndexMap, VisibleDateIndexMap) {
    let mut close_to_idx = BTreeMap::new();
    let mut date_to_idx = BTreeMap::new();
    for (idx, b) in vis_bars.iter().enumerate() {
        close_to_idx.insert(b.close_at, idx);
        date_to_idx.insert(b.session_date, idx);
    }
    (close_to_idx, date_to_idx)
}
pub(super) type TrainingChartSliceWithIndexMaps = (
    KlineWindowSliceDto,
    VisibleCloseIndexMap,
    VisibleDateIndexMap,
);

#[allow(clippy::too_many_arguments)]
pub(super) fn build_training_chart_slice_with_warmup(
    run_id: &str,
    dataset_version: i64,
    visible_until: DateTime<Utc>,
    history_start_at: DateTime<Utc>,
    masked: bool,
    out_instrument_id: &str,
    out_venue: &str,
    is_crypto: bool,
    raw_bars_with_warmup: &[BarRecord],
    visible_cas: &[CorporateActionRecord],
    viewport: &KlineViewport,
    raw_markers: &[ChartMarkerDto],
    max_pixel_columns: usize,
) -> Result<TrainingChartSliceWithIndexMaps, ErrorEnvelope> {
    let source_instrument_id = raw_bars_with_warmup
        .first()
        .map(|b| b.instrument_id.clone())
        .unwrap_or_else(|| out_instrument_id.to_string());
    let key = TrainingIndicatorCacheKey {
        run_id: run_id.to_string(),
        source_instrument_id,
        dataset_version,
        history_start_at,
        visible_until,
        masked,
        bars_fp: TrainingBarFingerprint::from_bars(raw_bars_with_warmup),
        cas_fp: TrainingCaFingerprint::from_cas(visible_cas),
        config: viewport.indicator_config.clone(),
    };

    TRAINING_SLICE_CACHE.with(|cell| {
        let mut cache_opt = cell.borrow_mut();
        let needs_recompute = match cache_opt.as_ref() {
            Some(cached) => cached.key != key,
            None => true,
        };

        if needs_recompute {
            let pit_adjusted = derive_point_in_time_split_adjusted_bars(
                raw_bars_with_warmup,
                visible_cas,
                visible_until,
            )
            .map_err(map_sim_err)?;
            let warmup_count = pit_adjusted
                .iter()
                .take_while(|b| b.open_at < history_start_at)
                .count();

            let raw_close_by_close_at: HashMap<DateTime<Utc>, Decimal> = raw_bars_with_warmup
                .iter()
                .map(|b| (b.close_at, b.close))
                .collect();

            let mut real_close_to_display_idx = BTreeMap::new();
            let mut real_date_to_display_idx = BTreeMap::new();
            let display_len = pit_adjusted.len().saturating_sub(warmup_count);
            let mut display_bars = Vec::with_capacity(display_len);
            let mut pit_price_scales = Vec::with_capacity(display_len);
            for (disp_idx, bar) in pit_adjusted[warmup_count..].iter().enumerate() {
                real_close_to_display_idx.insert(bar.close_at, disp_idx);
                real_date_to_display_idx.insert(bar.session_date, disp_idx);
                let scale = raw_close_by_close_at
                    .get(&bar.close_at)
                    .copied()
                    .filter(|raw_close| *raw_close > Decimal::ZERO)
                    .map(|raw_close| bar.close / raw_close)
                    .unwrap_or(Decimal::ONE);
                pit_price_scales.push(scale);
                if masked {
                    display_bars.push(scrub_bar_record(bar, disp_idx));
                } else {
                    display_bars.push(bar.clone());
                }
            }

            let sma_display = viewport
                .indicator_config
                .sma_periods
                .iter()
                .map(|&p| {
                    let full = compute_sma(&pit_adjusted, p, Some(visible_until));
                    (p, rebase_indicator_points(full, warmup_count, masked))
                })
                .collect();

            let ema_display = viewport
                .indicator_config
                .ema_periods
                .iter()
                .map(|&p| {
                    let full = compute_ema(&pit_adjusted, p, Some(visible_until));
                    (p, rebase_indicator_points(full, warmup_count, masked))
                })
                .collect();

            let macd_display = match viewport.indicator_config.macd {
                Some((fast, slow, sig)) => {
                    let full = compute_macd(&pit_adjusted, fast, slow, sig, Some(visible_until));
                    rebase_macd_points(full, warmup_count, masked)
                }
                None => Vec::new(),
            };

            let rsi_display = match viewport.indicator_config.rsi_period {
                Some(p) => {
                    let full = compute_rsi(&pit_adjusted, p, Some(visible_until));
                    rebase_indicator_points(full, warmup_count, masked)
                }
                None => Vec::new(),
            };

            *cache_opt = Some(CachedTrainingChartBase {
                key,
                display_bars,
                pit_price_scales,
                real_close_to_display_idx,
                real_date_to_display_idx,
                sma_display,
                ema_display,
                macd_display,
                rsi_display,
            });
        }

        let cached = cache_opt.as_ref().expect("training slice cache populated");
        let total_vis = cached.display_bars.len();
        let end = viewport.visible_end.min(total_vis);
        let start = viewport.visible_start.min(end);
        let bars = cached.display_bars[start..end].to_vec();

        let sma_series = cached
            .sma_display
            .iter()
            .map(|(p, series)| {
                let s_end = end.min(series.len());
                let s_start = start.min(s_end);
                (*p, series[s_start..s_end].to_vec())
            })
            .collect();
        let ema_series = cached
            .ema_display
            .iter()
            .map(|(p, series)| {
                let s_end = end.min(series.len());
                let s_start = start.min(s_end);
                (*p, series[s_start..s_end].to_vec())
            })
            .collect();
        let macd_series = if cached.macd_display.len() >= end {
            cached.macd_display[start..end].to_vec()
        } else {
            Vec::new()
        };
        let rsi_series = if cached.rsi_display.len() >= end {
            cached.rsi_display[start..end].to_vec()
        } else {
            Vec::new()
        };

        let cols = max_pixel_columns.max(1);
        let mut lod_candles = Vec::new();
        if bars.len() <= cols {
            for (local_idx, b) in bars.iter().enumerate() {
                let global_idx = start + local_idx;
                lod_candles.push(LodCandleDto {
                    start_bar_index: global_idx,
                    end_bar_index: global_idx,
                    session_date: b.session_date,
                    open: b.open.to_f64().unwrap_or(0.0),
                    high: b.high.to_f64().unwrap_or(0.0),
                    low: b.low.to_f64().unwrap_or(0.0),
                    close: b.close.to_f64().unwrap_or(0.0),
                    volume: b.volume.to_f64().unwrap_or(0.0),
                    has_gap_before: false,
                    is_incomplete: false,
                });
            }
        } else {
            for c in 0..cols {
                let b_start = c * bars.len() / cols;
                let b_end = ((c + 1) * bars.len() / cols).min(bars.len());
                if b_start >= b_end {
                    continue;
                }
                let bucket = &bars[b_start..b_end];
                let first = &bucket[0];
                let last = &bucket[bucket.len() - 1];
                let mut high = f64::NEG_INFINITY;
                let mut low = f64::INFINITY;
                let mut volume = 0.0;
                for b in bucket {
                    high = high.max(b.high.to_f64().unwrap_or(0.0));
                    low = low.min(b.low.to_f64().unwrap_or(0.0));
                    volume += b.volume.to_f64().unwrap_or(0.0);
                }
                lod_candles.push(LodCandleDto {
                    start_bar_index: start + b_start,
                    end_bar_index: start + b_end - 1,
                    session_date: last.session_date,
                    open: first.open.to_f64().unwrap_or(0.0),
                    high,
                    low,
                    close: last.close.to_f64().unwrap_or(0.0),
                    volume,
                    has_gap_before: false,
                    is_incomplete: false,
                });
            }
        }

        let mut low_min = f64::INFINITY;
        let mut high_max = f64::NEG_INFINITY;
        for b in &bars {
            low_min = low_min.min(b.low.to_f64().unwrap_or(0.0));
            high_max = high_max.max(b.high.to_f64().unwrap_or(0.0));
        }
        if !low_min.is_finite() || !high_max.is_finite() {
            low_min = 0.0;
            high_max = 1.0;
        }
        let price_span = (high_max - low_min).max(1e-9);

        let mut stack_counts: HashMap<usize, u32> = HashMap::new();
        let mut markers = Vec::new();
        for m in raw_markers {
            if m.bar_index < start || m.bar_index >= end || m.bar_index >= total_vis {
                continue;
            }
            let slot = stack_counts.entry(m.bar_index).or_insert(0);
            let current_slot = *slot;
            *slot += 1;

            let x_ratio = if bars.len() <= 1 {
                0.5
            } else {
                ((m.bar_index - start) as f32) / ((bars.len() - 1) as f32)
            }
            .clamp(0.0, 1.0);
            let mut marker = m.clone();
            let plot_price = match marker.kind {
                MarkerKind::Note => {
                    let pit_close = cached
                        .display_bars
                        .get(marker.bar_index)
                        .map(|b| b.close)
                        .unwrap_or(marker.price);
                    marker.price = pit_close;
                    pit_close
                }
                MarkerKind::BuyFill | MarkerKind::SellFill => {
                    let scale = cached
                        .pit_price_scales
                        .get(marker.bar_index)
                        .copied()
                        .unwrap_or(Decimal::ONE);
                    marker.price * scale
                }
                _ => marker.price,
            };
            let p = plot_price.to_f64().unwrap_or(low_min);
            let y_ratio = (((p - low_min) / price_span) as f32).clamp(0.0, 1.0);
            markers.push(ChartMarkerLayout {
                marker,
                x_ratio,
                y_ratio,
                stack_slot: current_slot,
            });
        }

        let timezone_label = if masked {
            "模拟训练回放时钟（已脱敏）".to_string()
        } else if is_crypto {
            "UTC".to_string()
        } else {
            "America/New_York (ET) / UTC".to_string()
        };

        let adjustment = if visible_cas
            .iter()
            .any(|c| c.action_type == CorporateActionType::Split)
        {
            AdjustmentKind::SplitAdjusted
        } else {
            AdjustmentKind::Raw
        };

        let slice = KlineWindowSliceDto {
            instrument_id: out_instrument_id.to_string(),
            venue: out_venue.to_string(),
            timeframe: raw_bars_with_warmup
                .first()
                .map(|b| b.timeframe.clone())
                .unwrap_or_else(|| "1d".to_string()),
            adjustment,
            visible_start_index: start,
            visible_end_index: end,
            total_visible_bars: bars.len(),
            bars,
            lod_candles,
            sma_series,
            ema_series,
            macd_series,
            rsi_series,
            markers,
            gap_indices: Vec::new(),
            has_incomplete_tail: false,
            is_stale: false,
            timezone_label,
        };

        Ok((
            slice,
            cached.real_close_to_display_idx.clone(),
            cached.real_date_to_display_idx.clone(),
        ))
    })
}

// ============================================================================
// 4. Deterministic State Reconstruction, Replay & Scrubbing Helpers
// ============================================================================

pub(super) fn generate_random_seed_u64() -> Result<u64, ErrorEnvelope> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| {
        ErrorEnvelope::new(
            "RNG_FAILED",
            format!("failed to generate training seed: {e}"),
            false,
        )
    })?;
    Ok(u64::from_be_bytes(bytes))
}

pub(crate) fn scrub_sim_event(
    ev: &SimEvent,
    masked: bool,
    quote_currency: &str,
    close_to_idx: &BTreeMap<DateTime<Utc>, usize>,
    date_to_idx: &BTreeMap<NaiveDate, usize>,
) -> SimEvent {
    if !masked {
        return ev.clone();
    }
    match ev.clone() {
        SimEvent::OrderSubmitted {
            order_ids,
            parent_group_id,
            cash_reserved,
            position_reserved,
        } => SimEvent::OrderSubmitted {
            order_ids,
            parent_group_id,
            cash_reserved,
            position_reserved,
        },
        SimEvent::OrderActivated {
            order_id,
            quantity,
            at_stage,
            at_path_price,
            ..
        } => SimEvent::OrderActivated {
            order_id,
            instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
            quantity,
            at_stage,
            at_path_price,
        },
        SimEvent::OrderFilled { mut fill } => {
            let idx = close_to_idx.get(&fill.bar_close_at).copied().unwrap_or(0);
            let (open_at, close_at) = synthetic_open_close_for_index(idx);
            fill.instrument_id = HIDDEN_INSTRUMENT_ID.to_string();
            fill.bar_session_date = synthetic_date_for_index(idx);
            fill.bar_open_at = open_at;
            fill.bar_close_at = close_at;
            if !fill.fee_currency.eq_ignore_ascii_case(quote_currency) {
                fill.fee_currency = "BASE_ASSET".to_string();
            }
            SimEvent::OrderFilled { fill }
        }
        SimEvent::OrderCancelled {
            order_id, reason, ..
        } => SimEvent::OrderCancelled {
            order_id,
            instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
            reason,
        },
        SimEvent::OrderExpired {
            order_id,
            bar_close_at,
            ..
        } => {
            let idx = close_to_idx.get(&bar_close_at).copied().unwrap_or(0);
            let (_, syn_close) = synthetic_open_close_for_index(idx);
            SimEvent::OrderExpired {
                order_id,
                instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
                bar_close_at: syn_close,
            }
        }
        SimEvent::OrderRejected {
            order_id,
            mut rejection,
            ..
        } => {
            rejection.message = "订单校验未通过（隐藏模式已脱敏）".to_string();
            SimEvent::OrderRejected {
                order_id,
                instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
                rejection,
            }
        }
        SimEvent::CorporateActionApplied {
            action_type,
            ex_date,
            ratio_or_amount,
            position_qty_before,
            position_qty_after,
            cash_credited,
            cancelled_order_ids,
            assumption_marker,
            ..
        } => {
            let idx = date_to_idx.get(&ex_date).copied().unwrap_or(0);
            SimEvent::CorporateActionApplied {
                action_id: format!("CA:HIDDEN:{}", idx + 1),
                instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
                action_type,
                ex_date: synthetic_date_for_index(idx),
                ratio_or_amount,
                position_qty_before,
                position_qty_after,
                cash_credited,
                cancelled_order_ids,
                assumption_marker,
            }
        }
        SimEvent::BarAdvanced { mut snapshot } => {
            let idx = close_to_idx
                .get(&snapshot.close_at)
                .copied()
                .unwrap_or(snapshot.step_index as usize);
            let (open_at, close_at) = synthetic_open_close_for_index(idx);
            snapshot.instrument_id = HIDDEN_INSTRUMENT_ID.to_string();
            snapshot.session_date = synthetic_date_for_index(idx);
            snapshot.open_at = open_at;
            snapshot.close_at = close_at;
            SimEvent::BarAdvanced { snapshot }
        }
        SimEvent::SessionFinished {
            cancelled_order_ids,
            ..
        } => SimEvent::SessionFinished {
            visible_until: None,
            cancelled_order_ids,
        },
        SimEvent::OrderModified {
            order_id,
            quantity,
            limit_price,
            stop_price,
            ..
        } => SimEvent::OrderModified {
            order_id,
            instrument_id: HIDDEN_INSTRUMENT_ID.to_string(),
            quantity,
            limit_price,
            stop_price,
            eligible_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        },
        ev @ SimEvent::CommandUndone { .. } => ev,
    }
}

pub(super) fn scrub_training_report(
    mut report: TrainingReport,
    masked: bool,
    history_bar_count: usize,
    train_start_at: DateTime<Utc>,
) -> (TrainingReport, Vec<String>) {
    let mut equity_bar_labels = Vec::with_capacity(report.equity_curve.len());
    for pt in &mut report.equity_curve {
        let disp_one_based = history_bar_count + pt.step_index.max(1);
        equity_bar_labels.push(bar_label_for_one_based(disp_one_based));
        if masked {
            let disp_zero = disp_one_based.saturating_sub(1);
            let (_, syn_close) = synthetic_open_close_for_index(disp_zero);
            pt.session_date = synthetic_date_for_index(disp_zero);
            pt.close_at = syn_close;
        }
    }

    if masked {
        report.visible_until = None;
        for grp in &mut report.closed_trade_groups {
            let entry_zero = if grp.entry_at < train_start_at {
                history_bar_count.saturating_sub(1)
            } else {
                history_bar_count + grp.entry_bar_index
            };
            let exit_zero = if grp.exit_at <= train_start_at {
                history_bar_count.saturating_sub(1)
            } else {
                history_bar_count + grp.exit_bar_index
            };
            let (entry_open, _) = synthetic_open_close_for_index(entry_zero);
            let (_, exit_close) = synthetic_open_close_for_index(exit_zero);
            grp.instrument_id = HIDDEN_INSTRUMENT_ID.to_string();
            grp.entry_session_date = synthetic_date_for_index(entry_zero);
            grp.exit_session_date = synthetic_date_for_index(exit_zero);
            grp.entry_at = entry_open;
            grp.exit_at = exit_close;
        }
        for op in &mut report.open_positions {
            op.instrument_id = HIDDEN_INSTRUMENT_ID.to_string();
            op.entry_session_date = synthetic_date_for_index(history_bar_count);
        }
        report.benchmark.instrument_id = HIDDEN_INSTRUMENT_ID.to_string();
        if report.benchmark.entry_session_date.is_some() {
            report.benchmark.entry_session_date = Some(synthetic_date_for_index(history_bar_count));
        }
    }

    (report, equity_bar_labels)
}

pub(super) fn scrub_order_dto(
    order: &SimOrder,
    masked: bool,
    close_to_idx: &BTreeMap<DateTime<Utc>, usize>,
) -> TrainingOrderDto {
    let disp_idx = close_to_idx
        .range(..=order.submitted_at)
        .next_back()
        .map(|(_, &idx)| idx)
        .unwrap_or(0);
    TrainingOrderDto {
        order_id: order.order_id.clone(),
        order_seq: order.order_seq,
        instrument_id: if masked {
            HIDDEN_INSTRUMENT_ID.to_string()
        } else {
            order.instrument_id.clone()
        },
        side: order.side,
        order_type: order.order_type,
        tif: order.tif,
        priority: order.priority,
        quantity: order.quantity,
        parent_group_id: order.parent_group_id.clone(),
        parent_order_id: order.parent_order_id.clone(),
        oco_peer_order_id: order.oco_peer_order_id.clone(),
        reservation_id: order.reservation_id.clone(),
        group_role: order.group_role,
        has_trade_plan: order.has_trade_plan,
        status: order.status,
        reject_code: order.reject_code,
        cancel_reason: order.cancel_reason.clone(),
        submitted_bar_index: disp_idx,
        submitted_bar_label: bar_label_for_one_based(disp_idx + 1),
        submitted_at: (!masked).then_some(order.submitted_at),
        updated_at: (!masked).then_some(order.updated_at),
    }
}

pub(super) fn scrub_fill_dto(
    fill: &SimFill,
    masked: bool,
    quote_currency: &str,
    close_to_idx: &BTreeMap<DateTime<Utc>, usize>,
) -> TrainingFillDto {
    let disp_idx = close_to_idx
        .get(&fill.bar_close_at)
        .copied()
        .or_else(|| {
            close_to_idx
                .range(..=fill.bar_close_at)
                .next_back()
                .map(|(_, &idx)| idx)
        })
        .unwrap_or(0);
    let fee_currency = if masked && !fill.fee_currency.eq_ignore_ascii_case(quote_currency) {
        "BASE_ASSET".to_string()
    } else {
        fill.fee_currency.clone()
    };
    TrainingFillDto {
        fill_id: fill.fill_id.clone(),
        fill_seq: fill.fill_seq,
        order_id: fill.order_id.clone(),
        order_seq: fill.order_seq,
        parent_group_id: fill.parent_group_id.clone(),
        group_role: fill.group_role,
        has_trade_plan: fill.has_trade_plan,
        instrument_id: if masked {
            HIDDEN_INSTRUMENT_ID.to_string()
        } else {
            fill.instrument_id.clone()
        },
        bar_index: disp_idx,
        bar_label: bar_label_for_one_based(disp_idx + 1),
        bar_session_date: (!masked).then_some(fill.bar_session_date),
        bar_close_at: (!masked).then_some(fill.bar_close_at),
        path_stage: fill.path_stage,
        raw_path_price: fill.raw_path_price,
        fill_price: fill.fill_price,
        gross_quantity: fill.gross_quantity,
        net_base_quantity: fill.net_base_quantity,
        side: fill.side,
        fee_amount: fill.fee_amount,
        fee_currency,
        fee_quote_value: fill.fee_quote_value,
        realized_pnl_delta: fill.realized_pnl_delta,
        ambiguous: fill.ambiguous,
    }
}

pub(super) fn scrub_note_dto(
    note: &TrainingNoteRecord,
    masked: bool,
    close_to_idx: &BTreeMap<DateTime<Utc>, usize>,
    history_bar_count: usize,
) -> TrainingNoteDto {
    let disp_idx = close_to_idx
        .get(&note.visible_until)
        .copied()
        .or_else(|| {
            close_to_idx
                .range(..=note.visible_until)
                .next_back()
                .map(|(_, &idx)| idx)
        })
        .unwrap_or_else(|| {
            history_bar_count
                .saturating_add(usize::try_from(note.bar_index.max(0)).unwrap_or(0))
                .saturating_sub(1)
        });
    TrainingNoteDto {
        note_id: note.note_id.clone(),
        run_id: note.run_id.clone(),
        bar_index: note.bar_index,
        display_bar_index: disp_idx,
        bar_label: bar_label_for_one_based(disp_idx + 1),
        visible_until: (!masked).then_some(note.visible_until),
        rationale: note.rationale.clone(),
        emotion_tag: note.emotion_tag.clone(),
        linked_order_id: note.linked_order_id.clone(),
        archived_journal_note_id: note.archived_journal_note_id.clone(),
        created_at: note.created_at.clone(),
        updated_at: note.updated_at.clone(),
    }
}
