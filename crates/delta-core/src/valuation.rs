use crate::ledger::{LedgerCategory, LedgerProjectionResult};
use crate::model::QualityStatus;
use chrono::NaiveDate;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxQuoteLookup {
    pub rate: Decimal,
    pub source_date: NaiveDate,
    pub quality: QualityStatus,
}

pub fn is_fiat_currency(currency: &str) -> bool {
    matches!(
        currency.to_ascii_uppercase().as_str(),
        "USD"
            | "HKD"
            | "CNY"
            | "EUR"
            | "GBP"
            | "JPY"
            | "AUD"
            | "CAD"
            | "CHF"
            | "NZD"
            | "SGD"
            | "KRW"
            | "INR"
            | "BRL"
            | "MXN"
            | "ZAR"
            | "NOK"
            | "SEK"
            | "DKK"
            | "PLN"
            | "CZK"
            | "HUF"
            | "RON"
            | "BGN"
            | "ISK"
            | "TRY"
            | "ILS"
            | "IDR"
            | "MYR"
            | "PHP"
            | "THB"
    )
}

/// Resolve an FX rate on `target_date` following S-07 / A-07:
/// - Exact match on `target_date` -> `QualityStatus::Exact`
/// - Fiat reference rate (ECB/Frankfurter) non-publication day -> use most recent published rate
///   not later than `target_date` within 4 calendar days -> `QualityStatus::CarriedForward`
/// - More than 4 calendar days without publication -> `None` (caller records `missing_inputs` & `Partial`)
/// - Same-version deterministic inverse rate `1 / rate` if direct rate is missing on the same date.
pub fn resolve_fx_rate_s07(
    base_currency: &str,
    quote_currency: &str,
    target_date: NaiveDate,
    is_stablecoin_pair: bool,
    rates_by_pair_date: &HashMap<(String, String, NaiveDate), Decimal>,
) -> Option<FxQuoteLookup> {
    let base = base_currency.to_ascii_uppercase();
    let quote = quote_currency.to_ascii_uppercase();
    if base == quote {
        return Some(FxQuoteLookup {
            rate: Decimal::ONE,
            source_date: target_date,
            quality: QualityStatus::Exact,
        });
    }

    let lookup_single_date = |d: NaiveDate| -> Option<Decimal> {
        if let Some(r) = rates_by_pair_date.get(&(base.clone(), quote.clone(), d)) {
            return (*r > Decimal::ZERO).then_some(*r);
        }
        // Same-version inverse rate (M1-W02 requirement)
        if let Some(inv) = rates_by_pair_date.get(&(quote.clone(), base.clone(), d)) {
            if *inv > Decimal::ZERO {
                return Some(
                    (Decimal::ONE / *inv)
                        .round_dp_with_strategy(10, RoundingStrategy::MidpointAwayFromZero)
                        .normalize(),
                );
            }
        }
        None
    };

    if let Some(rate) = lookup_single_date(target_date) {
        return Some(FxQuoteLookup {
            rate,
            source_date: target_date,
            quality: QualityStatus::Exact,
        });
    }

    if base != "USD" && quote != "USD" {
        let first = resolve_fx_rate_s07(
            &base,
            "USD",
            target_date,
            !is_fiat_currency(&base),
            rates_by_pair_date,
        );
        let second = resolve_fx_rate_s07(
            "USD",
            &quote,
            target_date,
            !is_fiat_currency(&quote),
            rates_by_pair_date,
        );
        if let (Some(first), Some(second)) = (first, second) {
            return Some(FxQuoteLookup {
                rate: first.rate.checked_mul(second.rate)?,
                source_date: first.source_date.min(second.source_date),
                quality: if first.quality == QualityStatus::CarriedForward
                    || second.quality == QualityStatus::CarriedForward
                {
                    QualityStatus::CarriedForward
                } else {
                    QualityStatus::Exact
                },
            });
        }
    }

    // For stablecoins, crypto trades 24x7; do NOT carry forward across missing days if missing on target_date,
    // or if fiat, carry forward up to 4 calendar days (S-07).
    if !is_stablecoin_pair {
        for days_back in 1..=4 {
            if let Some(prev_date) = target_date.checked_sub_days(chrono::Days::new(days_back)) {
                if let Some(rate) = lookup_single_date(prev_date) {
                    return Some(FxQuoteLookup {
                        rate,
                        source_date: prev_date,
                        quality: QualityStatus::CarriedForward,
                    });
                }
            }
        }
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValuationReport {
    pub report_currency: String,
    pub as_of_date: NaiveDate,
    pub account_scope: Vec<String>,
    pub cash_value: Decimal,
    pub holdings_market_value: Decimal,
    pub total_assets: Decimal,
    pub total_liabilities: Decimal,
    pub net_worth: Decimal,
    pub total_carrying_cost: Decimal,
    pub realized_pnl: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub dividend_income: Decimal,
    pub total_pnl: Option<Decimal>,
    pub fee_summary_explanatory: Decimal,
    pub net_external_cash_flow: Decimal,
    pub scope_and_currency_effect_pnl: Option<Decimal>,
    pub quality: QualityStatus,
    pub carried_forward_fx: Vec<(String, NaiveDate, NaiveDate)>,
    pub missing_inputs: Vec<String>,
    pub warnings: Vec<String>,
}

/// Evaluate portfolio / account scope at `as_of_date` in `report_currency`.
/// Never Fabricates zero for missing prices, missing FX, or unknown cost basis (AC-06).
/// Never assumes USDT or USDC == 1.0 USD (AC-06 / S-07).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_portfolio(
    projection: &LedgerProjectionResult,
    account_scope: &[String],
    report_currency: &str,
    as_of_date: NaiveDate,
    market_prices: &HashMap<(String, NaiveDate), Decimal>,
    asset_quote_currencies: &HashMap<String, String>,
    fx_rates: &HashMap<(String, String, NaiveDate), Decimal>,
    liabilities: &HashMap<String, (Decimal, String)>,
) -> ValuationReport {
    let scope_set: BTreeSet<String> = account_scope.iter().cloned().collect();
    let mut cash_value = Decimal::ZERO;
    let mut holdings_market_value = Decimal::ZERO;
    let mut total_carrying_cost = Decimal::ZERO;
    let mut realized_pnl_sum = Decimal::ZERO;
    let mut dividend_sum = Decimal::ZERO;
    let mut fee_sum = Decimal::ZERO;
    let mut net_external_cash_flow = Decimal::ZERO;

    let mut quality = QualityStatus::Exact;
    let mut carried_forward_fx = Vec::new();
    let mut missing_inputs = Vec::new();
    let mut warnings = Vec::new();
    let mut has_unknown_cost = false;
    for (account, asset) in &projection.unknown_cost_assets {
        if scope_set.contains(account) {
            has_unknown_cost = true;
            quality = QualityStatus::Partial;
            missing_inputs.push(format!("historical_cost_basis:{account}:{asset}"));
        }
    }

    let is_stable = |ccy: &str| matches!(ccy.to_ascii_uppercase().as_str(), "USDT" | "USDC");

    let resolve_fx = |from_ccy: &str,
                      quality: &mut QualityStatus,
                      carried_forward_fx: &mut Vec<(String, NaiveDate, NaiveDate)>,
                      missing_inputs: &mut Vec<String>|
     -> Option<Decimal> {
        let stable = is_stable(from_ccy) || is_stable(report_currency);
        match resolve_fx_rate_s07(from_ccy, report_currency, as_of_date, stable, fx_rates) {
            Some(found) => {
                if found.quality == QualityStatus::CarriedForward {
                    if *quality == QualityStatus::Exact {
                        *quality = QualityStatus::CarriedForward;
                    }
                    carried_forward_fx.push((
                        format!("{from_ccy}/{report_currency}"),
                        as_of_date,
                        found.source_date,
                    ));
                }
                Some(found.rate)
            }
            None => {
                *quality = QualityStatus::Partial;
                missing_inputs.push(format!("fx:{from_ccy}/{report_currency}@{as_of_date}"));
                None
            }
        }
    };

    for ((acct, ccy), qty) in &projection.cash_balances {
        if !scope_set.contains(acct) || qty.is_zero() {
            continue;
        }
        if let Some(fx) = resolve_fx(
            ccy,
            &mut quality,
            &mut carried_forward_fx,
            &mut missing_inputs,
        ) {
            cash_value += (*qty * fx).normalize();
        }
    }

    for ((acct, asset), holding) in &projection.holdings {
        if !scope_set.contains(acct) || holding.quantity.is_zero() {
            continue;
        }
        if !holding.cost_known {
            has_unknown_cost = true;
            quality = QualityStatus::Partial;
            missing_inputs.push(format!("cost_basis:{acct}:{asset}"));
        } else if let Some(book_to_rep_fx) = resolve_fx(
            &projection.book_currency,
            &mut quality,
            &mut carried_forward_fx,
            &mut missing_inputs,
        ) {
            total_carrying_cost += (holding.carrying_cost * book_to_rep_fx).normalize();
        }

        let price_opt = market_prices.get(&(asset.clone(), as_of_date)).copied();
        let quote_ccy = asset_quote_currencies
            .get(asset)
            .cloned()
            .unwrap_or_else(|| projection.book_currency.clone());

        let fx_opt = resolve_fx(
            &quote_ccy,
            &mut quality,
            &mut carried_forward_fx,
            &mut missing_inputs,
        );
        match (price_opt, fx_opt) {
            (Some(price), Some(fx)) => {
                holdings_market_value += (holding.quantity * price * fx).normalize();
            }
            (None, _) => {
                quality = QualityStatus::Partial;
                missing_inputs.push(format!("price:{asset}@{as_of_date}"));
            }
            (_, None) => {}
        }
    }

    let mut transit_cash_cost = Decimal::ZERO;
    for transit in &projection.in_transit_assets {
        if !scope_set.contains(&transit.source_account)
            || !scope_set.contains(&transit.destination_account)
        {
            continue;
        }
        if !transit.cost_known {
            has_unknown_cost = true;
            missing_inputs.push(format!("transit_cost:{}", transit.asset_id));
        }
        if let Some(rate) = resolve_fx(
            &transit.quote_currency,
            &mut quality,
            &mut carried_forward_fx,
            &mut missing_inputs,
        ) {
            if transit.is_cash {
                cash_value += transit.quantity * rate;
            } else if let Some(price) = market_prices.get(&(transit.asset_id.clone(), as_of_date)) {
                holdings_market_value += transit.quantity * *price * rate;
            } else {
                missing_inputs.push(format!("transit_price:{}", transit.asset_id));
                quality = QualityStatus::Partial;
            }
        }
        if let Some(rate) = resolve_fx(
            &projection.book_currency,
            &mut quality,
            &mut carried_forward_fx,
            &mut missing_inputs,
        ) {
            if transit.is_cash {
                transit_cash_cost += transit.carrying_cost * rate;
            } else {
                total_carrying_cost += transit.carrying_cost * rate;
            }
        }
    }

    let mut total_liabilities = Decimal::ZERO;
    for acct in &scope_set {
        if let Some((liab_amt, liab_ccy)) = liabilities.get(acct) {
            if let Some(fx) = resolve_fx(
                liab_ccy,
                &mut quality,
                &mut carried_forward_fx,
                &mut missing_inputs,
            ) {
                total_liabilities += (*liab_amt * fx).normalize();
            }
        }
        if let Some(rpnl) = projection.realized_pnl_by_account.get(acct) {
            realized_pnl_sum += *rpnl;
        }
        if let Some(div) = projection.dividend_income_by_account.get(acct) {
            dividend_sum += *div;
        }
        if let Some(fee) = projection.fee_expense_by_account.get(acct) {
            fee_sum += *fee;
        }
    }

    for flow in &projection.external_cash_flows {
        if !scope_set.contains(&flow.account_id)
            || (flow.internal_candidate
                && flow
                    .counterparty_account_id
                    .as_ref()
                    .is_some_and(|a| scope_set.contains(a)))
        {
            continue;
        }
        if let Some(asset) = &flow.position_asset_id {
            let date = flow.occurred_at.date_naive();
            let price = market_prices.get(&(asset.clone(), date));
            let currency = asset_quote_currencies
                .get(asset)
                .unwrap_or(&flow.native_currency);
            let rate = resolve_fx_rate_s07(
                currency,
                report_currency,
                date,
                !is_fiat_currency(currency) || !is_fiat_currency(report_currency),
                fx_rates,
            );
            if let (Some(price), Some(rate)) = (price, rate) {
                net_external_cash_flow += flow.position_quantity * *price * rate.rate;
            } else {
                missing_inputs.push(format!("position_transfer_value:{asset}@{date}"));
                quality = QualityStatus::Partial;
            }
        } else if report_currency.eq_ignore_ascii_case(&projection.book_currency) {
            net_external_cash_flow += flow.book_amount;
        } else if let Some(rate) = resolve_fx_rate_s07(
            &flow.native_currency,
            report_currency,
            flow.occurred_at.date_naive(),
            !is_fiat_currency(&flow.native_currency) || !is_fiat_currency(report_currency),
            fx_rates,
        ) {
            net_external_cash_flow += flow.native_amount * rate.rate;
        } else {
            quality = QualityStatus::Partial;
            missing_inputs.push(format!(
                "flow_fx:{}/{}@{}",
                flow.native_currency,
                report_currency,
                flow.occurred_at.date_naive()
            ));
        }
    }

    if !projection.pending_valuation_reasons.is_empty() {
        quality = QualityStatus::Partial;
        for reason in &projection.pending_valuation_reasons {
            missing_inputs.push(reason.clone());
        }
    }

    let book_fx = resolve_fx(
        &projection.book_currency,
        &mut quality,
        &mut carried_forward_fx,
        &mut missing_inputs,
    )
    .unwrap_or(Decimal::ZERO);
    realized_pnl_sum *= book_fx;
    dividend_sum *= book_fx;
    fee_sum *= book_fx;
    let cash_cost: Decimal = transit_cash_cost
        + projection
            .cash_carrying_costs
            .iter()
            .filter(|((acct, _), _)| scope_set.contains(acct))
            .map(|(_, c)| *c * book_fx)
            .sum::<Decimal>();
    let total_assets = (cash_value + holdings_market_value).normalize();
    let net_worth = (total_assets - total_liabilities).normalize();
    let net_external_cash_flow = net_external_cash_flow.normalize();

    let (realized_pnl, unrealized_pnl, total_pnl, scope_and_currency_effect_pnl) =
        if has_unknown_cost || !missing_inputs.is_empty() {
            warnings.push(
                "incomplete inputs or unknown cost basis: PnL is not fabricated as zero"
                    .to_string(),
            );
            (None, None, None, None)
        } else {
            let unrea =
                (holdings_market_value - total_carrying_cost + cash_value - cash_cost).normalize();
            let standalone_fees: Decimal = projection
                .transactions
                .iter()
                .flat_map(|tx| &tx.postings)
                .filter(|p| {
                    p.category == LedgerCategory::FeeExpense
                        && scope_set
                            .iter()
                            .any(|acct| p.ledger_account_id.starts_with(&format!("L:{acct}:")))
                })
                .map(|p| p.book_amount * book_fx)
                .sum();
            let components =
                (realized_pnl_sum + unrea + dividend_sum - standalone_fees).normalize();
            let tot = (net_worth - net_external_cash_flow).normalize();
            let translation = (tot - components).normalize();
            let scoped_transfer = projection.external_cash_flows.iter().any(|flow| {
                scope_set.contains(&flow.account_id)
                    && flow.internal_candidate
                    && flow
                        .counterparty_account_id
                        .as_ref()
                        .is_some_and(|a| !scope_set.contains(a))
            });
            if !scoped_transfer
                && report_currency.eq_ignore_ascii_case(&projection.book_currency)
                && translation.abs() > Decimal::new(1, 7)
            {
                quality = QualityStatus::Warning;
                warnings.push(format!("pnl_components_difference:{translation}"));
            }
            (
                Some(realized_pnl_sum.normalize()),
                Some(unrea),
                Some(tot),
                Some(translation),
            )
        };

    ValuationReport {
        report_currency: report_currency.to_string(),
        as_of_date,
        account_scope: scope_set.into_iter().collect(),
        cash_value,
        holdings_market_value,
        total_assets,
        total_liabilities,
        net_worth,
        total_carrying_cost: total_carrying_cost.normalize(),
        realized_pnl,
        unrealized_pnl,
        dividend_income: dividend_sum.normalize(),
        total_pnl,
        fee_summary_explanatory: fee_sum.normalize(),
        net_external_cash_flow,
        scope_and_currency_effect_pnl,
        quality,
        carried_forward_fx,
        missing_inputs,
        warnings,
    }
}

/// Decompose foreign currency exposure change into Price Impact and FX Impact (Financial Engine Sec 4):
/// - `price_impact = q * (p1 - p0) * fx0`
/// - `fx_impact = q * p1 * (fx1 - fx0)`
/// - `total_change = price_impact + fx_impact == q * p1 * fx1 - q * p0 * fx0`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceFxDecomposition {
    pub quantity: Decimal,
    pub price_impact: Decimal,
    pub fx_impact: Decimal,
    pub total_change: Decimal,
}

pub fn decompose_price_and_fx_impact(
    quantity: Decimal,
    p0: Decimal,
    p1: Decimal,
    fx0: Decimal,
    fx1: Decimal,
) -> PriceFxDecomposition {
    let price_impact = (quantity * (p1 - p0) * fx0).normalize();
    let fx_impact = (quantity * p1 * (fx1 - fx0)).normalize();
    let total_change = (price_impact + fx_impact).normalize();
    PriceFxDecomposition {
        quantity,
        price_impact,
        fx_impact,
        total_change,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubPeriodSnapshot {
    pub v_start_after_flow: Decimal,
    pub v_end_before_next_flow: Decimal,
}

/// Compute exact Time-Weighted Return (TWR) across sub-periods sliced at external cash flows (AC-07).
pub fn compute_twr(sub_periods: &[SubPeriodSnapshot]) -> Result<Decimal, String> {
    if sub_periods.is_empty() {
        return Err("no sub-periods provided for TWR".to_string());
    }
    let mut compound = Decimal::ONE;
    for (idx, sp) in sub_periods.iter().enumerate() {
        if sp.v_start_after_flow <= Decimal::ZERO {
            return Err(format!(
                "sub-period {idx} has non-positive starting value {}",
                sp.v_start_after_flow
            ));
        }
        let one_plus_r = sp.v_end_before_next_flow / sp.v_start_after_flow;
        compound *= one_plus_r;
    }
    Ok((compound - Decimal::ONE)
        .round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero)
        .normalize())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum XirrStatus {
    Converged {
        rate: Decimal,
        bracket: (String, String),
    },
    NoSignChange,
    NoRoot,
    MultipleRoots {
        roots: Vec<Decimal>,
    },
}

/// Compute MWR / XIRR using range scan + bisection / secant refinement (Financial Engine Sec 5.2, AC-07, AC-18).
/// Cash flows: investor perspective (initial value & inflows are negative, outflows & ending value are positive).
pub fn compute_xirr(cash_flows: &[(NaiveDate, Decimal)]) -> XirrStatus {
    // Merge same-day cash flows
    let mut merged: BTreeMap<NaiveDate, Decimal> = BTreeMap::new();
    for (d, cf) in cash_flows {
        *merged.entry(*d).or_insert(Decimal::ZERO) += *cf;
    }
    let flows: Vec<(NaiveDate, f64)> = merged
        .into_iter()
        .filter(|(_, v)| !v.is_zero())
        .filter_map(|(d, v)| v.to_f64().map(|f| (d, f)))
        .collect();

    if flows.len() < 2 {
        return XirrStatus::NoSignChange;
    }
    let has_pos = flows.iter().any(|(_, v)| *v > 0.0);
    let has_neg = flows.iter().any(|(_, v)| *v < 0.0);
    if !has_pos || !has_neg {
        return XirrStatus::NoSignChange;
    }

    let d0 = flows[0].0;
    let npv = |r: f64| -> f64 {
        flows
            .iter()
            .map(|(d, cf)| {
                let days = (*d - d0).num_days() as f64;
                cf / (1.0 + r).powf(days / 365.0)
            })
            .sum()
    };

    // Range scan across (-0.95, 10.0) to detect all sign changes (including multiple roots)
    let mut scan_points = Vec::new();
    let mut cur = -0.95_f64;
    while cur <= 10.0 {
        scan_points.push(cur);
        cur += if cur < 1.0 { 0.05 } else { 0.25 };
    }

    let mut brackets = Vec::new();
    for win in scan_points.windows(2) {
        let (a, b) = (win[0], win[1]);
        let fa = npv(a);
        let fb = npv(b);
        if fa.abs() < 1e-11 {
            brackets.push((a, a));
        } else if fa * fb < 0.0 {
            brackets.push((a, b));
        }
    }

    if brackets.is_empty() {
        return XirrStatus::NoRoot;
    }

    let mut roots = Vec::new();
    let mut last_bracket = (String::new(), String::new());
    for (mut lo, mut hi) in brackets {
        last_bracket = (format!("{lo:.4}"), format!("{hi:.4}"));
        let mut flo = npv(lo);
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            let fmid = npv(mid);
            if fmid.abs() < 1e-11 || (hi - lo).abs() < 1e-11 {
                lo = mid;
                break;
            }
            if flo * fmid <= 0.0 {
                hi = mid;
            } else {
                lo = mid;
                flo = fmid;
            }
        }
        if let Some(dec) = Decimal::from_f64_retain(lo) {
            let rounded = dec
                .round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero)
                .normalize();
            if !roots.contains(&rounded) {
                roots.push(rounded);
            }
        }
    }

    if roots.is_empty() {
        XirrStatus::NoRoot
    } else if roots.len() > 1 {
        XirrStatus::MultipleRoots { roots }
    } else {
        XirrStatus::Converged {
            rate: roots[0],
            bracket: last_bracket,
        }
    }
}

/// Compute maximum investment drawdown over a comparable net value series `N_t` (Financial Engine Sec 5.3).
pub fn compute_max_drawdown(comparable_nav_series: &[Decimal]) -> Option<Decimal> {
    if comparable_nav_series.is_empty() {
        return None;
    }
    let mut peak = Decimal::ZERO;
    let mut max_dd = Decimal::ZERO;
    for nav in comparable_nav_series {
        if *nav <= Decimal::ZERO {
            return None;
        }
        if *nav > peak {
            peak = *nav;
        }
        let dd = (Decimal::ONE - (*nav / peak)).max(Decimal::ZERO);
        if dd > max_dd {
            max_dd = dd;
        }
    }
    Some(
        max_dd
            .round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero)
            .normalize(),
    )
}
