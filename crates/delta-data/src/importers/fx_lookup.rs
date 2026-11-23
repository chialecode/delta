use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, NaiveDate, Utc};
use delta_core::{parse_decimal_lossless, to_canonical_text, EconomicEventType, LedgerEventInput};
use rusqlite::params;
use rust_decimal::Decimal;

use crate::storage::{SqliteStore, StorageError};

pub fn load_all_ledger_events(store: &SqliteStore) -> Result<Vec<LedgerEventInput>, StorageError> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT id, account_id, type, occurred_at, sequence_no, asset_id, instrument_id,
                quantity, price, quote_currency, fee_amount, fee_currency, cost_known,
                counterparty_account_id, transfer_group_id, correction_group_id, revision,
                active_in_correction_group, source_ref
         FROM economic_event
         ORDER BY occurred_at ASC, sequence_no ASC, id ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
            r.get::<_, String>(11)?,
            r.get::<_, i64>(12)? != 0,
            r.get::<_, Option<String>>(13)?,
            r.get::<_, Option<String>>(14)?,
            r.get::<_, String>(15)?,
            r.get::<_, i64>(16)?,
            r.get::<_, i64>(17)? != 0,
            r.get::<_, Option<String>>(18)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            id,
            acct,
            t_str,
            occ_str,
            seq,
            asset,
            inst,
            q_str,
            p_str,
            q_ccy,
            f_str,
            f_ccy,
            ck,
            cp,
            tg,
            cg,
            rev,
            active,
            sref,
        ) = row?;
        let ev_type = match t_str.as_str() {
            "opening_balance" => EconomicEventType::OpeningBalance,
            "buy" => EconomicEventType::Buy,
            "sell" => EconomicEventType::Sell,
            "cash_exchange" => EconomicEventType::CashExchange,
            "deposit" => EconomicEventType::Deposit,
            "withdrawal" => EconomicEventType::Withdrawal,
            "transfer_out" => EconomicEventType::TransferOut,
            "transfer_in" => EconomicEventType::TransferIn,
            "position_transfer_out" => EconomicEventType::PositionTransferOut,
            "position_transfer_in" => EconomicEventType::PositionTransferIn,
            "cash_dividend" => EconomicEventType::CashDividend,
            "interest" => EconomicEventType::Interest,
            "fee_only" => EconomicEventType::FeeOnly,
            "split" => EconomicEventType::Split,
            "reversal" => EconomicEventType::Reversal,
            _ => {
                return Err(StorageError::IntegrityFailed(
                    "unknown ledger event type".into(),
                ))
            }
        };
        let occurred_at = DateTime::parse_from_rfc3339(&occ_str)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| StorageError::IntegrityFailed("invalid ledger timestamp".into()))?;
        let quantity = parse_decimal_lossless(&q_str)?;
        let price = match p_str {
            Some(s) if !s.is_empty() => Some(parse_decimal_lossless(&s)?),
            _ => None,
        };
        let fee_amount = parse_decimal_lossless(&f_str)?;
        out.push(LedgerEventInput {
            event_id: id,
            account_id: acct,
            event_type: ev_type,
            occurred_at,
            sequence_no: seq,
            asset_id: asset,
            instrument_id: inst,
            quantity,
            price,
            quote_currency: q_ccy,
            fee_amount,
            fee_currency: f_ccy,
            cost_known: ck,
            counterparty_account_id: cp,
            transfer_group_id: tg,
            correction_group_id: Some(cg),
            revision: rev,
            active_in_correction_group: active,
            source_ref: sref,
        });
    }
    Ok(out)
}

pub fn load_book_fx_lookup(
    store: &SqliteStore,
    book_currency: &str,
) -> Result<HashMap<(String, NaiveDate), Decimal>, StorageError> {
    load_book_fx_lookup_at_version(store, book_currency, i64::MAX)
}

pub fn load_book_fx_lookup_at_version(
    store: &SqliteStore,
    book_currency: &str,
    version: i64,
) -> Result<HashMap<(String, NaiveDate), Decimal>, StorageError> {
    let mut stmt = store.conn().prepare_cached(
        "SELECT f.base_currency, f.quote_currency, f.rate, f.effective_date
         FROM fx_rate f
         JOIN dataset_publication p ON p.version_num = f.dataset_version
         WHERE f.dataset_version <= ?1
         ORDER BY f.dataset_version ASC,
                  CASE
                      WHEN f.base_currency = 'USDT' AND f.source = 'coinbase' THEN 2
                      WHEN f.base_currency = 'USDC' AND f.source = 'bitstamp' THEN 2
                      WHEN f.source = 'frankfurter' THEN 2
                      ELSE 1
                  END ASC,
                  f.source DESC",
    )?;
    let rows = stmt.query_map(params![version], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut map = HashMap::new();
    let book = book_currency.to_ascii_uppercase();
    let mut quotes = HashMap::new();
    let mut currencies = BTreeSet::new();
    let mut dates = BTreeSet::new();
    for row in rows {
        let (base, quote, r_str, d_str) = row?;
        let date = NaiveDate::parse_from_str(&d_str, "%Y-%m-%d")
            .map_err(|_| StorageError::IntegrityFailed("invalid FX date".into()))?;
        let rate = parse_decimal_lossless(&r_str)?;
        if rate <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed("non-positive FX rate".into()));
        }
        let base_up = base.to_ascii_uppercase();
        let quote_up = quote.to_ascii_uppercase();
        currencies.insert(base_up.clone());
        currencies.insert(quote_up.clone());
        dates.insert(date);
        quotes.insert((base_up, quote_up, date), rate);
    }
    for date in dates {
        for currency in &currencies {
            if let Some(found) = delta_core::resolve_fx_rate_s07(
                currency,
                &book,
                date,
                !delta_core::is_fiat_currency(currency) || !delta_core::is_fiat_currency(&book),
                &quotes,
            ) {
                map.insert((currency.clone(), date), found.rate);
            }
        }
    }
    Ok(map)
}

pub fn load_event_book_fx(
    store: &SqliteStore,
) -> Result<HashMap<(String, String), Decimal>, StorageError> {
    let mut stmt = store
        .conn()
        .prepare_cached("SELECT event_id, currency, rate FROM event_book_fx")?;
    let mut bindings = HashMap::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (event, currency, raw) = row?;
        let rate = parse_decimal_lossless(&raw)?;
        if rate <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "invalid event FX binding".into(),
            ));
        }
        bindings.insert((event, currency), rate);
    }
    Ok(bindings)
}

pub(crate) type EventFxBinding = (String, String, Decimal, NaiveDate, i64);

pub(crate) fn prepare_event_book_fx(
    store: &SqliteStore,
    book: &str,
    events: &[LedgerEventInput],
    rates: &HashMap<(String, NaiveDate), Decimal>,
) -> Result<Vec<EventFxBinding>, StorageError> {
    let version: i64 = store.conn().query_row(
        "SELECT COALESCE(MAX(version_num), 0) FROM dataset_publication",
        [],
        |r| r.get(0),
    )?;
    let mut out = Vec::new();
    for event in events {
        let mut candidate_currencies =
            vec![event.quote_currency.as_str(), event.fee_currency.as_str()];
        if event.event_type == EconomicEventType::CashExchange {
            candidate_currencies.push(event.asset_id.as_str());
        }
        for currency in candidate_currencies {
            if currency == event.fee_currency
                && event.fee_amount.is_zero()
                && currency != event.quote_currency
            {
                continue;
            }
            let currency = currency.to_ascii_uppercase();
            if currency.eq_ignore_ascii_case(book) {
                continue;
            }
            let date = event.occurred_at.date_naive();
            let found = (0..=4).find_map(|days| {
                if days > 0 && !delta_core::is_fiat_currency(&currency) {
                    return None;
                }
                let day = date.checked_sub_days(chrono::Days::new(days))?;
                rates.get(&(currency.clone(), day)).map(|rate| (*rate, day))
            });
            if let Some((rate, source_date)) = found {
                if !out
                    .iter()
                    .any(|(id, ccy, _, _, _)| id == &event.event_id && ccy == &currency)
                {
                    out.push((event.event_id.clone(), currency, rate, source_date, version));
                }
            }
        }
    }
    Ok(out)
}

pub(crate) fn persist_event_book_fx(
    tx: &rusqlite::Transaction<'_>,
    bindings: &[EventFxBinding],
) -> Result<(), StorageError> {
    if bindings.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(
        "INSERT OR IGNORE INTO event_book_fx(event_id, currency, rate, source_date, dataset_version)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (event, currency, rate, date, version) in bindings {
        stmt.execute(params![
            event,
            currency,
            to_canonical_text(*rate),
            date.to_string(),
            version
        ])?;
    }
    Ok(())
}
