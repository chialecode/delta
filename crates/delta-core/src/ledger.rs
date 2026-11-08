use crate::model::{AdjustmentKind, BarRecord, CorporateActionRecord, CorporateActionType};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LedgerError {
    #[error("unbalanced journal transaction {tx_id}: sum of book_amount is {sum}")]
    UnbalancedTransaction { tx_id: String, sum: Decimal },
    #[error("oversell rejected in spot mode for account {account_id}, asset {asset_id}: attempted {attempted}, available {available}")]
    OversellRejected {
        account_id: String,
        asset_id: String,
        attempted: Decimal,
        available: Decimal,
    },
    #[error("negative liability trick rejected for account {account_id}: use explicit Liability asset kind")]
    InvalidNegativeAsset { account_id: String },
    #[error(
        "missing book currency FX rate for {currency} at {occurred_at}: event enters pending state"
    )]
    MissingBookFx {
        currency: String,
        occurred_at: String,
    },
    #[error("invalid quantity or price: {0}")]
    InvalidAmount(String),
    #[error("unknown event or fill reference: {0}")]
    NotFound(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EconomicEventType {
    OpeningBalance,
    Buy,
    Sell,
    CashExchange,
    Deposit,
    Withdrawal,
    TransferOut,
    TransferIn,
    PositionTransferOut,
    PositionTransferIn,
    CashDividend,
    Interest,
    Split,
    FeeOnly,
    Reversal,
}

impl EconomicEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpeningBalance => "opening_balance",
            Self::Buy => "buy",
            Self::Sell => "sell",
            Self::CashExchange => "cash_exchange",
            Self::Deposit => "deposit",
            Self::Withdrawal => "withdrawal",
            Self::TransferOut => "transfer_out",
            Self::TransferIn => "transfer_in",
            Self::PositionTransferOut => "position_transfer_out",
            Self::PositionTransferIn => "position_transfer_in",
            Self::CashDividend => "cash_dividend",
            Self::Interest => "interest",
            Self::Split => "split",
            Self::FeeOnly => "fee_only",
            Self::Reversal => "reversal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerCategory {
    Cash,
    HoldingCost,
    RealizedPnl,
    DividendIncome,
    InterestIncome,
    FeeExpense,
    EquityCapital,
    ClearingTransit,
    Liability,
}

impl LedgerCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::HoldingCost => "holding_cost",
            Self::RealizedPnl => "realized_pnl",
            Self::DividendIncome => "dividend_income",
            Self::InterestIncome => "interest_income",
            Self::FeeExpense => "fee_expense",
            Self::EquityCapital => "equity_capital",
            Self::ClearingTransit => "clearing_transit",
            Self::Liability => "liability",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    pub id: String,
    pub transaction_id: String,
    pub ledger_account_id: String,
    pub category: LedgerCategory,
    pub native_asset_id: String,
    pub native_quantity: Decimal,
    /// Signed amount in library book_currency. Sum across a transaction MUST be exactly 0.
    pub book_amount: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalTransaction {
    pub id: String,
    pub event_id: String,
    pub book_currency: String,
    pub posting_rule_version: String,
    pub reversal_of: Option<String>,
    pub postings: Vec<Posting>,
}

impl JournalTransaction {
    pub fn verify_balanced(&self) -> Result<(), LedgerError> {
        let sum: Decimal = self.postings.iter().map(|p| p.book_amount).sum();
        if !sum.is_zero() {
            return Err(LedgerError::UnbalancedTransaction {
                tx_id: self.id.clone(),
                sum,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillFee {
    pub id: String,
    pub fill_id: String,
    pub amount: Decimal,
    pub currency: String,
    pub category: String,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub id: String,
    pub event_id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub base_asset_id: String,
    pub quote_currency: String,
    pub side: FillSide,
    pub quantity: Decimal,
    pub price: Decimal,
    pub fees: Vec<FillFee>,
    pub occurred_at: DateTime<Utc>,
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostLot {
    pub id: String,
    pub account_id: String,
    pub asset_id: String,
    pub instrument_id: Option<String>,
    pub source_fill_id: String,
    pub acquired_at: DateTime<Utc>,
    pub original_quantity: Decimal,
    pub remaining_quantity: Decimal,
    pub original_cost_book: Decimal,
    pub remaining_cost_book: Decimal,
    pub cost_known: bool,
    pub ledger_revision: i64,
}

impl CostLot {
    pub fn unit_cost(&self) -> Option<Decimal> {
        if !self.cost_known || self.remaining_quantity.is_zero() {
            None
        } else {
            Some(self.remaining_cost_book / self.remaining_quantity)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LotAllocation {
    pub id: String,
    pub sell_fill_id: String,
    pub lot_id: String,
    pub quantity: Decimal,
    pub cost_book: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldingProjection {
    pub account_id: String,
    pub asset_id: String,
    pub quantity: Decimal,
    pub carrying_cost: Decimal,
    pub cost_known: bool,
    pub ledger_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEventInput {
    pub event_id: String,
    pub account_id: String,
    pub event_type: EconomicEventType,
    pub occurred_at: DateTime<Utc>,
    pub sequence_no: i64,
    pub asset_id: String,
    pub instrument_id: Option<String>,
    pub quantity: Decimal,
    pub price: Option<Decimal>,
    pub quote_currency: String,
    pub fee_amount: Decimal,
    pub fee_currency: String,
    pub cost_known: bool,
    pub counterparty_account_id: Option<String>,
    pub transfer_group_id: Option<String>,
    pub correction_group_id: Option<String>,
    pub revision: i64,
    pub active_in_correction_group: bool,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerProjectionResult {
    pub book_currency: String,
    pub ledger_revision: i64,
    pub cash_balances: BTreeMap<(String, String), Decimal>,
    pub cash_carrying_costs: BTreeMap<(String, String), Decimal>,
    pub holdings: BTreeMap<(String, String), HoldingProjection>,
    pub open_lots: Vec<CostLot>,
    pub allocations: Vec<LotAllocation>,
    pub transactions: Vec<JournalTransaction>,
    pub realized_pnl_by_account: BTreeMap<String, Decimal>,
    pub realized_fx_pnl_by_account: BTreeMap<String, Decimal>,
    pub dividend_income_by_account: BTreeMap<String, Decimal>,
    pub fee_expense_by_account: BTreeMap<String, Decimal>,
    pub external_inflows_by_account: BTreeMap<String, Decimal>,
    pub external_outflows_by_account: BTreeMap<String, Decimal>,
    pub internal_transfer_fees: Decimal,
    pub pending_valuation_reasons: Vec<String>,
    pub unknown_cost_assets: Vec<(String, String)>,
    pub external_cash_flows: Vec<ExternalCashFlow>,
    pub in_transit_assets: Vec<InTransitAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCashFlow {
    pub account_id: String,
    pub counterparty_account_id: Option<String>,
    pub internal_candidate: bool,
    pub occurred_at: DateTime<Utc>,
    pub native_currency: String,
    /// Positive for capital invested, negative for capital withdrawn.
    pub native_amount: Decimal,
    pub book_amount: Decimal,
    pub position_asset_id: Option<String>,
    pub position_quantity: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InTransitAsset {
    pub source_account: String,
    pub destination_account: String,
    pub asset_id: String,
    pub quote_currency: String,
    pub quantity: Decimal,
    pub carrying_cost: Decimal,
    pub cost_known: bool,
    pub is_cash: bool,
}

/// Pure deterministic FIFO ledger engine supporting multi-currency, third-asset fees,
/// cross-account cash & position transfers, splits/dividends, and reversal corrections.
pub fn replay_ledger_events(
    book_currency: &str,
    events: &[LedgerEventInput],
    fx_lookup: &HashMap<(String, NaiveDate), Decimal>,
    ledger_revision: i64,
) -> Result<LedgerProjectionResult, LedgerError> {
    replay_ledger_events_with_bindings(
        book_currency,
        events,
        fx_lookup,
        &HashMap::new(),
        ledger_revision,
    )
}

pub fn replay_ledger_events_with_bindings(
    book_currency: &str,
    events: &[LedgerEventInput],
    fx_lookup: &HashMap<(String, NaiveDate), Decimal>,
    event_fx_bindings: &HashMap<(String, String), Decimal>,
    ledger_revision: i64,
) -> Result<LedgerProjectionResult, LedgerError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        replay_ledger_events_inner(
            book_currency,
            events,
            fx_lookup,
            event_fx_bindings,
            ledger_revision,
        )
    }))
    .unwrap_or_else(|_| {
        Err(LedgerError::InvalidAmount(
            "financial arithmetic exceeds supported Decimal range".into(),
        ))
    })
}

fn replay_ledger_events_inner(
    book_currency: &str,
    events: &[LedgerEventInput],
    fx_lookup: &HashMap<(String, NaiveDate), Decimal>,
    event_fx_bindings: &HashMap<(String, String), Decimal>,
    ledger_revision: i64,
) -> Result<LedgerProjectionResult, LedgerError> {
    let mut sorted_events: Vec<&LedgerEventInput> = events
        .iter()
        .filter(|e| e.active_in_correction_group)
        .collect();
    sorted_events.sort_by(|a, b| {
        let transfer_order = |event: &LedgerEventInput| match event.event_type {
            EconomicEventType::TransferOut | EconomicEventType::PositionTransferOut => 1,
            EconomicEventType::TransferIn | EconomicEventType::PositionTransferIn => 2,
            _ => 0,
        };
        a.occurred_at
            .cmp(&b.occurred_at)
            .then_with(|| transfer_order(a).cmp(&transfer_order(b)))
            .then_with(|| a.sequence_no.cmp(&b.sequence_no))
            .then_with(|| a.event_id.cmp(&b.event_id))
    });

    let mut cash_balances: BTreeMap<(String, String), Decimal> = BTreeMap::new();
    let mut lots_by_account_asset: BTreeMap<(String, String), Vec<CostLot>> = BTreeMap::new();
    let mut allocations: Vec<LotAllocation> = Vec::new();
    let mut transactions: Vec<JournalTransaction> = Vec::new();
    let mut realized_pnl_by_account: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut realized_fx_pnl_by_account: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut dividend_income_by_account: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut fee_expense_by_account: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut external_inflows_by_account: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut external_outflows_by_account: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut internal_transfer_fees = Decimal::ZERO;
    let mut pending_valuation_reasons: Vec<String> = Vec::new();
    let mut unknown_cost_assets: Vec<(String, String)> = Vec::new();
    let mut external_cash_flows = Vec::new();
    let mut position_transit: Vec<(&LedgerEventInput, Vec<CostLot>)> = Vec::new();

    let resolve_fx = |ccy: &str, date: NaiveDate| -> Option<Decimal> {
        if ccy.eq_ignore_ascii_case(book_currency) {
            return Some(Decimal::ONE);
        }
        let key = (ccy.to_ascii_uppercase(), date);
        if let Some(rate) = fx_lookup.get(&key) {
            return (*rate > Decimal::ZERO).then_some(*rate);
        }
        if !crate::valuation::is_fiat_currency(ccy) {
            return None;
        }
        // Check carried forward up to 4 days for fiat only (S-07)
        for days_back in 1..=4 {
            if let Some(prev_date) = date.checked_sub_days(chrono::Days::new(days_back)) {
                if let Some(rate) = fx_lookup.get(&(ccy.to_ascii_uppercase(), prev_date)) {
                    return (*rate > Decimal::ZERO).then_some(*rate);
                }
            }
        }
        None
    };

    // Helper to allocate FIFO lots for a disposal (sell or third-asset fee disposal or position transfer)
    let allocate_fifo = |lots: &mut Vec<CostLot>,
                         account_id: &str,
                         asset_id: &str,
                         sell_fill_id: &str,
                         qty_needed: Decimal,
                         allocations_out: &mut Vec<LotAllocation>|
     -> Result<(Decimal, bool, Vec<CostLot>), LedgerError> {
        let total_avail: Decimal = lots.iter().map(|l| l.remaining_quantity).sum();
        if total_avail < qty_needed {
            return Err(LedgerError::OversellRejected {
                account_id: account_id.to_string(),
                asset_id: asset_id.to_string(),
                attempted: qty_needed,
                available: total_avail,
            });
        }
        let mut remaining_to_alloc = qty_needed;
        let mut total_cost_allocated = Decimal::ZERO;
        let mut all_cost_known = true;
        let mut transferred_slices = Vec::new();

        for (idx, lot) in lots.iter_mut().enumerate() {
            if remaining_to_alloc.is_zero() {
                break;
            }
            if lot.remaining_quantity.is_zero() {
                continue;
            }
            if !lot.cost_known {
                all_cost_known = false;
            }
            let take_qty = remaining_to_alloc.min(lot.remaining_quantity);
            let alloc_cost = if take_qty == lot.remaining_quantity {
                // Absorb any remainder on the final portion of the lot (Financial Engine Sec 2)
                lot.remaining_cost_book
            } else {
                let raw_cost = (lot.remaining_cost_book * take_qty) / lot.remaining_quantity;
                raw_cost.round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero)
            };

            lot.remaining_quantity -= take_qty;
            lot.remaining_cost_book -= alloc_cost;
            remaining_to_alloc -= take_qty;
            total_cost_allocated += alloc_cost;

            allocations_out.push(LotAllocation {
                id: format!("ALLOC:{}:{idx}", sell_fill_id),
                sell_fill_id: sell_fill_id.to_string(),
                lot_id: lot.id.clone(),
                quantity: take_qty,
                cost_book: alloc_cost,
            });

            transferred_slices.push(CostLot {
                id: format!("{}:XFER:{idx}", lot.id),
                account_id: account_id.to_string(),
                asset_id: asset_id.to_string(),
                instrument_id: lot.instrument_id.clone(),
                source_fill_id: lot.source_fill_id.clone(),
                acquired_at: lot.acquired_at,
                original_quantity: take_qty,
                remaining_quantity: take_qty,
                original_cost_book: alloc_cost,
                remaining_cost_book: alloc_cost,
                cost_known: lot.cost_known,
                ledger_revision,
            });
        }
        lots.retain(|l| !l.remaining_quantity.is_zero());
        Ok((total_cost_allocated, all_cost_known, transferred_slices))
    };

    for ev in sorted_events {
        let ev_date = ev.occurred_at.date_naive();
        if (ev.quantity <= Decimal::ZERO && ev.event_type != EconomicEventType::Reversal)
            || ev.fee_amount < Decimal::ZERO
            || ev.price.is_some_and(|p| p < Decimal::ZERO)
        {
            return Err(LedgerError::InvalidAmount(ev.event_id.clone()));
        }
        if ev.event_type == EconomicEventType::OpeningBalance
            && ev.instrument_id.is_some()
            && ev.cost_known
            && ev.price.is_none()
        {
            return Err(LedgerError::InvalidAmount(
                "known opening position requires an explicit unit cost".into(),
            ));
        }
        if ev.fee_amount > Decimal::ZERO
            && !ev.fee_currency.eq_ignore_ascii_case(&ev.quote_currency)
            && !matches!(
                ev.event_type,
                EconomicEventType::Buy | EconomicEventType::Sell | EconomicEventType::CashExchange
            )
        {
            return Err(LedgerError::InvalidAmount(
                "non-trade fees in another asset require an explicit fee event".into(),
            ));
        }
        if ev.fee_amount > Decimal::ZERO
            && matches!(
                ev.event_type,
                EconomicEventType::Deposit
                    | EconomicEventType::OpeningBalance
                    | EconomicEventType::TransferIn
                    | EconomicEventType::PositionTransferIn
                    | EconomicEventType::PositionTransferOut
                    | EconomicEventType::Interest
                    | EconomicEventType::FeeOnly
                    | EconomicEventType::Split
            )
        {
            return Err(LedgerError::InvalidAmount(
                "this event requires its fee to be recorded as a separate fee event".into(),
            ));
        }
        let event_fx = |currency: &str, date: NaiveDate| {
            event_fx_bindings
                .get(&(ev.event_id.clone(), currency.to_ascii_uppercase()))
                .copied()
                .filter(|r| *r > Decimal::ZERO)
                .or_else(|| resolve_fx(currency, date))
        };
        let quote_fx =
            event_fx(&ev.quote_currency, ev_date).ok_or_else(|| LedgerError::MissingBookFx {
                currency: ev.quote_currency.clone(),
                occurred_at: ev.occurred_at.to_rfc3339(),
            })?;
        let before_capital = external_inflows_by_account
            .get(&ev.account_id)
            .copied()
            .unwrap_or_default()
            - external_outflows_by_account
                .get(&ev.account_id)
                .copied()
                .unwrap_or_default();

        match ev.event_type {
            EconomicEventType::OpeningBalance => {
                let cash_ccy = ev
                    .asset_id
                    .trim_start_matches("ASSET:FIAT:")
                    .to_ascii_uppercase();
                if ev.asset_id.eq_ignore_ascii_case(&ev.quote_currency)
                    || ev.asset_id.starts_with("ASSET:FIAT:")
                    || crate::valuation::is_fiat_currency(&cash_ccy)
                {
                    // The book amount uses the quote currency's event FX, so opening cash
                    // must be quoted in its own currency (1000 HKD is never 1000 USD).
                    if !cash_ccy.eq_ignore_ascii_case(&ev.quote_currency) {
                        return Err(LedgerError::InvalidAmount(format!(
                            "opening cash {cash_ccy} must be quoted in {cash_ccy}, got {}",
                            ev.quote_currency
                        )));
                    }
                    let ccy = cash_ccy;
                    *cash_balances
                        .entry((ev.account_id.clone(), ccy.clone()))
                        .or_insert(Decimal::ZERO) += ev.quantity;
                    let book_amt = (ev.quantity * quote_fx).normalize();
                    *external_inflows_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += book_amt;

                    let tx_id = format!("TX:{}", ev.event_id);
                    let tx = JournalTransaction {
                        id: tx_id.clone(),
                        event_id: ev.event_id.clone(),
                        book_currency: book_currency.to_string(),
                        posting_rule_version: "v1".to_string(),
                        reversal_of: None,
                        postings: vec![
                            Posting {
                                id: format!("P:{}:1", ev.event_id),
                                transaction_id: tx_id.clone(),
                                ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, ccy),
                                category: LedgerCategory::Cash,
                                native_asset_id: ccy.clone(),
                                native_quantity: ev.quantity,
                                book_amount: book_amt,
                            },
                            Posting {
                                id: format!("P:{}:2", ev.event_id),
                                transaction_id: tx_id,
                                ledger_account_id: format!("L:{}:EQUITY", ev.account_id),
                                category: LedgerCategory::EquityCapital,
                                native_asset_id: ccy,
                                native_quantity: -ev.quantity,
                                book_amount: -book_amt,
                            },
                        ],
                    };
                    tx.verify_balanced()?;
                    transactions.push(tx);
                } else {
                    // Opening position lot. A known cost needs an explicit unit cost even when
                    // the symbol is not registered; otherwise a zero cost would be fabricated.
                    if ev.cost_known && ev.price.is_none() {
                        return Err(LedgerError::InvalidAmount(
                            "known opening position requires an explicit unit cost".into(),
                        ));
                    }
                    let unit_price = ev.price.unwrap_or(Decimal::ZERO);
                    let cost_book = if ev.cost_known {
                        (ev.quantity * unit_price * quote_fx + ev.fee_amount * quote_fx).normalize()
                    } else {
                        unknown_cost_assets.push((ev.account_id.clone(), ev.asset_id.clone()));
                        Decimal::ZERO
                    };
                    lots_by_account_asset
                        .entry((ev.account_id.clone(), ev.asset_id.clone()))
                        .or_default()
                        .push(CostLot {
                            id: format!("LOT:{}", ev.event_id),
                            account_id: ev.account_id.clone(),
                            asset_id: ev.asset_id.clone(),
                            instrument_id: ev.instrument_id.clone(),
                            source_fill_id: ev.event_id.clone(),
                            acquired_at: ev.occurred_at,
                            original_quantity: ev.quantity,
                            remaining_quantity: ev.quantity,
                            original_cost_book: cost_book,
                            remaining_cost_book: cost_book,
                            cost_known: ev.cost_known,
                            ledger_revision,
                        });
                    let tx_id = format!("TX:{}", ev.event_id);
                    transactions.push(JournalTransaction {
                        id: tx_id.clone(),
                        event_id: ev.event_id.clone(),
                        book_currency: book_currency.into(),
                        posting_rule_version: "v2".into(),
                        reversal_of: None,
                        postings: vec![
                            Posting {
                                id: format!("P:{}:1", ev.event_id),
                                transaction_id: tx_id.clone(),
                                ledger_account_id: format!(
                                    "L:{}:HOLDING:{}",
                                    ev.account_id, ev.asset_id
                                ),
                                category: LedgerCategory::HoldingCost,
                                native_asset_id: ev.asset_id.clone(),
                                native_quantity: ev.quantity,
                                book_amount: cost_book,
                            },
                            Posting {
                                id: format!("P:{}:2", ev.event_id),
                                transaction_id: tx_id,
                                ledger_account_id: format!("L:{}:EQUITY", ev.account_id),
                                category: LedgerCategory::EquityCapital,
                                native_asset_id: ev.asset_id.clone(),
                                native_quantity: -ev.quantity,
                                book_amount: -cost_book,
                            },
                        ],
                    });
                    if ev.cost_known {
                        *external_inflows_by_account
                            .entry(ev.account_id.clone())
                            .or_default() += cost_book;
                    }
                }
            }
            EconomicEventType::Deposit => {
                let ccy = ev.quote_currency.to_ascii_uppercase();
                *cash_balances
                    .entry((ev.account_id.clone(), ccy.clone()))
                    .or_insert(Decimal::ZERO) += ev.quantity;
                let book_amt = (ev.quantity * quote_fx).normalize();
                *external_inflows_by_account
                    .entry(ev.account_id.clone())
                    .or_insert(Decimal::ZERO) += book_amt;

                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: ccy.clone(),
                            native_quantity: ev.quantity,
                            book_amount: book_amt,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!("L:{}:EQUITY", ev.account_id),
                            category: LedgerCategory::EquityCapital,
                            native_asset_id: ccy,
                            native_quantity: -ev.quantity,
                            book_amount: -book_amt,
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::Withdrawal => {
                let ccy = ev.quote_currency.to_ascii_uppercase();
                *cash_balances
                    .entry((ev.account_id.clone(), ccy.clone()))
                    .or_insert(Decimal::ZERO) -= ev.quantity + ev.fee_amount;
                let principal_book = (ev.quantity * quote_fx).normalize();
                let fee_book = (ev.fee_amount * quote_fx).normalize();
                *external_outflows_by_account
                    .entry(ev.account_id.clone())
                    .or_insert(Decimal::ZERO) += principal_book;
                if !fee_book.is_zero() {
                    *fee_expense_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += fee_book;
                }

                let tx_id = format!("TX:{}", ev.event_id);
                let mut postings = vec![
                    Posting {
                        id: format!("P:{}:1", ev.event_id),
                        transaction_id: tx_id.clone(),
                        ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, ccy),
                        category: LedgerCategory::Cash,
                        native_asset_id: ccy.clone(),
                        native_quantity: -(ev.quantity + ev.fee_amount),
                        book_amount: -(principal_book + fee_book),
                    },
                    Posting {
                        id: format!("P:{}:2", ev.event_id),
                        transaction_id: tx_id.clone(),
                        ledger_account_id: format!("L:{}:EQUITY", ev.account_id),
                        category: LedgerCategory::EquityCapital,
                        native_asset_id: ccy.clone(),
                        native_quantity: ev.quantity,
                        book_amount: principal_book,
                    },
                ];
                if !fee_book.is_zero() {
                    postings.push(Posting {
                        id: format!("P:{}:3", ev.event_id),
                        transaction_id: tx_id.clone(),
                        ledger_account_id: format!("L:{}:FEE", ev.account_id),
                        category: LedgerCategory::FeeExpense,
                        native_asset_id: ccy,
                        native_quantity: ev.fee_amount,
                        book_amount: fee_book,
                    });
                }
                let tx = JournalTransaction {
                    id: tx_id,
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings,
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::Buy => {
                if !ev.cost_known {
                    unknown_cost_assets.push((ev.account_id.clone(), ev.asset_id.clone()));
                }
                let price = ev
                    .price
                    .ok_or_else(|| LedgerError::InvalidAmount("Buy requires price".into()))?;
                let quote_ccy = ev.quote_currency.to_ascii_uppercase();
                let fee_ccy = ev.fee_currency.to_ascii_uppercase();
                let trade_quote_amt = ev.quantity * price;

                let fee_in_book = if ev.fee_amount.is_zero() {
                    Decimal::ZERO
                } else if fee_ccy == quote_ccy {
                    *cash_balances
                        .entry((ev.account_id.clone(), quote_ccy.clone()))
                        .or_insert(Decimal::ZERO) -= ev.fee_amount;
                    (ev.fee_amount * quote_fx).normalize()
                } else {
                    // Third-asset fee (AC-22): e.g. fee paid in BNB or OKB or another asset
                    let fee_fx_opt = event_fx(&fee_ccy, ev_date);
                    let fee_book_val = ev.fee_amount
                        * fee_fx_opt.ok_or_else(|| LedgerError::MissingBookFx {
                            currency: fee_ccy.clone(),
                            occurred_at: ev.occurred_at.to_rfc3339(),
                        })?;
                    // Reduce third-asset holding lot or cash balance
                    if let Some(lots) =
                        lots_by_account_asset.get_mut(&(ev.account_id.clone(), fee_ccy.clone()))
                    {
                        if !lots.is_empty() {
                            let (disposed_cost, known, _) = allocate_fifo(
                                lots,
                                &ev.account_id,
                                &fee_ccy,
                                &format!("{}:FEE", ev.event_id),
                                ev.fee_amount,
                                &mut allocations,
                            )?;
                            if known && fee_fx_opt.is_some() {
                                let fee_disp_pnl = fee_book_val - disposed_cost;
                                *realized_pnl_by_account
                                    .entry(ev.account_id.clone())
                                    .or_insert(Decimal::ZERO) += fee_disp_pnl;
                            }
                        } else {
                            *cash_balances
                                .entry((ev.account_id.clone(), fee_ccy.clone()))
                                .or_insert(Decimal::ZERO) -= ev.fee_amount;
                        }
                    } else {
                        *cash_balances
                            .entry((ev.account_id.clone(), fee_ccy.clone()))
                            .or_insert(Decimal::ZERO) -= ev.fee_amount;
                    }
                    fee_book_val
                };

                // The quote leg is settled only from cash; the cash FIFO pass below verifies
                // the balance and its historical cost. Also consuming quote-asset lots here
                // would deduct the same principal twice.
                *cash_balances
                    .entry((ev.account_id.clone(), quote_ccy.clone()))
                    .or_insert(Decimal::ZERO) -= trade_quote_amt;

                let principal_book = (trade_quote_amt * quote_fx).normalize();
                let total_cost_book = (principal_book + fee_in_book).normalize();
                if !fee_in_book.is_zero() {
                    *fee_expense_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += fee_in_book;
                }

                lots_by_account_asset
                    .entry((ev.account_id.clone(), ev.asset_id.clone()))
                    .or_default()
                    .push(CostLot {
                        id: format!("LOT:{}", ev.event_id),
                        account_id: ev.account_id.clone(),
                        asset_id: ev.asset_id.clone(),
                        instrument_id: ev.instrument_id.clone(),
                        source_fill_id: ev.event_id.clone(),
                        acquired_at: ev.occurred_at,
                        original_quantity: ev.quantity,
                        remaining_quantity: ev.quantity,
                        original_cost_book: total_cost_book,
                        remaining_cost_book: total_cost_book,
                        cost_known: ev.cost_known,
                        ledger_revision,
                    });

                // Balanced postings: Debit HoldingCost total_cost_book, Credit Cash total_cost_book
                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!(
                                "L:{}:HOLDING:{}",
                                ev.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::HoldingCost,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: ev.quantity,
                            book_amount: total_cost_book,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, quote_ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: quote_ccy,
                            native_quantity: -(trade_quote_amt
                                + if fee_ccy == ev.quote_currency.to_ascii_uppercase() {
                                    ev.fee_amount
                                } else {
                                    Decimal::ZERO
                                }),
                            book_amount: -total_cost_book,
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::Sell => {
                let price = ev
                    .price
                    .ok_or_else(|| LedgerError::InvalidAmount("Sell requires price".into()))?;
                let quote_ccy = ev.quote_currency.to_ascii_uppercase();
                let fee_ccy = ev.fee_currency.to_ascii_uppercase();
                let gross_quote_amt = ev.quantity * price;

                let fee_in_book = if ev.fee_amount.is_zero() {
                    Decimal::ZERO
                } else if fee_ccy == quote_ccy {
                    (ev.fee_amount * quote_fx).normalize()
                } else {
                    let fee_fx_opt = event_fx(&fee_ccy, ev_date);
                    let fee_book_val = ev.fee_amount
                        * fee_fx_opt.ok_or_else(|| LedgerError::MissingBookFx {
                            currency: fee_ccy.clone(),
                            occurred_at: ev.occurred_at.to_rfc3339(),
                        })?;
                    if let Some(lots) =
                        lots_by_account_asset.get_mut(&(ev.account_id.clone(), fee_ccy.clone()))
                    {
                        if !lots.is_empty() {
                            let (disposed_cost, known, _) = allocate_fifo(
                                lots,
                                &ev.account_id,
                                &fee_ccy,
                                &format!("{}:FEE", ev.event_id),
                                ev.fee_amount,
                                &mut allocations,
                            )?;
                            if known {
                                *realized_pnl_by_account
                                    .entry(ev.account_id.clone())
                                    .or_default() += fee_book_val - disposed_cost;
                            }
                        } else {
                            *cash_balances
                                .entry((ev.account_id.clone(), fee_ccy.clone()))
                                .or_default() -= ev.fee_amount;
                        }
                    } else {
                        *cash_balances
                            .entry((ev.account_id.clone(), fee_ccy.clone()))
                            .or_default() -= ev.fee_amount;
                    }
                    fee_book_val
                };

                let net_quote_proceeds = if fee_ccy == quote_ccy {
                    gross_quote_amt - ev.fee_amount
                } else {
                    gross_quote_amt
                };
                *cash_balances
                    .entry((ev.account_id.clone(), quote_ccy.clone()))
                    .or_insert(Decimal::ZERO) += net_quote_proceeds;

                let lots = lots_by_account_asset
                    .entry((ev.account_id.clone(), ev.asset_id.clone()))
                    .or_default();
                let (allocated_cost_book, cost_known, _) = allocate_fifo(
                    lots,
                    &ev.account_id,
                    &ev.asset_id,
                    &ev.event_id,
                    ev.quantity,
                    &mut allocations,
                )?;

                let gross_proceeds_book = (gross_quote_amt * quote_fx).normalize();
                let net_proceeds_book = (gross_proceeds_book - fee_in_book).normalize();
                if !fee_in_book.is_zero() {
                    *fee_expense_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += fee_in_book;
                }

                let realized_pnl = if cost_known {
                    let pnl = (net_proceeds_book - allocated_cost_book).normalize();
                    *realized_pnl_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += pnl;
                    pnl
                } else {
                    pending_valuation_reasons.push(format!(
                        "sell of {} with unknown cost basis (event {})",
                        ev.asset_id, ev.event_id
                    ));
                    Decimal::ZERO
                };

                // Balanced postings: Debit Cash net_proceeds_book, Credit HoldingCost allocated_cost_book, Credit RealizedPnl realized_pnl
                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, quote_ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: quote_ccy,
                            native_quantity: net_quote_proceeds,
                            book_amount: net_proceeds_book,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!(
                                "L:{}:HOLDING:{}",
                                ev.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::HoldingCost,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: -ev.quantity,
                            book_amount: -allocated_cost_book,
                        },
                        Posting {
                            id: format!("P:{}:3", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!(
                                "L:{}:{}",
                                ev.account_id,
                                if cost_known { "PNL" } else { "COST_PENDING" }
                            ),
                            category: if cost_known {
                                LedgerCategory::RealizedPnl
                            } else {
                                LedgerCategory::ClearingTransit
                            },
                            native_asset_id: book_currency.to_string(),
                            native_quantity: -realized_pnl,
                            book_amount: if cost_known {
                                -realized_pnl
                            } else {
                                allocated_cost_book - net_proceeds_book
                            },
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::TransferOut => {
                // Cash transfer out from account (AC-04): principal + separate fee
                let ccy = ev.quote_currency.to_ascii_uppercase();
                let total_out = ev.quantity + ev.fee_amount;
                *cash_balances
                    .entry((ev.account_id.clone(), ccy.clone()))
                    .or_insert(Decimal::ZERO) -= total_out;
                let principal_book = (ev.quantity * quote_fx).normalize();
                let fee_book = (ev.fee_amount * quote_fx).normalize();

                *external_outflows_by_account
                    .entry(ev.account_id.clone())
                    .or_insert(Decimal::ZERO) += principal_book;
                if !fee_book.is_zero() {
                    internal_transfer_fees += fee_book;
                    *fee_expense_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += fee_book;
                }

                let tx_id = format!("TX:{}", ev.event_id);
                let mut postings = vec![
                    Posting {
                        id: format!("P:{}:1", ev.event_id),
                        transaction_id: tx_id.clone(),
                        ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, ccy),
                        category: LedgerCategory::Cash,
                        native_asset_id: ccy.clone(),
                        native_quantity: -total_out,
                        book_amount: -(principal_book + fee_book),
                    },
                    Posting {
                        id: format!("P:{}:2", ev.event_id),
                        transaction_id: tx_id.clone(),
                        ledger_account_id: format!("L:{}:TRANSIT", ev.account_id),
                        category: LedgerCategory::ClearingTransit,
                        native_asset_id: ccy.clone(),
                        native_quantity: ev.quantity,
                        book_amount: principal_book,
                    },
                ];
                if !fee_book.is_zero() {
                    postings.push(Posting {
                        id: format!("P:{}:3", ev.event_id),
                        transaction_id: tx_id.clone(),
                        ledger_account_id: format!("L:{}:FEE", ev.account_id),
                        category: LedgerCategory::FeeExpense,
                        native_asset_id: ccy,
                        native_quantity: ev.fee_amount,
                        book_amount: fee_book,
                    });
                }
                let tx = JournalTransaction {
                    id: tx_id,
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings,
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::TransferIn => {
                let ccy = ev.quote_currency.to_ascii_uppercase();
                *cash_balances
                    .entry((ev.account_id.clone(), ccy.clone()))
                    .or_insert(Decimal::ZERO) += ev.quantity;
                let principal_book = (ev.quantity * quote_fx).normalize();
                *external_inflows_by_account
                    .entry(ev.account_id.clone())
                    .or_insert(Decimal::ZERO) += principal_book;

                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: ccy.clone(),
                            native_quantity: ev.quantity,
                            book_amount: principal_book,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!("L:{}:TRANSIT", ev.account_id),
                            category: LedgerCategory::ClearingTransit,
                            native_asset_id: ccy,
                            native_quantity: -ev.quantity,
                            book_amount: -principal_book,
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::PositionTransferOut => {
                // Cross-account security/crypto transfer (AC-22): move FIFO lots to counterparty without realizing gain
                let lots = lots_by_account_asset
                    .entry((ev.account_id.clone(), ev.asset_id.clone()))
                    .or_default();
                let (moved_cost, _known, slices) = allocate_fifo(
                    lots,
                    &ev.account_id,
                    &ev.asset_id,
                    &ev.event_id,
                    ev.quantity,
                    &mut allocations,
                )?;
                if ev.counterparty_account_id.is_none() {
                    return Err(LedgerError::InvalidAmount(
                        "position transfer requires a destination account".into(),
                    ));
                }
                position_transit.push((ev, slices));
                *external_outflows_by_account
                    .entry(ev.account_id.clone())
                    .or_default() += moved_cost;
                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!(
                                "L:{}:HOLDING:{}",
                                ev.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::HoldingCost,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: -ev.quantity,
                            book_amount: -moved_cost,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!(
                                "L:{}:TRANSIT:{}",
                                ev.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::ClearingTransit,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: ev.quantity,
                            book_amount: moved_cost,
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::PositionTransferIn => {
                let index = position_transit.iter().position(|(out, _)| out.counterparty_account_id.as_deref() == Some(&ev.account_id) && ev.counterparty_account_id.as_deref() == Some(&out.account_id) && out.asset_id == ev.asset_id && out.quantity == ev.quantity && (ev.transfer_group_id.is_none() || ev.transfer_group_id == out.transfer_group_id)).ok_or_else(|| LedgerError::NotFound("position transfer-in requires a matching source transfer-out; use opening_balance for external holdings".into()))?;
                let (out, mut slices) = position_transit.remove(index);
                let cost: Decimal = slices.iter().map(|lot| lot.remaining_cost_book).sum();
                if slices.iter().any(|lot| !lot.cost_known) {
                    unknown_cost_assets.push((ev.account_id.clone(), ev.asset_id.clone()));
                }
                for slice in &mut slices {
                    slice.account_id = ev.account_id.clone();
                    slice.id = format!("{}:{}", slice.id, ev.event_id);
                }
                lots_by_account_asset
                    .entry((ev.account_id.clone(), ev.asset_id.clone()))
                    .or_default()
                    .extend(slices);
                *external_inflows_by_account
                    .entry(ev.account_id.clone())
                    .or_default() += cost;
                let tx_id = format!("TX:{}", ev.event_id);
                transactions.push(JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.into(),
                    posting_rule_version: "v3".into(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!(
                                "L:{}:HOLDING:{}",
                                ev.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::HoldingCost,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: ev.quantity,
                            book_amount: cost,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!(
                                "L:{}:TRANSIT:{}",
                                out.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::ClearingTransit,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: -ev.quantity,
                            book_amount: -cost,
                        },
                    ],
                });
            }
            EconomicEventType::CashDividend => {
                // Cash dividend recorded on pay_date (AC-05 / A-05)
                let ccy = ev.quote_currency.to_ascii_uppercase();
                let net_div = ev.quantity - ev.fee_amount;
                *cash_balances
                    .entry((ev.account_id.clone(), ccy.clone()))
                    .or_insert(Decimal::ZERO) += net_div;
                let net_div_book = (net_div * quote_fx).normalize();
                *dividend_income_by_account
                    .entry(ev.account_id.clone())
                    .or_insert(Decimal::ZERO) += net_div_book;

                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v1".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: ccy.clone(),
                            native_quantity: net_div,
                            book_amount: net_div_book,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!("L:{}:DIV:{}", ev.account_id, ev.asset_id),
                            category: LedgerCategory::DividendIncome,
                            native_asset_id: ccy,
                            native_quantity: -net_div,
                            book_amount: -net_div_book,
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::Split => {
                // Split ratio is stored in ev.quantity (e.g. 2 for 2:1, 4 for 4:1):
                // Quantity multiplies by ratio, total cost basis remains strictly unchanged (AC-05)
                let ratio = ev.quantity;
                if ratio <= Decimal::ZERO {
                    return Err(LedgerError::InvalidAmount(format!(
                        "invalid split ratio {ratio}"
                    )));
                }
                if let Some(lots) =
                    lots_by_account_asset.get_mut(&(ev.account_id.clone(), ev.asset_id.clone()))
                {
                    for lot in lots.iter_mut() {
                        lot.original_quantity = (lot.original_quantity * ratio).normalize();
                        lot.remaining_quantity = (lot.remaining_quantity * ratio).normalize();
                        // remaining_cost_book is UNCHANGED!
                    }
                }
            }
            EconomicEventType::FeeOnly
                if lots_by_account_asset
                    .get(&(ev.account_id.clone(), ev.asset_id.clone()))
                    .is_some_and(|lots| !lots.is_empty()) =>
            {
                if !ev.asset_id.eq_ignore_ascii_case(&ev.quote_currency) {
                    return Err(LedgerError::InvalidAmount(
                        "an asset fee event must quote the paid asset currency".into(),
                    ));
                }
                let lots = lots_by_account_asset
                    .get_mut(&(ev.account_id.clone(), ev.asset_id.clone()))
                    .unwrap();
                let (cost, known, _) = allocate_fifo(
                    lots,
                    &ev.account_id,
                    &ev.asset_id,
                    &ev.event_id,
                    ev.quantity,
                    &mut allocations,
                )?;
                let value = ev.quantity * quote_fx;
                *fee_expense_by_account
                    .entry(ev.account_id.clone())
                    .or_default() += value;
                if known {
                    *realized_pnl_by_account
                        .entry(ev.account_id.clone())
                        .or_default() += value - cost;
                } else {
                    unknown_cost_assets.push((ev.account_id.clone(), ev.asset_id.clone()));
                }
                let tx_id = format!("TX:{}", ev.event_id);
                transactions.push(JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.into(),
                    posting_rule_version: "v3".into(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!(
                                "L:{}:HOLDING:{}",
                                ev.account_id, ev.asset_id
                            ),
                            category: LedgerCategory::HoldingCost,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: -ev.quantity,
                            book_amount: -cost,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!("L:{}:FEE", ev.account_id),
                            category: LedgerCategory::FeeExpense,
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: ev.quantity,
                            book_amount: value,
                        },
                        Posting {
                            id: format!("P:{}:3", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!(
                                "L:{}:{}",
                                ev.account_id,
                                if known { "PNL" } else { "COST_PENDING" }
                            ),
                            category: if known {
                                LedgerCategory::RealizedPnl
                            } else {
                                LedgerCategory::ClearingTransit
                            },
                            native_asset_id: ev.asset_id.clone(),
                            native_quantity: Decimal::ZERO,
                            book_amount: cost - value,
                        },
                    ],
                });
            }
            EconomicEventType::Interest | EconomicEventType::FeeOnly => {
                let is_fee = ev.event_type == EconomicEventType::FeeOnly;
                let qty = if is_fee { -ev.quantity } else { ev.quantity };
                *cash_balances
                    .entry((ev.account_id.clone(), ev.quote_currency.clone()))
                    .or_default() += qty;
                let amount = qty * quote_fx;
                if is_fee {
                    *fee_expense_by_account
                        .entry(ev.account_id.clone())
                        .or_default() -= amount;
                } else {
                    *dividend_income_by_account
                        .entry(ev.account_id.clone())
                        .or_default() += amount;
                }
                let tx_id = format!("TX:{}", ev.event_id);
                transactions.push(JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.into(),
                    posting_rule_version: "v2".into(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!(
                                "L:{}:CASH:{}",
                                ev.account_id, ev.quote_currency
                            ),
                            category: LedgerCategory::Cash,
                            native_asset_id: ev.quote_currency.clone(),
                            native_quantity: qty,
                            book_amount: amount,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!("L:{}:INCOME", ev.account_id),
                            category: if is_fee {
                                LedgerCategory::FeeExpense
                            } else {
                                LedgerCategory::InterestIncome
                            },
                            native_asset_id: ev.quote_currency.clone(),
                            native_quantity: -qty,
                            book_amount: -amount,
                        },
                    ],
                });
            }
            EconomicEventType::CashExchange => {
                // LEDGER-01: Cash / stablecoin exchange (e.g. USD -> USDT or USDT -> USD).
                // Acquires `ev.quantity` of `target_ccy` (`ev.asset_id`) at `price` units of
                // `source_ccy` (`ev.quote_currency`) per 1 unit of `target_ccy`.
                let price = ev.price.ok_or_else(|| {
                    LedgerError::InvalidAmount("CashExchange requires exchange rate/price".into())
                })?;
                if price <= Decimal::ZERO {
                    return Err(LedgerError::InvalidAmount(
                        "CashExchange rate must be positive".into(),
                    ));
                }
                let target_ccy = crate::valuation::normalize_cash_currency_code(&ev.asset_id);
                let source_ccy = crate::valuation::normalize_cash_currency_code(&ev.quote_currency);
                if target_ccy == source_ccy {
                    return Err(LedgerError::InvalidAmount(
                        "CashExchange source and target currencies must differ".into(),
                    ));
                }
                let fee_ccy = crate::valuation::normalize_cash_currency_code(&ev.fee_currency);
                let source_principal = (ev.quantity * price).normalize();

                let fee_in_book = if ev.fee_amount.is_zero() {
                    Decimal::ZERO
                } else if fee_ccy == source_ccy {
                    *cash_balances
                        .entry((ev.account_id.clone(), source_ccy.clone()))
                        .or_insert(Decimal::ZERO) -= ev.fee_amount;
                    (ev.fee_amount * quote_fx).normalize()
                } else if fee_ccy == target_ccy {
                    let target_fx = event_fx(&target_ccy, ev_date).ok_or_else(|| {
                        LedgerError::MissingBookFx {
                            currency: target_ccy.clone(),
                            occurred_at: ev.occurred_at.to_rfc3339(),
                        }
                    })?;
                    *cash_balances
                        .entry((ev.account_id.clone(), target_ccy.clone()))
                        .or_insert(Decimal::ZERO) -= ev.fee_amount;
                    (ev.fee_amount * target_fx).normalize()
                } else {
                    return Err(LedgerError::InvalidAmount(
                        "CashExchange fee currency must match source or target currency".into(),
                    ));
                };

                *cash_balances
                    .entry((ev.account_id.clone(), source_ccy.clone()))
                    .or_insert(Decimal::ZERO) -= source_principal;
                *cash_balances
                    .entry((ev.account_id.clone(), target_ccy.clone()))
                    .or_insert(Decimal::ZERO) += ev.quantity;

                let principal_book = (source_principal * quote_fx).normalize();
                let total_cost_book = (principal_book + fee_in_book).normalize();
                if !fee_in_book.is_zero() {
                    *fee_expense_by_account
                        .entry(ev.account_id.clone())
                        .or_insert(Decimal::ZERO) += fee_in_book;
                }

                let source_out_qty = source_principal
                    + if fee_ccy == source_ccy {
                        ev.fee_amount
                    } else {
                        Decimal::ZERO
                    };
                let target_in_qty = ev.quantity
                    - if fee_ccy == target_ccy {
                        ev.fee_amount
                    } else {
                        Decimal::ZERO
                    };

                // Outflow posting comes FIRST so the cash FIFO pass disposes source_ccy first
                // and records any FX gain/loss on source_ccy, then credits target_ccy cash_lots
                // with `total_cost_book` (retaining the exchange cost!).
                let tx_id = format!("TX:{}", ev.event_id);
                let tx = JournalTransaction {
                    id: tx_id.clone(),
                    event_id: ev.event_id.clone(),
                    book_currency: book_currency.to_string(),
                    posting_rule_version: "v3".to_string(),
                    reversal_of: None,
                    postings: vec![
                        Posting {
                            id: format!("P:{}:1", ev.event_id),
                            transaction_id: tx_id.clone(),
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, source_ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: source_ccy,
                            native_quantity: -source_out_qty,
                            book_amount: -total_cost_book,
                        },
                        Posting {
                            id: format!("P:{}:2", ev.event_id),
                            transaction_id: tx_id,
                            ledger_account_id: format!("L:{}:CASH:{}", ev.account_id, target_ccy),
                            category: LedgerCategory::Cash,
                            native_asset_id: target_ccy,
                            native_quantity: target_in_qty,
                            book_amount: total_cost_book,
                        },
                    ],
                };
                tx.verify_balanced()?;
                transactions.push(tx);
            }
            EconomicEventType::Reversal => {}
        }
        let after_capital = external_inflows_by_account
            .get(&ev.account_id)
            .copied()
            .unwrap_or_default()
            - external_outflows_by_account
                .get(&ev.account_id)
                .copied()
                .unwrap_or_default();
        let book_amount = after_capital - before_capital;
        if !book_amount.is_zero()
            || matches!(
                ev.event_type,
                EconomicEventType::PositionTransferIn | EconomicEventType::PositionTransferOut
            )
        {
            external_cash_flows.push(ExternalCashFlow {
                account_id: ev.account_id.clone(),
                counterparty_account_id: ev.counterparty_account_id.clone(),
                internal_candidate: matches!(
                    ev.event_type,
                    EconomicEventType::TransferIn
                        | EconomicEventType::TransferOut
                        | EconomicEventType::PositionTransferIn
                        | EconomicEventType::PositionTransferOut
                ),
                occurred_at: ev.occurred_at,
                native_currency: ev.quote_currency.clone(),
                native_amount: book_amount / quote_fx,
                book_amount,
                position_asset_id: matches!(
                    ev.event_type,
                    EconomicEventType::PositionTransferIn | EconomicEventType::PositionTransferOut
                )
                .then(|| ev.asset_id.clone()),
                position_quantity: if matches!(
                    ev.event_type,
                    EconomicEventType::PositionTransferOut
                ) {
                    -ev.quantity
                } else {
                    ev.quantity
                },
            });
        }
        if matches!(
            ev.event_type,
            EconomicEventType::Buy | EconomicEventType::Sell
        ) && ev.fee_amount > Decimal::ZERO
            && !ev.fee_currency.eq_ignore_ascii_case(&ev.quote_currency)
        {
            let fee_value = ev.fee_amount * event_fx(&ev.fee_currency, ev_date).unwrap();
            let fee_alloc_id = format!("{}:FEE", ev.event_id);
            let disposed_cost: Decimal = allocations
                .iter()
                .filter(|a| a.sell_fill_id == fee_alloc_id)
                .map(|a| a.cost_book)
                .sum();
            let is_holding = allocations.iter().any(|a| a.sell_fill_id == fee_alloc_id);
            let credit = if is_holding { disposed_cost } else { fee_value };
            let tx = transactions.last_mut().unwrap();
            let cash = tx
                .postings
                .iter_mut()
                .find(|p| p.category == LedgerCategory::Cash)
                .unwrap();
            cash.book_amount += fee_value;
            tx.postings.push(Posting {
                id: format!("P:{}:THIRD_FEE", ev.event_id),
                transaction_id: tx.id.clone(),
                ledger_account_id: format!(
                    "L:{}:{}:{}",
                    ev.account_id,
                    if is_holding { "HOLDING" } else { "CASH" },
                    ev.fee_currency
                ),
                category: if is_holding {
                    LedgerCategory::HoldingCost
                } else {
                    LedgerCategory::Cash
                },
                native_asset_id: ev.fee_currency.clone(),
                native_quantity: -ev.fee_amount,
                book_amount: -credit,
            });
            if is_holding {
                tx.postings.push(Posting {
                    id: format!("P:{}:FEE_GAIN", ev.event_id),
                    transaction_id: tx.id.clone(),
                    ledger_account_id: format!("L:{}:REALIZED", ev.account_id),
                    category: LedgerCategory::RealizedPnl,
                    native_asset_id: ev.fee_currency.clone(),
                    native_quantity: Decimal::ZERO,
                    book_amount: credit - fee_value,
                });
            }
            tx.verify_balanced()?;
        }
    }

    let mut holdings: BTreeMap<(String, String), HoldingProjection> = BTreeMap::new();
    let mut open_lots: Vec<CostLot> = Vec::new();
    for ((acct, asset), lots) in &lots_by_account_asset {
        let qty: Decimal = lots.iter().map(|l| l.remaining_quantity).sum();
        let cost: Decimal = lots.iter().map(|l| l.remaining_cost_book).sum();
        let cost_known = lots.iter().all(|l| l.cost_known);
        if !qty.is_zero() {
            holdings.insert(
                (acct.clone(), asset.clone()),
                HoldingProjection {
                    account_id: acct.clone(),
                    asset_id: asset.clone(),
                    quantity: qty.normalize(),
                    carrying_cost: cost.normalize(),
                    cost_known,
                    ledger_revision,
                },
            );
            open_lots.extend(lots.iter().cloned());
        }
    }

    // Cash also has historical carrying costs. FIFO disposals recognize FX gains;
    // terminal cash valuation exposes the remaining unrealized FX change.
    type CashSlices = Vec<(Decimal, Decimal)>;
    fn take_cash(
        lots: &mut CashSlices,
        needed: Decimal,
        owner: &str,
        asset: &str,
    ) -> Result<CashSlices, LedgerError> {
        let mut quantity = needed;
        let mut slices = Vec::new();
        for (available, basis) in lots.iter_mut() {
            if quantity.is_zero() {
                break;
            }
            let take = quantity.min(*available);
            if take.is_zero() {
                continue;
            }
            let cost = if take == *available {
                *basis
            } else {
                *basis * take / *available
            };
            *available -= take;
            *basis -= cost;
            quantity -= take;
            slices.push((take, cost));
        }
        lots.retain(|(quantity, _)| *quantity > Decimal::ZERO);
        if quantity > Decimal::ZERO {
            return Err(LedgerError::InvalidAmount(format!(
                "missing opening cash for {owner}:{asset}"
            )));
        }
        Ok(slices)
    }
    let mut cash_lots: BTreeMap<(String, String), CashSlices> = BTreeMap::new();
    let mut cash_transit: Vec<(&LedgerEventInput, CashSlices)> = Vec::new();
    let event_map: HashMap<_, _> = events
        .iter()
        .map(|event| (event.event_id.as_str(), event))
        .collect();
    for tx in &mut transactions {
        let event = event_map[tx.event_id.as_str()];
        let mut gains = Vec::new();
        let mut transit_basis = None;
        for posting in &mut tx.postings {
            if posting.category != LedgerCategory::Cash || posting.native_quantity.is_zero() {
                continue;
            }
            let Some((owner, _)) = posting
                .ledger_account_id
                .trim_start_matches("L:")
                .rsplit_once(":CASH:")
            else {
                continue;
            };
            let owner = owner.to_string();
            let key = (owner.clone(), posting.native_asset_id.clone());
            let lots = cash_lots.entry(key).or_default();
            let mut gain = Decimal::ZERO;
            if event.event_type == EconomicEventType::TransferOut
                && event.counterparty_account_id.is_some()
            {
                if !event
                    .fee_currency
                    .eq_ignore_ascii_case(&event.quote_currency)
                    && !event.fee_amount.is_zero()
                {
                    return Err(LedgerError::InvalidAmount("cash transfer fee requires the transfer currency; use an explicit fee event for another asset".into()));
                }
                let principal = take_cash(lots, event.quantity, &owner, &posting.native_asset_id)?;
                let principal_cost: Decimal = principal.iter().map(|(_, cost)| *cost).sum();
                let fees = take_cash(lots, event.fee_amount, &owner, &posting.native_asset_id)?;
                let fee_cost: Decimal = fees.iter().map(|(_, cost)| *cost).sum();
                let fee_value =
                    -posting.book_amount * event.fee_amount / (event.quantity + event.fee_amount);
                gain = fee_value - fee_cost;
                posting.book_amount = -principal_cost - fee_cost;
                transit_basis = Some(principal_cost);
                cash_transit.push((event, principal));
            } else if event.event_type == EconomicEventType::TransferIn
                && event.counterparty_account_id.is_some()
            {
                let matched = cash_transit.iter().position(|(out, _)| {
                    out.account_id == event.counterparty_account_id.as_deref().unwrap()
                        && out.counterparty_account_id.as_deref() == Some(&event.account_id)
                        && out.quote_currency == event.quote_currency
                        && out.quantity == event.quantity
                        && (event.transfer_group_id.is_none()
                            || event.transfer_group_id == out.transfer_group_id)
                });
                if let Some(index) = matched {
                    let (_, slices) = cash_transit.remove(index);
                    let cost: Decimal = slices.iter().map(|(_, cost)| *cost).sum();
                    lots.extend(slices);
                    posting.book_amount = cost;
                    transit_basis = Some(cost);
                } else {
                    lots.push((posting.native_quantity, posting.book_amount));
                    if events
                        .iter()
                        .any(|e| Some(&e.account_id) == event.counterparty_account_id.as_ref())
                    {
                        pending_valuation_reasons
                            .push(format!("unmatched_transfer_in:{}", event.event_id));
                    }
                }
            } else if posting.native_quantity > Decimal::ZERO {
                lots.push((posting.native_quantity, posting.book_amount));
            } else {
                let slices = take_cash(
                    lots,
                    -posting.native_quantity,
                    &owner,
                    &posting.native_asset_id,
                )?;
                let cost: Decimal = slices.iter().map(|(_, cost)| *cost).sum();
                gain = -posting.book_amount - cost;
                posting.book_amount = -cost;
            }
            if !gain.is_zero() {
                *realized_pnl_by_account.entry(owner.clone()).or_default() += gain;
                *realized_fx_pnl_by_account.entry(owner.clone()).or_default() += gain;
                gains.push(Posting {
                    id: format!("{}:FX", posting.id),
                    transaction_id: tx.id.clone(),
                    ledger_account_id: format!("L:{owner}:REALIZED"),
                    category: LedgerCategory::RealizedPnl,
                    native_asset_id: posting.native_asset_id.clone(),
                    native_quantity: Decimal::ZERO,
                    book_amount: -gain,
                });
            }
        }
        if let Some(basis) = transit_basis {
            for posting in &mut tx.postings {
                if posting.category == LedgerCategory::ClearingTransit {
                    posting.book_amount = if event.event_type == EconomicEventType::TransferIn {
                        -basis
                    } else {
                        basis
                    };
                }
            }
        }
        tx.postings.extend(gains);
        tx.posting_rule_version = "v3".into();
        tx.verify_balanced()?;
    }
    let mut in_transit_assets = Vec::new();
    for (event, slices) in cash_transit {
        in_transit_assets.push(InTransitAsset {
            source_account: event.account_id.clone(),
            destination_account: event.counterparty_account_id.clone().unwrap(),
            asset_id: event.quote_currency.clone(),
            quote_currency: event.quote_currency.clone(),
            quantity: event.quantity,
            carrying_cost: slices.iter().map(|(_, cost)| *cost).sum(),
            cost_known: true,
            is_cash: true,
        });
    }
    for (event, lots) in position_transit {
        in_transit_assets.push(InTransitAsset {
            source_account: event.account_id.clone(),
            destination_account: event.counterparty_account_id.clone().unwrap(),
            asset_id: event.asset_id.clone(),
            quote_currency: event.quote_currency.clone(),
            quantity: event.quantity,
            carrying_cost: lots.iter().map(|lot| lot.remaining_cost_book).sum(),
            cost_known: lots.iter().all(|lot| lot.cost_known),
            is_cash: false,
        });
    }
    let cash_carrying_costs = cash_lots
        .into_iter()
        .map(|(key, lots)| (key, lots.iter().map(|(_, c)| *c).sum()))
        .collect();
    Ok(LedgerProjectionResult {
        book_currency: book_currency.to_string(),
        ledger_revision,
        cash_balances,
        cash_carrying_costs,
        holdings,
        open_lots,
        allocations,
        transactions,
        realized_pnl_by_account,
        realized_fx_pnl_by_account,
        dividend_income_by_account,
        fee_expense_by_account,
        external_inflows_by_account,
        external_outflows_by_account,
        internal_transfer_fees,
        pending_valuation_reasons,
        unknown_cost_assets,
        external_cash_flows,
        in_transit_assets,
    })
}

/// Derive split-adjusted or total-return-adjusted bar series from canonical `raw` bars and
/// `corporate_action` records (S-08).
pub fn derive_adjusted_bars(
    bars: &[BarRecord],
    actions: &[CorporateActionRecord],
    target_adjustment: AdjustmentKind,
) -> Result<Vec<BarRecord>, LedgerError> {
    if target_adjustment == AdjustmentKind::Raw {
        return Ok(bars.to_vec());
    }
    let priority = |source: &str| match source {
        "tiingo" => 3,
        "yahoo" => 2,
        _ => 1,
    };
    let mut normalized: Vec<&CorporateActionRecord> = Vec::new();
    for action in actions {
        if let Some(index) = normalized.iter().position(|old| {
            old.instrument_id == action.instrument_id
                && old.ex_date == action.ex_date
                && old.action_type == action.action_type
        }) {
            if (action.factor_version, priority(&action.source))
                > (
                    normalized[index].factor_version,
                    priority(&normalized[index].source),
                )
            {
                normalized[index] = action;
            }
        } else {
            normalized.push(action);
        }
    }
    let mut result = Vec::with_capacity(bars.len());
    for bar in bars {
        let mut split_factor = Decimal::ONE;
        let mut dividend_factor = Decimal::ONE;
        for action in &normalized {
            if action.instrument_id != bar.instrument_id || action.ex_date <= bar.session_date {
                continue;
            }
            match action.action_type {
                CorporateActionType::Split => {
                    if action.ratio_or_amount <= Decimal::ZERO {
                        return Err(LedgerError::InvalidAmount(
                            "split ratio must be positive".into(),
                        ));
                    }
                    split_factor = split_factor
                        .checked_mul(action.ratio_or_amount)
                        .ok_or_else(|| {
                            LedgerError::InvalidAmount("split factor overflow".into())
                        })?;
                }
                CorporateActionType::CashDividend
                    if target_adjustment == AdjustmentKind::TotalReturnAdjusted =>
                {
                    let reference = bars
                        .iter()
                        .filter(|candidate| {
                            candidate.instrument_id == bar.instrument_id
                                && candidate.session_date < action.ex_date
                        })
                        .max_by_key(|candidate| candidate.session_date)
                        .ok_or_else(|| {
                            LedgerError::NotFound(
                                "previous close required for dividend adjustment".into(),
                            )
                        })?;
                    let same_day_split: Decimal = normalized
                        .iter()
                        .filter(|other| {
                            other.instrument_id == action.instrument_id
                                && other.ex_date == action.ex_date
                                && other.action_type == CorporateActionType::Split
                        })
                        .map(|other| other.ratio_or_amount)
                        .product();
                    if same_day_split <= Decimal::ZERO {
                        return Err(LedgerError::InvalidAmount(
                            "invalid dividend split factor".into(),
                        ));
                    }
                    let previous_close = reference.close / same_day_split;
                    if action.ratio_or_amount < Decimal::ZERO
                        || previous_close <= action.ratio_or_amount
                    {
                        return Err(LedgerError::InvalidAmount(
                            "dividend adjustment requires positive ex-dividend reference price"
                                .into(),
                        ));
                    }
                    dividend_factor = dividend_factor
                        .checked_mul((previous_close - action.ratio_or_amount) / previous_close)
                        .ok_or_else(|| {
                            LedgerError::InvalidAmount("dividend factor overflow".into())
                        })?;
                }
                _ => {}
            }
        }
        let price_factor = dividend_factor / split_factor;
        let adjust = |price: Decimal| {
            price
                .checked_mul(price_factor)
                .map(|p| {
                    p.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero)
                        .normalize()
                })
                .ok_or_else(|| LedgerError::InvalidAmount("adjusted price overflow".into()))
        };
        let mut adjusted = bar.clone();
        adjusted.adjustment = target_adjustment;
        adjusted.open = adjust(bar.open)?;
        adjusted.high = adjust(bar.high)?;
        adjusted.low = adjust(bar.low)?;
        adjusted.close = adjust(bar.close)?;
        adjusted.volume = bar
            .volume
            .checked_mul(split_factor)
            .ok_or_else(|| LedgerError::InvalidAmount("adjusted volume overflow".into()))?
            .normalize();
        result.push(adjusted);
    }
    Ok(result)
}
