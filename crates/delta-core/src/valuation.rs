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

pub fn normalize_cash_currency_code(symbol_or_asset_id: &str) -> String {
    symbol_or_asset_id
        .trim_start_matches("ASSET:FIAT:")
        .trim_start_matches("ASSET:STABLE:")
        .to_ascii_uppercase()
}

pub fn is_cash_or_stablecoin_currency(symbol_or_asset_id: &str) -> bool {
    let code = normalize_cash_currency_code(symbol_or_asset_id);
    is_fiat_currency(&code) || matches!(code.as_str(), "USDT" | "USDC")
}

fn lookup_pair_single_date(
    base: &str,
    quote: &str,
    d: NaiveDate,
    rates_by_pair_date: &HashMap<(String, String, NaiveDate), Decimal>,
) -> Option<Decimal> {
    if let Some(r) = rates_by_pair_date.get(&(base.to_string(), quote.to_string(), d)) {
        return (*r > Decimal::ZERO).then_some(*r);
    }
    // Same-version inverse rate (M1-W02 requirement)
    if let Some(inv) = rates_by_pair_date.get(&(quote.to_string(), base.to_string(), d)) {
        if *inv > Decimal::ZERO {
            return Some(
                (Decimal::ONE / *inv)
                    .round_dp_with_strategy(10, RoundingStrategy::MidpointAwayFromZero)
                    .normalize(),
            );
        }
    }
    None
}

fn is_non_fiat_pair(from_ccy: &str, to_ccy: &str) -> bool {
    !is_fiat_currency(from_ccy) || !is_fiat_currency(to_ccy)
}

fn merge_quality(a: QualityStatus, b: QualityStatus) -> QualityStatus {
    let rank = |q: QualityStatus| match q {
        QualityStatus::Exact => 0,
        QualityStatus::CarriedForward => 1,
        QualityStatus::Warning => 2,
        QualityStatus::Partial => 3,
        QualityStatus::Unavailable => 4,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

fn standalone_fees_in_scope(
    projection: &LedgerProjectionResult,
    scope_set: &BTreeSet<String>,
    book_fx: Decimal,
) -> Decimal {
    projection
        .transactions
        .iter()
        .flat_map(|tx| &tx.postings)
        .filter(|p| {
            p.category == LedgerCategory::FeeExpense
                && p.ledger_account_id
                    .strip_prefix("L:")
                    .and_then(|rest| rest.rsplit_once(':'))
                    .is_some_and(|(acct, _)| scope_set.contains(acct))
        })
        .map(|p| p.book_amount * book_fx)
        .sum()
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

    if let Some(rate) = lookup_pair_single_date(&base, &quote, target_date, rates_by_pair_date) {
        return Some(FxQuoteLookup {
            rate,
            source_date: target_date,
            quality: QualityStatus::Exact,
        });
    }

    // Exact same-day USD triangulation before any carry-forward.
    if base != "USD" && quote != "USD" {
        if let (Some(first), Some(second)) = (
            lookup_pair_single_date(&base, "USD", target_date, rates_by_pair_date),
            lookup_pair_single_date("USD", &quote, target_date, rates_by_pair_date),
        ) {
            if let Some(rate) = first.checked_mul(second) {
                return Some(FxQuoteLookup {
                    rate: crate::decimal::canon_decimal(rate),
                    source_date: target_date,
                    quality: QualityStatus::Exact,
                });
            }
        }
    }

    // For non-fiat pairs (crypto / stablecoins trade 24x7), do NOT carry forward direct pair;
    // for fiat pairs, carry forward up to 4 calendar days (S-07).
    let direct_cf = if !is_stablecoin_pair {
        (1..=4).find_map(|days_back| {
            let prev_date = target_date.checked_sub_days(chrono::Days::new(days_back))?;
            let rate = lookup_pair_single_date(&base, &quote, prev_date, rates_by_pair_date)?;
            Some(FxQuoteLookup {
                rate,
                source_date: prev_date,
                quality: QualityStatus::CarriedForward,
            })
        })
    } else {
        None
    };

    // Triangulated carry-forward via USD (non-fiat legs remain exact-date only; fiat legs can
    // carry forward unless caller explicitly suppressed carry-forward on a pure-fiat pair).
    let triangulated_cf = if base != "USD" && quote != "USD" {
        let base_no_cf =
            !is_fiat_currency(&base) || (is_stablecoin_pair && is_fiat_currency(&quote));
        let quote_no_cf =
            !is_fiat_currency(&quote) || (is_stablecoin_pair && is_fiat_currency(&base));
        let first = resolve_fx_rate_s07(&base, "USD", target_date, base_no_cf, rates_by_pair_date);
        let second =
            resolve_fx_rate_s07("USD", &quote, target_date, quote_no_cf, rates_by_pair_date);
        match (first, second) {
            (Some(first), Some(second)) => {
                first
                    .rate
                    .checked_mul(second.rate)
                    .map(|rate| FxQuoteLookup {
                        rate: crate::decimal::canon_decimal(rate),
                        source_date: first.source_date.min(second.source_date),
                        quality: if first.quality == QualityStatus::CarriedForward
                            || second.quality == QualityStatus::CarriedForward
                        {
                            QualityStatus::CarriedForward
                        } else {
                            QualityStatus::Exact
                        },
                    })
            }
            _ => None,
        }
    } else {
        None
    };

    match (direct_cf, triangulated_cf) {
        (Some(direct), Some(tri)) => {
            if direct.source_date >= tri.source_date {
                Some(direct)
            } else {
                Some(tri)
            }
        }
        (Some(direct), None) => Some(direct),
        (None, Some(tri)) => Some(tri),
        (None, None) => None,
    }
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

    let resolve_fx_at = |from_ccy: &str,
                         date: NaiveDate,
                         quality: &mut QualityStatus,
                         carried_forward_fx: &mut Vec<(String, NaiveDate, NaiveDate)>,
                         missing_inputs: &mut Vec<String>,
                         missing_prefix: &str|
     -> Option<Decimal> {
        let non_fiat = is_non_fiat_pair(from_ccy, report_currency);
        match resolve_fx_rate_s07(from_ccy, report_currency, date, non_fiat, fx_rates) {
            Some(found) => {
                if found.quality == QualityStatus::CarriedForward {
                    *quality = merge_quality(*quality, QualityStatus::CarriedForward);
                    let entry = (
                        format!("{from_ccy}/{report_currency}"),
                        date,
                        found.source_date,
                    );
                    if !carried_forward_fx.contains(&entry) {
                        carried_forward_fx.push(entry);
                    }
                }
                Some(found.rate)
            }
            None => {
                *quality = merge_quality(*quality, QualityStatus::Partial);
                let item = format!("{missing_prefix}:{from_ccy}/{report_currency}@{date}");
                if !missing_inputs.contains(&item) {
                    missing_inputs.push(item);
                }
                None
            }
        }
    };
    let resolve_fx = |from_ccy: &str,
                      quality: &mut QualityStatus,
                      carried_forward_fx: &mut Vec<(String, NaiveDate, NaiveDate)>,
                      missing_inputs: &mut Vec<String>|
     -> Option<Decimal> {
        resolve_fx_at(
            from_ccy,
            as_of_date,
            quality,
            carried_forward_fx,
            missing_inputs,
            "fx",
        )
    };

    // Resolve book_currency -> report_currency once for this valuation point to avoid repeated
    // lookups and duplicate `carried_forward_fx` / `missing_inputs` entries.
    let book_fx_opt = resolve_fx(
        &projection.book_currency,
        &mut quality,
        &mut carried_forward_fx,
        &mut missing_inputs,
    );

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
            quality = merge_quality(quality, QualityStatus::Partial);
            missing_inputs.push(format!("cost_basis:{acct}:{asset}"));
        } else if let Some(book_to_rep_fx) = book_fx_opt {
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
                quality = merge_quality(quality, QualityStatus::Partial);
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
            quality = merge_quality(quality, QualityStatus::Partial);
            missing_inputs.push(format!("transit_cost:{}", transit.asset_id));
        }
        let transit_quote_ccy = if transit.is_cash {
            transit.quote_currency.as_str()
        } else {
            asset_quote_currencies
                .get(&transit.asset_id)
                .map(String::as_str)
                .unwrap_or(transit.quote_currency.as_str())
        };
        if let Some(rate) = resolve_fx(
            transit_quote_ccy,
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
                quality = merge_quality(quality, QualityStatus::Partial);
            }
        }
        if let Some(rate) = book_fx_opt {
            if transit.is_cash {
                transit_cash_cost += transit.carrying_cost * rate;
            } else if transit.cost_known {
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
                is_non_fiat_pair(currency, report_currency),
                fx_rates,
            );
            if let (Some(price), Some(rate)) = (price, rate) {
                if rate.quality == QualityStatus::CarriedForward {
                    quality = merge_quality(quality, QualityStatus::CarriedForward);
                    let entry = (
                        format!("{currency}/{report_currency}"),
                        date,
                        rate.source_date,
                    );
                    if !carried_forward_fx.contains(&entry) {
                        carried_forward_fx.push(entry);
                    }
                }
                net_external_cash_flow += flow.position_quantity * *price * rate.rate;
            } else {
                missing_inputs.push(format!("position_transfer_value:{asset}@{date}"));
                quality = merge_quality(quality, QualityStatus::Partial);
            }
        } else if report_currency.eq_ignore_ascii_case(&projection.book_currency) {
            net_external_cash_flow += flow.book_amount;
        } else if let Some(rate) = resolve_fx_at(
            &flow.native_currency,
            flow.occurred_at.date_naive(),
            &mut quality,
            &mut carried_forward_fx,
            &mut missing_inputs,
            "flow_fx",
        ) {
            net_external_cash_flow += flow.native_amount * rate;
        }
    }

    if !projection.pending_valuation_reasons.is_empty() {
        quality = merge_quality(quality, QualityStatus::Partial);
        for reason in &projection.pending_valuation_reasons {
            missing_inputs.push(reason.clone());
        }
    }

    let book_fx = book_fx_opt.unwrap_or(Decimal::ZERO);
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
            let standalone_fees = standalone_fees_in_scope(projection, &scope_set, book_fx);
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
                quality = merge_quality(quality, QualityStatus::Warning);
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
        if sp.v_end_before_next_flow < Decimal::ZERO {
            return Err(format!(
                "sub-period {idx} has negative ending value {}",
                sp.v_end_before_next_flow
            ));
        }
        let one_plus_r = sp.v_end_before_next_flow / sp.v_start_after_flow;
        compound = compound
            .checked_mul(one_plus_r)
            .ok_or_else(|| format!("TWR compound product overflow at sub-period {idx}"))?;
    }
    Ok(crate::decimal::canon_decimal(
        (compound - Decimal::ONE).round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero),
    ))
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

/// Compute MWR / XIRR using range scan + bisection refinement (Financial Engine Sec 5.2, AC-07, AC-18).
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
        let base = 1.0 + r;
        if base <= 0.0 {
            return f64::NAN;
        }
        flows
            .iter()
            .map(|(d, cf)| {
                let days = (*d - d0).num_days() as f64;
                cf / base.powf(days / 365.0)
            })
            .sum()
    };

    // Exact scan grid covering (-0.9999, 1000.0): log-spaced near -1, exact 0.05 steps across
    // [-0.95, 1.0] (hitting exact 0.0), exact 0.25 steps across (1.0, 10.0], and log extension
    // up to 1000.0 for short-horizon portfolios.
    let mut scan_points = vec![-0.9999_f64, -0.999, -0.995, -0.99, -0.98];
    for step in -19..=20 {
        scan_points.push((step as f64) * 0.05);
    }
    for step in 5..=40 {
        scan_points.push((step as f64) * 0.25);
    }
    scan_points.extend_from_slice(&[12.0, 15.0, 20.0, 30.0, 50.0, 100.0, 250.0, 500.0, 1000.0]);

    let mut brackets = Vec::new();
    let values: Vec<f64> = scan_points.iter().map(|&r| npv(r)).collect();
    for i in 0..scan_points.len() {
        let a = scan_points[i];
        let fa = values[i];
        if !fa.is_finite() {
            continue;
        }
        if fa.abs() < 1e-11 {
            brackets.push((a, a));
            continue;
        }
        if i + 1 < scan_points.len() {
            let b = scan_points[i + 1];
            let fb = values[i + 1];
            if fb.is_finite() && fb.abs() >= 1e-11 && fa.signum() != fb.signum() {
                brackets.push((a, b));
            }
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
        if !flo.is_finite() {
            continue;
        }
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            let fmid = npv(mid);
            if !fmid.is_finite() {
                break;
            }
            if fmid.abs() < 1e-11 || (hi - lo).abs() < 1e-11 {
                lo = mid;
                break;
            }
            if flo.signum() != fmid.signum() {
                hi = mid;
            } else {
                lo = mid;
                flo = fmid;
            }
        }
        if let Some(dec) = Decimal::from_f64_retain(lo) {
            let rounded = crate::decimal::canon_decimal(
                dec.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero),
            );
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualValuation {
    pub id: String,
    pub account_id: String,
    pub asset_or_liability_id: String,
    pub is_liability: bool,
    pub value: Decimal,
    pub currency: String,
    pub as_of: NaiveDate,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodBreakdownReport {
    pub account_scope: Vec<String>,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub report_currency: String,
    pub calculation_version: String,
    pub opening_net_worth: Option<Decimal>,
    pub net_external_cash_flow: Option<Decimal>,
    pub investment_pnl: Option<Decimal>,
    pub ending_net_worth: Option<Decimal>,
    pub known_partial_net_worth: Decimal,
    pub excluded_items: Vec<String>,
    pub realized_pnl: Option<Decimal>,
    pub unrealized_pnl: Option<Decimal>,
    pub dividend_and_interest_income: Option<Decimal>,
    pub fee_expense: Option<Decimal>,
    pub explanatory_total_fees: Decimal,
    pub fx_impact: Option<Decimal>,
    pub unattributed_pnl: Option<Decimal>,
    pub unattributed_reasons: Vec<String>,
    pub max_drawdown: Option<Decimal>,
    pub drawdown_status: String,
    pub manual_valuations: Vec<ManualValuation>,
    pub quality: QualityStatus,
    pub missing_inputs: Vec<String>,
    pub warnings: Vec<String>,
}

/// Build a scoped period P&L breakdown from `start_report` (opening before `start_date`) and `end_report` (`end_date`),
/// incorporating manual asset/liability valuations (`val.as_of <= date`) and comparable NAV drawdown series (`M2-W02`).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_period_breakdown(
    account_scope: &[String],
    start_date: NaiveDate,
    end_date: NaiveDate,
    report_currency: &str,
    start_valuation: &ValuationReport,
    end_valuation: &ValuationReport,
    start_projection: &LedgerProjectionResult,
    end_projection: &LedgerProjectionResult,
    fx_rates: &HashMap<(String, String, NaiveDate), Decimal>,
    manual_valuations: &[ManualValuation],
    comparable_nav_series: &[Decimal],
) -> PeriodBreakdownReport {
    let scope_set: BTreeSet<String> = account_scope.iter().cloned().collect();
    let scope_vec: Vec<String> = scope_set.iter().cloned().collect();

    let mut missing_inputs = end_valuation.missing_inputs.clone();
    for m in &start_valuation.missing_inputs {
        if !missing_inputs.contains(m) {
            missing_inputs.push(m.clone());
        }
    }
    let mut warnings = end_valuation.warnings.clone();
    for w in &start_valuation.warnings {
        if !warnings.contains(w) {
            warnings.push(w.clone());
        }
    }
    let mut quality = merge_quality(start_valuation.quality, end_valuation.quality);

    // Apply latest manual valuations per (account_id, asset_or_liability_id) whose as_of <= target_date
    let resolve_manual_net = |target_date: NaiveDate,
                              missing: &mut Vec<String>,
                              qual: &mut QualityStatus|
     -> (Decimal, Vec<ManualValuation>) {
        let mut latest_by_key: BTreeMap<(String, String), &ManualValuation> = BTreeMap::new();
        for mv in manual_valuations {
            if !scope_set.contains(&mv.account_id) || mv.as_of > target_date {
                continue;
            }
            let key = (mv.account_id.clone(), mv.asset_or_liability_id.clone());
            if latest_by_key
                .get(&key)
                .is_none_or(|prev| mv.as_of >= prev.as_of)
            {
                latest_by_key.insert(key, mv);
            }
        }
        let mut net = Decimal::ZERO;
        let mut used = Vec::new();
        for mv in latest_by_key.values() {
            let non_fiat = is_non_fiat_pair(&mv.currency, report_currency);
            match resolve_fx_rate_s07(
                &mv.currency,
                report_currency,
                target_date,
                non_fiat,
                fx_rates,
            ) {
                Some(found) => {
                    if found.quality == QualityStatus::CarriedForward {
                        *qual = merge_quality(*qual, QualityStatus::CarriedForward);
                    }
                    let converted = (mv.value.abs() * found.rate).normalize();
                    if mv.is_liability {
                        net -= converted;
                    } else {
                        net += converted;
                    }
                    used.push((*mv).clone());
                }
                None => {
                    *qual = merge_quality(*qual, QualityStatus::Partial);
                    let item = format!(
                        "manual_valuation_fx:{}:{}/{}@{}",
                        mv.asset_or_liability_id, mv.currency, report_currency, target_date
                    );
                    if !missing.contains(&item) {
                        missing.push(item);
                    }
                }
            }
        }
        (net.normalize(), used)
    };

    let opening_date = start_date.pred_opt().unwrap_or(start_date);
    let (start_manual_net, _) = resolve_manual_net(opening_date, &mut missing_inputs, &mut quality);
    let (end_manual_net, used_manual) =
        resolve_manual_net(end_date, &mut missing_inputs, &mut quality);

    let known_opening = (start_valuation.net_worth + start_manual_net).normalize();
    let known_ending = (end_valuation.net_worth + end_manual_net).normalize();
    let excluded_items = missing_inputs.clone();

    let explanatory_total_fees = (end_valuation.fee_summary_explanatory
        - start_valuation.fee_summary_explanatory)
        .max(Decimal::ZERO)
        .normalize();

    if !missing_inputs.is_empty() {
        return PeriodBreakdownReport {
            account_scope: scope_vec,
            start_date,
            end_date,
            report_currency: report_currency.to_string(),
            calculation_version: "m2-ledger-1".to_string(),
            opening_net_worth: None,
            net_external_cash_flow: None,
            investment_pnl: None,
            ending_net_worth: None,
            known_partial_net_worth: known_ending,
            excluded_items,
            realized_pnl: None,
            unrealized_pnl: None,
            dividend_and_interest_income: None,
            fee_expense: None,
            explanatory_total_fees,
            fx_impact: None,
            unattributed_pnl: None,
            unattributed_reasons: vec![
                "incomplete valuation inputs or unknown cost basis".to_string()
            ],
            max_drawdown: None,
            drawdown_status: "unavailable: missing valuation or cost inputs".to_string(),
            manual_valuations: used_manual,
            quality: QualityStatus::Partial,
            missing_inputs,
            warnings,
        };
    }

    let net_flow =
        (end_valuation.net_external_cash_flow - start_valuation.net_external_cash_flow).normalize();
    let inv_pnl = (known_ending - known_opening - net_flow).normalize();

    let book_fx_start = resolve_fx_rate_s07(
        &start_projection.book_currency,
        report_currency,
        start_valuation.as_of_date,
        is_non_fiat_pair(&start_projection.book_currency, report_currency),
        fx_rates,
    )
    .map(|r| r.rate)
    .unwrap_or(Decimal::ONE);
    let book_fx_end = resolve_fx_rate_s07(
        &end_projection.book_currency,
        report_currency,
        end_date,
        is_non_fiat_pair(&end_projection.book_currency, report_currency),
        fx_rates,
    )
    .map(|r| r.rate)
    .unwrap_or(Decimal::ONE);

    let realized_fx_start: Decimal = start_projection
        .realized_fx_pnl_by_account
        .iter()
        .filter(|(a, _)| scope_set.contains(*a))
        .map(|(_, v)| *v * book_fx_start)
        .sum();
    let realized_fx_end: Decimal = end_projection
        .realized_fx_pnl_by_account
        .iter()
        .filter(|(a, _)| scope_set.contains(*a))
        .map(|(_, v)| *v * book_fx_end)
        .sum();
    let period_realized_fx = (realized_fx_end - realized_fx_start).normalize();

    let total_realized_delta = (end_valuation.realized_pnl.unwrap_or_default()
        - start_valuation.realized_pnl.unwrap_or_default())
    .normalize();
    let trading_realized = (total_realized_delta - period_realized_fx).normalize();

    let unrealized_delta = (end_valuation.unrealized_pnl.unwrap_or_default()
        - start_valuation.unrealized_pnl.unwrap_or_default()
        + (end_manual_net - start_manual_net))
        .normalize();

    let div_delta = (end_valuation.dividend_income - start_valuation.dividend_income).normalize();

    let fee_delta = (standalone_fees_in_scope(end_projection, &scope_set, book_fx_end)
        - standalone_fees_in_scope(start_projection, &scope_set, book_fx_start))
    .normalize();

    let translation_delta = (end_valuation
        .scope_and_currency_effect_pnl
        .unwrap_or_default()
        - start_valuation
            .scope_and_currency_effect_pnl
            .unwrap_or_default())
    .normalize();
    let fx_impact = (period_realized_fx + translation_delta).normalize();

    let attributed =
        (trading_realized + unrealized_delta + div_delta - fee_delta + fx_impact).normalize();
    let residual = (inv_pnl - attributed).normalize();
    let mut unattributed_reasons = Vec::new();
    if !residual.is_zero() {
        unattributed_reasons
            .push("cross-period multi-currency flow timing or manual valuation shift".to_string());
    }

    let (max_drawdown, drawdown_status) = if comparable_nav_series.is_empty() {
        (
            None,
            "daily_approximation: single-point or flat period".to_string(),
        )
    } else {
        match compute_max_drawdown(comparable_nav_series) {
            Some(dd) => (Some(dd), "daily_approximation".to_string()),
            None => (
                None,
                "unavailable: non-positive comparable NAV in period".to_string(),
            ),
        }
    };

    PeriodBreakdownReport {
        account_scope: scope_vec,
        start_date,
        end_date,
        report_currency: report_currency.to_string(),
        calculation_version: "m2-ledger-1".to_string(),
        opening_net_worth: Some(known_opening),
        net_external_cash_flow: Some(net_flow),
        investment_pnl: Some(inv_pnl),
        ending_net_worth: Some(known_ending),
        known_partial_net_worth: known_ending,
        excluded_items,
        realized_pnl: Some(trading_realized),
        unrealized_pnl: Some(unrealized_delta),
        dividend_and_interest_income: Some(div_delta),
        fee_expense: Some(-fee_delta),
        explanatory_total_fees,
        fx_impact: Some(fx_impact),
        unattributed_pnl: Some(residual),
        unattributed_reasons,
        max_drawdown,
        drawdown_status,
        manual_valuations: used_manual,
        quality,
        missing_inputs,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::parse_decimal_lossless;
    use crate::ledger::{
        replay_ledger_events, EconomicEventType, InTransitAsset, LedgerEventInput,
    };
    use chrono::TimeZone;

    fn dec(s: &str) -> Decimal {
        parse_decimal_lossless(s).unwrap()
    }

    #[test]
    fn non_fiat_fx_never_carries_forward_while_direct_fiat_prefers_freshest_date() {
        let wed = NaiveDate::from_ymd_opt(2026, 1, 7).unwrap();
        let thu = NaiveDate::from_ymd_opt(2026, 1, 8).unwrap();
        let fri = NaiveDate::from_ymd_opt(2026, 1, 9).unwrap();
        let sat = NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();

        let mut fx = HashMap::new();
        fx.insert(("BTC".into(), "USD".into(), fri), dec("50000"));
        fx.insert(("DAI".into(), "USD".into(), fri), dec("1.001"));
        fx.insert(("USDT".into(), "USD".into(), sat), dec("0.9995"));
        fx.insert(("USD".into(), "HKD".into(), fri), dec("7.8"));
        fx.insert(("EUR".into(), "HKD".into(), fri), dec("8.5"));
        fx.insert(("EUR".into(), "USD".into(), thu), dec("1.05"));
        fx.insert(("USD".into(), "HKD".into(), wed), dec("7.7"));

        // Non-fiat BTC & DAI must NOT carry forward onto Saturday.
        assert!(resolve_fx_rate_s07("BTC", "USD", sat, true, &fx).is_none());
        assert!(resolve_fx_rate_s07("DAI", "USD", sat, true, &fx).is_none());

        // USDT/HKD on Saturday: exact Saturday USDT/USD * carried-forward Friday USD/HKD.
        let usdt_hkd = resolve_fx_rate_s07("USDT", "HKD", sat, true, &fx).unwrap();
        assert_eq!(usdt_hkd.quality, QualityStatus::CarriedForward);
        assert_eq!(usdt_hkd.source_date, fri);
        assert_eq!(usdt_hkd.rate, dec("7.7961"));

        // EUR/HKD on Saturday: prefers fresher Friday direct EUR/HKD (8.5) over stale Thu/Wed USD triangulation.
        let eur_hkd = resolve_fx_rate_s07("EUR", "HKD", sat, false, &fx).unwrap();
        assert_eq!(eur_hkd.quality, QualityStatus::CarriedForward);
        assert_eq!(eur_hkd.source_date, fri);
        assert_eq!(eur_hkd.rate, dec("8.5"));
    }

    #[test]
    fn evaluate_portfolio_marks_unknown_transit_partial_and_blocks_crypto_carry_forward() {
        let fri = NaiveDate::from_ymd_opt(2026, 1, 9).unwrap();
        let sat = NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
        let dep = LedgerEventInput {
            event_id: "E1".into(),
            account_id: "ACC-1".into(),
            event_type: EconomicEventType::Deposit,
            occurred_at: chrono::Utc.with_ymd_and_hms(2026, 1, 9, 10, 0, 0).unwrap(),
            sequence_no: 1,
            asset_id: "USD".into(),
            instrument_id: None,
            quantity: dec("1000"),
            price: None,
            quote_currency: "USD".into(),
            fee_amount: Decimal::ZERO,
            fee_currency: "USD".into(),
            cost_known: true,
            counterparty_account_id: None,
            transfer_group_id: None,
            correction_group_id: None,
            revision: 1,
            active_in_correction_group: true,
            source_ref: None,
        };
        let mut proj = replay_ledger_events("USD", &[dep], &HashMap::new(), 1).unwrap();
        // Inject unknown-cost in-transit position between two in-scope accounts.
        proj.in_transit_assets.push(InTransitAsset {
            source_account: "ACC-1".into(),
            destination_account: "ACC-2".into(),
            asset_id: "AAPL".into(),
            quote_currency: "USD".into(),
            quantity: dec("2"),
            carrying_cost: Decimal::ZERO,
            cost_known: false,
            is_cash: false,
        });
        let mut prices = HashMap::new();
        prices.insert(("AAPL".to_string(), sat), dec("150"));
        let rep = evaluate_portfolio(
            &proj,
            &["ACC-1".into(), "ACC-2".into()],
            "USD",
            sat,
            &prices,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(rep.quality, QualityStatus::Partial);
        assert!(rep.realized_pnl.is_none());
        assert!(rep.missing_inputs.iter().any(|m| m == "transit_cost:AAPL"));

        // Non-fiat DAI cash must not carry forward Friday FX onto Saturday.
        let mut proj_dai = proj;
        proj_dai.in_transit_assets.clear();
        proj_dai
            .cash_balances
            .insert(("ACC-1".into(), "DAI".into()), dec("100"));
        let mut fx = HashMap::new();
        fx.insert(("DAI".into(), "USD".into(), fri), dec("1"));
        let rep_dai = evaluate_portfolio(
            &proj_dai,
            &["ACC-1".into()],
            "USD",
            sat,
            &HashMap::new(),
            &HashMap::new(),
            &fx,
            &HashMap::new(),
        );
        assert_eq!(rep_dai.quality, QualityStatus::Partial);
        assert!(rep_dai
            .missing_inputs
            .iter()
            .any(|m| m.starts_with("fx:DAI/USD@")));
    }

    #[test]
    fn compute_twr_rejects_negative_ending_nav_and_xirr_handles_short_horizon() {
        let err = compute_twr(&[
            SubPeriodSnapshot {
                v_start_after_flow: dec("100"),
                v_end_before_next_flow: dec("-50"),
            },
            SubPeriodSnapshot {
                v_start_after_flow: dec("100"),
                v_end_before_next_flow: dec("-50"),
            },
        ])
        .unwrap_err();
        assert!(err.contains("negative ending value"));

        // Short-horizon gain (+10% over 7 days -> annualized > 1000%, previously NoRoot)
        let d0 = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let d1 = NaiveDate::from_ymd_opt(2026, 1, 8).unwrap();
        let xirr = compute_xirr(&[(d0, dec("-1000")), (d1, dec("1100"))]);
        assert!(matches!(xirr, XirrStatus::Converged { rate, .. } if rate > dec("10")));

        // Exact r = 10.0 endpoint (11x in 1 year)
        let d_yr = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        let xirr_10 = compute_xirr(&[(d0, dec("-100")), (d_yr, dec("1100"))]);
        assert!(matches!(xirr_10, XirrStatus::Converged { rate, .. } if rate == dec("10")));
    }
}
