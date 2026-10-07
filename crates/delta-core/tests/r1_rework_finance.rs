//! R1 rework coverage for in-transit valuation, reconciliation, overflow,
//! manual marks and drawdown (F-07/F-09/F-13).

use chrono::{TimeZone, Utc};
use delta_core::events::{EconomicEvent, EventPayload, TransferKind};
use delta_core::ids::{AccountId, AssetId, EventId, TransferGroupId};
use delta_core::ledger::{apply_effective, IdentityFx, InstrumentMap, LedgerEngine};
use delta_core::money::{parse_decimal, CurrencyCode};
use delta_core::pnl::basic_drawdown;
use delta_core::valuation::{
    apply_manual_marks, reconcile_balances, value_accounts, BalanceObservation, ManualKind,
    ManualMark, PriceMap, Quality,
};
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

fn no_prices() -> PriceMap {
    PriceMap {
        quotes: BTreeMap::new(),
    }
}

fn ts(y: i32, m: u32, d: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
}

fn ev(acc: &str, id: &str, at: chrono::DateTime<Utc>, seq: i64, p: EventPayload) -> EconomicEvent {
    EconomicEvent {
        id: EventId(id.into()),
        account_id: AccountId(acc.into()),
        occurred_at: at,
        recorded_at: at,
        seq,
        source_ref: None,
        correction_group: None,
        revision: 0,
        reverses: None,
        payload: p,
    }
}

fn usd_fx() -> IdentityFx {
    IdentityFx {
        currency: CurrencyCode::usd(),
    }
}

#[test]
fn r1_a_07_overlong_decimal_is_rejected_not_truncated() {
    let big = "9".repeat(29);
    let err = parse_decimal(&big).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert!(err.to_string().contains("overflow") || err.to_string().contains("28"));
    assert!(parse_decimal("").is_err());
    assert!(parse_decimal("1e99").is_err());
    assert_eq!(parse_decimal("600.60").unwrap(), dec!(600.60));
}

#[test]
fn r1_a_06_in_transit_cash_stays_in_portfolio_value_until_it_arrives() {
    let at = ts(2026, 5, 1);
    let events = vec![
        ev(
            "a",
            "dep",
            at,
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
        ev(
            "a",
            "out",
            at,
            2,
            EventPayload::Transfer {
                kind: TransferKind::Out,
                group: TransferGroupId("wire".into()),
                counterparty: AccountId("b".into()),
                asset: AssetId("USD".into()),
                principal: dec!(400),
                fee: None,
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &usd_fx(), &InstrumentMap::default());
    assert_eq!(
        engine.accounts[&AccountId("a".into())].cash_of(&AssetId("USD".into())),
        dec!(600)
    );
    assert_eq!(engine.in_transit().len(), 1);
    let both = |id: &AccountId| id.0 == "a" || id.0 == "b";
    let portfolio = value_accounts(
        &engine,
        at,
        &no_prices(),
        &usd_fx(),
        &CurrencyCode::usd(),
        &both,
    )
    .unwrap();
    assert_eq!(portfolio.total_market_value, Some(dec!(1000)));
    let only_a = |id: &AccountId| id.0 == "a";
    let single = value_accounts(
        &engine,
        at,
        &no_prices(),
        &usd_fx(),
        &CurrencyCode::usd(),
        &only_a,
    )
    .unwrap();
    assert_eq!(single.total_market_value, Some(dec!(600)));
}

#[test]
fn r1_a_06_reconciliation_reports_the_diff_and_does_not_rewrite_the_ledger() {
    let at = ts(2026, 5, 2);
    let events = vec![ev(
        "a",
        "dep",
        at,
        1,
        EventPayload::CashDeposit {
            asset: AssetId("USD".into()),
            amount: dec!(10),
        },
    )];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &usd_fx(), &InstrumentMap::default());
    let before = engine.accounts[&AccountId("a".into())].cash_of(&AssetId("USD".into()));
    let diffs = reconcile_balances(
        &engine,
        &[BalanceObservation {
            account_id: AccountId("a".into()),
            asset: AssetId("USD".into()),
            quantity: dec!(12),
            as_of: at,
        }],
        &|_| true,
    )
    .unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].difference, dec!(2));
    assert_eq!(
        engine.accounts[&AccountId("a".into())].cash_of(&AssetId("USD".into())),
        before
    );
}

#[test]
fn r1_a_09_manual_liability_reduces_net_worth_and_stale_mark_is_not_applied() {
    let at = ts(2026, 6, 1);
    let summary = value_accounts(
        &LedgerEngine::new(CurrencyCode::usd()),
        at,
        &no_prices(),
        &usd_fx(),
        &CurrencyCode::usd(),
        &|_| true,
    )
    .unwrap();
    let marks = vec![
        ManualMark {
            account_id: AccountId("a".into()),
            asset: AssetId("house".into()),
            value: dec!(50),
            currency: CurrencyCode::usd(),
            as_of: at,
            kind: ManualKind::Asset,
            valid_until: None,
        },
        ManualMark {
            account_id: AccountId("a".into()),
            asset: AssetId("loan".into()),
            value: dec!(20),
            currency: CurrencyCode::usd(),
            as_of: at,
            kind: ManualKind::Liability,
            valid_until: None,
        },
        ManualMark {
            account_id: AccountId("a".into()),
            asset: AssetId("old-car".into()),
            value: dec!(999),
            currency: CurrencyCode::usd(),
            as_of: ts(2026, 1, 1),
            kind: ManualKind::Asset,
            valid_until: Some(ts(2026, 2, 1)),
        },
    ];
    let wealth = apply_manual_marks(
        &summary,
        &marks,
        at,
        &usd_fx(),
        &CurrencyCode::usd(),
        &|_| true,
    )
    .unwrap();
    assert_eq!(wealth.liability_value, dec!(20));
    assert_eq!(wealth.known_net_worth, dec!(30));
    assert!(wealth.notes.iter().any(|n| n.contains("expired")));
    assert_ne!(wealth.quality, Quality::Complete);
}

#[test]
fn r1_a_09_drawdown_without_removed_flows_is_not_an_investment_drawdown() {
    let raw = basic_drawdown(&[dec!(100), dec!(80)], false);
    assert!(!raw.comparable);
    assert!(raw.label.contains("not an investment drawdown"));
    assert!(raw.max_drawdown.is_none());
    let clean = basic_drawdown(&[dec!(100), dec!(80), dec!(90)], true);
    assert!(clean.comparable);
    assert_eq!(clean.max_drawdown, Some(dec!(0.2)));
}
