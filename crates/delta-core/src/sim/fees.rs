use crate::decimal::canon_decimal;
use crate::sim::{OrderSide, SimError};
use chrono::NaiveDate;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
    if step <= Decimal::ZERO || value <= Decimal::ZERO {
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
    let upper = lower.checked_add(step).map(canon_decimal);
    Err(StepValidationViolation {
        invalid_value: canon_decimal(value),
        step_size: step,
        nearest_lower: (lower > Decimal::ZERO).then_some(lower),
        nearest_upper: upper,
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
        OrderSide::Buy => canon_decimal((raw_price - rem).saturating_add(tick)),
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

fn checked_mul_sim(a: Decimal, b: Decimal, ctx: &str) -> Result<Decimal, SimError> {
    a.checked_mul(b)
        .ok_or_else(|| SimError::ArithmeticOverflow(ctx.to_string()))
}

fn checked_add_sim(a: Decimal, b: Decimal, ctx: &str) -> Result<Decimal, SimError> {
    a.checked_add(b)
        .ok_or_else(|| SimError::ArithmeticOverflow(ctx.to_string()))
}

fn checked_sub_sim(a: Decimal, b: Decimal, ctx: &str) -> Result<Decimal, SimError> {
    a.checked_sub(b)
        .ok_or_else(|| SimError::ArithmeticOverflow(ctx.to_string()))
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
        let notional = checked_mul_sim(fill.quantity, fill.fill_price, "fee_fill_notional")?;
        let var_raw = if fee_in_received_base {
            let rate_fee = checked_mul_sim(fill.quantity, profile.notional_rate, "fee_base_rate")?;
            let unit_fee = checked_mul_sim(fill.quantity, profile.per_unit_fee, "fee_base_unit")?;
            checked_add_sim(rate_fee, unit_fee, "fee_base_var")?
        } else {
            let rate_fee = checked_mul_sim(notional, profile.notional_rate, "fee_notional_rate")?;
            let unit_fee = checked_mul_sim(fill.quantity, profile.per_unit_fee, "fee_unit")?;
            checked_add_sim(rate_fee, unit_fee, "fee_quote_var")?
        };
        let fixed_raw = if idx == 0 {
            profile.per_order_fixed_fee
        } else {
            Decimal::ZERO
        };

        let mut fill_top_up = Decimal::ZERO;
        let fill_fee = match profile.charging_tier {
            FeeChargingTier::PerFill => {
                let raw_total = checked_add_sim(var_raw, fixed_raw, "fee_per_fill_raw")?;
                let mut f = profile
                    .rounding_mode
                    .round_value(raw_total, profile.rounding_scale);
                let mut next_cum = checked_add_sim(cumulative_settled, f, "fee_cum_settled")?;
                if let Some(max_f) = max_rounded {
                    if next_cum > max_f {
                        max_cap_applied = true;
                        f = (max_f - cumulative_settled).max(Decimal::ZERO);
                        next_cum = checked_add_sim(cumulative_settled, f, "fee_cum_capped")?;
                    }
                }
                if is_last {
                    if let Some(min_f) = min_rounded {
                        if next_cum < min_f {
                            fill_top_up = canon_decimal(min_f - next_cum);
                            min_top_up_applied = fill_top_up;
                            f = canon_decimal(checked_add_sim(f, fill_top_up, "fee_min_top_up")?);
                        }
                    }
                }
                canon_decimal(f)
            }
            FeeChargingTier::PerOrder => {
                let fill_raw = checked_add_sim(var_raw, fixed_raw, "fee_per_order_raw")?;
                cumulative_unrounded =
                    checked_add_sim(cumulative_unrounded, fill_raw, "fee_cum_unrounded")?;
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

        cumulative_settled =
            canon_decimal(checked_add_sim(cumulative_settled, fill_fee, "fee_total")?);

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
            let quote_val = canon_decimal(checked_mul_sim(
                fill_fee,
                fill.fill_price,
                "fee_quote_value",
            )?);
            let cash_delta = canon_decimal(-notional);
            (net_base, quote_val, cash_delta)
        } else if side == OrderSide::Buy {
            let cash_out = canon_decimal(checked_add_sim(notional, fill_fee, "buy_cash_out")?);
            (canon_decimal(fill.quantity), fill_fee, -cash_out)
        } else {
            let cash_in = canon_decimal(checked_sub_sim(notional, fill_fee, "sell_cash_in")?);
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

    let mut total_fee_quote_value = Decimal::ZERO;
    for f in &settled_fills {
        total_fee_quote_value = checked_add_sim(
            total_fee_quote_value,
            f.fee_quote_value,
            "total_fee_quote_value",
        )?;
    }

    Ok(OrderFeeSettlement {
        fee_profile_id: profile.id.clone(),
        side,
        fee_currency,
        fills: settled_fills,
        total_fee_amount: cumulative_settled,
        total_fee_quote_value: canon_decimal(total_fee_quote_value),
        min_fee_top_up_applied: min_top_up_applied,
        max_fee_cap_applied: max_cap_applied,
    })
}
