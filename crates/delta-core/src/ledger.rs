//! FIFO ledger engine.
//!
//! Applies effective economic events per account, maintaining cash balances,
//! FIFO cost lots, realized PnL, fee and income aggregates, and a pending area
//! (待入账区) for events that cannot be fully valued yet. Events are never
//! mutated; oversell and unknown costs follow financial-engine §2.
//!
//! Implementation note: helpers are free functions over `&mut AccountLedger`
//! plus engine parameters, so no `&self` call happens while an account is
//! mutably borrowed.

use crate::error::{DomainError, DomainResult};
use crate::events::{Corrections, EconomicEvent, EventPayload, Fee, OpeningCost, TransferKind};
use crate::ids::{AccountId, AssetId, EventId, InstrumentId, TransferGroupId};
use crate::money::{checked_add, checked_div, checked_mul, checked_sub, CurrencyCode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// FX lookup provided by the host (market data or test resolver).
pub trait FxResolver {
    /// Rate converting 1 `base` into `quote`, effective at `at`.
    /// Returns the rate and its effective time. Identity is implicit.
    fn rate(
        &self,
        base: &CurrencyCode,
        quote: &CurrencyCode,
        at: DateTime<Utc>,
    ) -> Option<(Decimal, DateTime<Utc>)>;
}

/// Identity resolver for a single-currency ledger (tests and USD-only books).
#[derive(Debug, Clone)]
pub struct IdentityFx {
    pub currency: CurrencyCode,
}

impl FxResolver for IdentityFx {
    fn rate(
        &self,
        base: &CurrencyCode,
        quote: &CurrencyCode,
        at: DateTime<Utc>,
    ) -> Option<(Decimal, DateTime<Utc>)> {
        if base == quote && *base == self.currency {
            Some((Decimal::ONE, at))
        } else {
            None
        }
    }
}

/// Resolves an instrument to its base asset (the held asset side).
pub trait InstrumentResolver {
    fn base_asset(&self, instrument: &InstrumentId) -> Option<AssetId>;
}

/// Map-backed resolver used by stores and tests.
#[derive(Debug, Clone, Default)]
pub struct InstrumentMap(pub BTreeMap<InstrumentId, AssetId>);

impl InstrumentResolver for InstrumentMap {
    fn base_asset(&self, instrument: &InstrumentId) -> Option<AssetId> {
        self.0.get(instrument).cloned()
    }
}

/// Double-entry ledger account categories (data-model §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerCategory {
    Cash,
    Holding,
    RealizedPnl,
    FeeExpense,
    Income,
    Equity,
    TransferClearing,
}

/// One posting row: native quantity of `asset` and signed book-currency amount.
/// `book_amount` values of one transaction sum to exactly zero. Debits on
/// asset accounts are positive; income accounts carry credits (negative).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Posting {
    pub category: LedgerCategory,
    /// Sub-kind for explanation: `opening`, `deposit`, `withdrawal`, fee
    /// category, income kind, or the instrument/asset context.
    pub sub: Option<String>,
    pub asset: AssetId,
    #[serde(with = "crate::money::decimal_string")]
    pub native_quantity: Decimal,
    #[serde(with = "crate::money::decimal_string")]
    pub book_amount: Decimal,
}

/// A balanced journal transaction for one event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalTransaction {
    pub event_id: EventId,
    pub account_id: AccountId,
    pub book_currency: CurrencyCode,
    pub postings: Vec<Posting>,
}

impl JournalTransaction {
    pub fn balances_to_zero(&self) -> bool {
        let mut sum = Decimal::ZERO;
        for p in &self.postings {
            match checked_add(sum, p.book_amount) {
                Ok(s) => sum = s,
                Err(_) => return false,
            }
        }
        sum == Decimal::ZERO
    }
}

/// A FIFO cost lot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLot {
    pub source_event: EventId,
    /// Instrument context when the lot came from a trade (`None` for openings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_instrument: Option<InstrumentId>,
    #[serde(with = "crate::money::decimal_string")]
    pub quantity: Decimal,
    /// Remaining total cost in `cost_currency`; `None` marks unknown cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_total: Option<Decimal>,
    pub cost_currency: CurrencyCode,
}

/// State of one held asset within one account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HoldingState {
    pub lots: Vec<CostLot>,
    /// Cost currency fixed by the first cost-carrying lot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_currency: Option<CurrencyCode>,
    /// Realized PnL from known-cost trade disposals in `cost_currency`
    /// (`None` before the first one). Disposals touching unknown-cost lots
    /// are counted in `unknown_cost_disposals` instead; any such count makes
    /// a realized total incomplete (unknown is never reported as zero).
    pub realized_pnl: Option<Decimal>,
    pub realized_count: u64,
    pub unknown_cost_disposals: u64,
    /// Realized differences from paying fees with this asset, in the book
    /// currency (fee value minus relieved cost at event time).
    pub fee_disposal_pnl: Option<Decimal>,
}

impl HoldingState {
    pub fn quantity(&self) -> Decimal {
        self.lots.iter().fold(Decimal::ZERO, |a, l| a + l.quantity)
    }
    pub fn carrying_cost(&self) -> Option<Decimal> {
        let mut total = Decimal::ZERO;
        for lot in &self.lots {
            total = checked_add(total, lot.cost_total?).ok()?;
        }
        Some(total)
    }
}

/// Income aggregates in native cash currencies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IncomeSummary {
    /// Dividends per cash asset.
    pub dividends: BTreeMap<AssetId, Decimal>,
    /// Interest per cash asset.
    pub interest: BTreeMap<AssetId, Decimal>,
}

/// Fee totals as explanation items (financial-engine §2: never subtracted
/// again from PnL that already embeds them).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeeSummary {
    /// Quote-currency fees embedded in trades, per fee asset.
    pub trade_fees: BTreeMap<AssetId, Decimal>,
    /// Standalone/transfer/third-asset fees valued at event time, per asset.
    pub valued_fees: BTreeMap<AssetId, Decimal>,
}

/// An event held in the pending area (待入账区): recorded but not applied,
/// e.g. missing FX or missing fee-asset price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingItem {
    pub event_id: EventId,
    pub reason: String,
    pub missing: String,
}

/// Ledger state of one account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountLedger {
    pub cash: BTreeMap<AssetId, Decimal>,
    pub holdings: BTreeMap<AssetId, HoldingState>,
    pub income: IncomeSummary,
    pub fees: FeeSummary,
    pub pending: Vec<PendingItem>,
}

impl AccountLedger {
    pub fn cash_of(&self, asset: &AssetId) -> Decimal {
        self.cash.get(asset).copied().unwrap_or(Decimal::ZERO)
    }
    pub fn holding(&self, asset: &AssetId) -> Option<&HoldingState> {
        self.holdings.get(asset)
    }
}

/// Result of applying one event.
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    Posted { transaction: JournalTransaction },
    Pending { pending: PendingItem },
}

#[derive(Debug, Clone)]
struct TransferAllocation {
    source_account: AccountId,
    counterparty: AccountId,
    principal: Decimal,
    is_cash: bool,
    asset: AssetId,
    moved_lots: Vec<CostLot>,
    /// Book value moved through clearing; the inbound leg books the same
    /// amount so the pair nets to zero (lots keep their historical cost).
    book_amount: Decimal,
}

/// Quantity that has left the source account and not yet arrived. Valuation
/// keeps it while both legs are inside the scope so a portfolio does not
/// show a false loss and a later false gain (financial-engine §3).
#[derive(Debug, Clone, PartialEq)]
pub struct InTransitPosition {
    pub group: TransferGroupId,
    pub source_account: AccountId,
    pub counterparty: AccountId,
    pub asset: AssetId,
    pub quantity: Decimal,
    /// Book value of the moved known cost (cash: principal at transfer time).
    pub book_amount: Decimal,
    pub is_cash: bool,
    /// False when any moved lot has unknown cost (`book_amount` is partial).
    pub cost_complete: bool,
}

/// Cross-account engine state: applies events in (occurred_at, seq) order and
/// pairs transfers by group so cost lots move between accounts unchanged
/// (financial-engine §3: moving never fabricates gains).
#[derive(Debug, Clone)]
pub struct LedgerEngine {
    pub book_currency: CurrencyCode,
    /// Registered cash/currency assets (USD, USDT, USDC are distinct). Any
    /// other asset is a position asset even if its id looks like a symbol.
    pub cash_assets: HashSet<AssetId>,
    pub accounts: BTreeMap<AccountId, AccountLedger>,
    transfer_pool: HashMap<TransferGroupId, TransferAllocation>,
    processed: HashSet<EventId>,
}

impl LedgerEngine {
    pub fn new(book_currency: CurrencyCode) -> Self {
        Self {
            book_currency,
            cash_assets: HashSet::new(),
            accounts: BTreeMap::new(),
            transfer_pool: HashMap::new(),
            processed: HashSet::new(),
        }
    }

    /// Register a cash asset. The book currency is cash by default.
    pub fn with_cash_asset(mut self, asset: AssetId) -> Self {
        self.cash_assets.insert(asset);
        self
    }

    pub fn is_cash(&self, asset: &AssetId) -> bool {
        asset.0 == self.book_currency.0 || self.cash_assets.contains(asset)
    }

    /// Outbound legs still waiting for a matching inbound.
    pub fn in_transit(&self) -> Vec<InTransitPosition> {
        let mut out: Vec<InTransitPosition> = self
            .transfer_pool
            .iter()
            .map(|(group, alloc)| InTransitPosition {
                group: group.clone(),
                source_account: alloc.source_account.clone(),
                counterparty: alloc.counterparty.clone(),
                asset: alloc.asset.clone(),
                quantity: alloc.principal,
                book_amount: alloc.book_amount,
                is_cash: alloc.is_cash,
                cost_complete: alloc.moved_lots.iter().all(|l| l.cost_total.is_some()),
            })
            .collect();
        out.sort_by(|a, b| a.group.cmp(&b.group));
        out
    }

    /// Apply events sorted by (occurred_at, seq). Re-applying the same event
    /// id is skipped (idempotent rebuilds).
    pub fn apply_all(
        &mut self,
        events: &[EconomicEvent],
        fx: &dyn FxResolver,
        instruments: &dyn InstrumentResolver,
    ) -> Vec<(EventId, ApplyOutcome)> {
        let mut sorted: Vec<&EconomicEvent> = events.iter().collect();
        sorted.sort_by_key(|e| e.sort_key());
        let mut out = Vec::new();
        for e in sorted {
            if self.processed.contains(&e.id) {
                continue;
            }
            let outcome = apply_one(self, e, fx, instruments);
            self.processed.insert(e.id.clone());
            out.push((e.id.clone(), outcome));
        }
        out
    }

    /// Events that produced pending items (待入账区) across accounts.
    pub fn pending_items(&self) -> Vec<(&AccountId, &PendingItem)> {
        let mut out = Vec::new();
        for (id, ledger) in &self.accounts {
            for p in &ledger.pending {
                out.push((id, p));
            }
        }
        out
    }
}

/// How one holding is restored when an event ends up pending.
#[derive(Debug)]
enum HoldingUndo {
    /// The holding did not exist before the event.
    Absent,
    /// Full copy (disposals, splits, fee payments mutate existing lots).
    Full(HoldingState),
    /// Append-only change (buys, openings, inbound transfers): truncate.
    Appended {
        lots_len: usize,
        cost_currency: Option<CurrencyCode>,
    },
}

/// Pre-event state of everything `try_apply` may mutate: only the event's
/// own account and the transfer pool entry of its group are ever touched.
/// A pending event is restored from this snapshot so it carries no ledger
/// effect (待入账区 means recorded but not applied).
#[derive(Debug)]
struct Undo {
    account: AccountId,
    existed: bool,
    cash: BTreeMap<AssetId, Decimal>,
    fees: FeeSummary,
    income: IncomeSummary,
    holdings: Vec<(AssetId, HoldingUndo)>,
    pool: Option<(TransferGroupId, Option<TransferAllocation>)>,
}

/// Holdings an event may touch, with whether they only grow.
fn touched_holdings(
    e: &EconomicEvent,
    instruments: &dyn InstrumentResolver,
) -> Vec<(AssetId, bool)> {
    let mut out: Vec<(AssetId, bool)> = Vec::new();
    let mut add = |asset: AssetId, append_only: bool| {
        if let Some(entry) = out.iter_mut().find(|(a, _)| *a == asset) {
            entry.1 &= append_only;
        } else {
            out.push((asset, append_only));
        }
    };
    match &e.payload {
        EventPayload::OpeningPosition { asset, .. } => add(asset.clone(), true),
        EventPayload::Buy {
            instrument, fees, ..
        } => {
            if let Some(base) = instruments.base_asset(instrument) {
                add(base, true);
            }
            for f in fees {
                add(f.asset.clone(), false);
            }
        }
        EventPayload::Sell {
            instrument, fees, ..
        } => {
            if let Some(base) = instruments.base_asset(instrument) {
                add(base, false);
            }
            for f in fees {
                add(f.asset.clone(), false);
            }
        }
        EventPayload::Transfer {
            kind, asset, fee, ..
        } => {
            add(asset.clone(), *kind == TransferKind::In);
            if let Some(f) = fee {
                add(f.asset.clone(), false);
            }
        }
        EventPayload::Fee { asset, .. } => add(asset.clone(), false),
        EventPayload::StockSplit { instrument, .. } => {
            if let Some(base) = instruments.base_asset(instrument) {
                add(base, false);
            }
        }
        EventPayload::CashDeposit { .. }
        | EventPayload::CashWithdrawal { .. }
        | EventPayload::Interest { .. }
        | EventPayload::DividendCash { .. } => {}
    }
    out
}

fn snapshot(
    engine: &LedgerEngine,
    e: &EconomicEvent,
    instruments: &dyn InstrumentResolver,
) -> Undo {
    let ledger = engine.accounts.get(&e.account_id);
    let holdings = touched_holdings(e, instruments)
        .into_iter()
        .map(|(asset, append_only)| {
            let undo = match ledger.and_then(|l| l.holdings.get(&asset)) {
                None => HoldingUndo::Absent,
                Some(h) if append_only => HoldingUndo::Appended {
                    lots_len: h.lots.len(),
                    cost_currency: h.cost_currency.clone(),
                },
                Some(h) => HoldingUndo::Full(h.clone()),
            };
            (asset, undo)
        })
        .collect();
    let pool = match &e.payload {
        EventPayload::Transfer { group, .. } => {
            Some((group.clone(), engine.transfer_pool.get(group).cloned()))
        }
        _ => None,
    };
    Undo {
        account: e.account_id.clone(),
        existed: ledger.is_some(),
        cash: ledger.map(|l| l.cash.clone()).unwrap_or_default(),
        fees: ledger.map(|l| l.fees.clone()).unwrap_or_default(),
        income: ledger.map(|l| l.income.clone()).unwrap_or_default(),
        holdings,
        pool,
    }
}

fn restore(engine: &mut LedgerEngine, undo: Undo) {
    if let Some((group, before)) = undo.pool {
        match before {
            Some(alloc) => {
                engine.transfer_pool.insert(group, alloc);
            }
            None => {
                engine.transfer_pool.remove(&group);
            }
        }
    }
    if !undo.existed {
        engine.accounts.remove(&undo.account);
        return;
    }
    let Some(ledger) = engine.accounts.get_mut(&undo.account) else {
        return;
    };
    ledger.cash = undo.cash;
    ledger.fees = undo.fees;
    ledger.income = undo.income;
    for (asset, h) in undo.holdings {
        match h {
            HoldingUndo::Absent => {
                ledger.holdings.remove(&asset);
            }
            HoldingUndo::Full(state) => {
                ledger.holdings.insert(asset, state);
            }
            HoldingUndo::Appended {
                lots_len,
                cost_currency,
            } => {
                if let Some(state) = ledger.holdings.get_mut(&asset) {
                    state.lots.truncate(lots_len);
                    state.cost_currency = cost_currency;
                }
            }
        }
    }
}

fn apply_one(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    fx: &dyn FxResolver,
    instruments: &dyn InstrumentResolver,
) -> ApplyOutcome {
    let undo = snapshot(engine, e, instruments);
    match try_apply(engine, e, fx, instruments) {
        Ok(t) => ApplyOutcome::Posted { transaction: t },
        Err(pending) => {
            restore(engine, undo);
            let item = PendingItem {
                event_id: e.id.clone(),
                reason: pending.reason,
                missing: pending.missing,
            };
            engine
                .accounts
                .entry(e.account_id.clone())
                .or_default()
                .pending
                .push(item.clone());
            ApplyOutcome::Pending { pending: item }
        }
    }
}

/// Convert a native-currency value to the book currency at `at`.
fn to_book(
    value: Decimal,
    native: &CurrencyCode,
    at: DateTime<Utc>,
    fx: &dyn FxResolver,
    book: &CurrencyCode,
) -> Option<Decimal> {
    if native == book {
        return Some(value);
    }
    let (rate, _) = fx.rate(native, book, at)?;
    checked_mul(value, rate).ok()
}

fn post(
    e: &EconomicEvent,
    book: &CurrencyCode,
    rows: Vec<Posting>,
) -> DomainResult<JournalTransaction> {
    let t = JournalTransaction {
        event_id: e.id.clone(),
        account_id: e.account_id.clone(),
        book_currency: book.clone(),
        postings: rows,
    };
    if !t.balances_to_zero() {
        return Err(DomainError::Conflict(
            e.id.0.clone(),
            "journal transaction does not balance".into(),
        ));
    }
    Ok(t)
}

/// Internal pending reason; converted to `PendingItem` at the boundary.
#[derive(Debug)]
pub(crate) struct PendingReason {
    reason: String,
    missing: String,
}

impl PendingReason {
    fn invalid(msg: impl Into<String>) -> Self {
        Self {
            reason: msg.into(),
            missing: String::new(),
        }
    }
    fn missing_fx(ccy: &CurrencyCode) -> Self {
        Self {
            reason: format!("missing FX rate to book currency at event time ({ccy})"),
            missing: ccy.0.clone(),
        }
    }
    fn missing_fee_price(ccy: &CurrencyCode) -> Self {
        Self {
            reason: format!("missing price for fee asset ({ccy})"),
            missing: ccy.0.clone(),
        }
    }
    fn oversell(asset: &str, available: Decimal, requested: Decimal) -> Self {
        Self {
            reason: format!(
                "oversell rejected for {asset}: have {available}, requested {requested}"
            ),
            missing: String::new(),
        }
    }
}

/// Event amounts are magnitudes; direction comes from the event type, so a
/// zero or negative amount is invalid input rather than a reversed flow.
fn require_positive(amount: Decimal, what: &str) -> Result<(), PendingReason> {
    if amount <= Decimal::ZERO {
        return Err(PendingReason::invalid(format!(
            "{what} must be positive, got {amount}"
        )));
    }
    Ok(())
}

pub fn currency_of(asset: &AssetId) -> CurrencyCode {
    CurrencyCode(asset.0.clone())
}

fn asset_of_currency(ccy: &CurrencyCode) -> AssetId {
    AssetId(ccy.0.clone())
}

fn try_apply(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    fx: &dyn FxResolver,
    instruments: &dyn InstrumentResolver,
) -> Result<JournalTransaction, PendingReason> {
    match &e.payload {
        EventPayload::OpeningPosition {
            asset,
            quantity,
            cost,
        } => apply_opening(engine, e, asset, *quantity, cost, fx),
        EventPayload::Buy {
            instrument,
            quantity,
            price,
            quote_currency,
            fees,
        } => apply_trade(
            engine,
            e,
            instruments,
            instrument,
            *quantity,
            *price,
            quote_currency,
            fees,
            fx,
            true,
        ),
        EventPayload::Sell {
            instrument,
            quantity,
            price,
            quote_currency,
            fees,
        } => apply_trade(
            engine,
            e,
            instruments,
            instrument,
            *quantity,
            *price,
            quote_currency,
            fees,
            fx,
            false,
        ),
        EventPayload::CashDeposit { asset, amount } => {
            require_positive(*amount, "deposit amount")?;
            apply_cash_flow(engine, e, asset, *amount, fx, "deposit")
        }
        EventPayload::CashWithdrawal { asset, amount } => {
            require_positive(*amount, "withdrawal amount")?;
            apply_cash_flow(engine, e, asset, -*amount, fx, "withdrawal")
        }
        EventPayload::Transfer {
            kind,
            group,
            counterparty,
            asset,
            principal,
            fee,
        } => apply_transfer(
            engine,
            e,
            *kind,
            group,
            counterparty,
            asset,
            *principal,
            fee.as_ref(),
            fx,
        ),
        EventPayload::Fee {
            asset,
            amount,
            category,
        } => {
            require_positive(*amount, "fee amount")?;
            apply_standalone_fee(engine, e, asset, *amount, category, fx)
        }
        EventPayload::Interest { asset, amount } => {
            require_positive(*amount, "interest amount")?;
            apply_income(engine, e, asset, *amount, fx, "interest")
        }
        EventPayload::DividendCash {
            cash_asset,
            amount,
            instrument,
        } => {
            require_positive(*amount, "dividend amount")?;
            apply_dividend(engine, e, cash_asset, *amount, instrument, fx)
        }
        EventPayload::StockSplit {
            instrument,
            ratio_num,
            ratio_den,
        } => apply_stock_split(engine, e, instruments, instrument, *ratio_num, *ratio_den),
    }
}

fn apply_opening(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    asset: &AssetId,
    quantity: Decimal,
    cost: &Option<OpeningCost>,
    fx: &dyn FxResolver,
) -> Result<JournalTransaction, PendingReason> {
    if quantity <= Decimal::ZERO {
        return Err(PendingReason::invalid(format!(
            "opening quantity must be positive, got {quantity}"
        )));
    }
    let cash = engine.is_cash(asset);
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    if cash {
        let current = ledger.cash_of(asset);
        let next = checked_add(current, quantity)
            .map_err(|err| PendingReason::invalid(format!("opening cash overflow: {err}")))?;
        ledger.cash.insert(asset.clone(), next);
        let ccy = currency_of(asset);
        let book = to_book(quantity, &ccy, e.occurred_at, fx, &engine.book_currency)
            .ok_or_else(|| PendingReason::missing_fx(&ccy))?;
        return post(
            e,
            &engine.book_currency,
            vec![
                Posting {
                    category: LedgerCategory::Cash,
                    sub: Some("opening".into()),
                    asset: asset.clone(),
                    native_quantity: quantity,
                    book_amount: book,
                },
                Posting {
                    category: LedgerCategory::Equity,
                    sub: Some("opening".into()),
                    asset: asset.clone(),
                    native_quantity: -quantity,
                    book_amount: -book,
                },
            ],
        )
        .map_err(|err| PendingReason::invalid(err.to_string()));
    }
    let (known, cost_total, cost_ccy) = match cost {
        Some(OpeningCost::Known { total, currency }) => (true, *total, currency.clone()),
        Some(OpeningCost::Unknown) | None => (false, Decimal::ZERO, currency_of(asset)),
    };
    let holding = ledger.holdings.entry(asset.clone()).or_default();
    if let Some(ccy) = &holding.cost_currency {
        if known && *ccy != cost_ccy {
            return Err(PendingReason::invalid(format!(
                "opening cost currency {cost_ccy} conflicts with existing {ccy}"
            )));
        }
    } else if known {
        holding.cost_currency = Some(cost_ccy.clone());
    }
    holding.lots.push(CostLot {
        source_event: e.id.clone(),
        source_instrument: None,
        quantity,
        cost_total: if known { Some(cost_total) } else { None },
        cost_currency: cost_ccy.clone(),
    });
    let book_cost = if known {
        to_book(
            cost_total,
            &cost_ccy,
            e.occurred_at,
            fx,
            &engine.book_currency,
        )
        .ok_or_else(|| PendingReason::missing_fx(&cost_ccy))?
    } else {
        Decimal::ZERO
    };
    let rows = vec![
        Posting {
            category: LedgerCategory::Holding,
            sub: Some(if known {
                "opening".into()
            } else {
                "opening_unknown_cost".into()
            }),
            asset: asset.clone(),
            native_quantity: quantity,
            book_amount: book_cost,
        },
        Posting {
            category: LedgerCategory::Equity,
            sub: Some("opening".into()),
            asset: asset.clone(),
            native_quantity: -quantity,
            book_amount: -book_cost,
        },
    ];
    post(e, &engine.book_currency, rows).map_err(|err| PendingReason::invalid(err.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn apply_trade(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    instruments: &dyn InstrumentResolver,
    instrument: &InstrumentId,
    quantity: Decimal,
    price: Decimal,
    quote: &CurrencyCode,
    fees: &[Fee],
    fx: &dyn FxResolver,
    is_buy: bool,
) -> Result<JournalTransaction, PendingReason> {
    if quantity <= Decimal::ZERO {
        return Err(PendingReason::invalid("trade quantity must be positive"));
    }
    if price <= Decimal::ZERO {
        return Err(PendingReason::invalid("trade price must be positive"));
    }
    let base = instruments
        .base_asset(instrument)
        .ok_or_else(|| PendingReason::invalid(format!("unknown instrument {instrument}")))?;
    let gross = checked_mul(quantity, price)
        .map_err(|err| PendingReason::invalid(format!("gross overflow: {err}")))?;
    let quote_fees: Decimal = fees
        .iter()
        .filter(|f| currency_of(&f.asset) == *quote)
        .fold(Decimal::ZERO, |a, f| checked_add(a, f.amount).unwrap_or(a));

    // Third-asset fees: valued at event time into the quote currency; a
    // missing price holds the whole event (待补全), never silently skips the
    // fee. On buys the fee is capitalized into the acquired cost (financial-
    // engine §2: fees attributed to the acquisition enter analysis cost); on
    // sells it is expensed and funded by the fee-asset disposal.
    let mut fee_rows: Vec<Posting> = Vec::new();
    let mut third_fee_value_quote = Decimal::ZERO;
    for f in fees {
        let fee_ccy = currency_of(&f.asset);
        if fee_ccy == *quote {
            continue;
        }
        let (rate, _) = fx
            .rate(&fee_ccy, quote, e.occurred_at)
            .ok_or_else(|| PendingReason::missing_fee_price(&fee_ccy))?;
        let fee_value = checked_mul(f.amount, rate)
            .map_err(|err| PendingReason::invalid(format!("fee value overflow: {err}")))?;
        third_fee_value_quote = checked_add(third_fee_value_quote, fee_value)
            .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
        fee_rows.extend(dispose_fee_asset(
            engine, e, &f.asset, f.amount, fee_value, quote, fx, is_buy,
        )?);
    }

    let mut rows: Vec<Posting> = Vec::new();
    let quote_asset = asset_of_currency(quote);
    if is_buy {
        // Cash pays the gross amount and quote-currency fees only; a
        // third-asset fee was already paid by disposing the fee asset above
        // and is capitalized into the acquired cost, never charged twice.
        let cash_out = checked_add(gross, quote_fees)
            .map_err(|err| PendingReason::invalid(format!("cost overflow: {err}")))?;
        let cost_total = checked_add(cash_out, third_fee_value_quote)
            .map_err(|err| PendingReason::invalid(format!("cost overflow: {err}")))?;
        let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
        let holding = ledger.holdings.entry(base.clone()).or_default();
        if let Some(ccy) = &holding.cost_currency {
            if *ccy != *quote {
                return Err(PendingReason::invalid(format!(
                    "instrument quote {quote} conflicts with holding cost currency {ccy}"
                )));
            }
        } else {
            holding.cost_currency = Some(quote.clone());
        }
        holding.lots.push(CostLot {
            source_event: e.id.clone(),
            source_instrument: Some(instrument.clone()),
            quantity,
            cost_total: Some(cost_total),
            cost_currency: quote.clone(),
        });
        let cash_now = ledger.cash_of(&quote_asset);
        let next_cash = checked_sub(cash_now, cash_out)
            .map_err(|err| PendingReason::invalid(format!("cash underflow: {err}")))?;
        if next_cash < Decimal::ZERO {
            // Spot, long-only, no financing: a buy without sufficient cash
            // means missing history (opening/deposit) — hold in 待补全.
            return Err(PendingReason {
                reason: format!(
                    "insufficient cash for buy: have {cash_now} {quote}, need {cash_out}"
                ),
                missing: quote_asset.0.clone(),
            });
        }
        ledger.cash.insert(quote_asset.clone(), next_cash);
        if quote_fees > Decimal::ZERO {
            let f = ledger
                .fees
                .trade_fees
                .entry(quote_asset.clone())
                .or_default();
            *f = checked_add(*f, quote_fees)
                .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
        }
        let cost_book = to_book(cost_total, quote, e.occurred_at, fx, &engine.book_currency)
            .ok_or_else(|| PendingReason::missing_fx(quote))?;
        let cash_book = to_book(cash_out, quote, e.occurred_at, fx, &engine.book_currency)
            .ok_or_else(|| PendingReason::missing_fx(quote))?;
        rows.push(Posting {
            category: LedgerCategory::Holding,
            sub: Some(format!("buy:{instrument}")),
            asset: base.clone(),
            native_quantity: quantity,
            book_amount: cost_book,
        });
        rows.push(Posting {
            category: LedgerCategory::Cash,
            sub: Some(format!("buy:{instrument}")),
            asset: quote_asset,
            native_quantity: -cash_out,
            book_amount: -cash_book,
        });
    } else {
        let net = checked_sub(gross, quote_fees)
            .map_err(|err| PendingReason::invalid(format!("proceeds overflow: {err}")))?;
        let disposal = dispose_lots(e, engine, &e.account_id, &base, quantity)?;
        if disposal.consumed_cost.is_some() && disposal.cost_currency != *quote {
            // Realized PnL is proceeds minus cost in one currency; mixing a
            // EUR cost basis with USD proceeds needs an explicit FX policy.
            return Err(PendingReason {
                reason: format!(
                    "cost currency {} differs from trade quote {quote}; cross-currency realized PnL needs an FX policy (待补全)",
                    disposal.cost_currency
                ),
                missing: format!("fx policy {} -> {quote}", disposal.cost_currency),
            });
        }
        let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
        let cash_now = ledger.cash_of(&quote_asset);
        let next_cash = checked_add(cash_now, net)
            .map_err(|err| PendingReason::invalid(format!("cash overflow: {err}")))?;
        ledger.cash.insert(quote_asset.clone(), next_cash);
        if quote_fees > Decimal::ZERO {
            let f = ledger
                .fees
                .trade_fees
                .entry(quote_asset.clone())
                .or_default();
            *f = checked_add(*f, quote_fees)
                .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
        }
        let net_book = to_book(net, quote, e.occurred_at, fx, &engine.book_currency)
            .ok_or_else(|| PendingReason::missing_fx(quote))?;
        let cost_book = disposal.known_cost_book(e.occurred_at, fx, &engine.book_currency)?;
        let realized = match disposal.consumed_cost {
            Some(c) => checked_sub(net, c).ok(),
            None => None,
        };
        if let Some(holding) = ledger.holdings.get_mut(&base) {
            match realized {
                Some(r) => {
                    holding.realized_pnl = Some(
                        checked_add(holding.realized_pnl.unwrap_or(Decimal::ZERO), r).map_err(
                            |err| PendingReason::invalid(format!("realized overflow: {err}")),
                        )?,
                    );
                    holding.realized_count += 1;
                }
                None => holding.unknown_cost_disposals += 1,
            }
        }
        rows.push(Posting {
            category: LedgerCategory::Cash,
            sub: Some(format!("sell:{instrument}")),
            asset: quote_asset,
            native_quantity: net,
            book_amount: net_book,
        });
        rows.push(Posting {
            category: LedgerCategory::Holding,
            sub: Some(format!("sell:{instrument}")),
            asset: base.clone(),
            native_quantity: -quantity,
            book_amount: -cost_book,
        });
        if let Some(r) = realized {
            let r_book = to_book(
                r,
                &disposal.cost_currency,
                e.occurred_at,
                fx,
                &engine.book_currency,
            )
            .ok_or_else(|| PendingReason::missing_fx(&disposal.cost_currency))?;
            rows.push(Posting {
                category: LedgerCategory::RealizedPnl,
                sub: Some(format!("sell:{instrument}")),
                asset: base.clone(),
                native_quantity: Decimal::ZERO,
                book_amount: -r_book,
            });
        } else {
            let rest = checked_sub(net_book, cost_book)
                .map_err(|err| PendingReason::invalid(format!("proceeds overflow: {err}")))?;
            rows.push(unknown_cost_row(&base, &format!("sell:{instrument}"), rest));
        }
    }
    rows.extend(fee_rows);
    post(e, &engine.book_currency, rows).map_err(|err| PendingReason::invalid(err.to_string()))
}

#[derive(Debug, Clone)]
struct Disposal {
    /// Total relieved cost; `None` when any consumed lot had unknown cost.
    consumed_cost: Option<Decimal>,
    /// Relieved cost of the known-cost lots only (equals `consumed_cost`
    /// when every lot was known).
    known_cost: Decimal,
    cost_currency: CurrencyCode,
}

impl Disposal {
    /// Book value of the relieved known cost (no FX lookup for zero, so a
    /// purely unknown-cost disposal never needs a rate for its cost side).
    fn known_cost_book(
        &self,
        at: DateTime<Utc>,
        fx: &dyn FxResolver,
        book: &CurrencyCode,
    ) -> Result<Decimal, PendingReason> {
        if self.known_cost == Decimal::ZERO {
            return Ok(Decimal::ZERO);
        }
        to_book(self.known_cost, &self.cost_currency, at, fx, book)
            .ok_or_else(|| PendingReason::missing_fx(&self.cost_currency))
    }
}

/// Balancing row for a disposal whose cost is (partly) unknown: the
/// difference between value received and known cost relieved is parked in
/// equity as 待补全成本, never booked as realized PnL (unknown is not zero).
fn unknown_cost_row(asset: &AssetId, context: &str, value_minus_cost_book: Decimal) -> Posting {
    Posting {
        category: LedgerCategory::Equity,
        sub: Some(format!("unknown_cost_disposal:{context}")),
        asset: asset.clone(),
        native_quantity: Decimal::ZERO,
        book_amount: -value_minus_cost_book,
    }
}

/// FIFO-dispose `quantity` of `asset` from one account's holdings.
/// Partial takes allocate cost with high precision; a fully consumed lot
/// transfers its exact remaining cost (last-take absorbs the rounding
/// residual), keeping cumulative cost conserved.
fn dispose_lots(
    _e: &EconomicEvent,
    engine: &mut LedgerEngine,
    account: &AccountId,
    asset: &AssetId,
    quantity: Decimal,
) -> Result<Disposal, PendingReason> {
    let ledger = engine.accounts.entry(account.clone()).or_default();
    let total = ledger
        .holding(asset)
        .map(|h| h.quantity())
        .unwrap_or(Decimal::ZERO);
    if quantity > total {
        return Err(PendingReason::oversell(&asset.0, total, quantity));
    }
    let holding = ledger.holdings.get_mut(asset).expect("quantity > 0");
    let cost_ccy = holding
        .cost_currency
        .clone()
        .unwrap_or_else(|| currency_of(asset));
    let mut remaining = quantity;
    let mut consumed = Decimal::ZERO;
    let mut has_unknown = false;
    while remaining > Decimal::ZERO {
        let lot = &mut holding.lots[0];
        let take = if lot.quantity <= remaining {
            lot.quantity
        } else {
            remaining
        };
        match lot.cost_total {
            Some(ct) => {
                let consumed_cost = if take == lot.quantity {
                    ct
                } else {
                    checked_div(
                        checked_mul(ct, take).map_err(|err| {
                            PendingReason::invalid(format!("cost allocation overflow: {err}"))
                        })?,
                        lot.quantity,
                    )
                    .map_err(|err| {
                        PendingReason::invalid(format!("cost allocation failed: {err}"))
                    })?
                };
                consumed = checked_add(consumed, consumed_cost).map_err(|err| {
                    PendingReason::invalid(format!("cost accumulation overflow: {err}"))
                })?;
                if take == lot.quantity {
                    holding.lots.remove(0);
                } else {
                    lot.quantity = checked_sub(lot.quantity, take).map_err(|err| {
                        PendingReason::invalid(format!("lot quantity underflow: {err}"))
                    })?;
                    lot.cost_total = Some(checked_sub(ct, consumed_cost).map_err(|err| {
                        PendingReason::invalid(format!("lot cost underflow: {err}"))
                    })?);
                }
            }
            None => {
                has_unknown = true;
                if take == lot.quantity {
                    holding.lots.remove(0);
                } else {
                    lot.quantity = checked_sub(lot.quantity, take).map_err(|err| {
                        PendingReason::invalid(format!("lot quantity underflow: {err}"))
                    })?;
                }
            }
        }
        remaining = checked_sub(remaining, take).map_err(|err| {
            PendingReason::invalid(format!("disposal remainder underflow: {err}"))
        })?;
    }
    Ok(Disposal {
        consumed_cost: if has_unknown { None } else { Some(consumed) },
        known_cost: consumed,
        cost_currency: cost_ccy,
    })
}

/// Third-asset fee: reduce the fee asset by FIFO disposal (or cash). The fee
/// value is recorded as a fee explanation item (book currency). When
/// `capitalize` (buy-side attribution) the value enters the acquired cost and
/// only the disposal rows are posted; otherwise a FeeExpense row is posted and
/// funded by the disposal (financial-engine §2).
#[allow(clippy::too_many_arguments)]
fn dispose_fee_asset(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    fee_asset: &AssetId,
    amount: Decimal,
    value_in_quote: Decimal,
    quote: &CurrencyCode,
    fx: &dyn FxResolver,
    capitalize: bool,
) -> Result<Vec<Posting>, PendingReason> {
    let held_as_holding = engine
        .accounts
        .entry(e.account_id.clone())
        .or_default()
        .holdings
        .contains_key(fee_asset);
    let disposal = if held_as_holding {
        Some(dispose_lots(e, engine, &e.account_id, fee_asset, amount)?)
    } else {
        let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
        let cash = ledger.cash_of(fee_asset);
        if cash < amount {
            return Err(PendingReason::oversell(&fee_asset.0, cash, amount));
        }
        ledger.cash.insert(fee_asset.clone(), cash - amount);
        None
    };
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    let value_book = to_book(
        value_in_quote,
        quote,
        e.occurred_at,
        fx,
        &engine.book_currency,
    )
    .ok_or_else(|| PendingReason::missing_fx(quote))?;
    let f = ledger
        .fees
        .valued_fees
        .entry(fee_asset.clone())
        .or_default();
    *f = checked_add(*f, value_book)
        .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
    let mut rows = Vec::new();
    if !capitalize {
        rows.push(Posting {
            category: LedgerCategory::FeeExpense,
            sub: Some(format!("fee_asset:{}", fee_asset.0)),
            asset: fee_asset.clone(),
            native_quantity: -amount,
            book_amount: value_book,
        });
    }
    if !held_as_holding {
        // Fee paid from another cash balance: credit that cash at the fee
        // value so the transaction balances (debit is the expense row or the
        // capitalized acquisition cost).
        rows.push(Posting {
            category: LedgerCategory::Cash,
            sub: Some(format!("fee_asset:{}", fee_asset.0)),
            asset: fee_asset.clone(),
            native_quantity: -amount,
            book_amount: -value_book,
        });
    }
    if let Some(d) = disposal {
        let cost_book = d.known_cost_book(e.occurred_at, fx, &engine.book_currency)?;
        rows.push(Posting {
            category: LedgerCategory::Holding,
            sub: Some(format!("fee_disposal:{}", fee_asset.0)),
            asset: fee_asset.clone(),
            native_quantity: -amount,
            book_amount: -cost_book,
        });
        if d.consumed_cost.is_none() {
            let rest = checked_sub(value_book, cost_book).map_err(|err| {
                PendingReason::invalid(format!("fee disposal diff overflow: {err}"))
            })?;
            rows.push(unknown_cost_row(
                fee_asset,
                &format!("fee_disposal:{}", fee_asset.0),
                rest,
            ));
        } else {
            let diff = checked_sub(value_book, cost_book).map_err(|err| {
                PendingReason::invalid(format!("fee disposal diff overflow: {err}"))
            })?;
            let holding = ledger.holdings.get_mut(fee_asset).unwrap();
            holding.fee_disposal_pnl = Some(
                checked_add(holding.fee_disposal_pnl.unwrap_or(Decimal::ZERO), diff)
                    .map_err(|err| PendingReason::invalid(format!("fee pnl overflow: {err}")))?,
            );
            if diff != Decimal::ZERO {
                rows.push(Posting {
                    category: LedgerCategory::RealizedPnl,
                    sub: Some(format!("fee_disposal:{}", fee_asset.0)),
                    asset: fee_asset.clone(),
                    native_quantity: Decimal::ZERO,
                    book_amount: -diff,
                });
            }
        }
    }
    Ok(rows)
}

fn apply_cash_flow(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    asset: &AssetId,
    signed_amount: Decimal,
    fx: &dyn FxResolver,
    sub: &str,
) -> Result<JournalTransaction, PendingReason> {
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    let current = ledger.cash_of(asset);
    let next = checked_add(current, signed_amount)
        .map_err(|err| PendingReason::invalid(format!("cash flow overflow: {err}")))?;
    if next < Decimal::ZERO {
        // Spot, no financing: withdrawing more than the balance means
        // missing history, held for completion instead of negative cash.
        return Err(PendingReason {
            reason: format!(
                "insufficient cash for {sub}: have {current} {asset}, need {}",
                -signed_amount
            ),
            missing: asset.0.clone(),
        });
    }
    ledger.cash.insert(asset.clone(), next);
    let ccy = currency_of(asset);
    let book = to_book(
        signed_amount,
        &ccy,
        e.occurred_at,
        fx,
        &engine.book_currency,
    )
    .ok_or_else(|| PendingReason::missing_fx(&ccy))?;
    post(
        e,
        &engine.book_currency,
        vec![
            Posting {
                category: LedgerCategory::Cash,
                sub: Some(sub.into()),
                asset: asset.clone(),
                native_quantity: signed_amount,
                book_amount: book,
            },
            Posting {
                category: LedgerCategory::Equity,
                sub: Some(sub.into()),
                asset: asset.clone(),
                native_quantity: -signed_amount,
                book_amount: -book,
            },
        ],
    )
    .map_err(|err| PendingReason::invalid(err.to_string()))
}

fn apply_income(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    asset: &AssetId,
    amount: Decimal,
    fx: &dyn FxResolver,
    kind: &str,
) -> Result<JournalTransaction, PendingReason> {
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    let current = ledger.cash_of(asset);
    let next = checked_add(current, amount)
        .map_err(|err| PendingReason::invalid(format!("income cash overflow: {err}")))?;
    ledger.cash.insert(asset.clone(), next);
    let income = ledger.income.interest.entry(asset.clone()).or_default();
    *income = checked_add(*income, amount)
        .map_err(|err| PendingReason::invalid(format!("interest overflow: {err}")))?;
    let ccy = currency_of(asset);
    let book = to_book(amount, &ccy, e.occurred_at, fx, &engine.book_currency)
        .ok_or_else(|| PendingReason::missing_fx(&ccy))?;
    post(
        e,
        &engine.book_currency,
        vec![
            Posting {
                category: LedgerCategory::Cash,
                sub: Some(kind.into()),
                asset: asset.clone(),
                native_quantity: amount,
                book_amount: book,
            },
            Posting {
                category: LedgerCategory::Income,
                sub: Some(kind.into()),
                asset: asset.clone(),
                native_quantity: Decimal::ZERO,
                book_amount: -book,
            },
        ],
    )
    .map_err(|err| PendingReason::invalid(err.to_string()))
}

fn apply_dividend(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    cash_asset: &AssetId,
    amount: Decimal,
    instrument: &InstrumentId,
    fx: &dyn FxResolver,
) -> Result<JournalTransaction, PendingReason> {
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    let current = ledger.cash_of(cash_asset);
    let next = checked_add(current, amount)
        .map_err(|err| PendingReason::invalid(format!("dividend cash overflow: {err}")))?;
    ledger.cash.insert(cash_asset.clone(), next);
    let div = ledger
        .income
        .dividends
        .entry(cash_asset.clone())
        .or_default();
    *div = checked_add(*div, amount)
        .map_err(|err| PendingReason::invalid(format!("dividend overflow: {err}")))?;
    let ccy = currency_of(cash_asset);
    let book = to_book(amount, &ccy, e.occurred_at, fx, &engine.book_currency)
        .ok_or_else(|| PendingReason::missing_fx(&ccy))?;
    post(
        e,
        &engine.book_currency,
        vec![
            Posting {
                category: LedgerCategory::Cash,
                sub: Some(format!("dividend:{instrument}")),
                asset: cash_asset.clone(),
                native_quantity: amount,
                book_amount: book,
            },
            Posting {
                category: LedgerCategory::Income,
                sub: Some(format!("dividend:{instrument}")),
                asset: cash_asset.clone(),
                native_quantity: Decimal::ZERO,
                book_amount: -book,
            },
        ],
    )
    .map_err(|err| PendingReason::invalid(err.to_string()))
}

fn apply_standalone_fee(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    asset: &AssetId,
    amount: Decimal,
    category: &str,
    fx: &dyn FxResolver,
) -> Result<JournalTransaction, PendingReason> {
    let held_as_holding = engine
        .accounts
        .entry(e.account_id.clone())
        .or_default()
        .holdings
        .contains_key(asset);
    let mut rows: Vec<Posting>;
    if held_as_holding {
        // Fee paid from a held asset: value it at event time (missing price
        // holds the event), dispose cost by FIFO, record the difference.
        let ccy = currency_of(asset);
        let book_value = to_book(amount, &ccy, e.occurred_at, fx, &engine.book_currency)
            .ok_or_else(|| PendingReason::missing_fee_price(&ccy))?;
        let disposal = dispose_lots(e, engine, &e.account_id, asset, amount)?;
        let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
        let f = ledger.fees.valued_fees.entry(asset.clone()).or_default();
        *f = checked_add(*f, book_value)
            .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
        let cost_book = disposal.known_cost_book(e.occurred_at, fx, &engine.book_currency)?;
        rows = vec![
            Posting {
                category: LedgerCategory::FeeExpense,
                sub: Some(category.into()),
                asset: asset.clone(),
                native_quantity: -amount,
                book_amount: book_value,
            },
            Posting {
                category: LedgerCategory::Holding,
                sub: Some(format!("fee_disposal:{category}")),
                asset: asset.clone(),
                native_quantity: -amount,
                book_amount: -cost_book,
            },
        ];
        if disposal.consumed_cost.is_none() {
            let rest = checked_sub(book_value, cost_book)
                .map_err(|err| PendingReason::invalid(format!("fee diff overflow: {err}")))?;
            rows.push(unknown_cost_row(
                asset,
                &format!("fee_disposal:{category}"),
                rest,
            ));
        } else {
            // Gain/loss of using the asset to pay: fee value minus relieved
            // cost, both in book currency; a gain is a credit (negative).
            let diff = checked_sub(book_value, cost_book)
                .map_err(|err| PendingReason::invalid(format!("fee diff overflow: {err}")))?;
            if let Some(holding) = ledger.holdings.get_mut(asset) {
                holding.fee_disposal_pnl = Some(
                    checked_add(holding.fee_disposal_pnl.unwrap_or(Decimal::ZERO), diff).map_err(
                        |err| PendingReason::invalid(format!("fee pnl overflow: {err}")),
                    )?,
                );
            }
            if diff != Decimal::ZERO {
                rows.push(Posting {
                    category: LedgerCategory::RealizedPnl,
                    sub: Some(format!("fee_disposal:{category}")),
                    asset: asset.clone(),
                    native_quantity: Decimal::ZERO,
                    book_amount: -diff,
                });
            }
        }
    } else {
        let ccy = currency_of(asset);
        let book = to_book(amount, &ccy, e.occurred_at, fx, &engine.book_currency)
            .ok_or_else(|| PendingReason::missing_fx(&ccy))?;
        let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
        let cash = ledger.cash_of(asset);
        if cash < amount {
            return Err(PendingReason::oversell(&asset.0, cash, amount));
        }
        ledger.cash.insert(asset.clone(), cash - amount);
        let f = ledger.fees.valued_fees.entry(asset.clone()).or_default();
        *f = checked_add(*f, book)
            .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
        rows = vec![
            Posting {
                category: LedgerCategory::FeeExpense,
                sub: Some(category.into()),
                asset: asset.clone(),
                native_quantity: -amount,
                book_amount: book,
            },
            Posting {
                category: LedgerCategory::Cash,
                sub: Some(category.into()),
                asset: asset.clone(),
                native_quantity: -amount,
                book_amount: -book,
            },
        ];
    }
    post(e, &engine.book_currency, rows).map_err(|err| PendingReason::invalid(err.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn apply_transfer(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    kind: TransferKind,
    group: &TransferGroupId,
    counterparty: &AccountId,
    asset: &AssetId,
    principal: Decimal,
    fee: Option<&Fee>,
    fx: &dyn FxResolver,
) -> Result<JournalTransaction, PendingReason> {
    if principal <= Decimal::ZERO {
        return Err(PendingReason::invalid(
            "transfer principal must be positive",
        ));
    }
    match kind {
        TransferKind::Out => {
            let is_cash = engine.is_cash(asset);
            let moved_lots = if is_cash {
                let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
                let cash = ledger.cash_of(asset);
                if cash < principal {
                    return Err(PendingReason::oversell(&asset.0, cash, principal));
                }
                ledger.cash.insert(asset.clone(), cash - principal);
                Vec::new()
            } else {
                let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
                let total = ledger
                    .holding(asset)
                    .map(|h| h.quantity())
                    .unwrap_or(Decimal::ZERO);
                if principal > total {
                    return Err(PendingReason::oversell(&asset.0, total, principal));
                }
                let holding = ledger.holdings.get_mut(asset).expect("checked above");
                take_lots_fifo(holding, principal)
                    .map_err(|err| PendingReason::invalid(err.to_string()))?
            };
            // Cash moves at its book value now; a position moves at the book
            // value of its known historical cost (no market price or asset
            // "FX" is needed, and no gain is fabricated by moving it).
            let book = if is_cash {
                let ccy = currency_of(asset);
                to_book(principal, &ccy, e.occurred_at, fx, &engine.book_currency)
                    .ok_or_else(|| PendingReason::missing_fx(&ccy))?
            } else {
                let mut total = Decimal::ZERO;
                for lot in &moved_lots {
                    if let Some(c) = lot.cost_total {
                        let v = to_book(
                            c,
                            &lot.cost_currency,
                            e.occurred_at,
                            fx,
                            &engine.book_currency,
                        )
                        .ok_or_else(|| PendingReason::missing_fx(&lot.cost_currency))?;
                        total = checked_add(total, v).map_err(|err| {
                            PendingReason::invalid(format!("transfer cost overflow: {err}"))
                        })?;
                    }
                }
                total
            };
            engine.transfer_pool.insert(
                group.clone(),
                TransferAllocation {
                    source_account: e.account_id.clone(),
                    counterparty: counterparty.clone(),
                    principal,
                    is_cash,
                    asset: asset.clone(),
                    moved_lots,
                    book_amount: book,
                },
            );
            let mut rows = vec![
                Posting {
                    category: if is_cash {
                        LedgerCategory::Cash
                    } else {
                        LedgerCategory::Holding
                    },
                    sub: Some(format!("transfer_out:{group}")),
                    asset: asset.clone(),
                    native_quantity: -principal,
                    book_amount: -book,
                },
                Posting {
                    category: LedgerCategory::TransferClearing,
                    sub: Some(format!("out:{group}")),
                    asset: asset.clone(),
                    native_quantity: principal,
                    book_amount: book,
                },
            ];
            if let Some(f) = fee {
                rows.extend(fee_cash_rows(engine, e, f, fx)?);
            }
            post(e, &engine.book_currency, rows)
                .map_err(|err| PendingReason::invalid(err.to_string()))
        }
        TransferKind::In => {
            // Require the paired outbound; missing pair goes to 待核对
            // (pending) instead of fabricating cost or balance.
            let alloc = engine
                .transfer_pool
                .remove(group)
                .ok_or_else(|| PendingReason {
                    reason: "transfer inbound has no paired outbound (待核对)".into(),
                    missing: format!("transfer_group {group}"),
                })?;
            if alloc.principal != principal {
                return Err(PendingReason {
                    reason: "transfer principal mismatch between legs".into(),
                    missing: format!("out {} vs in {principal}", alloc.principal),
                });
            }
            if alloc.asset != *asset || alloc.is_cash != engine.is_cash(asset) {
                return Err(PendingReason {
                    reason: "transfer legs reference different assets (待核对)".into(),
                    missing: format!("transfer_group {group}"),
                });
            }
            if alloc.source_account == e.account_id {
                return Err(PendingReason {
                    reason: "transfer inbound leg on the same account as outbound (待核对)".into(),
                    missing: format!("transfer_group {group}"),
                });
            }
            let is_cash = alloc.is_cash;
            let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
            if is_cash {
                let current = ledger.cash_of(asset);
                let next = checked_add(current, principal).map_err(|err| {
                    PendingReason::invalid(format!("transfer cash overflow: {err}"))
                })?;
                ledger.cash.insert(asset.clone(), next);
            } else {
                let holding = ledger.holdings.entry(asset.clone()).or_default();
                for lot in &alloc.moved_lots {
                    holding.lots.push(CostLot {
                        source_event: e.id.clone(),
                        source_instrument: lot.source_instrument.clone(),
                        quantity: lot.quantity,
                        cost_total: lot.cost_total,
                        cost_currency: lot.cost_currency.clone(),
                    });
                }
                if holding.cost_currency.is_none() {
                    if let Some(first) = alloc.moved_lots.first() {
                        holding.cost_currency = Some(first.cost_currency.clone());
                    }
                }
            }
            let book = alloc.book_amount;
            let mut rows = vec![
                Posting {
                    category: if is_cash {
                        LedgerCategory::Cash
                    } else {
                        LedgerCategory::Holding
                    },
                    sub: Some(format!("transfer_in:{group}")),
                    asset: asset.clone(),
                    native_quantity: principal,
                    book_amount: book,
                },
                Posting {
                    category: LedgerCategory::TransferClearing,
                    sub: Some(format!("in:{group}")),
                    asset: asset.clone(),
                    native_quantity: -principal,
                    book_amount: -book,
                },
            ];
            if let Some(f) = fee {
                rows.extend(fee_cash_rows(engine, e, f, fx)?);
            }
            post(e, &engine.book_currency, rows)
                .map_err(|err| PendingReason::invalid(err.to_string()))
        }
    }
}

/// Fee paid from cash on a transfer leg: expense + cash credit rows.
fn fee_cash_rows(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    f: &Fee,
    fx: &dyn FxResolver,
) -> Result<Vec<Posting>, PendingReason> {
    if !engine.is_cash(&f.asset) {
        // Third-asset fee on transfers uses the same path as trade fees.
        let ccy = currency_of(&f.asset);
        let book_ccy = engine.book_currency.clone();
        let (rate, _) = fx
            .rate(&ccy, &book_ccy, e.occurred_at)
            .ok_or_else(|| PendingReason::missing_fee_price(&ccy))?;
        let value = checked_mul(f.amount, rate)
            .map_err(|err| PendingReason::invalid(format!("fee value overflow: {err}")))?;
        return dispose_fee_asset(engine, e, &f.asset, f.amount, value, &book_ccy, fx, false);
    }
    let ccy = currency_of(&f.asset);
    let book = to_book(f.amount, &ccy, e.occurred_at, fx, &engine.book_currency)
        .ok_or_else(|| PendingReason::missing_fx(&ccy))?;
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    let cash = ledger.cash_of(&f.asset);
    if cash < f.amount {
        return Err(PendingReason::oversell(&f.asset.0, cash, f.amount));
    }
    ledger.cash.insert(f.asset.clone(), cash - f.amount);
    let g = ledger.fees.valued_fees.entry(f.asset.clone()).or_default();
    *g = checked_add(*g, book)
        .map_err(|err| PendingReason::invalid(format!("fee total overflow: {err}")))?;
    Ok(vec![
        Posting {
            category: LedgerCategory::FeeExpense,
            sub: Some("transfer_fee".into()),
            asset: f.asset.clone(),
            native_quantity: -f.amount,
            book_amount: book,
        },
        Posting {
            category: LedgerCategory::Cash,
            sub: Some("transfer_fee".into()),
            asset: f.asset.clone(),
            native_quantity: -f.amount,
            book_amount: -book,
        },
    ])
}

fn apply_stock_split(
    engine: &mut LedgerEngine,
    e: &EconomicEvent,
    instruments: &dyn InstrumentResolver,
    instrument: &InstrumentId,
    ratio_num: i64,
    ratio_den: i64,
) -> Result<JournalTransaction, PendingReason> {
    if ratio_num <= 0 || ratio_den <= 0 {
        return Err(PendingReason::invalid("split ratio must be positive"));
    }
    let base = instruments
        .base_asset(instrument)
        .ok_or_else(|| PendingReason::invalid(format!("unknown instrument {instrument}")))?;
    let ledger = engine.accounts.entry(e.account_id.clone()).or_default();
    let holding = ledger
        .holdings
        .get_mut(&base)
        .ok_or_else(|| PendingReason::invalid(format!("no holdings to split for {base}")))?;
    if holding.quantity() == Decimal::ZERO {
        return Err(PendingReason::invalid(format!(
            "no holdings to split for {base}"
        )));
    }
    // Total cost unchanged; quantity scales; unit cost scales down.
    for lot in &mut holding.lots {
        lot.quantity = checked_mul(lot.quantity, Decimal::from(ratio_num))
            .and_then(|q| checked_div(q, Decimal::from(ratio_den)))
            .map_err(|err| PendingReason::invalid(format!("split overflow: {err}")))?;
    }
    // No monetary effect: record the corporate action with zero rows.
    post(
        e,
        &engine.book_currency,
        vec![
            Posting {
                category: LedgerCategory::Holding,
                sub: Some(format!("split:{instrument}")),
                asset: base.clone(),
                native_quantity: Decimal::ZERO,
                book_amount: Decimal::ZERO,
            },
            Posting {
                category: LedgerCategory::Equity,
                sub: Some(format!("split:{instrument}")),
                asset: base.clone(),
                native_quantity: Decimal::ZERO,
                book_amount: Decimal::ZERO,
            },
        ],
    )
    .map_err(|err| PendingReason::invalid(err.to_string()))
}

/// FIFO-take whole lots out of a holding (used by transfers). Removed lots
/// keep their cost identity so the inbound leg can re-create them.
fn take_lots_fifo(holding: &mut HoldingState, quantity: Decimal) -> DomainResult<Vec<CostLot>> {
    let total = holding.quantity();
    if quantity > total {
        return Err(DomainError::Oversell {
            instrument: String::new(),
            available: total.to_string(),
            requested: quantity.to_string(),
        });
    }
    let mut remaining = quantity;
    let mut moved = Vec::new();
    while remaining > Decimal::ZERO {
        let lot = &mut holding.lots[0];
        let take = if lot.quantity <= remaining {
            lot.quantity
        } else {
            remaining
        };
        if take == lot.quantity {
            moved.push(holding.lots.remove(0));
        } else {
            let ct = lot.cost_total;
            let partial = CostLot {
                source_event: lot.source_event.clone(),
                source_instrument: lot.source_instrument.clone(),
                quantity: take,
                cost_total: match ct {
                    Some(c) => Some(checked_div(checked_mul(c, take)?, lot.quantity)?),
                    None => None,
                },
                cost_currency: lot.cost_currency.clone(),
            };
            moved.push(partial);
            lot.quantity = checked_sub(lot.quantity, take)?;
            if let Some(c) = lot.cost_total.as_mut() {
                *c = checked_sub(
                    *c,
                    checked_div(checked_mul(*c, take)?, lot.quantity + take)?,
                )?;
            }
        }
        remaining = checked_sub(remaining, take)?;
    }
    Ok(moved)
}

/// Convenience: resolve effective events and apply them.
pub fn apply_effective(
    engine: &mut LedgerEngine,
    recorded: &[EconomicEvent],
    fx: &dyn FxResolver,
    instruments: &dyn InstrumentResolver,
) -> Vec<(EventId, ApplyOutcome)> {
    let view = Corrections::resolve(recorded);
    let effective = view.effective_events(recorded);
    engine.apply_all(&effective, fx, instruments)
}
