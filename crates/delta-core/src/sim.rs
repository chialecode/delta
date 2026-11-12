use crate::decimal::to_canonical_text;
use crate::indicators::filter_visible_final_bars;
use crate::model::{
    AdjustmentKind, BarRecord, CorporateActionRecord, CorporateActionType, Instrument,
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Frozen simulation engine version identifier for M4 (`S-36`).
pub const SIM_ENGINE_VERSION: &str = "m4-sim-1";

/// Market-order cash reservation buffer (`10%` above last visible close, `S-40`).
pub const MARKET_BUY_RESERVATION_BUFFER_RATE: &str = "0.10";

/// Normalize a `Decimal` so `-0` and trailing fractional zeros never cause canonical divergence.
#[inline]
pub fn canon_decimal(value: Decimal) -> Decimal {
    if value.is_zero() {
        Decimal::ZERO
    } else {
        value.normalize()
    }
}

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
// 1. W01 Pure Math Helpers: Counter-mode PRNG (S-38) & PIT Split Adjustment (S-37)
// ============================================================================

/// Compute one deterministic 64-bit random word from `SHA-256(seed || purpose || counter)` (`S-38`).
/// Uses explicit big-endian byte length framing for `seed` and `purpose` to eliminate boundary collisions.
pub fn prng_block_u64(seed: &[u8], purpose: &str, counter: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update((seed.len() as u64).to_be_bytes());
    hasher.update(seed);
    hasher.update((purpose.len() as u64).to_be_bytes());
    hasher.update(purpose.as_bytes());
    hasher.update(counter.to_be_bytes());
    let digest = hasher.finalize();
    let mut first_eight = [0u8; 8];
    first_eight.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first_eight)
}

/// Deterministic counter-mode PRNG backed by `SHA-256(seed || purpose || counter)` (`S-38`).
/// Algorithm is frozen with `m4-sim-1` and independent of external RNG crate upgrades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicPrng {
    seed: Vec<u8>,
    purpose: String,
    counter: u64,
}

impl DeterministicPrng {
    pub fn new(seed: impl AsRef<[u8]>, purpose: impl Into<String>) -> Self {
        Self {
            seed: seed.as_ref().to_vec(),
            purpose: purpose.into(),
            counter: 0,
        }
    }

    pub fn with_counter(seed: impl AsRef<[u8]>, purpose: impl Into<String>, counter: u64) -> Self {
        Self {
            seed: seed.as_ref().to_vec(),
            purpose: purpose.into(),
            counter,
        }
    }

    pub fn counter(&self) -> u64 {
        self.counter
    }

    pub fn next_u64(&mut self) -> u64 {
        let word = prng_block_u64(&self.seed, &self.purpose, self.counter);
        self.counter = self.counter.wrapping_add(1);
        word
    }

    /// Rejection-sampled unbiased integer in `[0, upper_exclusive)` (`S-38`).
    pub fn next_u64_below(&mut self, upper_exclusive: u64) -> Result<u64, SimError> {
        if upper_exclusive == 0 {
            return Err(SimError::InvalidSamplingRange(
                "upper_exclusive must be at least 1".into(),
            ));
        }
        // 2^64 mod upper_exclusive == upper_exclusive.wrapping_neg() % upper_exclusive
        let threshold = upper_exclusive.wrapping_neg() % upper_exclusive;
        loop {
            let candidate = self.next_u64();
            if candidate >= threshold {
                return Ok(candidate % upper_exclusive);
            }
        }
    }

    /// Rejection-sampled unbiased slice index in `[0, len)`.
    pub fn next_index(&mut self, len: usize) -> Result<usize, SimError> {
        let idx = self.next_u64_below(len as u64)?;
        Ok(idx as usize)
    }
}

/// Deterministically sample an initial candidate index plus `redraw_count` explicit pre-start redraws
/// from a candidate pool of size `pool_len` (`S-38`, `M4-A-02`).
/// When `pool_len > 1`, each redraw avoids immediately repeating the previous draw.
pub fn sample_candidate_sequence(
    seed: &str,
    pool_len: usize,
    redraw_count: u32,
) -> Result<Vec<usize>, SimError> {
    if pool_len == 0 {
        return Err(SimError::InvalidSamplingRange(
            "candidate pool is empty".into(),
        ));
    }
    let mut prng = DeterministicPrng::new(seed.as_bytes(), "training_candidate_sample");
    let mut history = Vec::with_capacity((redraw_count as usize) + 1);
    let first = prng.next_index(pool_len)?;
    history.push(first);
    for _ in 0..redraw_count {
        if pool_len == 1 {
            let _ = prng.next_u64();
            history.push(0);
        } else {
            let prev = *history.last().unwrap_or(&0);
            let offset = prng.next_index(pool_len - 1)?;
            let next = if offset >= prev { offset + 1 } else { offset };
            history.push(next);
        }
    }
    Ok(history)
}

fn corporate_action_source_priority(source: &str) -> u8 {
    match source {
        "tiingo" => 3,
        "yahoo" => 2,
        _ => 1,
    }
}

fn deduplicate_corporate_actions<'a>(
    actions: &'a [CorporateActionRecord],
) -> Vec<&'a CorporateActionRecord> {
    let mut dedup: BTreeMap<(String, NaiveDate, u8), &'a CorporateActionRecord> = BTreeMap::new();
    for action in actions {
        let type_disc = match action.action_type {
            CorporateActionType::Split => 0,
            CorporateActionType::CashDividend => 1,
        };
        let key = (action.instrument_id.clone(), action.ex_date, type_disc);
        match dedup.get(&key) {
            Some(existing) => {
                let new_rank = (
                    action.factor_version,
                    corporate_action_source_priority(&action.source),
                    &action.id,
                );
                let old_rank = (
                    existing.factor_version,
                    corporate_action_source_priority(&existing.source),
                    &existing.id,
                );
                if new_rank > old_rank {
                    dedup.insert(key, action);
                }
            }
            None => {
                dedup.insert(key, action);
            }
        }
    }
    dedup.into_values().collect()
}

/// Derive point-in-time split-adjusted bars at `visible_until` (`S-37`, `M4-A-03`):
/// - Strictly truncates bars to `is_final && close_at <= visible_until`.
/// - Applies ONLY `Split` corporate actions where `ex_date <= current_visible_session_date`
///   backwards to bars with `bar.session_date < ex_date`; future splits (`ex_date > cutoff`)
///   are excluded prior to validation so future sentinels can never leak.
/// - Cash dividends are never adjusted into prices (`S-37`).
pub fn derive_point_in_time_split_adjusted_bars(
    raw_bars: &[BarRecord],
    corporate_actions: &[CorporateActionRecord],
    visible_until: DateTime<Utc>,
) -> Result<Vec<BarRecord>, SimError> {
    let vis_refs = filter_visible_final_bars(raw_bars, Some(visible_until));
    if vis_refs.is_empty() {
        return Ok(Vec::new());
    }

    let utc_cutoff_date = visible_until.date_naive();
    let mut max_session_by_instrument: BTreeMap<String, NaiveDate> = BTreeMap::new();
    for bar in &vis_refs {
        let entry = max_session_by_instrument
            .entry(bar.instrument_id.clone())
            .or_insert(bar.session_date);
        if bar.session_date > *entry {
            *entry = bar.session_date;
        }
    }

    // Filter to visible Split actions BEFORE inspecting ratio_or_amount so future sentinel rows are untouched.
    let visible_splits: Vec<CorporateActionRecord> = corporate_actions
        .iter()
        .filter(|ca| {
            if ca.action_type != CorporateActionType::Split {
                return false;
            }
            let Some(&inst_max_session) = max_session_by_instrument.get(&ca.instrument_id) else {
                return false;
            };
            let cutoff = inst_max_session.min(utc_cutoff_date);
            ca.ex_date <= cutoff
        })
        .cloned()
        .collect();

    let dedup_splits = deduplicate_corporate_actions(&visible_splits);

    let mut out = Vec::with_capacity(vis_refs.len());
    for bar in vis_refs {
        let mut split_factor = Decimal::ONE;
        for action in &dedup_splits {
            if action.instrument_id != bar.instrument_id || action.ex_date <= bar.session_date {
                continue;
            }
            if action.ratio_or_amount <= Decimal::ZERO {
                return Err(SimError::InvalidCorporateAction {
                    action_id: action.id.clone(),
                    reason: format!(
                        "split ratio must be positive, got {}",
                        action.ratio_or_amount
                    ),
                });
            }
            split_factor = split_factor
                .checked_mul(action.ratio_or_amount)
                .ok_or_else(|| SimError::ArithmeticOverflow("pit_split_factor".into()))?;
        }

        let adjust_price = |raw_price: Decimal| -> Result<Decimal, SimError> {
            if split_factor == Decimal::ONE {
                return Ok(canon_decimal(raw_price));
            }
            let scale = 6_u32.max(raw_price.scale());
            let divided = raw_price
                .checked_div(split_factor)
                .ok_or_else(|| SimError::ArithmeticOverflow("pit_split_price".into()))?;
            Ok(canon_decimal(divided.round_dp_with_strategy(
                scale,
                RoundingStrategy::MidpointAwayFromZero,
            )))
        };

        let mut adjusted = bar.clone();
        adjusted.adjustment = AdjustmentKind::SplitAdjusted;
        adjusted.open = adjust_price(bar.open)?;
        adjusted.high = adjust_price(bar.high)?;
        adjusted.low = adjust_price(bar.low)?;
        adjusted.close = adjust_price(bar.close)?;
        adjusted.volume = canon_decimal(
            bar.volume
                .checked_mul(split_factor)
                .ok_or_else(|| SimError::ArithmeticOverflow("pit_split_volume".into()))?,
        );
        out.push(adjusted);
    }
    Ok(out)
}

// ============================================================================
// 2. Tick & Lot Validation + Adverse Tick Rounding (S-39, M4-A-04)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepValidationViolation {
    pub invalid_value: Decimal,
    pub step_size: Decimal,
    pub nearest_lower: Option<Decimal>,
    pub nearest_upper: Option<Decimal>,
}

/// Check that `value > 0` and `value` is an exact integer multiple of `step_size` (`tick_size` or `lot_size`).
/// Returns nearest valid lower (> 0) and upper multiples when violated (`M4-A-04`).
pub fn validate_step_multiple(
    value: Decimal,
    step_size: Decimal,
) -> Result<Decimal, StepValidationViolation> {
    let step = canon_decimal(step_size);
    if value <= Decimal::ZERO {
        return Err(StepValidationViolation {
            invalid_value: canon_decimal(value),
            step_size: step,
            nearest_lower: None,
            nearest_upper: (step > Decimal::ZERO).then_some(step),
        });
    }
    let rem = value % step;
    if rem.is_zero() {
        return Ok(canon_decimal(value));
    }
    let lower = canon_decimal(value - rem);
    let upper = canon_decimal(lower + step);
    Err(StepValidationViolation {
        invalid_value: canon_decimal(value),
        step_size: step,
        nearest_lower: (lower > Decimal::ZERO).then_some(lower),
        nearest_upper: Some(upper),
    })
}

/// Round an execution price adversely to `tick_size` (`S-39`, `M4-A-04`):
/// - `Buy` rounds UP (ceiling tick).
/// - `Sell` rounds DOWN (floor tick, clamped at `tick_size` so execution price remains strictly positive).
pub fn round_price_adverse_tick(
    raw_price: Decimal,
    side: OrderSide,
    tick_size: Decimal,
) -> Decimal {
    let tick = canon_decimal(tick_size);
    if tick <= Decimal::ZERO {
        return canon_decimal(raw_price);
    }
    if raw_price <= Decimal::ZERO {
        return tick;
    }
    let rem = raw_price % tick;
    if rem.is_zero() {
        return canon_decimal(raw_price);
    }
    match side {
        OrderSide::Buy => canon_decimal((raw_price - rem) + tick),
        OrderSide::Sell => {
            let floored = canon_decimal(raw_price - rem);
            if floored > Decimal::ZERO {
                floored
            } else {
                tick
            }
        }
    }
}

// ============================================================================
// 3. Composable Fee Profile & Multi-Fill Settlement Engine (S-41, M4-A-07, M4-A-08)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeMarketCategory {
    Any,
    UsEquity,
    CryptoSpot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeCurrencyMode {
    /// Fee is charged in the instrument's quote currency (e.g. USD, USDT).
    Quote,
    /// On a Buy, fee is charged in the received base asset and deducted from received base quantity;
    /// on a Sell, received asset is the quote currency (`M4-A-07`).
    ReceivedAsset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeRoundingMode {
    HalfUp,
    HalfEven,
    Up,
    Down,
}

impl FeeRoundingMode {
    pub fn to_strategy(self) -> RoundingStrategy {
        match self {
            Self::HalfUp => RoundingStrategy::MidpointAwayFromZero,
            Self::HalfEven => RoundingStrategy::MidpointNearestEven,
            Self::Up => RoundingStrategy::AwayFromZero,
            Self::Down => RoundingStrategy::ToZero,
        }
    }

    pub fn round_value(self, value: Decimal, scale: u32) -> Decimal {
        canon_decimal(value.round_dp_with_strategy(scale, self.to_strategy()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeChargingTier {
    /// Variable fee rounded on each fill; per-order fixed/min/max applied once across the order (`M4-A-07`).
    PerFill,
    /// Variable fees accumulated unrounded across fills and settled/rounded once at order end (`M4-A-07`).
    PerOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SlippageSpec {
    Zero,
    Bps(Decimal),
    Ticks(u32),
}

impl SlippageSpec {
    pub fn canonical(self) -> Self {
        match self {
            Self::Zero => Self::Zero,
            Self::Bps(bps) if bps.is_zero() => Self::Zero,
            Self::Bps(bps) => Self::Bps(canon_decimal(bps)),
            Self::Ticks(0) => Self::Zero,
            Self::Ticks(n) => Self::Ticks(n),
        }
    }

    pub fn delta_for_price(self, reference_price: Decimal, tick_size: Decimal) -> Decimal {
        match self {
            Self::Zero => Decimal::ZERO,
            Self::Bps(bps) => (reference_price * bps) / Decimal::from(10_000),
            Self::Ticks(n) => Decimal::from(n) * tick_size,
        }
    }
}

/// Immutable, versioned fee & execution environment profile (`S-41`, `M4-A-07`, `M4-A-08`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeProfileVersion {
    pub id: String,
    pub preset_key: String,
    pub name: String,
    pub version: u32,
    pub market_category: FeeMarketCategory,
    /// Proportional rate (e.g. `0.001` for `0.10%`).
    pub notional_rate: Decimal,
    /// Fee per base unit / share in settled fee currency.
    pub per_unit_fee: Decimal,
    /// Fixed fee charged once per order (if at least one fill occurs).
    pub per_order_fixed_fee: Decimal,
    /// Minimum total fee per executed order (settled once when order finishes, never per fill).
    pub min_fee_per_order: Option<Decimal>,
    /// Maximum total fee cap per executed order.
    pub max_fee_per_order: Option<Decimal>,
    pub fee_currency_mode: FeeCurrencyMode,
    pub rounding_mode: FeeRoundingMode,
    pub rounding_scale: u32,
    pub charging_tier: FeeChargingTier,
    pub slippage: SlippageSpec,
    /// One-way half-spread in basis points applied adversely to Market orders (`S-39`, `S-41`).
    pub half_spread_bps: Decimal,
    pub is_builtin: bool,
    /// True ONLY for real broker/exchange presets; requires verified `official_source_url` + `official_retrieved_at` (`M4-A-08`).
    pub is_platform_preset: bool,
    pub official_source_url: Option<String>,
    pub official_retrieved_at: Option<NaiveDate>,
    pub effective_from: Option<NaiveDate>,
    pub effective_to: Option<NaiveDate>,
    pub assumption_note: String,
}

impl FeeProfileVersion {
    pub fn canonical(mut self) -> Self {
        self.notional_rate = canon_decimal(self.notional_rate);
        self.per_unit_fee = canon_decimal(self.per_unit_fee);
        self.per_order_fixed_fee = canon_decimal(self.per_order_fixed_fee);
        self.min_fee_per_order = self.min_fee_per_order.map(canon_decimal);
        self.max_fee_per_order = self.max_fee_per_order.map(canon_decimal);
        self.slippage = self.slippage.canonical();
        self.half_spread_bps = canon_decimal(self.half_spread_bps);
        self
    }

    pub fn validate(&self) -> Result<(), SimError> {
        let invalid = |reason: &str| SimError::InvalidFeeProfile {
            profile_id: self.id.clone(),
            reason: reason.to_string(),
        };
        if self.id.trim().is_empty() || self.name.trim().is_empty() {
            return Err(invalid("id and name must be non-empty"));
        }
        if self.version == 0 {
            return Err(invalid("version must be >= 1"));
        }
        if self.notional_rate < Decimal::ZERO
            || self.per_unit_fee < Decimal::ZERO
            || self.per_order_fixed_fee < Decimal::ZERO
            || self.half_spread_bps < Decimal::ZERO
        {
            return Err(invalid("fee rates and half-spread must be non-negative"));
        }
        if self.notional_rate >= Decimal::ONE {
            return Err(invalid("notional_rate must be < 1"));
        }
        if let SlippageSpec::Bps(bps) = self.slippage {
            if bps < Decimal::ZERO {
                return Err(invalid("slippage bps must be non-negative"));
            }
        }
        if self.rounding_scale > 18 {
            return Err(invalid("rounding_scale must be <= 18"));
        }
        if let Some(min_f) = self.min_fee_per_order {
            if min_f < Decimal::ZERO {
                return Err(invalid("min_fee_per_order must be non-negative"));
            }
        }
        if let Some(max_f) = self.max_fee_per_order {
            if max_f < Decimal::ZERO {
                return Err(invalid("max_fee_per_order must be non-negative"));
            }
        }
        if let (Some(min_f), Some(max_f)) = (self.min_fee_per_order, self.max_fee_per_order) {
            if min_f > max_f {
                return Err(invalid("min_fee_per_order cannot exceed max_fee_per_order"));
            }
        }
        if self.is_platform_preset {
            let has_url = self
                .official_source_url
                .as_ref()
                .is_some_and(|u| u.starts_with("https://"));
            if !has_url || self.official_retrieved_at.is_none() {
                return Err(invalid(
                    "platform presets must carry a verified official https URL and retrieval date",
                ));
            }
        }
        if self.assumption_note.trim().is_empty() {
            return Err(invalid("assumption_note must be non-empty"));
        }
        Ok(())
    }

    /// Create a new immutable version from `self` without mutating `self` (`M4-A-08`).
    pub fn derive_next_version(
        &self,
        new_id: impl Into<String>,
        edit: impl FnOnce(&mut FeeProfileVersion),
    ) -> Result<FeeProfileVersion, SimError> {
        let mut next = self.clone();
        next.id = new_id.into();
        next.version = self
            .version
            .checked_add(1)
            .ok_or_else(|| SimError::ArithmeticOverflow("fee_profile_version".into()))?;
        next.is_builtin = false;
        edit(&mut next);
        let next = next.canonical();
        next.validate()?;
        Ok(next)
    }
}

/// Return the built-in M4 fee profile templates (`S-41`, `M4-A-08`).
///
/// Verification note (`2026-11-12`): Futu US equity schedules vary by legal entity (HK/SG/US),
/// package choice (fixed vs monthly tiered platform fee), and sell-only regulatory pass-throughs;
/// OKX spot schedules vary across Regular/VIP 1-9 tiers, OKB holdings, and Maker vs Taker roles
/// that daily OHLC bars cannot distinguish. Per `S-41`, platform-specific Futu/OKX presets remain
/// in `LOOP-06` and only the three explicit scenario-assumption templates are shipped built-in.
pub fn builtin_fee_profiles() -> Vec<FeeProfileVersion> {
    vec![
        FeeProfileVersion {
            id: "builtin-zero-cost-v1".to_string(),
            preset_key: "zero_cost".to_string(),
            name: "Zero Cost (Scenario Assumption)".to_string(),
            version: 1,
            market_category: FeeMarketCategory::Any,
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
            is_builtin: true,
            is_platform_preset: false,
            official_source_url: None,
            official_retrieved_at: None,
            effective_from: None,
            effective_to: None,
            assumption_note: "Scenario assumption: zero commission, zero spread, zero slippage"
                .to_string(),
        }
        .canonical(),
        FeeProfileVersion {
            id: "builtin-us-equity-per-share-example-v1".to_string(),
            preset_key: "us_equity_per_share_example".to_string(),
            name: "US Equity Per-Share Example (Scenario Assumption)".to_string(),
            version: 1,
            market_category: FeeMarketCategory::UsEquity,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::new(5, 3), // 0.005 USD / share
            per_order_fixed_fee: Decimal::ZERO,
            min_fee_per_order: Some(Decimal::ONE), // 1.00 USD / order min
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage: SlippageSpec::Bps(Decimal::from(2)),
            half_spread_bps: Decimal::ONE,
            is_builtin: true,
            is_platform_preset: false,
            official_source_url: None,
            official_retrieved_at: None,
            effective_from: None,
            effective_to: None,
            assumption_note: "Scenario assumption: illustrative $0.005/share (min $1.00/order), 1 bps half-spread, 2 bps slippage; not a live broker schedule (Futu/OKX platform presets deferred to LOOP-06)".to_string(),
        }
        .canonical(),
        FeeProfileVersion {
            id: "builtin-crypto-rate-example-v1".to_string(),
            preset_key: "crypto_rate_example".to_string(),
            name: "Crypto Notional Rate Example (Scenario Assumption)".to_string(),
            version: 1,
            market_category: FeeMarketCategory::CryptoSpot,
            notional_rate: Decimal::new(1, 3), // 0.001 (0.10%)
            per_unit_fee: Decimal::ZERO,
            per_order_fixed_fee: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 6,
            charging_tier: FeeChargingTier::PerOrder,
            slippage: SlippageSpec::Bps(Decimal::from(3)),
            half_spread_bps: Decimal::from(2),
            is_builtin: true,
            is_platform_preset: false,
            official_source_url: None,
            official_retrieved_at: None,
            effective_from: None,
            effective_to: None,
            assumption_note: "Scenario assumption: illustrative 0.10% notional fee in quote currency, 2 bps half-spread, 3 bps slippage; not a live exchange tier schedule (Futu/OKX platform presets deferred to LOOP-06)".to_string(),
        }
        .canonical(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticFillInput {
    pub fill_id: String,
    pub quantity: Decimal,
    pub fill_price: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledFillFee {
    pub fill_id: String,
    pub gross_quantity: Decimal,
    pub net_received_base_quantity: Decimal,
    pub fill_price: Decimal,
    pub fee_amount: Decimal,
    pub fee_currency: String,
    pub fee_quote_value: Decimal,
    pub net_quote_cash_delta: Decimal,
    pub min_fee_top_up_applied: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderFeeSettlement {
    pub fee_profile_id: String,
    pub side: OrderSide,
    pub fee_currency: String,
    pub fills: Vec<SettledFillFee>,
    pub total_fee_amount: Decimal,
    pub total_fee_quote_value: Decimal,
    pub min_fee_top_up_applied: Decimal,
    pub max_fee_cap_applied: bool,
}

/// Settle composable fees over a completed order consisting of one or multiple fills (`S-41`, `M4-A-07`).
///
/// Guarantees:
/// - `min_fee_per_order` and `max_fee_per_order` are applied ONCE across the order when it ends,
///   never duplicated per fill.
/// - `fills.iter().map(|f| f.fee_amount).sum() == total_fee_amount` holds identically.
/// - When `fee_currency_mode == ReceivedAsset` on a `Buy`, fee is denominated in `base_asset_id`
///   and deducted from `net_received_base_quantity` (`M4-A-07`).
pub fn settle_order_fees(
    profile: &FeeProfileVersion,
    side: OrderSide,
    base_asset_id: &str,
    quote_currency: &str,
    fills: &[SyntheticFillInput],
) -> Result<OrderFeeSettlement, SimError> {
    profile.validate()?;
    let fee_in_received_base =
        side == OrderSide::Buy && profile.fee_currency_mode == FeeCurrencyMode::ReceivedAsset;
    let fee_currency = if fee_in_received_base {
        base_asset_id.to_string()
    } else {
        quote_currency.to_string()
    };

    if fills.is_empty() {
        return Ok(OrderFeeSettlement {
            fee_profile_id: profile.id.clone(),
            side,
            fee_currency,
            fills: Vec::new(),
            total_fee_amount: Decimal::ZERO,
            total_fee_quote_value: Decimal::ZERO,
            min_fee_top_up_applied: Decimal::ZERO,
            max_fee_cap_applied: false,
        });
    }

    let min_rounded = profile
        .min_fee_per_order
        .map(|m| profile.rounding_mode.round_value(m, profile.rounding_scale));
    let max_rounded = profile
        .max_fee_per_order
        .map(|m| profile.rounding_mode.round_value(m, profile.rounding_scale));

    let mut settled_fills = Vec::with_capacity(fills.len());
    let mut cumulative_settled = Decimal::ZERO;
    let mut cumulative_unrounded = Decimal::ZERO;
    let mut min_top_up_applied = Decimal::ZERO;
    let mut max_cap_applied = false;
    let last_idx = fills.len() - 1;

    for (idx, fill) in fills.iter().enumerate() {
        if fill.quantity <= Decimal::ZERO || fill.fill_price <= Decimal::ZERO {
            return Err(SimError::InvalidFeeProfile {
                profile_id: profile.id.clone(),
                reason: "fill quantity and fill_price must be positive".into(),
            });
        }
        let is_last = idx == last_idx;
        let var_raw = if fee_in_received_base {
            fill.quantity * profile.notional_rate + fill.quantity * profile.per_unit_fee
        } else {
            let notional = fill.quantity * fill.fill_price;
            notional * profile.notional_rate + fill.quantity * profile.per_unit_fee
        };
        let fixed_raw = if idx == 0 {
            profile.per_order_fixed_fee
        } else {
            Decimal::ZERO
        };

        let mut fill_top_up = Decimal::ZERO;
        let fill_fee = match profile.charging_tier {
            FeeChargingTier::PerFill => {
                let mut f = profile
                    .rounding_mode
                    .round_value(var_raw + fixed_raw, profile.rounding_scale);
                let mut next_cum = cumulative_settled + f;
                if let Some(max_f) = max_rounded {
                    if next_cum > max_f {
                        max_cap_applied = true;
                        f = (max_f - cumulative_settled).max(Decimal::ZERO);
                        next_cum = cumulative_settled + f;
                    }
                }
                if is_last {
                    if let Some(min_f) = min_rounded {
                        if next_cum < min_f {
                            fill_top_up = canon_decimal(min_f - next_cum);
                            min_top_up_applied = fill_top_up;
                            f = canon_decimal(f + fill_top_up);
                        }
                    }
                }
                canon_decimal(f)
            }
            FeeChargingTier::PerOrder => {
                cumulative_unrounded += var_raw + fixed_raw;
                if !is_last {
                    let mut target = profile
                        .rounding_mode
                        .round_value(cumulative_unrounded, profile.rounding_scale);
                    if let Some(max_f) = max_rounded {
                        if target > max_f {
                            max_cap_applied = true;
                            target = max_f;
                        }
                    }
                    canon_decimal((target - cumulative_settled).max(Decimal::ZERO))
                } else {
                    let pre_min_rounded = profile
                        .rounding_mode
                        .round_value(cumulative_unrounded, profile.rounding_scale);
                    let mut clamped_raw = cumulative_unrounded;
                    if let Some(min_f) = profile.min_fee_per_order {
                        if clamped_raw < min_f {
                            clamped_raw = min_f;
                        }
                    }
                    if let Some(max_f) = profile.max_fee_per_order {
                        if clamped_raw > max_f {
                            max_cap_applied = true;
                            clamped_raw = max_f;
                        }
                    }
                    let mut final_total = profile
                        .rounding_mode
                        .round_value(clamped_raw, profile.rounding_scale);
                    if let Some(max_f) = max_rounded {
                        if final_total > max_f {
                            max_cap_applied = true;
                            final_total = max_f;
                        }
                    }
                    if let Some(min_f) = min_rounded {
                        if final_total < min_f {
                            final_total = min_f;
                        }
                    }
                    let base_capped = match max_rounded {
                        Some(max_f) => pre_min_rounded.min(max_f),
                        None => pre_min_rounded,
                    };
                    if final_total > base_capped {
                        fill_top_up = canon_decimal(final_total - base_capped);
                        min_top_up_applied = fill_top_up;
                    }
                    canon_decimal((final_total - cumulative_settled).max(Decimal::ZERO))
                }
            }
        };

        cumulative_settled = canon_decimal(cumulative_settled + fill_fee);

        let (net_base_qty, fee_quote_val, net_cash_delta) = if fee_in_received_base {
            if fill_fee >= fill.quantity {
                return Err(SimError::InvalidFeeProfile {
                    profile_id: profile.id.clone(),
                    reason: format!(
                        "received-asset fee {} meets or exceeds fill quantity {}",
                        fill_fee, fill.quantity
                    ),
                });
            }
            let net_base = canon_decimal(fill.quantity - fill_fee);
            let quote_val = canon_decimal(fill_fee * fill.fill_price);
            let cash_delta = canon_decimal(-(fill.quantity * fill.fill_price));
            (net_base, quote_val, cash_delta)
        } else if side == OrderSide::Buy {
            let cash_out = canon_decimal(fill.quantity * fill.fill_price + fill_fee);
            (canon_decimal(fill.quantity), fill_fee, -cash_out)
        } else {
            let cash_in = canon_decimal(fill.quantity * fill.fill_price - fill_fee);
            (Decimal::ZERO, fill_fee, cash_in)
        };

        settled_fills.push(SettledFillFee {
            fill_id: fill.fill_id.clone(),
            gross_quantity: canon_decimal(fill.quantity),
            net_received_base_quantity: net_base_qty,
            fill_price: canon_decimal(fill.fill_price),
            fee_amount: fill_fee,
            fee_currency: fee_currency.clone(),
            fee_quote_value: fee_quote_val,
            net_quote_cash_delta: net_cash_delta,
            min_fee_top_up_applied: fill_top_up,
        });
    }

    let total_fee_quote_value = canon_decimal(
        settled_fills
            .iter()
            .map(|f| f.fee_quote_value)
            .sum::<Decimal>(),
    );

    Ok(OrderFeeSettlement {
        fee_profile_id: profile.id.clone(),
        side,
        fee_currency,
        fills: settled_fills,
        total_fee_amount: cumulative_settled,
        total_fee_quote_value,
        min_fee_top_up_applied: min_top_up_applied,
        max_fee_cap_applied: max_cap_applied,
    })
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
}

impl SimRejectCode {
    pub fn as_str(self) -> &'static str {
        match self {
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
        };
        state.normalize_fields();
        Ok(state)
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
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_json().as_bytes());
        hex::encode(hasher.finalize())
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

fn validate_tick_field(
    value: Decimal,
    tick_size: Decimal,
    field_name: &str,
) -> Result<Decimal, Box<OrderRejection>> {
    validate_step_multiple(value, tick_size).map_err(|v| {
        Box::new(OrderRejection {
            code: SimRejectCode::InvalidTickSize,
            message: format!(
                "{field_name} {} violates tick_size {}; nearest valid lower={:?}, upper={:?}",
                to_canonical_text(v.invalid_value),
                to_canonical_text(v.step_size),
                v.nearest_lower.map(to_canonical_text),
                v.nearest_upper.map(to_canonical_text),
            ),
            field: Some(field_name.to_string()),
            nearest_lower: v.nearest_lower,
            nearest_upper: v.nearest_upper,
            required: None,
            available: None,
        })
    })
}

/// Validate an `OrderSubmitRequest` against instrument tick/lot rules, spot long-only rules,
/// and cash/position reservation limits without mutating `state` (`S-39`, `S-40`, `M4-A-04`, `M4-A-06`).
pub fn preview_order_submission(
    state: &SimState,
    req: &OrderSubmitRequest,
) -> Result<OrderCostPreview, SimError> {
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    let spec = state
        .instruments
        .get(&req.instrument_id)
        .ok_or_else(|| SimError::UnknownInstrument(req.instrument_id.clone()))?;

    let qty = validate_step_multiple(req.quantity, spec.lot_size).map_err(|v| {
        SimError::rejected(OrderRejection {
            code: SimRejectCode::InvalidLotSize,
            message: format!(
                "quantity {} violates lot_size {}; nearest valid lower={:?}, upper={:?}",
                to_canonical_text(v.invalid_value),
                to_canonical_text(v.step_size),
                v.nearest_lower.map(to_canonical_text),
                v.nearest_upper.map(to_canonical_text),
            ),
            field: Some("quantity".to_string()),
            nearest_lower: v.nearest_lower,
            nearest_upper: v.nearest_upper,
            required: None,
            available: None,
        })
    })?;

    let avail_cash = state.available_cash();
    let avail_pos = state.available_position(&req.instrument_id);
    let buffer_mult =
        Decimal::ONE + Decimal::from_str_exact(MARKET_BUY_RESERVATION_BUFFER_RATE).unwrap();

    match &req.kind {
        OrderSubmitKind::Single {
            side,
            order_type,
            attached_bracket,
        } => {
            if matches!(order_type, SimOrderType::StopLimit { .. }) {
                return Err(SimError::rejected(OrderRejection {
                    code: SimRejectCode::UnsupportedOrderType,
                    message: "stop_limit orders are not supported in M4".to_string(),
                    field: Some("order_type".to_string()),
                    nearest_lower: None,
                    nearest_upper: None,
                    required: None,
                    available: None,
                }));
            }
            if *side == OrderSide::Sell && attached_bracket.is_some() {
                return Err(SimError::rejected(OrderRejection {
                    code: SimRejectCode::InvalidOrderParams,
                    message: "attached bracket is only supported on buy entry orders in spot long-only mode".to_string(),
                    field: Some("attached_bracket".to_string()),
                    nearest_lower: None,
                    nearest_upper: None,
                    required: None,
                    available: None,
                }));
            }

            let entry_explicit_price = match order_type {
                SimOrderType::Market => None,
                SimOrderType::Limit { limit_price } => Some(
                    validate_tick_field(*limit_price, spec.tick_size, "limit_price")
                        .map_err(SimError::OrderRejected)?,
                ),
                SimOrderType::StopMarket { stop_price } => Some(
                    validate_tick_field(*stop_price, spec.tick_size, "stop_price")
                        .map_err(SimError::OrderRejected)?,
                ),
                SimOrderType::StopLimit { .. } => unreachable!(),
            };

            if let Some(bracket) = attached_bracket {
                if bracket.take_profit_limit.is_none() && bracket.stop_loss_stop.is_none() {
                    return Err(SimError::rejected(OrderRejection {
                        code: SimRejectCode::InvalidOrderParams,
                        message:
                            "attached bracket must specify take_profit_limit and/or stop_loss_stop"
                                .to_string(),
                        field: Some("attached_bracket".to_string()),
                        nearest_lower: None,
                        nearest_upper: None,
                        required: None,
                        available: None,
                    }));
                }
                let tp_opt = match bracket.take_profit_limit {
                    Some(tp) => Some(
                        validate_tick_field(tp, spec.tick_size, "take_profit_limit")
                            .map_err(SimError::OrderRejected)?,
                    ),
                    None => None,
                };
                let sl_opt = match bracket.stop_loss_stop {
                    Some(sl) => Some(
                        validate_tick_field(sl, spec.tick_size, "stop_loss_stop")
                            .map_err(SimError::OrderRejected)?,
                    ),
                    None => None,
                };
                if let (Some(tp), Some(sl)) = (tp_opt, sl_opt) {
                    if tp <= sl {
                        return Err(SimError::rejected(OrderRejection {
                            code: SimRejectCode::InvalidOrderParams,
                            message: "attached take_profit_limit must be strictly greater than stop_loss_stop".to_string(),
                            field: Some("attached_bracket".to_string()),
                            nearest_lower: None,
                            nearest_upper: None,
                            required: None,
                            available: None,
                        }));
                    }
                }
                if let Some(entry_p) = entry_explicit_price {
                    if let Some(tp) = tp_opt {
                        if tp <= entry_p {
                            return Err(SimError::rejected(OrderRejection {
                                code: SimRejectCode::InvalidOrderParams,
                                message: "attached take_profit_limit must exceed entry order price"
                                    .to_string(),
                                field: Some("take_profit_limit".to_string()),
                                nearest_lower: None,
                                nearest_upper: None,
                                required: None,
                                available: None,
                            }));
                        }
                    }
                    if let Some(sl) = sl_opt {
                        if sl >= entry_p {
                            return Err(SimError::rejected(OrderRejection {
                                code: SimRejectCode::InvalidOrderParams,
                                message: "attached stop_loss_stop must be below entry order price"
                                    .to_string(),
                                field: Some("stop_loss_stop".to_string()),
                                nearest_lower: None,
                                nearest_upper: None,
                                required: None,
                                available: None,
                            }));
                        }
                    }
                }
            }

            match side {
                OrderSide::Buy => {
                    let reserve_unit_price = match order_type {
                        SimOrderType::Limit { .. } => entry_explicit_price.unwrap(),
                        SimOrderType::Market => {
                            let last_close = state
                                .last_close_by_instrument
                                .get(&req.instrument_id)
                                .copied()
                                .ok_or_else(|| {
                                    SimError::rejected(OrderRejection {
                                        code: SimRejectCode::MissingReferencePrice,
                                        message: format!(
                                            "missing visible reference close for market buy on {}",
                                            req.instrument_id
                                        ),
                                        field: None,
                                        nearest_lower: None,
                                        nearest_upper: None,
                                        required: None,
                                        available: None,
                                    })
                                })?;
                            canon_decimal(last_close * buffer_mult)
                        }
                        SimOrderType::StopMarket { .. } => {
                            let stop_p = entry_explicit_price.unwrap();
                            let buffered_close = state
                                .last_close_by_instrument
                                .get(&req.instrument_id)
                                .copied()
                                .map(|c| c * buffer_mult)
                                .unwrap_or(stop_p * buffer_mult);
                            canon_decimal(stop_p.max(buffered_close))
                        }
                        SimOrderType::StopLimit { .. } => unreachable!(),
                    };

                    let settlement = settle_order_fees(
                        &state.fee_profile,
                        OrderSide::Buy,
                        &spec.base_asset_id,
                        &spec.quote_currency,
                        &[SyntheticFillInput {
                            fill_id: "PREVIEW".to_string(),
                            quantity: qty,
                            fill_price: reserve_unit_price,
                        }],
                    )?;
                    let cash_fee = if state.fee_profile.fee_currency_mode == FeeCurrencyMode::Quote
                    {
                        settlement.total_fee_amount
                    } else {
                        Decimal::ZERO
                    };
                    let required_cash = canon_decimal(qty * reserve_unit_price + cash_fee);
                    if required_cash > avail_cash {
                        return Err(SimError::rejected(OrderRejection {
                            code: SimRejectCode::InsufficientFunds,
                            message: format!(
                                "insufficient available cash: required {}, available {}",
                                to_canonical_text(required_cash),
                                to_canonical_text(avail_cash),
                            ),
                            field: Some("quantity".to_string()),
                            nearest_lower: None,
                            nearest_upper: None,
                            required: Some(required_cash),
                            available: Some(avail_cash),
                        }));
                    }
                    Ok(OrderCostPreview {
                        instrument_id: req.instrument_id.clone(),
                        quantity: qty,
                        reference_price: Some(reserve_unit_price),
                        estimated_notional: Some(canon_decimal(qty * reserve_unit_price)),
                        estimated_fee: Some(settlement.total_fee_amount),
                        fee_currency: settlement.fee_currency,
                        cash_reservation_required: required_cash,
                        position_reservation_required: Decimal::ZERO,
                        available_cash_before: avail_cash,
                        available_position_before: avail_pos,
                    })
                }
                OrderSide::Sell => {
                    if qty > avail_pos {
                        return Err(SimError::rejected(OrderRejection {
                            code: SimRejectCode::InsufficientPosition,
                            message: format!(
                                "insufficient available position for {}: required {}, available {}",
                                req.instrument_id,
                                to_canonical_text(qty),
                                to_canonical_text(avail_pos),
                            ),
                            field: Some("quantity".to_string()),
                            nearest_lower: None,
                            nearest_upper: None,
                            required: Some(qty),
                            available: Some(avail_pos),
                        }));
                    }
                    let ref_p = entry_explicit_price.or_else(|| {
                        state
                            .last_close_by_instrument
                            .get(&req.instrument_id)
                            .copied()
                    });
                    let est_fee = match ref_p {
                        Some(p) if p > Decimal::ZERO => Some(
                            settle_order_fees(
                                &state.fee_profile,
                                OrderSide::Sell,
                                &spec.base_asset_id,
                                &spec.quote_currency,
                                &[SyntheticFillInput {
                                    fill_id: "PREVIEW".to_string(),
                                    quantity: qty,
                                    fill_price: p,
                                }],
                            )?
                            .total_fee_amount,
                        ),
                        _ => None,
                    };
                    Ok(OrderCostPreview {
                        instrument_id: req.instrument_id.clone(),
                        quantity: qty,
                        reference_price: ref_p.map(canon_decimal),
                        estimated_notional: ref_p.map(|p| canon_decimal(qty * p)),
                        estimated_fee: est_fee,
                        fee_currency: spec.quote_currency.clone(),
                        cash_reservation_required: Decimal::ZERO,
                        position_reservation_required: qty,
                        available_cash_before: avail_cash,
                        available_position_before: avail_pos,
                    })
                }
            }
        }
        OrderSubmitKind::Oco {
            take_profit_limit,
            stop_loss_stop,
            ..
        } => {
            let tp = validate_tick_field(*take_profit_limit, spec.tick_size, "take_profit_limit")
                .map_err(SimError::OrderRejected)?;
            let sl = validate_tick_field(*stop_loss_stop, spec.tick_size, "stop_loss_stop")
                .map_err(SimError::OrderRejected)?;
            if tp <= sl {
                return Err(SimError::rejected(OrderRejection {
                    code: SimRejectCode::InvalidOrderParams,
                    message: "OCO take_profit_limit must be strictly greater than stop_loss_stop"
                        .to_string(),
                    field: Some("take_profit_limit".to_string()),
                    nearest_lower: None,
                    nearest_upper: None,
                    required: None,
                    available: None,
                }));
            }
            if qty > avail_pos {
                return Err(SimError::rejected(OrderRejection {
                    code: SimRejectCode::InsufficientPosition,
                    message: format!(
                        "insufficient available position for OCO on {}: required {}, available {}",
                        req.instrument_id,
                        to_canonical_text(qty),
                        to_canonical_text(avail_pos),
                    ),
                    field: Some("quantity".to_string()),
                    nearest_lower: None,
                    nearest_upper: None,
                    required: Some(qty),
                    available: Some(avail_pos),
                }));
            }
            let settlement = settle_order_fees(
                &state.fee_profile,
                OrderSide::Sell,
                &spec.base_asset_id,
                &spec.quote_currency,
                &[SyntheticFillInput {
                    fill_id: "PREVIEW".to_string(),
                    quantity: qty,
                    fill_price: tp,
                }],
            )?;
            Ok(OrderCostPreview {
                instrument_id: req.instrument_id.clone(),
                quantity: qty,
                reference_price: Some(tp),
                estimated_notional: Some(canon_decimal(qty * tp)),
                estimated_fee: Some(settlement.total_fee_amount),
                fee_currency: spec.quote_currency.clone(),
                cash_reservation_required: Decimal::ZERO,
                position_reservation_required: qty,
                available_cash_before: avail_cash,
                available_position_before: avail_pos,
            })
        }
    }
}

fn release_order_reservation(state: &mut SimState, order_id: &str) {
    let Some(res_id) = state
        .orders
        .get(order_id)
        .and_then(|o| o.reservation_id.clone())
    else {
        return;
    };
    state.cash_reservations.remove(&res_id);
    let remove_pos_res = if let Some(pos_res) = state.position_reservations.get_mut(&res_id) {
        pos_res.order_ids.retain(|id| id != order_id);
        pos_res.order_ids.is_empty()
    } else {
        false
    };
    if remove_pos_res {
        state.position_reservations.remove(&res_id);
    }
}

fn consume_position_reservation_fully(state: &mut SimState, res_id: &str) {
    state.position_reservations.remove(res_id);
}

fn cancel_order_internal(
    state: &mut SimState,
    order_id: &str,
    reason: &str,
    updated_at: DateTime<Utc>,
    events: &mut Vec<SimEvent>,
) {
    let Some(order) = state.orders.get_mut(order_id) else {
        return;
    };
    if !order.status.is_open_or_pending() {
        return;
    }
    let inst_id = order.instrument_id.clone();
    order.status = OrderStatus::Cancelled;
    order.cancel_reason = Some(reason.to_string());
    order.updated_at = updated_at;
    release_order_reservation(state, order_id);
    events.push(SimEvent::OrderCancelled {
        order_id: order_id.to_string(),
        instrument_id: inst_id,
        reason: reason.to_string(),
    });
}

fn next_order_id(order_seq: u64) -> String {
    format!("SIM-ORD-{order_seq:06}")
}

fn next_fill_id(fill_seq: u64) -> String {
    format!("SIM-FILL-{fill_seq:06}")
}

fn next_group_id(group_seq: u64) -> String {
    format!("SIM-GRP-{group_seq:06}")
}

fn next_res_id(res_seq: u64) -> String {
    format!("SIM-RES-{res_seq:06}")
}

fn submit_order_into_state(
    state: &mut SimState,
    req: OrderSubmitRequest,
) -> Result<Vec<SimEvent>, SimError> {
    let preview = preview_order_submission(state, &req)?;
    let submitted_at = state
        .visible_until
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    let eligible_at = req
        .eligible_at_hint
        .filter(|&t| t >= submitted_at)
        .unwrap_or(submitted_at);

    let mut created_ids = Vec::new();
    let mut parent_group_id_out = None;

    match req.kind {
        OrderSubmitKind::Single {
            side,
            order_type,
            attached_bracket,
        } => {
            let order_type = order_type.canonical();
            let entry_seq = state.next_order_seq;
            state.next_order_seq += 1;
            let entry_id = next_order_id(entry_seq);

            let has_plan = attached_bracket.is_some();
            let group_id = if has_plan {
                let g = next_group_id(state.next_group_seq);
                state.next_group_seq += 1;
                parent_group_id_out = Some(g.clone());
                Some(g)
            } else {
                None
            };

            let res_id = if preview.cash_reservation_required > Decimal::ZERO {
                let r = next_res_id(state.next_reservation_seq);
                state.next_reservation_seq += 1;
                state.cash_reservations.insert(
                    r.clone(),
                    CashReservation {
                        reservation_id: r.clone(),
                        order_id: entry_id.clone(),
                        instrument_id: req.instrument_id.clone(),
                        amount: preview.cash_reservation_required,
                    },
                );
                Some(r)
            } else if preview.position_reservation_required > Decimal::ZERO {
                let r = next_res_id(state.next_reservation_seq);
                state.next_reservation_seq += 1;
                state.position_reservations.insert(
                    r.clone(),
                    PositionReservation {
                        reservation_id: r.clone(),
                        instrument_id: req.instrument_id.clone(),
                        quantity: preview.position_reservation_required,
                        order_ids: vec![entry_id.clone()],
                    },
                );
                Some(r)
            } else {
                None
            };

            state.orders.insert(
                entry_id.clone(),
                SimOrder {
                    order_id: entry_id.clone(),
                    order_seq: entry_seq,
                    instrument_id: req.instrument_id.clone(),
                    side,
                    order_type,
                    tif: req.tif,
                    priority: req.priority,
                    quantity: preview.quantity,
                    parent_group_id: group_id.clone(),
                    parent_order_id: None,
                    oco_peer_order_id: None,
                    reservation_id: res_id,
                    group_role: if has_plan {
                        OrderGroupRole::BracketEntry
                    } else {
                        OrderGroupRole::Standalone
                    },
                    has_trade_plan: has_plan,
                    status: OrderStatus::Accepted,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at,
                    eligible_at,
                    updated_at: submitted_at,
                    activated_bar_close_at: None,
                    activated_stage: None,
                    activated_path_price: None,
                },
            );
            created_ids.push(entry_id.clone());

            if let Some(bracket) = attached_bracket {
                let bracket = bracket.canonical();
                let tp_id = bracket.take_profit_limit.map(|_| {
                    let s = state.next_order_seq;
                    state.next_order_seq += 1;
                    (s, next_order_id(s))
                });
                let sl_id = bracket.stop_loss_stop.map(|_| {
                    let s = state.next_order_seq;
                    state.next_order_seq += 1;
                    (s, next_order_id(s))
                });

                if let (Some(tp_price), Some((tp_seq, tp_ord_id))) =
                    (bracket.take_profit_limit, tp_id.clone())
                {
                    state.orders.insert(
                        tp_ord_id.clone(),
                        SimOrder {
                            order_id: tp_ord_id.clone(),
                            order_seq: tp_seq,
                            instrument_id: req.instrument_id.clone(),
                            side: OrderSide::Sell,
                            order_type: SimOrderType::Limit {
                                limit_price: tp_price,
                            },
                            tif: bracket.tif,
                            priority: req.priority,
                            quantity: preview.quantity,
                            parent_group_id: group_id.clone(),
                            parent_order_id: Some(entry_id.clone()),
                            oco_peer_order_id: sl_id.as_ref().map(|(_, id)| id.clone()),
                            reservation_id: None,
                            group_role: OrderGroupRole::BracketTakeProfit,
                            has_trade_plan: true,
                            status: OrderStatus::PendingParent,
                            reject_code: None,
                            cancel_reason: None,
                            submitted_at,
                            eligible_at,
                            updated_at: submitted_at,
                            activated_bar_close_at: None,
                            activated_stage: None,
                            activated_path_price: None,
                        },
                    );
                    created_ids.push(tp_ord_id);
                }
                if let (Some(sl_price), Some((sl_seq, sl_ord_id))) = (bracket.stop_loss_stop, sl_id)
                {
                    state.orders.insert(
                        sl_ord_id.clone(),
                        SimOrder {
                            order_id: sl_ord_id.clone(),
                            order_seq: sl_seq,
                            instrument_id: req.instrument_id.clone(),
                            side: OrderSide::Sell,
                            order_type: SimOrderType::StopMarket {
                                stop_price: sl_price,
                            },
                            tif: bracket.tif,
                            priority: req.priority,
                            quantity: preview.quantity,
                            parent_group_id: group_id,
                            parent_order_id: Some(entry_id),
                            oco_peer_order_id: tp_id.map(|(_, id)| id),
                            reservation_id: None,
                            group_role: OrderGroupRole::BracketStopLoss,
                            has_trade_plan: true,
                            status: OrderStatus::PendingParent,
                            reject_code: None,
                            cancel_reason: None,
                            submitted_at,
                            eligible_at,
                            updated_at: submitted_at,
                            activated_bar_close_at: None,
                            activated_stage: None,
                            activated_path_price: None,
                        },
                    );
                    created_ids.push(sl_ord_id);
                }
            }
        }
        OrderSubmitKind::Oco {
            take_profit_limit,
            stop_loss_stop,
            is_trade_plan,
        } => {
            let group_id = next_group_id(state.next_group_seq);
            state.next_group_seq += 1;
            parent_group_id_out = Some(group_id.clone());

            let tp_seq = state.next_order_seq;
            state.next_order_seq += 1;
            let tp_id = next_order_id(tp_seq);

            let sl_seq = state.next_order_seq;
            state.next_order_seq += 1;
            let sl_id = next_order_id(sl_seq);

            let res_id = next_res_id(state.next_reservation_seq);
            state.next_reservation_seq += 1;
            state.position_reservations.insert(
                res_id.clone(),
                PositionReservation {
                    reservation_id: res_id.clone(),
                    instrument_id: req.instrument_id.clone(),
                    quantity: preview.quantity,
                    order_ids: vec![tp_id.clone(), sl_id.clone()],
                },
            );

            state.orders.insert(
                tp_id.clone(),
                SimOrder {
                    order_id: tp_id.clone(),
                    order_seq: tp_seq,
                    instrument_id: req.instrument_id.clone(),
                    side: OrderSide::Sell,
                    order_type: SimOrderType::Limit {
                        limit_price: canon_decimal(take_profit_limit),
                    },
                    tif: req.tif,
                    priority: req.priority,
                    quantity: preview.quantity,
                    parent_group_id: Some(group_id.clone()),
                    parent_order_id: None,
                    oco_peer_order_id: Some(sl_id.clone()),
                    reservation_id: Some(res_id.clone()),
                    group_role: OrderGroupRole::OcoTakeProfit,
                    has_trade_plan: is_trade_plan,
                    status: OrderStatus::Accepted,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at,
                    eligible_at,
                    updated_at: submitted_at,
                    activated_bar_close_at: None,
                    activated_stage: None,
                    activated_path_price: None,
                },
            );
            state.orders.insert(
                sl_id.clone(),
                SimOrder {
                    order_id: sl_id.clone(),
                    order_seq: sl_seq,
                    instrument_id: req.instrument_id,
                    side: OrderSide::Sell,
                    order_type: SimOrderType::StopMarket {
                        stop_price: canon_decimal(stop_loss_stop),
                    },
                    tif: req.tif,
                    priority: req.priority,
                    quantity: preview.quantity,
                    parent_group_id: Some(group_id),
                    parent_order_id: None,
                    oco_peer_order_id: Some(tp_id.clone()),
                    reservation_id: Some(res_id),
                    group_role: OrderGroupRole::OcoStopLoss,
                    has_trade_plan: is_trade_plan,
                    status: OrderStatus::Accepted,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at,
                    eligible_at,
                    updated_at: submitted_at,
                    activated_bar_close_at: None,
                    activated_stage: None,
                    activated_path_price: None,
                },
            );
            created_ids.push(tp_id);
            created_ids.push(sl_id);
        }
    }

    Ok(vec![SimEvent::OrderSubmitted {
        order_ids: created_ids,
        parent_group_id: parent_group_id_out,
        cash_reserved: preview.cash_reservation_required,
        position_reserved: preview.position_reservation_required,
    }])
}

fn cancel_order_command(
    state: &mut SimState,
    order_id: &str,
    reason: Option<String>,
) -> Result<Vec<SimEvent>, SimError> {
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    let Some(order) = state.orders.get(order_id).cloned() else {
        return Err(SimError::UnknownOrder(order_id.to_string()));
    };
    if !order.status.is_open_or_pending() {
        return Err(SimError::OrderNotCancellable {
            order_id: order_id.to_string(),
            status: order.status,
        });
    }

    let updated_at = state.visible_until.unwrap_or(order.submitted_at);
    let reason_str = reason.unwrap_or_else(|| "user_cancelled".to_string());
    let mut events = Vec::new();

    cancel_order_internal(state, order_id, &reason_str, updated_at, &mut events);

    // Cancel OCO peer if present
    if let Some(peer_id) = order.oco_peer_order_id {
        cancel_order_internal(
            state,
            &peer_id,
            "oco_peer_cancelled",
            updated_at,
            &mut events,
        );
    }

    // Cancel any PendingParent attached bracket legs owned by this parent order
    let child_ids: Vec<String> = state
        .orders
        .values()
        .filter(|o| {
            o.parent_order_id.as_deref() == Some(order_id) && o.status == OrderStatus::PendingParent
        })
        .map(|o| o.order_id.clone())
        .collect();
    for child_id in child_ids {
        cancel_order_internal(
            state,
            &child_id,
            "parent_order_cancelled",
            updated_at,
            &mut events,
        );
    }

    Ok(events)
}

#[derive(Debug, Clone)]
struct TriggerMatch {
    order_id: String,
    priority: i32,
    instrument_id: String,
    order_seq: u64,
    trigger_path_price: Decimal,
    distance_from_cur: Decimal,
    fill_price: Decimal,
}

fn compute_execution_price_for_order(
    order: &SimOrder,
    stage: BarPathStage,
    trigger_path_price: Decimal,
    tick_size: Decimal,
    fee_profile: &FeeProfileVersion,
) -> Decimal {
    let spread_delta = (trigger_path_price * fee_profile.half_spread_bps) / Decimal::from(10_000);
    let slip_delta = fee_profile
        .slippage
        .delta_for_price(trigger_path_price, tick_size);

    match order.order_type {
        SimOrderType::Market => {
            let raw = match order.side {
                OrderSide::Buy => trigger_path_price + spread_delta + slip_delta,
                OrderSide::Sell => trigger_path_price - spread_delta - slip_delta,
            };
            round_price_adverse_tick(raw, order.side, tick_size)
        }
        SimOrderType::Limit { limit_price } => match order.side {
            OrderSide::Buy => {
                if trigger_path_price < limit_price {
                    let raw = trigger_path_price + slip_delta;
                    round_price_adverse_tick(raw, OrderSide::Buy, tick_size).min(limit_price)
                } else {
                    // Exact resting limit touch (already validated on tick grid)
                    let _ = stage;
                    canon_decimal(limit_price)
                }
            }
            OrderSide::Sell => {
                if trigger_path_price > limit_price {
                    let raw = trigger_path_price - slip_delta;
                    round_price_adverse_tick(raw, OrderSide::Sell, tick_size).max(limit_price)
                } else {
                    canon_decimal(limit_price)
                }
            }
        },
        SimOrderType::StopMarket { .. } => {
            let raw = match order.side {
                OrderSide::Buy => trigger_path_price + slip_delta,
                OrderSide::Sell => trigger_path_price - slip_delta,
            };
            round_price_adverse_tick(raw, order.side, tick_size)
        }
        SimOrderType::StopLimit { .. } => unreachable!("StopLimit rejected at submit"),
    }
}

fn check_order_trigger_on_subsegment(
    order: &SimOrder,
    stage: BarPathStage,
    cur: Decimal,
    seg_end: Decimal,
    tick_size: Decimal,
    fee_profile: &FeeProfileVersion,
) -> Option<TriggerMatch> {
    let trigger_opt = match order.order_type {
        SimOrderType::Market => (stage == BarPathStage::AtOpen).then_some(cur),
        SimOrderType::Limit { limit_price } => match order.side {
            OrderSide::Buy => {
                if cur <= limit_price {
                    Some(cur)
                } else if seg_end < cur && seg_end <= limit_price {
                    Some(limit_price)
                } else {
                    None
                }
            }
            OrderSide::Sell => {
                if cur >= limit_price {
                    Some(cur)
                } else if seg_end > cur && seg_end >= limit_price {
                    Some(limit_price)
                } else {
                    None
                }
            }
        },
        SimOrderType::StopMarket { stop_price } => match order.side {
            OrderSide::Buy => {
                if cur >= stop_price {
                    Some(cur)
                } else if seg_end > cur && seg_end >= stop_price {
                    Some(stop_price)
                } else {
                    None
                }
            }
            OrderSide::Sell => {
                if cur <= stop_price {
                    Some(cur)
                } else if seg_end < cur && seg_end <= stop_price {
                    Some(stop_price)
                } else {
                    None
                }
            }
        },
        SimOrderType::StopLimit { .. } => None,
    }?;

    let distance = (trigger_opt - cur).abs();
    let fill_price =
        compute_execution_price_for_order(order, stage, trigger_opt, tick_size, fee_profile);
    Some(TriggerMatch {
        order_id: order.order_id.clone(),
        priority: order.priority,
        instrument_id: order.instrument_id.clone(),
        order_seq: order.order_seq,
        trigger_path_price: canon_decimal(trigger_opt),
        distance_from_cur: canon_decimal(distance),
        fill_price,
    })
}

/// Check whether an order's OCO peer would ALSO have been reachable on the remaining bar path
/// starting from the pair's activation point on this bar (`S-39`, `M4-A-05`).
fn is_oco_dual_touch_ambiguous(
    state: &SimState,
    order: &SimOrder,
    bar: &BarRecord,
    current_stage: BarPathStage,
) -> bool {
    let Some(peer_id) = &order.oco_peer_order_id else {
        return false;
    };
    let Some(peer) = state.orders.get(peer_id) else {
        return false;
    };
    if !peer.status.is_open_or_pending() {
        return false;
    }

    // Determine where this OCO pair was active on the current bar:
    // - If activated intra-bar on this exact bar, start from (activated_stage, activated_path_price).
    // - Otherwise, it was active from AtOpen at bar.open.
    let (act_stage, act_price) = if order.activated_bar_close_at == Some(bar.close_at) {
        (
            order.activated_stage.unwrap_or(BarPathStage::AtOpen),
            order.activated_path_price.unwrap_or(bar.open),
        )
    } else {
        (BarPathStage::AtOpen, bar.open)
    };

    // If an order already gapped/triggered immediately at bar.open (AtOpen), bar.open is a known
    // print before any intrabar oscillation, so AtOpen gap triggers are not path-order ambiguous.
    if current_stage == BarPathStage::AtOpen && act_stage == BarPathStage::AtOpen {
        return false;
    }

    // Remaining price range on the bar from (act_stage, act_price) through close:
    let (min_rem, max_rem) = remaining_bar_extrema(bar, act_stage, act_price);
    let self_reachable = order_would_reach_extrema(order, act_price, min_rem, max_rem);
    let peer_reachable = order_would_reach_extrema(peer, act_price, min_rem, max_rem);
    self_reachable && peer_reachable
}

fn remaining_bar_extrema(
    bar: &BarRecord,
    from_stage: BarPathStage,
    from_price: Decimal,
) -> (Decimal, Decimal) {
    match from_stage {
        BarPathStage::AtOpen => (bar.low, bar.high),
        BarPathStage::OpenToLow => (from_price.min(bar.low), from_price.max(bar.high)),
        BarPathStage::LowToHigh => (from_price.min(bar.close), from_price.max(bar.high)),
        BarPathStage::HighToClose => (from_price.min(bar.close), from_price.max(bar.close)),
    }
}

fn order_would_reach_extrema(
    order: &SimOrder,
    act_price: Decimal,
    min_rem: Decimal,
    max_rem: Decimal,
) -> bool {
    match (order.side, order.order_type) {
        (OrderSide::Sell, SimOrderType::Limit { limit_price }) => {
            act_price >= limit_price || max_rem >= limit_price
        }
        (OrderSide::Sell, SimOrderType::StopMarket { stop_price }) => {
            act_price <= stop_price || min_rem <= stop_price
        }
        (OrderSide::Buy, SimOrderType::Limit { limit_price }) => {
            act_price <= limit_price || min_rem <= limit_price
        }
        (OrderSide::Buy, SimOrderType::StopMarket { stop_price }) => {
            act_price >= stop_price || max_rem >= stop_price
        }
        (_, SimOrderType::Market) => true,
        _ => false,
    }
}

fn allocate_fifo_lots_for_sell(
    lots: &mut Vec<SimLot>,
    sell_qty: Decimal,
) -> Result<Decimal, SimError> {
    let avail: Decimal = lots.iter().map(|l| l.remaining_quantity).sum();
    if avail < sell_qty {
        return Err(SimError::rejected(OrderRejection {
            code: SimRejectCode::InsufficientPosition,
            message: format!(
                "FIFO allocation oversell: needed {}, available {}",
                to_canonical_text(sell_qty),
                to_canonical_text(avail),
            ),
            field: Some("quantity".to_string()),
            nearest_lower: None,
            nearest_upper: None,
            required: Some(sell_qty),
            available: Some(avail),
        }));
    }
    let mut rem = sell_qty;
    let mut total_cost = Decimal::ZERO;
    for lot in lots.iter_mut() {
        if rem.is_zero() {
            break;
        }
        if lot.remaining_quantity.is_zero() {
            continue;
        }
        let take = rem.min(lot.remaining_quantity);
        let cost = if take == lot.remaining_quantity {
            lot.remaining_cost_quote
        } else {
            canon_decimal(
                ((lot.remaining_cost_quote * take) / lot.remaining_quantity)
                    .round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero),
            )
        };
        lot.remaining_quantity = canon_decimal(lot.remaining_quantity - take);
        lot.remaining_cost_quote = canon_decimal(lot.remaining_cost_quote - cost);
        rem = canon_decimal(rem - take);
        total_cost = canon_decimal(total_cost + cost);
    }
    lots.retain(|l| l.remaining_quantity > Decimal::ZERO);
    Ok(total_cost)
}

fn apply_pre_open_corporate_actions_for_bar(
    state: &mut SimState,
    bar: &BarRecord,
    corporate_actions: &[CorporateActionRecord],
    events: &mut Vec<SimEvent>,
) -> Result<(), SimError> {
    let relevant: Vec<CorporateActionRecord> = corporate_actions
        .iter()
        .filter(|ca| ca.instrument_id == bar.instrument_id && ca.ex_date == bar.session_date)
        .cloned()
        .collect();
    if relevant.is_empty() {
        return Ok(());
    }
    let dedup = deduplicate_corporate_actions(&relevant);

    for action in dedup {
        let action_key = (
            action.instrument_id.clone(),
            action.ex_date,
            action.action_type.as_str().to_string(),
        );
        if state.applied_corporate_actions.contains(&action_key) {
            continue;
        }

        match action.action_type {
            CorporateActionType::Split => {
                let ratio = canon_decimal(action.ratio_or_amount);
                if ratio <= Decimal::ZERO {
                    return Err(SimError::InvalidCorporateAction {
                        action_id: action.id.clone(),
                        reason: format!("split ratio must be positive, got {ratio}"),
                    });
                }
                let qty_before = state.position_quantity(&bar.instrument_id);
                if let Some(lots) = state.lots_by_instrument.get_mut(&bar.instrument_id) {
                    for lot in lots.iter_mut() {
                        lot.original_quantity =
                            canon_decimal(lot.original_quantity.checked_mul(ratio).ok_or_else(
                                || SimError::ArithmeticOverflow("split_orig_qty".into()),
                            )?);
                        lot.remaining_quantity =
                            canon_decimal(lot.remaining_quantity.checked_mul(ratio).ok_or_else(
                                || SimError::ArithmeticOverflow("split_rem_qty".into()),
                            )?);
                        // Total cost basis (`remaining_cost_quote`) is strictly unchanged (`S-42`)!
                    }
                }
                let qty_after = state.position_quantity(&bar.instrument_id);

                // Cancel all open/pending orders for this instrument with reason `corporate_action_split` (`S-42`, `M4-A-03`)
                let open_ids: Vec<String> = state
                    .orders
                    .values()
                    .filter(|o| {
                        o.instrument_id == bar.instrument_id && o.status.is_open_or_pending()
                    })
                    .map(|o| o.order_id.clone())
                    .collect();
                let mut cancel_events = Vec::new();
                for ord_id in &open_ids {
                    cancel_order_internal(
                        state,
                        ord_id,
                        "corporate_action_split",
                        bar.open_at,
                        &mut cancel_events,
                    );
                }

                state.applied_corporate_actions.insert(action_key);
                events.push(SimEvent::CorporateActionApplied {
                    action_id: action.id.clone(),
                    instrument_id: bar.instrument_id.clone(),
                    action_type: CorporateActionType::Split,
                    ex_date: action.ex_date,
                    ratio_or_amount: ratio,
                    position_qty_before: qty_before,
                    position_qty_after: qty_after,
                    cash_credited: Decimal::ZERO,
                    cancelled_order_ids: open_ids,
                    assumption_marker: None,
                });
                events.extend(cancel_events);
            }
            CorporateActionType::CashDividend => {
                let div_per_share = canon_decimal(action.ratio_or_amount);
                if div_per_share < Decimal::ZERO {
                    return Err(SimError::InvalidCorporateAction {
                        action_id: action.id.clone(),
                        reason: format!("cash dividend cannot be negative, got {div_per_share}"),
                    });
                }
                let qty = state.position_quantity(&bar.instrument_id);
                let cash_credited = if qty > Decimal::ZERO && div_per_share > Decimal::ZERO {
                    let credit =
                        canon_decimal(qty.checked_mul(div_per_share).ok_or_else(|| {
                            SimError::ArithmeticOverflow("dividend_credit".into())
                        })?);
                    state.cash_balance = canon_decimal(state.cash_balance + credit);
                    state.dividend_income = canon_decimal(state.dividend_income + credit);
                    state.ex_date_dividend_assumed = true;
                    credit
                } else {
                    Decimal::ZERO
                };

                let marker = if cash_credited > Decimal::ZERO {
                    Some("ex_date_pre_open_cash_credit".to_string())
                } else {
                    None
                };
                state.applied_corporate_actions.insert(action_key);
                events.push(SimEvent::CorporateActionApplied {
                    action_id: action.id.clone(),
                    instrument_id: bar.instrument_id.clone(),
                    action_type: CorporateActionType::CashDividend,
                    ex_date: action.ex_date,
                    ratio_or_amount: div_per_share,
                    position_qty_before: qty,
                    position_qty_after: qty,
                    cash_credited,
                    cancelled_order_ids: Vec::new(),
                    assumption_marker: marker,
                });
            }
        }
    }
    Ok(())
}

fn validate_bar_ohlc(bar: &BarRecord) -> Result<(), SimError> {
    if !bar.is_final {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "simulation only accepts closed final bars (is_final = true)".into(),
        });
    }
    if bar.open <= Decimal::ZERO
        || bar.high <= Decimal::ZERO
        || bar.low <= Decimal::ZERO
        || bar.close <= Decimal::ZERO
    {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "bar OHLC prices must all be strictly positive".into(),
        });
    }
    if bar.low > bar.open
        || bar.low > bar.close
        || bar.high < bar.open
        || bar.high < bar.close
        || bar.low > bar.high
    {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: format!(
                "invalid OHLC envelope: O={}, H={}, L={}, C={}",
                bar.open, bar.high, bar.low, bar.close
            ),
        });
    }
    if bar.volume < Decimal::ZERO {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "bar volume cannot be negative".into(),
        });
    }
    if bar.close_at < bar.open_at {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "bar close_at precedes open_at".into(),
        });
    }
    Ok(())
}

/// Process a batch of simultaneous bars (single bar in M4 training, or multi-instrument bars
/// sharing `event_time` in M5 backtest) with deterministic ordering `(event_time, priority, instrument_id, order_seq)`.
pub fn step_multi_bars(
    state: &SimState,
    step_bars: &[SimStepBar],
    corporate_actions: &[CorporateActionRecord],
) -> Result<(SimState, Vec<SimEvent>), SimError> {
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    if step_bars.is_empty() {
        return Err(SimError::MissingBarInput);
    }

    let mut next = state.clone();
    next.command_seq += 1;

    // Sort incoming bars deterministically by `(close_at, open_at, instrument_id)`
    let mut sorted_bars: Vec<&SimStepBar> = step_bars.iter().collect();
    sorted_bars.sort_by(|a, b| {
        a.bar
            .close_at
            .cmp(&b.bar.close_at)
            .then_with(|| a.bar.open_at.cmp(&b.bar.open_at))
            .then_with(|| a.bar.instrument_id.cmp(&b.bar.instrument_id))
    });

    // Validate instruments, OHLC invariants, and strict per-instrument + global replay clock monotonicity
    let mut seen_instruments = BTreeSet::new();
    for sb in &sorted_bars {
        if !next.instruments.contains_key(&sb.bar.instrument_id) {
            return Err(SimError::UnknownInstrument(sb.bar.instrument_id.clone()));
        }
        if !seen_instruments.insert(sb.bar.instrument_id.clone()) {
            return Err(SimError::InvalidBar {
                bar_id: sb.bar.id.clone(),
                reason: format!("duplicate bar in same step for {}", sb.bar.instrument_id),
            });
        }
        validate_bar_ohlc(&sb.bar)?;
        if let Some(prev_inst_close) = next.last_close_at_by_instrument.get(&sb.bar.instrument_id) {
            if sb.bar.close_at <= *prev_inst_close {
                return Err(SimError::ClockNotMonotonic {
                    current: Some(*prev_inst_close),
                    incoming: sb.bar.close_at,
                });
            }
        }
        if let Some(prev_global) = next.visible_until {
            if sb.bar.close_at <= prev_global {
                return Err(SimError::ClockNotMonotonic {
                    current: Some(prev_global),
                    incoming: sb.bar.close_at,
                });
            }
        }
    }

    let mut events = Vec::new();
    let mut pos_before_by_inst: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut had_buy_by_inst: BTreeMap<String, bool> = BTreeMap::new();
    let mut had_sell_by_inst: BTreeMap<String, bool> = BTreeMap::new();

    for sb in &sorted_bars {
        pos_before_by_inst.insert(
            sb.bar.instrument_id.clone(),
            next.position_quantity(&sb.bar.instrument_id),
        );
        had_buy_by_inst.insert(sb.bar.instrument_id.clone(), false);
        had_sell_by_inst.insert(sb.bar.instrument_id.clone(), false);
        apply_pre_open_corporate_actions_for_bar(
            &mut next,
            &sb.bar,
            corporate_actions,
            &mut events,
        )?;
    }

    // Activate eligible Accepted orders across tradable/untradable bars
    let bar_map: BTreeMap<String, &SimStepBar> = sorted_bars
        .iter()
        .map(|sb| (sb.bar.instrument_id.clone(), *sb))
        .collect();

    for order in next.orders.values_mut() {
        if order.status == OrderStatus::Accepted {
            if let Some(sb) = bar_map.get(&order.instrument_id) {
                if sb.bar.close_at > order.submitted_at && sb.bar.open_at >= order.eligible_at {
                    order.status = OrderStatus::Active;
                    order.updated_at = sb.bar.open_at;
                }
            }
        }
    }

    // Traverse stages 0..=3 synchronously across all tradable bars in deterministic order
    let stages = [
        BarPathStage::AtOpen,
        BarPathStage::OpenToLow,
        BarPathStage::LowToHigh,
        BarPathStage::HighToClose,
    ];

    for stage in stages {
        let mut cur_price_by_inst: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
        for sb in &sorted_bars {
            if !sb.is_effectively_tradable() {
                continue;
            }
            let (seg_start, seg_end) = match stage {
                BarPathStage::AtOpen => (sb.bar.open, sb.bar.open),
                BarPathStage::OpenToLow => (sb.bar.open, sb.bar.low),
                BarPathStage::LowToHigh => (sb.bar.low, sb.bar.high),
                BarPathStage::HighToClose => (sb.bar.high, sb.bar.close),
            };
            cur_price_by_inst.insert(sb.bar.instrument_id.clone(), (seg_start, seg_end));
        }

        let max_iters = next.orders.len().saturating_mul(4).saturating_add(16);
        for _ in 0..max_iters {
            let mut candidates: Vec<TriggerMatch> = Vec::new();

            for order in next.orders.values() {
                if order.status != OrderStatus::Active {
                    continue;
                }
                let Some(&(cur, seg_end)) = cur_price_by_inst.get(&order.instrument_id) else {
                    continue;
                };
                let Some(sb) = bar_map.get(&order.instrument_id) else {
                    continue;
                };
                if !(sb.bar.close_at > order.submitted_at && sb.bar.open_at >= order.eligible_at) {
                    continue;
                }
                // Enforce attached bracket post-entry activation rule (`S-39`, `M4-A-05`):
                // If activated on this bar at `act_stage`, never trigger on an earlier stage,
                // and at `AtOpen` an order activated BY an `AtOpen` fill only runs on `OpenToLow`+.
                if order.activated_bar_close_at == Some(sb.bar.close_at) {
                    if let Some(act_stage) = order.activated_stage {
                        if stage < act_stage
                            || (stage == BarPathStage::AtOpen && act_stage == BarPathStage::AtOpen)
                        {
                            continue;
                        }
                    }
                }

                let spec = &next.instruments[&order.instrument_id];
                if let Some(m) = check_order_trigger_on_subsegment(
                    order,
                    stage,
                    cur,
                    seg_end,
                    spec.tick_size,
                    &next.fee_profile,
                ) {
                    candidates.push(m);
                }
            }

            if candidates.is_empty() {
                break;
            }

            // Order candidates deterministically:
            // - For single instrument on a segment, earlier path touch (`distance_from_cur`) comes first!
            // - Across instruments / ties at exact same distance: `(event_time, priority, instrument_id, order_seq)` (`S-36`).
            candidates.sort_by(|a, b| {
                let ta = bar_map[&a.instrument_id].bar.open_at;
                let tb = bar_map[&b.instrument_id].bar.open_at;
                ta.cmp(&tb)
                    .then_with(|| {
                        if a.instrument_id == b.instrument_id {
                            a.distance_from_cur.cmp(&b.distance_from_cur)
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    })
                    .then_with(|| a.priority.cmp(&b.priority))
                    .then_with(|| a.instrument_id.cmp(&b.instrument_id))
                    .then_with(|| a.order_seq.cmp(&b.order_seq))
            });

            let chosen = candidates.remove(0);
            if let Some(entry) = cur_price_by_inst.get_mut(&chosen.instrument_id) {
                entry.0 = chosen.trigger_path_price;
            }

            execute_triggered_order(
                &mut next,
                &chosen,
                stage,
                &bar_map[&chosen.instrument_id].bar,
                &mut had_buy_by_inst,
                &mut had_sell_by_inst,
                &mut events,
            )?;
        }
    }

    // End of bar: expire remaining active/accepted DAY orders on each bar (including untradable/zero-volume bars, `M4-A-06`)
    for sb in &sorted_bars {
        let day_expire_ids: Vec<String> = next
            .orders
            .values()
            .filter(|o| {
                o.instrument_id == sb.bar.instrument_id
                    && matches!(o.status, OrderStatus::Active | OrderStatus::Accepted)
                    && o.tif == TimeInForce::Day
                    && sb.bar.close_at > o.submitted_at
                    && sb.bar.open_at >= o.eligible_at
            })
            .map(|o| o.order_id.clone())
            .collect();

        for ord_id in day_expire_ids {
            let Some(ord_snapshot) = next.orders.get(&ord_id).cloned() else {
                continue;
            };
            if !matches!(
                ord_snapshot.status,
                OrderStatus::Active | OrderStatus::Accepted
            ) {
                continue;
            }
            if let Some(o) = next.orders.get_mut(&ord_id) {
                o.status = OrderStatus::Expired;
                o.updated_at = sb.bar.close_at;
            }
            release_order_reservation(&mut next, &ord_id);
            events.push(SimEvent::OrderExpired {
                order_id: ord_id.clone(),
                instrument_id: sb.bar.instrument_id.clone(),
                bar_close_at: sb.bar.close_at,
            });

            // Cancel any PendingParent attached bracket legs owned by an expired DAY entry
            let pending_children: Vec<String> = next
                .orders
                .values()
                .filter(|c| {
                    c.parent_order_id.as_deref() == Some(ord_id.as_str())
                        && c.status == OrderStatus::PendingParent
                })
                .map(|c| c.order_id.clone())
                .collect();
            for child_id in pending_children {
                cancel_order_internal(
                    &mut next,
                    &child_id,
                    "parent_order_expired",
                    sb.bar.close_at,
                    &mut events,
                );
            }
        }

        next.last_close_by_instrument
            .insert(sb.bar.instrument_id.clone(), canon_decimal(sb.bar.close));
        next.last_close_at_by_instrument
            .insert(sb.bar.instrument_id.clone(), sb.bar.close_at);
        next.visible_until = Some(match next.visible_until {
            Some(prev) => prev.max(sb.bar.close_at),
            None => sb.bar.close_at,
        });
        next.step_count += 1;

        let pos_after = next.position_quantity(&sb.bar.instrument_id);
        let cost_after = next.position_cost(&sb.bar.instrument_id);
        let equity = next.total_equity_at_last_close();
        let snapshot = SimBarSnapshot {
            step_index: next.step_count,
            instrument_id: sb.bar.instrument_id.clone(),
            session_date: sb.bar.session_date,
            open_at: sb.bar.open_at,
            close_at: sb.bar.close_at,
            open: canon_decimal(sb.bar.open),
            high: canon_decimal(sb.bar.high),
            low: canon_decimal(sb.bar.low),
            close: canon_decimal(sb.bar.close),
            volume: canon_decimal(sb.bar.volume),
            tradable: sb.is_effectively_tradable(),
            had_buy_fill: *had_buy_by_inst.get(&sb.bar.instrument_id).unwrap_or(&false),
            had_sell_fill: *had_sell_by_inst
                .get(&sb.bar.instrument_id)
                .unwrap_or(&false),
            position_qty_before: *pos_before_by_inst
                .get(&sb.bar.instrument_id)
                .unwrap_or(&Decimal::ZERO),
            cash_after: next.cash_balance,
            position_qty_after: pos_after,
            position_cost_after: cost_after,
            equity_at_close: equity,
        };
        next.bar_snapshots.push(snapshot.clone());
        events.push(SimEvent::BarAdvanced {
            snapshot: Box::new(snapshot),
        });
    }

    next.normalize_fields();
    Ok((next, events))
}

fn execute_triggered_order(
    state: &mut SimState,
    trig: &TriggerMatch,
    stage: BarPathStage,
    bar: &BarRecord,
    had_buy_by_inst: &mut BTreeMap<String, bool>,
    had_sell_by_inst: &mut BTreeMap<String, bool>,
    events: &mut Vec<SimEvent>,
) -> Result<(), SimError> {
    let Some(order) = state.orders.get(&trig.order_id).cloned() else {
        return Ok(());
    };
    if order.status != OrderStatus::Active {
        return Ok(());
    }
    let spec = state.instruments[&order.instrument_id].clone();

    let settlement = settle_order_fees(
        &state.fee_profile,
        order.side,
        &spec.base_asset_id,
        &spec.quote_currency,
        &[SyntheticFillInput {
            fill_id: "EXEC".to_string(),
            quantity: order.quantity,
            fill_price: trig.fill_price,
        }],
    )?;
    let settled_fill = &settlement.fills[0];

    // Check `INSUFFICIENT_FUNDS_AT_FILL` (`S-40`, `M4-A-06`):
    // Total cash required at fill must not exceed `available_cash + this_order_cash_reservation`.
    let order_cash_reserved = order
        .reservation_id
        .as_ref()
        .and_then(|rid| state.cash_reservations.get(rid))
        .map(|r| r.amount)
        .unwrap_or(Decimal::ZERO);
    let max_spendable_for_order = canon_decimal(state.available_cash() + order_cash_reserved);

    if settled_fill.net_quote_cash_delta < Decimal::ZERO {
        let cash_needed = -settled_fill.net_quote_cash_delta;
        if cash_needed > max_spendable_for_order {
            if let Some(o) = state.orders.get_mut(&order.order_id) {
                o.status = OrderStatus::Rejected;
                o.reject_code = Some(SimRejectCode::InsufficientFundsAtFill);
                o.updated_at = bar.close_at;
            }
            release_order_reservation(state, &order.order_id);
            let rejection = OrderRejection {
                code: SimRejectCode::InsufficientFundsAtFill,
                message: format!(
                    "whole order rejected at fill: required cash {}, spendable (available + order reservation) {}",
                    to_canonical_text(cash_needed),
                    to_canonical_text(max_spendable_for_order),
                ),
                field: None,
                nearest_lower: None,
                nearest_upper: None,
                required: Some(cash_needed),
                available: Some(max_spendable_for_order),
            };
            events.push(SimEvent::OrderRejected {
                order_id: order.order_id.clone(),
                instrument_id: order.instrument_id.clone(),
                rejection,
            });
            if let Some(peer_id) = &order.oco_peer_order_id {
                cancel_order_internal(state, peer_id, "oco_peer_rejected", bar.close_at, events);
            }
            // Cancel any PendingParent attached bracket legs owned by this rejected entry
            let pending_children: Vec<String> = state
                .orders
                .values()
                .filter(|c| {
                    c.parent_order_id.as_deref() == Some(order.order_id.as_str())
                        && c.status == OrderStatus::PendingParent
                })
                .map(|c| c.order_id.clone())
                .collect();
            for child_id in pending_children {
                cancel_order_internal(
                    state,
                    &child_id,
                    "parent_order_rejected",
                    bar.close_at,
                    events,
                );
            }
            return Ok(());
        }
    }

    let ambiguous = is_oco_dual_touch_ambiguous(state, &order, bar, stage);
    if ambiguous {
        state.ambiguous_fill_count += 1;
    }

    let fill_seq = state.next_fill_seq;
    state.next_fill_seq += 1;
    let fill_id = next_fill_id(fill_seq);

    let (net_base_qty, realized_pnl_delta) = match order.side {
        OrderSide::Buy => {
            // Release cash reservation first, then deduct actual cash cost
            release_order_reservation(state, &order.order_id);
            state.cash_balance =
                canon_decimal(state.cash_balance + settled_fill.net_quote_cash_delta);
            let lot_cost = canon_decimal(-settled_fill.net_quote_cash_delta);
            let net_qty = settled_fill.net_received_base_quantity;
            state
                .lots_by_instrument
                .entry(order.instrument_id.clone())
                .or_default()
                .push(SimLot {
                    lot_id: format!("SIM-LOT-{fill_seq:06}"),
                    source_fill_id: fill_id.clone(),
                    instrument_id: order.instrument_id.clone(),
                    acquired_at: bar.close_at,
                    original_quantity: net_qty,
                    remaining_quantity: net_qty,
                    original_cost_quote: lot_cost,
                    remaining_cost_quote: lot_cost,
                });
            had_buy_by_inst.insert(order.instrument_id.clone(), true);
            (net_qty, Decimal::ZERO)
        }
        OrderSide::Sell => {
            if let Some(rid) = &order.reservation_id {
                consume_position_reservation_fully(state, rid);
            }
            let lots = state
                .lots_by_instrument
                .entry(order.instrument_id.clone())
                .or_default();
            let allocated_cost = allocate_fifo_lots_for_sell(lots, order.quantity)?;
            let net_proceeds = settled_fill.net_quote_cash_delta;
            state.cash_balance = canon_decimal(state.cash_balance + net_proceeds);
            let pnl = canon_decimal(net_proceeds - allocated_cost);
            state.realized_pnl = canon_decimal(state.realized_pnl + pnl);
            had_sell_by_inst.insert(order.instrument_id.clone(), true);
            (order.quantity, pnl)
        }
    };

    state.total_fees_quote = canon_decimal(state.total_fees_quote + settled_fill.fee_quote_value);

    if let Some(o) = state.orders.get_mut(&order.order_id) {
        o.status = OrderStatus::Filled;
        o.updated_at = bar.close_at;
    }

    let sim_fill = SimFill {
        fill_id,
        fill_seq,
        order_id: order.order_id.clone(),
        order_seq: order.order_seq,
        parent_group_id: order.parent_group_id.clone(),
        group_role: order.group_role,
        has_trade_plan: order.has_trade_plan,
        instrument_id: order.instrument_id.clone(),
        bar_session_date: bar.session_date,
        bar_open_at: bar.open_at,
        bar_close_at: bar.close_at,
        path_stage: stage,
        raw_path_price: trig.trigger_path_price,
        fill_price: settled_fill.fill_price,
        gross_quantity: order.quantity,
        net_base_quantity: net_base_qty,
        side: order.side,
        fee_amount: settled_fill.fee_amount,
        fee_currency: settled_fill.fee_currency.clone(),
        fee_quote_value: settled_fill.fee_quote_value,
        realized_pnl_delta,
        ambiguous,
    };
    state.fills.push(sim_fill.clone());
    events.push(SimEvent::OrderFilled {
        fill: Box::new(sim_fill),
    });

    // If this order had an OCO peer, cancel the other OCO leg immediately (`S-39`, `M4-A-05`)
    if let Some(peer_id) = &order.oco_peer_order_id {
        cancel_order_internal(state, peer_id, "oco_peer_filled", bar.close_at, events);
    }

    // If this was a Buy entry with PendingParent attached bracket leg(s), activate them NOW
    // for path segments strictly AFTER `(stage, trig.trigger_path_price)` (`S-39`, `M4-A-05`)!
    if order.side == OrderSide::Buy {
        let mut child_ids: Vec<String> = state
            .orders
            .values()
            .filter(|c| {
                c.parent_order_id.as_deref() == Some(order.order_id.as_str())
                    && c.status == OrderStatus::PendingParent
            })
            .map(|c| c.order_id.clone())
            .collect();
        child_ids.sort();

        if !child_ids.is_empty() {
            let rem_lot = net_base_qty % spec.lot_size;
            let exit_qty = canon_decimal(net_base_qty - rem_lot);
            if exit_qty <= Decimal::ZERO
                || exit_qty > state.available_position(&order.instrument_id)
            {
                for cid in child_ids {
                    cancel_order_internal(
                        state,
                        &cid,
                        "insufficient_received_lot_for_bracket",
                        bar.close_at,
                        events,
                    );
                }
            } else {
                let res_id = next_res_id(state.next_reservation_seq);
                state.next_reservation_seq += 1;
                state.position_reservations.insert(
                    res_id.clone(),
                    PositionReservation {
                        reservation_id: res_id.clone(),
                        instrument_id: order.instrument_id.clone(),
                        quantity: exit_qty,
                        order_ids: child_ids.clone(),
                    },
                );
                for cid in child_ids {
                    if let Some(c) = state.orders.get_mut(&cid) {
                        c.quantity = exit_qty;
                        c.reservation_id = Some(res_id.clone());
                        c.status = OrderStatus::Active;
                        c.updated_at = bar.close_at;
                        c.activated_bar_close_at = Some(bar.close_at);
                        c.activated_stage = Some(stage);
                        c.activated_path_price = Some(trig.trigger_path_price);
                    }
                    events.push(SimEvent::OrderActivated {
                        order_id: cid,
                        instrument_id: order.instrument_id.clone(),
                        quantity: exit_qty,
                        at_stage: stage,
                        at_path_price: trig.trigger_path_price,
                    });
                }
            }
        }
    }

    Ok(())
}

/// Pure single-step state transition function (`S-36`):
/// `step(state, command, bar) -> Result<(SimState, Vec<SimEvent>), SimError>`.
pub fn step(
    state: &SimState,
    command: SimCommand,
    bar: Option<&SimStepBar>,
) -> Result<(SimState, Vec<SimEvent>), SimError> {
    match command {
        SimCommand::SubmitOrder(req) => {
            let mut next = state.clone();
            let events = submit_order_into_state(&mut next, req)?;
            next.command_seq += 1;
            next.normalize_fields();
            Ok((next, events))
        }
        SimCommand::CancelOrder { order_id, reason } => {
            let mut next = state.clone();
            let events = cancel_order_command(&mut next, &order_id, reason)?;
            next.command_seq += 1;
            next.normalize_fields();
            Ok((next, events))
        }
        SimCommand::AdvanceBar { corporate_actions } => {
            let sb = bar.ok_or(SimError::MissingBarInput)?;
            step_multi_bars(state, std::slice::from_ref(sb), &corporate_actions)
        }
        SimCommand::Finish => {
            if state.finished {
                return Err(SimError::SessionFinished);
            }
            let mut next = state.clone();
            next.command_seq += 1;
            let now = next
                .visible_until
                .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
            let open_ids: Vec<String> = next
                .orders
                .values()
                .filter(|o| o.status.is_open_or_pending())
                .map(|o| o.order_id.clone())
                .collect();
            let mut events = Vec::new();
            for oid in &open_ids {
                cancel_order_internal(&mut next, oid, "session_finished", now, &mut events);
            }
            next.finished = true;
            events.push(SimEvent::SessionFinished {
                visible_until: next.visible_until,
                cancelled_order_ids: open_ids,
            });
            next.normalize_fields();
            Ok((next, events))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::parse_decimal_lossless;
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
}
