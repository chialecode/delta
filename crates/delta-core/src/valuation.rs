//! Valuation: market values, unrealized PnL and quality reporting.
//!
//! Missing prices/FX produce explicit `None` + quality flags — never zero.
//! Valuation never reads prices after the target time (visible_until rule).

use crate::error::DomainResult;
use crate::events::{EconomicEvent, EventPayload, TransferKind};
use crate::ids::{AccountId, AssetId};
use crate::ledger::{AccountLedger, FxResolver, HoldingState, LedgerEngine};
use crate::money::{checked_add, checked_mul, checked_sub, CurrencyCode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A price observation for one asset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceQuote {
    #[serde(with = "crate::money::decimal_string")]
    pub price: Decimal,
    pub currency: CurrencyCode,
    pub as_of: DateTime<Utc>,
    /// Source flagged staleness (market open but data missing, etc.).
    pub stale: bool,
}

/// Price lookup bounded by the valuation time.
pub trait PriceSource {
    /// Latest quote for `asset` with `as_of <= at`; `None` when unavailable.
    fn price(&self, asset: &AssetId, at: DateTime<Utc>) -> Option<PriceQuote>;
}

/// Map-backed price source for tests and demo data.
#[derive(Debug, Clone, Default)]
pub struct PriceMap {
    pub quotes: BTreeMap<AssetId, Vec<PriceQuote>>,
}

impl PriceMap {
    pub fn insert(&mut self, asset: AssetId, quote: PriceQuote) {
        self.quotes.entry(asset).or_default().push(quote);
    }
}

impl PriceSource for PriceMap {
    fn price(&self, asset: &AssetId, at: DateTime<Utc>) -> Option<PriceQuote> {
        let quotes = self.quotes.get(asset)?;
        quotes
            .iter()
            .filter(|q| q.as_of <= at)
            .max_by_key(|q| q.as_of)
            .cloned()
    }
}

/// Valuation quality for a computed figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    #[default]
    Complete,
    Partial,
    Unavailable,
}

/// Valued position of one asset in one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValuedPosition {
    pub account_id: AccountId,
    pub asset: AssetId,
    #[serde(with = "crate::money::decimal_string")]
    pub quantity: Decimal,
    /// Carrying cost in its cost currency (`None` = unknown cost present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrying_cost: Option<Decimal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<CurrencyCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceQuote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fx_rate_to_reporting: Option<Decimal>,
    /// Market value in the reporting currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_value: Option<Decimal>,
    /// Unrealized PnL in the reporting currency (`None` when cost or price is
    /// unknown — unknown cost is never reported as zero PnL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrealized_pnl: Option<Decimal>,
    pub quality: Quality,
    pub warnings: Vec<String>,
    /// Inputs that were needed but unavailable (price, FX), listed so a
    /// partial result names exactly what is missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_inputs: Vec<String>,
}

/// Total valuation of a set of accounts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValuationSummary {
    pub positions: Vec<ValuedPosition>,
    pub cash: Vec<ValuedPosition>,
    /// Total market value in reporting currency (`None` if any component
    /// could not be valued).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_market_value: Option<Decimal>,
    /// Sum of the components that could be valued. Equals the total when
    /// complete; when partial it is the known part shown next to the
    /// missing inputs, never presented as the total.
    #[serde(with = "crate::money::decimal_string")]
    pub known_market_value: Decimal,
    pub quality: Quality,
    pub missing_inputs: Vec<String>,
}

/// Convert a native value to reporting currency with an FX resolver.
fn to_reporting(
    value: Decimal,
    native: &CurrencyCode,
    at: DateTime<Utc>,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> Option<Decimal> {
    if native == reporting {
        return Some(value);
    }
    let (rate, _) = fx.rate(native, reporting, at)?;
    checked_mul(value, rate).ok()
}

/// Value one holding asset in one account.
#[allow(clippy::too_many_arguments)]
fn value_position(
    account_id: &AccountId,
    asset: &AssetId,
    quantity: Decimal,
    holding: Option<&HoldingState>,
    quote: Option<PriceQuote>,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> ValuedPosition {
    let mut missing: Vec<String> = Vec::new();
    let carrying_cost = holding.and_then(|h| h.carrying_cost());
    let cost_ccy = holding.and_then(|h| h.cost_currency.clone());
    let (market_value, fx_rate) = match &quote {
        Some(q) => {
            let rate = if q.currency == *reporting {
                Some(Decimal::ONE)
            } else {
                match fx.rate(&q.currency, reporting, q.as_of) {
                    Some((r, _)) => Some(r),
                    None => {
                        missing.push(format!("fx {} -> {}", q.currency, reporting));
                        None
                    }
                }
            };
            match rate {
                Some(r) => (
                    checked_mul(quantity, q.price)
                        .ok()
                        .and_then(|native| checked_mul(native, r).ok()),
                    Some(r),
                ),
                None => (None, None),
            }
        }
        None => {
            missing.push(format!("price for {asset}"));
            (None, None)
        }
    };
    // Unrealized PnL: market value minus converted carrying cost. Cost is
    // converted at the valuation-time rate for the reporting view (S1 rule:
    // original-currency PnL stays available on the position row).
    let mut warnings: Vec<String> = Vec::new();
    let unrealized = match (&quote, carrying_cost, cost_ccy.clone()) {
        (Some(q), Some(cost), Some(ccy)) => {
            let cost_in_reporting = if ccy == *reporting {
                Some(cost)
            } else {
                to_reporting(cost, &ccy, q.as_of, fx, reporting)
            };
            match (market_value, cost_in_reporting) {
                (Some(mv), Some(c)) => checked_sub(mv, c).ok(),
                _ => {
                    missing.push("fx for cost basis".into());
                    None
                }
            }
        }
        (Some(_), None, _) if holding.is_some() => {
            warnings.push(format!(
                "{asset} has unknown cost; unrealized PnL not computed"
            ));
            None
        }
        _ => None,
    };
    // Cash rows (no holding) have no cost basis, so a missing unrealized
    // figure only degrades positions that carry cost.
    let quality = if market_value.is_none() {
        Quality::Unavailable
    } else if (holding.is_some() && unrealized.is_none()) || q_stale(&quote) || !missing.is_empty()
    {
        Quality::Partial
    } else {
        Quality::Complete
    };
    ValuedPosition {
        account_id: account_id.clone(),
        asset: asset.clone(),
        quantity,
        carrying_cost,
        cost_currency: cost_ccy,
        price: quote,
        fx_rate_to_reporting: fx_rate,
        market_value,
        unrealized_pnl: unrealized,
        quality,
        warnings,
        missing_inputs: missing,
    }
}

fn q_stale(q: &Option<PriceQuote>) -> bool {
    q.as_ref().is_some_and(|p| p.stale)
}

/// Value a whole engine workspace at `at` in `reporting` currency.
pub fn value_workspace(
    engine: &LedgerEngine,
    at: DateTime<Utc>,
    prices: &dyn PriceSource,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> DomainResult<ValuationSummary> {
    value_accounts(engine, at, prices, fx, reporting, &|_| true)
}

/// Value only the accounts accepted by `in_scope` (scope isolation: other
/// accounts in the same engine never enter the result).
pub fn value_accounts(
    engine: &LedgerEngine,
    at: DateTime<Utc>,
    prices: &dyn PriceSource,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
    in_scope: &dyn Fn(&AccountId) -> bool,
) -> DomainResult<ValuationSummary> {
    let mut positions = Vec::new();
    let mut cash_rows = Vec::new();
    let mut missing_inputs: Vec<String> = Vec::new();
    let mut total = Decimal::ZERO;
    let mut all_valued = true;

    for (account_id, ledger) in &engine.accounts {
        if !in_scope(account_id) {
            continue;
        }
        for (asset, holding) in &ledger.holdings {
            if holding.quantity() == Decimal::ZERO {
                // Closed position: nothing to value; realized results stay
                // on the holding. A missing price must not void the total.
                continue;
            }
            let quote = prices.price(asset, at);
            let vp = value_position(
                account_id,
                asset,
                holding.quantity(),
                Some(holding),
                quote,
                fx,
                reporting,
            );
            missing_inputs.extend(vp.missing_inputs.iter().cloned());
            if vp.market_value.is_none() {
                all_valued = false;
                missing_inputs.extend(vp.warnings.iter().cloned());
            } else {
                total = checked_add(total, vp.market_value.unwrap_or(Decimal::ZERO))?;
            }
            positions.push(vp);
        }
        for (asset, amount) in &ledger.cash {
            if *amount == Decimal::ZERO {
                continue;
            }
            // Cash in the reporting currency needs no price; other cash
            // (stablecoins, foreign currency) is valued via its own FX rate —
            // USDT is never assumed to equal USD (AC-06).
            let ccy = crate::ledger::currency_of(asset);
            let vp = if ccy == *reporting {
                ValuedPosition {
                    account_id: account_id.clone(),
                    asset: asset.clone(),
                    quantity: *amount,
                    carrying_cost: None,
                    cost_currency: None,
                    price: Some(PriceQuote {
                        price: Decimal::ONE,
                        currency: ccy.clone(),
                        as_of: at,
                        stale: false,
                    }),
                    fx_rate_to_reporting: Some(Decimal::ONE),
                    market_value: Some(*amount),
                    unrealized_pnl: Some(Decimal::ZERO),
                    quality: Quality::Complete,
                    warnings: Vec::new(),
                    missing_inputs: Vec::new(),
                }
            } else {
                let quote = prices.price(asset, at).or_else(|| {
                    // Without a price series for the cash asset, fall back to
                    // its FX rate at the valuation time.
                    fx.rate(&ccy, reporting, at).map(|(r, _)| PriceQuote {
                        price: r,
                        currency: reporting.clone(),
                        as_of: at,
                        stale: false,
                    })
                });
                value_position(account_id, asset, *amount, None, quote, fx, reporting)
            };
            if vp.market_value.is_none() {
                all_valued = false;
                missing_inputs.extend(vp.warnings.iter().cloned());
                missing_inputs.extend(vp.missing_inputs.iter().cloned());
                missing_inputs.push(format!("valuation for cash asset {asset}"));
            } else {
                total = checked_add(total, vp.market_value.unwrap_or(Decimal::ZERO))?;
            }
            cash_rows.push(vp);
        }
    }

    // In-transit quantity stays in the valuation only when both legs are in
    // scope. The source balance was already reduced, so omitting it would
    // drop the value until the inbound posts. It is priced at `at` like any
    // balance; the moved historical cost rides along (financial-engine §3).
    for item in engine.in_transit() {
        if !in_scope(&item.source_account) || !in_scope(&item.counterparty) {
            continue;
        }
        let book = &engine.book_currency;
        let ccy = crate::ledger::currency_of(&item.asset);
        let quote = if item.is_cash && ccy == *reporting {
            Some(PriceQuote {
                price: Decimal::ONE,
                currency: ccy.clone(),
                as_of: at,
                stale: false,
            })
        } else if item.is_cash {
            prices.price(&item.asset, at).or_else(|| {
                fx.rate(&ccy, reporting, at).map(|(r, _)| PriceQuote {
                    price: r,
                    currency: reporting.clone(),
                    as_of: at,
                    stale: false,
                })
            })
        } else {
            prices.price(&item.asset, at)
        };
        let mut row = value_position(
            &item.source_account,
            &item.asset,
            item.quantity,
            None,
            quote,
            fx,
            reporting,
        );
        if !item.is_cash {
            if item.cost_complete {
                row.carrying_cost = Some(item.book_amount);
                row.cost_currency = Some(book.clone());
                let cost = to_reporting(item.book_amount, book, at, fx, reporting);
                row.unrealized_pnl = match (row.market_value, cost) {
                    (Some(mv), Some(c)) => Some(checked_sub(mv, c)?),
                    _ => None,
                };
            } else {
                row.cost_currency = Some(book.clone());
                row.unrealized_pnl = None;
                if row.market_value.is_some() {
                    row.quality = Quality::Partial;
                }
                row.warnings.push(format!(
                    "{} in transit has unknown cost; unrealized PnL not computed",
                    item.asset
                ));
            }
        }
        row.warnings
            .push(format!("in transit {} until inbound posts", item.group));
        missing_inputs.extend(row.missing_inputs.iter().cloned());
        if row.market_value.is_none() {
            all_valued = false;
            missing_inputs.push(format!("valuation for in-transit {}", item.asset));
        } else {
            total = checked_add(total, row.market_value.unwrap_or(Decimal::ZERO))?;
        }
        if item.is_cash {
            cash_rows.push(row);
        } else {
            positions.push(row);
        }
    }

    let quality = if !all_valued {
        if total == Decimal::ZERO {
            Quality::Unavailable
        } else {
            Quality::Partial
        }
    } else {
        Quality::Complete
    };

    Ok(ValuationSummary {
        positions,
        cash: cash_rows,
        total_market_value: if all_valued { Some(total) } else { None },
        known_market_value: total,
        quality,
        missing_inputs,
    })
}

/// A concrete account ledger snapshot accessor used by PnL (avoids exposing
/// engine internals).
pub fn account_ledgers(engine: &LedgerEngine) -> &BTreeMap<AccountId, AccountLedger> {
    &engine.accounts
}

/// Equity contributions/withdrawals extracted from events for PnL flows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalFlows {
    /// (account, asset, signed amount in native currency, time).
    pub flows: Vec<(AccountId, AssetId, Decimal, DateTime<Utc>)>,
}

/// Extract external flows from effective events for a scope view.
/// Portfolio view: paired transfer principals are internal; single-account
/// view: transfers are external for the non-scope side and flows for both
/// sides when only one side is in scope (financial-engine §3, AC-04).
pub fn extract_external_flows(
    events: &[EconomicEvent],
    scope_accounts: &dyn Fn(&AccountId) -> bool,
) -> ExternalFlows {
    let mut flows = Vec::new();
    // Group transfers to decide pairing within scope.
    let mut transfer_pairs: std::collections::HashMap<String, Vec<&EconomicEvent>> =
        std::collections::HashMap::new();
    for e in events {
        if let EventPayload::Transfer { group, .. } = &e.payload {
            transfer_pairs.entry(group.0.clone()).or_default().push(e);
        }
    }
    for e in events {
        let in_scope = scope_accounts(&e.account_id);
        match &e.payload {
            EventPayload::CashDeposit { asset, amount } => {
                if in_scope {
                    flows.push((e.account_id.clone(), asset.clone(), *amount, e.occurred_at));
                }
            }
            EventPayload::CashWithdrawal { asset, amount } => {
                if in_scope {
                    flows.push((e.account_id.clone(), asset.clone(), -*amount, e.occurred_at));
                }
            }
            EventPayload::Transfer {
                kind,
                group,
                asset,
                principal,
                ..
            } => {
                let both_in_scope = transfer_pairs.get(&group.0).is_some_and(|legs| {
                    legs.len() >= 2 && legs.iter().all(|l| scope_accounts(&l.account_id))
                });
                if in_scope && !both_in_scope {
                    // Single-account view: principal crosses the scope boundary.
                    let sign = if *kind == TransferKind::In {
                        Decimal::ONE
                    } else {
                        -Decimal::ONE
                    };
                    flows.push((
                        e.account_id.clone(),
                        asset.clone(),
                        sign * *principal,
                        e.occurred_at,
                    ));
                }
            }
            _ => {}
        }
    }
    ExternalFlows { flows }
}

/// Convert flows to reporting currency at their own event times (financial-
/// engine §4: never re-fill with the period-end rate).
pub fn flows_in_reporting(
    flows: &ExternalFlows,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
) -> DomainResult<Option<Decimal>> {
    let mut total = Decimal::ZERO;
    for (_, asset, amount, at) in &flows.flows {
        let ccy = crate::ledger::currency_of(asset);
        let converted = match to_reporting(*amount, &ccy, *at, fx, reporting) {
            Some(v) => v,
            None => return Ok(None),
        };
        total = checked_add(total, converted)?;
    }
    Ok(Some(total))
}

/// A user-entered mark that is not implied by the market ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualKind {
    Asset,
    Liability,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManualMark {
    pub account_id: AccountId,
    pub asset: AssetId,
    /// Positive magnitude in `currency`.
    #[serde(with = "crate::money::decimal_string")]
    pub value: Decimal,
    pub currency: CurrencyCode,
    pub as_of: DateTime<Utc>,
    pub kind: ManualKind,
    /// Marks observed after this instant are stale and are not applied as
    /// current value (financial-engine §4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<DateTime<Utc>>,
}

/// Assets, liabilities and net worth after manual marks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WealthView {
    #[serde(with = "crate::money::decimal_string")]
    pub asset_value: Decimal,
    #[serde(with = "crate::money::decimal_string")]
    pub liability_value: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net_worth: Option<Decimal>,
    #[serde(with = "crate::money::decimal_string")]
    pub known_net_worth: Decimal,
    pub quality: Quality,
    pub missing_inputs: Vec<String>,
    pub notes: Vec<String>,
}

/// Fold manual assets and liabilities onto a market valuation. Liabilities
/// reduce net worth. A stale mark is listed and omitted, never treated as
/// today's value.
pub fn apply_manual_marks(
    summary: &ValuationSummary,
    marks: &[ManualMark],
    at: DateTime<Utc>,
    fx: &dyn FxResolver,
    reporting: &CurrencyCode,
    in_scope: &dyn Fn(&AccountId) -> bool,
) -> DomainResult<WealthView> {
    let mut assets = summary.known_market_value;
    let mut liabilities = Decimal::ZERO;
    let mut missing = summary.missing_inputs.clone();
    let mut notes = Vec::new();
    let mut complete = summary.quality == Quality::Complete;
    // One mark per account/asset/side: the latest at or before `at`. A newer
    // mark replaces an older one; a later-entered mark is not valid for
    // earlier dates (financial-engine §4). Ties keep the later input.
    let mut latest: BTreeMap<(AccountId, AssetId, bool), &ManualMark> = BTreeMap::new();
    let mut future_only: BTreeMap<(AccountId, AssetId, bool), &ManualMark> = BTreeMap::new();
    for mark in marks.iter().filter(|m| in_scope(&m.account_id)) {
        let key = (
            mark.account_id.clone(),
            mark.asset.clone(),
            mark.kind == ManualKind::Liability,
        );
        if mark.as_of > at {
            future_only.entry(key).or_insert(mark);
            continue;
        }
        match latest.get(&key) {
            Some(prev) if prev.as_of > mark.as_of => {}
            _ => {
                latest.insert(key, mark);
            }
        }
    }
    for (key, mark) in &future_only {
        if !latest.contains_key(key) {
            complete = false;
            missing.push(format!(
                "manual valuation {} is dated after the query",
                mark.asset
            ));
        }
    }
    for mark in latest.into_values() {
        if mark
            .valid_until
            .is_some_and(|until| at > until || mark.as_of > until)
        {
            complete = false;
            missing.push(format!(
                "stale manual valuation {} as of {}",
                mark.asset, mark.as_of
            ));
            notes.push(format!(
                "{} manual mark is expired and was not applied",
                mark.asset
            ));
            continue;
        }
        // Liabilities and other assets use the same valuation time as the
        // rest of the view (financial-engine §4), so FX is taken at `at`.
        let converted = match to_reporting(mark.value, &mark.currency, at, fx, reporting) {
            Some(v) => v,
            None => {
                complete = false;
                missing.push(format!(
                    "fx {} -> {reporting} for manual {}",
                    mark.currency, mark.asset
                ));
                continue;
            }
        };
        match mark.kind {
            ManualKind::Asset => assets = checked_add(assets, converted)?,
            ManualKind::Liability => liabilities = checked_add(liabilities, converted)?,
        }
    }
    let known = checked_sub(assets, liabilities)?;
    let quality = if complete {
        Quality::Complete
    } else if known == Decimal::ZERO && summary.known_market_value == Decimal::ZERO {
        Quality::Unavailable
    } else {
        Quality::Partial
    };
    Ok(WealthView {
        asset_value: assets,
        liability_value: liabilities,
        net_worth: if complete { Some(known) } else { None },
        known_net_worth: known,
        quality,
        missing_inputs: missing,
        notes,
    })
}

/// One observed external balance compared with the ledger. Differences are
/// reported and never written back (FR-DATA-05).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalanceDiff {
    pub account_id: AccountId,
    pub asset: AssetId,
    #[serde(with = "crate::money::decimal_string")]
    pub ledger_quantity: Decimal,
    #[serde(with = "crate::money::decimal_string")]
    pub observed_quantity: Decimal,
    #[serde(with = "crate::money::decimal_string")]
    pub difference: Decimal,
}

#[derive(Debug, Clone)]
pub struct BalanceObservation {
    pub account_id: AccountId,
    pub asset: AssetId,
    pub quantity: Decimal,
    pub as_of: DateTime<Utc>,
}

/// Compare ledger cash and holdings with external observations. Does not
/// mutate the engine.
pub fn reconcile_balances(
    engine: &LedgerEngine,
    observations: &[BalanceObservation],
    in_scope: &dyn Fn(&AccountId) -> bool,
) -> DomainResult<Vec<BalanceDiff>> {
    let mut diffs = Vec::new();
    for obs in observations {
        if !in_scope(&obs.account_id) {
            continue;
        }
        let ledger_qty = engine
            .accounts
            .get(&obs.account_id)
            .map(|ledger| {
                if engine.is_cash(&obs.asset) {
                    ledger.cash_of(&obs.asset)
                } else {
                    ledger
                        .holding(&obs.asset)
                        .map(|h| h.quantity())
                        .unwrap_or(Decimal::ZERO)
                }
            })
            .unwrap_or(Decimal::ZERO);
        let difference = checked_sub(obs.quantity, ledger_qty)?;
        if difference != Decimal::ZERO {
            diffs.push(BalanceDiff {
                account_id: obs.account_id.clone(),
                asset: obs.asset.clone(),
                ledger_quantity: ledger_qty,
                observed_quantity: obs.quantity,
                difference,
            });
        }
    }
    Ok(diffs)
}
