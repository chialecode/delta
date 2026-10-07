//! Period PnL and net-worth reporting (financial-engine §4).
//!
//! 区间损益 = 期末净资产 − 期初净资产 − 净外部资金流, with flows converted at
//! their own event-time rates. Missing inputs produce explicit quality flags.

use crate::error::DomainResult;
use crate::events::{Corrections, EconomicEvent};
use crate::ids::AccountId;
use crate::ledger::{FxResolver, LedgerEngine};
use crate::money::{checked_add, checked_div, checked_mul, checked_sub, CurrencyCode};
use crate::valuation::{
    extract_external_flows, flows_in_reporting, value_accounts, PriceSource, Quality,
    ValuationSummary,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// PnL breakdown for one scope and period.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeriodPnl {
    /// Start / end net worth in the reporting currency (`None` = cannot be
    /// valued completely at that boundary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_net_worth: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_net_worth: Option<Decimal>,
    /// Net external flow (in − out) converted at flow-time rates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_external_flow: Option<Decimal>,
    /// Total PnL: end − start − net flow (only when all three are known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pnl: Option<Decimal>,
    /// In-period realized PnL (end snapshot minus start snapshot), reported
    /// in the reporting currency at the period-end rate; `None` when any
    /// in-period disposal had unknown cost or a rate is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realized_pnl: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl: Option<Decimal>,
    /// In-period fee explanation total (never subtracted again).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fees_valued: Option<Decimal>,
    /// In-period dividends and interest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub income: Option<Decimal>,
    pub quality: Quality,
    pub missing_inputs: Vec<String>,
}

fn net_worth_of(
    engine: &LedgerEngine,
    at: DateTime<Utc>,
    prices: &dyn PriceSource,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
    in_scope: &dyn Fn(&AccountId) -> bool,
    missing: &mut Vec<String>,
) -> Option<Decimal> {
    let v: ValuationSummary = value_accounts(engine, at, prices, fx, reporting, in_scope).ok()?;
    missing.extend(v.missing_inputs.iter().cloned());
    v.total_market_value
}

/// Cumulative ledger aggregates of the scoped accounts in one snapshot,
/// converted to the reporting currency at `at` (`None` = not convertible).
struct Aggregates {
    realized: Option<Decimal>,
    unknown_cost_disposals: u64,
    fees: Option<Decimal>,
    income: Option<Decimal>,
}

fn accumulate(acc: &mut Option<Decimal>, v: Option<Decimal>) -> DomainResult<()> {
    *acc = match (*acc, v) {
        (Some(a), Some(b)) => Some(checked_add(a, b)?),
        _ => None,
    };
    Ok(())
}

fn aggregates(
    engine: &LedgerEngine,
    at: DateTime<Utc>,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
    in_scope: &dyn Fn(&AccountId) -> bool,
) -> DomainResult<Aggregates> {
    let book = &engine.book_currency;
    let mut out = Aggregates {
        realized: Some(Decimal::ZERO),
        unknown_cost_disposals: 0,
        fees: Some(Decimal::ZERO),
        income: Some(Decimal::ZERO),
    };
    for (account_id, ledger) in &engine.accounts {
        if !in_scope(account_id) {
            continue;
        }
        for holding in ledger.holdings.values() {
            out.unknown_cost_disposals += holding.unknown_cost_disposals;
            if let Some(r) = holding.realized_pnl {
                // Realized PnL is kept in the holding's cost currency.
                let converted = holding
                    .cost_currency
                    .as_ref()
                    .and_then(|ccy| to_rep(r, ccy, at, fx, reporting));
                accumulate(&mut out.realized, converted)?;
            }
            // Fee-payment disposal differences are kept in book currency.
            if let Some(r) = holding.fee_disposal_pnl {
                accumulate(&mut out.realized, to_rep(r, book, at, fx, reporting))?;
            }
        }
        // Fee totals are explanation items (already embedded in cost,
        // proceeds or disposal differences) and are never subtracted again.
        // Valued fees are kept in book currency, trade fees in their asset.
        for f in ledger.fees.valued_fees.values() {
            accumulate(&mut out.fees, to_rep(*f, book, at, fx, reporting))?;
        }
        for (asset_key, f) in &ledger.fees.trade_fees {
            let ccy = crate::ledger::currency_of(asset_key);
            accumulate(&mut out.fees, to_rep(*f, &ccy, at, fx, reporting))?;
        }
        for (asset_key, d) in ledger
            .income
            .dividends
            .iter()
            .chain(ledger.income.interest.iter())
        {
            let ccy = crate::ledger::currency_of(asset_key);
            accumulate(&mut out.income, to_rep(*d, &ccy, at, fx, reporting))?;
        }
    }
    Ok(out)
}

fn period_delta(end: Option<Decimal>, start: Option<Decimal>) -> DomainResult<Option<Decimal>> {
    match (end, start) {
        (Some(e), Some(s)) => Ok(Some(checked_sub(e, s)?)),
        _ => Ok(None),
    }
}

/// Compute the period PnL for a scope. `engine_start`/`engine_end` are ledger
/// snapshots rebuilt as of `start_at` and `end_at` (each engine only contains
/// events before its boundary); `period_events` are the effective events with
/// `start_at <= occurred_at < end_at`, used for external flows.
///
/// Only `scope_accounts` enter net worth, flows and aggregates. Realized,
/// fee and income figures are the in-period change (end snapshot minus start
/// snapshot), both sides converted at the period-end rate.
#[allow(clippy::too_many_arguments)]
pub fn period_pnl(
    engine_start: &LedgerEngine,
    engine_end: &LedgerEngine,
    period_events: &[EconomicEvent],
    scope_accounts: &[AccountId],
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
    prices: &dyn PriceSource,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> DomainResult<PeriodPnl> {
    let in_scope = |a: &AccountId| scope_accounts.iter().any(|s| s == a);
    let mut missing: Vec<String> = Vec::new();
    let start_net_worth = net_worth_of(
        engine_start,
        start_at,
        prices,
        fx,
        reporting,
        &in_scope,
        &mut missing,
    );
    let end_net_worth = net_worth_of(
        engine_end,
        end_at,
        prices,
        fx,
        reporting,
        &in_scope,
        &mut missing,
    );

    let flows = extract_external_flows(period_events, &in_scope);
    let net_external_flow = flows_in_reporting(&flows, fx, reporting)?;
    if net_external_flow.is_none() {
        missing.push("fx for external flows".into());
    }

    let agg_start = aggregates(engine_start, end_at, fx, reporting, &in_scope)?;
    let agg_end = aggregates(engine_end, end_at, fx, reporting, &in_scope)?;
    let mut realized = period_delta(agg_end.realized, agg_start.realized)?;
    if agg_end.unknown_cost_disposals > agg_start.unknown_cost_disposals {
        // A disposal touching unknown-cost lots makes the realized total
        // incomplete even if other disposals have known cost.
        realized = None;
        missing.push("unknown cost: realized PnL incomplete".into());
    } else if realized.is_none() {
        missing.push("fx for realized PnL".into());
    }
    let fees = period_delta(agg_end.fees, agg_start.fees)?;
    if fees.is_none() {
        missing.push("fx for fees".into());
    }
    let income = period_delta(agg_end.income, agg_start.income)?;
    if income.is_none() {
        missing.push("fx for income".into());
    }

    // Unrealized PnL from the end-of-period valuation.
    let end_valuation = value_accounts(engine_end, end_at, prices, fx, reporting, &in_scope)?;
    let mut unrealized = Decimal::ZERO;
    let mut unrealized_known = true;
    for vp in end_valuation
        .positions
        .iter()
        .chain(end_valuation.cash.iter())
    {
        match vp.unrealized_pnl {
            Some(u) => unrealized = checked_add(unrealized, u)?,
            None if vp.cost_currency.is_some() || vp.carrying_cost.is_some() => {
                // Unknown cost, or known cost without price/FX: not computable.
                unrealized_known = false;
                if vp.carrying_cost.is_none() {
                    missing.push(format!("{}: unknown cost, unrealized incomplete", vp.asset));
                }
            }
            None => {}
        }
    }
    missing.extend(end_valuation.missing_inputs.iter().cloned());
    missing.sort();
    missing.dedup();

    let total_pnl = match (end_net_worth, start_net_worth, net_external_flow) {
        (Some(e), Some(s), Some(f)) => Some(checked_sub(checked_sub(e, s)?, f)?),
        _ => None,
    };

    let complete = start_net_worth.is_some()
        && end_net_worth.is_some()
        && net_external_flow.is_some()
        && realized.is_some()
        && fees.is_some()
        && income.is_some()
        && unrealized_known;
    let quality = if complete {
        Quality::Complete
    } else if total_pnl.is_some() {
        Quality::Partial
    } else {
        Quality::Unavailable
    };

    Ok(PeriodPnl {
        start_net_worth,
        end_net_worth,
        net_external_flow,
        total_pnl,
        realized_pnl: realized,
        unrealized_pnl: if unrealized_known {
            Some(unrealized)
        } else {
            None
        },
        fees_valued: fees,
        income,
        quality,
        missing_inputs: missing,
    })
}

fn to_rep(
    v: Decimal,
    native: &CurrencyCode,
    at: DateTime<Utc>,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> Option<Decimal> {
    if native == reporting {
        return Some(v);
    }
    let (rate, _) = fx.rate(native, reporting, at)?;
    checked_mul(v, rate).ok()
}

/// Guard against reading event payloads after the period boundary in callers
/// that rebuild engines for a bounded window.
pub fn events_within(events: &[EconomicEvent], end_at: DateTime<Utc>) -> Vec<EconomicEvent> {
    events
        .iter()
        .filter(|e| e.occurred_at < end_at)
        .cloned()
        .collect()
}

/// Resolve effective events once for report builders.
pub fn effective(recorded: &[EconomicEvent]) -> Vec<EconomicEvent> {
    Corrections::resolve(recorded).effective_events(recorded)
}

/// Basic peak-to-trough drawdown on a comparable net-asset-value series.
/// External flows must already have been removed; otherwise the result is
/// explicitly not an investment drawdown (financial-engine §5.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawdownReport {
    pub comparable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_drawdown: Option<Decimal>,
    pub label: String,
}

pub fn basic_drawdown(nav: &[Decimal], flows_removed: bool) -> DrawdownReport {
    if !flows_removed {
        return DrawdownReport {
            comparable: false,
            max_drawdown: None,
            label: "not an investment drawdown; external flows were not removed".into(),
        };
    }
    if nav.len() < 2 || nav.iter().any(|n| *n <= Decimal::ZERO) {
        return DrawdownReport {
            comparable: false,
            max_drawdown: None,
            label: "not comparable; need at least two positive net-asset values".into(),
        };
    }
    let mut peak = nav[0];
    let mut max_dd = Decimal::ZERO;
    for n in nav.iter().skip(1) {
        if *n > peak {
            peak = *n;
        }
        let dd = checked_div(checked_sub(peak, *n).unwrap_or(Decimal::ZERO), peak)
            .unwrap_or(Decimal::ZERO);
        if dd > max_dd {
            max_dd = dd;
        }
    }
    DrawdownReport {
        comparable: true,
        max_drawdown: Some(max_dd),
        label: "investment_drawdown".into(),
    }
}

/// Net deposits + openings for a scope (used by "净投入" reporting).
pub fn net_invested(
    events: &[EconomicEvent],
    scope_accounts: &[AccountId],
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> DomainResult<Option<Decimal>> {
    let flows = extract_external_flows(events, &|a: &AccountId| {
        scope_accounts.iter().any(|s| s == a)
    });
    flows_in_reporting(&flows, fx, reporting)
}
