//! A-review regressions (docs/evidence/r1-review.md F-01..F-07): hand-computed
//! expectations for pending atomicity, third-asset fees, transfer pairing,
//! unknown-cost realized PnL, period aggregates and partial valuation.

use chrono::{TimeZone, Utc};
use delta_core::events::{EconomicEvent, EventPayload, Fee, OpeningCost, TransferKind};
use delta_core::ids::{AccountId, AssetId, EventId, InstrumentId, TransferGroupId};
use delta_core::ledger::{
    apply_effective, ApplyOutcome, FxResolver, IdentityFx, InstrumentMap, LedgerEngine,
};
use delta_core::money::CurrencyCode;
use delta_core::pnl::period_pnl;
use delta_core::valuation::{value_workspace, PriceMap, PriceQuote, Quality};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

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

fn usd() -> IdentityFx {
    IdentityFx {
        currency: CurrencyCode::usd(),
    }
}

fn aapl() -> InstrumentMap {
    let mut m = InstrumentMap::default();
    m.0.insert(InstrumentId("NASDAQ:AAPL".into()), AssetId("AAPL".into()));
    m
}

struct TableFx(Vec<((&'static str, &'static str), Decimal)>);

impl FxResolver for TableFx {
    fn rate(
        &self,
        base: &CurrencyCode,
        quote: &CurrencyCode,
        at: chrono::DateTime<Utc>,
    ) -> Option<(Decimal, chrono::DateTime<Utc>)> {
        if base == quote {
            return Some((Decimal::ONE, at));
        }
        self.0
            .iter()
            .find(|((b, q), _)| *b == base.as_str() && *q == quote.as_str())
            .map(|(_, r)| (*r, at))
    }
}

fn deposit(
    acc: &str,
    id: &str,
    at: chrono::DateTime<Utc>,
    seq: i64,
    asset: &str,
    amount: Decimal,
) -> EconomicEvent {
    ev(
        acc,
        id,
        at,
        seq,
        EventPayload::CashDeposit {
            asset: AssetId(asset.into()),
            amount,
        },
    )
}

fn buy_aapl(
    id: &str,
    at: chrono::DateTime<Utc>,
    seq: i64,
    qty: Decimal,
    price: Decimal,
) -> EconomicEvent {
    ev(
        "a",
        id,
        at,
        seq,
        EventPayload::Buy {
            instrument: InstrumentId("NASDAQ:AAPL".into()),
            quantity: qty,
            price,
            quote_currency: CurrencyCode::usd(),
            fees: vec![],
        },
    )
}

fn sell_aapl(
    id: &str,
    at: chrono::DateTime<Utc>,
    seq: i64,
    qty: Decimal,
    price: Decimal,
) -> EconomicEvent {
    ev(
        "a",
        id,
        at,
        seq,
        EventPayload::Sell {
            instrument: InstrumentId("NASDAQ:AAPL".into()),
            quantity: qty,
            price,
            quote_currency: CurrencyCode::usd(),
            fees: vec![],
        },
    )
}

fn assert_all_posted_and_balanced(outcomes: &[(EventId, ApplyOutcome)]) {
    for (id, o) in outcomes {
        match o {
            ApplyOutcome::Posted { transaction } => {
                assert!(transaction.balances_to_zero(), "{id} must balance");
            }
            ApplyOutcome::Pending { pending } => {
                panic!("{id} unexpectedly pending: {}", pending.reason)
            }
        }
    }
}

// ---- F-01: a pending event must not change ledger state -----------------------

#[test]
fn r1_a_07_insufficient_cash_buy_is_pending_without_side_effects() {
    let events = vec![
        deposit("a", "dep", ts(2026, 2, 1), 1, "USD", dec!(100)),
        buy_aapl("buy", ts(2026, 2, 2), 2, dec!(10), dec!(100)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &usd(), &aapl());
    assert!(matches!(out[1].1, ApplyOutcome::Pending { .. }));
    let a = &engine.accounts[&AccountId("a".into())];
    assert_eq!(
        a.cash_of(&AssetId("USD".into())),
        dec!(100),
        "cash untouched"
    );
    let qty = a
        .holding(&AssetId("AAPL".into()))
        .map(|h| h.quantity())
        .unwrap_or_default();
    assert_eq!(qty, dec!(0), "no phantom position from a pending buy");
}

#[test]
fn r1_a_07_missing_fx_deposit_is_pending_without_side_effects() {
    // EUR is a registered cash asset, but no EUR->USD rate exists.
    let events = vec![deposit("a", "dep", ts(2026, 2, 1), 1, "EUR", dec!(100))];
    let mut engine = LedgerEngine::new(CurrencyCode::usd()).with_cash_asset(AssetId("EUR".into()));
    let out = apply_effective(&mut engine, &events, &usd(), &InstrumentMap::default());
    assert!(matches!(out[0].1, ApplyOutcome::Pending { .. }));
    let a = &engine.accounts[&AccountId("a".into())];
    assert_eq!(a.cash_of(&AssetId("EUR".into())), dec!(0));
    assert_eq!(a.pending.len(), 1);
}

#[test]
fn r1_a_07_withdrawal_beyond_cash_is_pending() {
    let events = vec![
        deposit("a", "dep", ts(2026, 2, 1), 1, "USD", dec!(100)),
        ev(
            "a",
            "wd",
            ts(2026, 2, 2),
            2,
            EventPayload::CashWithdrawal {
                asset: AssetId("USD".into()),
                amount: dec!(150),
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &usd(), &InstrumentMap::default());
    assert!(matches!(out[1].1, ApplyOutcome::Pending { .. }));
    assert_eq!(
        engine.accounts[&AccountId("a".into())].cash_of(&AssetId("USD".into())),
        dec!(100)
    );
}

#[test]
fn r1_a_07_non_positive_cash_flow_amount_is_rejected() {
    let events = vec![deposit("a", "dep", ts(2026, 2, 1), 1, "USD", dec!(-5))];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &usd(), &InstrumentMap::default());
    assert!(matches!(out[0].1, ApplyOutcome::Pending { .. }));
    assert_eq!(
        engine.accounts[&AccountId("a".into())].cash_of(&AssetId("USD".into())),
        dec!(0)
    );
}

// ---- F-02: third-asset fee on a buy is paid once, from the fee asset -----------

#[test]
fn r1_a_07_third_asset_fee_buy_charges_fee_asset_only() {
    // 20000 USDT; buy 0.01 BNB @ 60000 (600 USDT); buy 0.5 BTC @ 20000 with a
    // 0.001 BNB fee worth 60 USDT. Hand-computed: USDT 20000-600-10000 = 9400;
    // BNB 0.009; BTC cost 10000 + 60 = 10060 (fee capitalized).
    let mut instruments = InstrumentMap::default();
    instruments.0.insert(
        InstrumentId("BINANCE:BTCUSDT".into()),
        AssetId("BTC".into()),
    );
    instruments.0.insert(
        InstrumentId("BINANCE:BNBUSDT".into()),
        AssetId("BNB".into()),
    );
    let fx = TableFx(vec![
        (("USDT", "USD"), dec!(1)),
        (("BNB", "USDT"), dec!(60000)),
    ]);
    let events = vec![
        deposit("ex", "dep", ts(2026, 4, 1), 1, "USDT", dec!(20000)),
        ev(
            "ex",
            "bnb",
            ts(2026, 4, 1),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("BINANCE:BNBUSDT".into()),
                quantity: dec!(0.01),
                price: dec!(60000),
                quote_currency: CurrencyCode("USDT".into()),
                fees: vec![],
            },
        ),
        ev(
            "ex",
            "btc",
            ts(2026, 4, 2),
            3,
            EventPayload::Buy {
                instrument: InstrumentId("BINANCE:BTCUSDT".into()),
                quantity: dec!(0.5),
                price: dec!(20000),
                quote_currency: CurrencyCode("USDT".into()),
                fees: vec![Fee {
                    asset: AssetId("BNB".into()),
                    amount: dec!(0.001),
                    category: "platform".into(),
                }],
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd()).with_cash_asset(AssetId("USDT".into()));
    let out = apply_effective(&mut engine, &events, &fx, &instruments);
    assert_all_posted_and_balanced(&out);
    let ex = &engine.accounts[&AccountId("ex".into())];
    assert_eq!(ex.cash_of(&AssetId("USDT".into())), dec!(9400));
    assert_eq!(
        ex.holding(&AssetId("BNB".into())).unwrap().quantity(),
        dec!(0.009)
    );
    assert_eq!(
        ex.holding(&AssetId("BTC".into())).unwrap().carrying_cost(),
        Some(dec!(10060))
    );
}

// ---- F-03: standalone fee paid from a held asset balances ---------------------

#[test]
fn r1_a_07_standalone_fee_from_held_asset_balances_and_values_in_book() {
    // Buy 0.01 BNB @ 60000 USD (cost 600). Later pay a 0.001 BNB fee when BNB
    // is worth 70000 USD: expense 70, cost relieved 60, realized gain 10.
    let mut instruments = InstrumentMap::default();
    instruments
        .0
        .insert(InstrumentId("X:BNBUSD".into()), AssetId("BNB".into()));
    let fx = TableFx(vec![(("BNB", "USD"), dec!(70000))]);
    let events = vec![
        deposit("a", "dep", ts(2026, 4, 1), 1, "USD", dec!(1000)),
        ev(
            "a",
            "bnb",
            ts(2026, 4, 1),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("X:BNBUSD".into()),
                quantity: dec!(0.01),
                price: dec!(60000),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
        ev(
            "a",
            "fee",
            ts(2026, 4, 5),
            3,
            EventPayload::Fee {
                asset: AssetId("BNB".into()),
                amount: dec!(0.001),
                category: "platform".into(),
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &fx, &instruments);
    assert_all_posted_and_balanced(&out);
    let a = &engine.accounts[&AccountId("a".into())];
    let bnb = a.holding(&AssetId("BNB".into())).unwrap();
    assert_eq!(bnb.quantity(), dec!(0.009));
    assert_eq!(bnb.carrying_cost(), Some(dec!(540)));
    assert_eq!(bnb.fee_disposal_pnl, Some(dec!(10)));
    assert_eq!(
        a.fees.valued_fees.get(&AssetId("BNB".into())),
        Some(&dec!(70))
    );
}

// ---- F-04: a mismatched inbound leg must not lose the outbound principal --------

#[test]
fn r1_a_06_transfer_mismatch_keeps_outbound_for_corrected_inbound() {
    let transfer = |acc: &str, id: &str, seq: i64, kind: TransferKind, amount: Decimal| {
        ev(
            acc,
            id,
            ts(2026, 3, 2),
            seq,
            EventPayload::Transfer {
                kind,
                group: TransferGroupId("g1".into()),
                counterparty: AccountId(if acc == "a" { "b" } else { "a" }.into()),
                asset: AssetId("USD".into()),
                principal: amount,
                fee: None,
            },
        )
    };
    let events = vec![
        deposit("a", "dep", ts(2026, 3, 1), 1, "USD", dec!(1000)),
        transfer("a", "out", 2, TransferKind::Out, dec!(500)),
        transfer("b", "in-bad", 3, TransferKind::In, dec!(400)),
        transfer("b", "in-fixed", 4, TransferKind::In, dec!(500)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &usd(), &InstrumentMap::default());
    assert!(
        matches!(out[2].1, ApplyOutcome::Pending { .. }),
        "mismatch is pending"
    );
    assert!(
        matches!(out[3].1, ApplyOutcome::Posted { .. }),
        "the matching inbound leg still pairs"
    );
    let a = &engine.accounts[&AccountId("a".into())];
    let b = &engine.accounts[&AccountId("b".into())];
    assert_eq!(a.cash_of(&AssetId("USD".into())), dec!(500));
    assert_eq!(
        b.cash_of(&AssetId("USD".into())),
        dec!(500),
        "principal not lost"
    );
}

// ---- F-05: realized PnL with any unknown-cost disposal is incomplete -----------

#[test]
fn r1_a_07_mixed_unknown_and_known_disposals_keep_realized_incomplete() {
    let events = vec![
        ev(
            "a",
            "open",
            ts(2026, 2, 1),
            1,
            EventPayload::OpeningPosition {
                asset: AssetId("AAPL".into()),
                quantity: dec!(10),
                cost: Some(OpeningCost::Unknown),
            },
        ),
        deposit("a", "dep", ts(2026, 2, 1), 2, "USD", dec!(1000)),
        buy_aapl("buy", ts(2026, 2, 2), 3, dec!(10), dec!(50)),
        sell_aapl("s1", ts(2026, 2, 3), 4, dec!(15), dec!(60)),
        sell_aapl("s2", ts(2026, 2, 4), 5, dec!(5), dec!(60)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &usd(), &aapl());
    let empty = LedgerEngine::new(CurrencyCode::usd());
    let pnl = period_pnl(
        &empty,
        &engine,
        &events,
        &[AccountId("a".into())],
        ts(2026, 1, 1),
        ts(2026, 3, 1),
        &PriceMap::default(),
        &usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(
        pnl.realized_pnl, None,
        "unknown cost is never folded into a known total"
    );
    assert_ne!(pnl.quality, Quality::Complete);
}

#[test]
fn r1_a_07_cross_currency_cost_basis_is_not_mixed_into_realized() {
    // Opening AAPL with a known EUR cost, then sold in USD: the engine must
    // not subtract EUR cost from USD proceeds.
    let events = vec![
        ev(
            "a",
            "open",
            ts(2026, 2, 1),
            1,
            EventPayload::OpeningPosition {
                asset: AssetId("AAPL".into()),
                quantity: dec!(10),
                cost: Some(OpeningCost::Known {
                    total: dec!(900),
                    currency: CurrencyCode("EUR".into()),
                }),
            },
        ),
        sell_aapl("s", ts(2026, 2, 3), 2, dec!(10), dec!(100)),
    ];
    let fx = TableFx(vec![(("EUR", "USD"), dec!(1.1))]);
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &fx, &aapl());
    match &out[1].1 {
        ApplyOutcome::Pending { pending } => {
            assert!(
                pending.reason.contains("cost currency"),
                "{}",
                pending.reason
            )
        }
        ApplyOutcome::Posted { .. } => {
            let h = engine.accounts[&AccountId("a".into())]
                .holding(&AssetId("AAPL".into()))
                .unwrap();
            assert_ne!(
                h.realized_pnl,
                Some(dec!(100)),
                "1000 USD - 900 EUR is not 100"
            );
        }
    }
}

// ---- F-06: period aggregates cover the period only ------------------------------

#[test]
fn r1_a_09_period_realized_fees_income_are_period_only() {
    // AC-02 trades happen before the period [2026-01-09, 2026-01-10); the
    // period has no trades, so realized/fees/income in the period are 0.
    let mut events = vec![
        deposit("a", "dep", ts(2026, 1, 5), 1, "USD", dec!(2000)),
        ev(
            "a",
            "buy",
            ts(2026, 1, 6),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(10),
                price: dec!(100),
                quote_currency: CurrencyCode::usd(),
                fees: vec![Fee {
                    asset: AssetId("USD".into()),
                    amount: dec!(1),
                    category: "commission".into(),
                }],
            },
        ),
        ev(
            "a",
            "sell",
            ts(2026, 1, 8),
            3,
            EventPayload::Sell {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(4),
                price: dec!(110),
                quote_currency: CurrencyCode::usd(),
                fees: vec![Fee {
                    asset: AssetId("USD".into()),
                    amount: dec!(1),
                    category: "commission".into(),
                }],
            },
        ),
    ];
    events.sort_by_key(|e| e.sort_key());
    let mut prices = PriceMap::default();
    for (day, px) in [(8, dec!(110)), (9, dec!(105))] {
        prices.insert(
            AssetId("AAPL".into()),
            PriceQuote {
                price: px,
                currency: CurrencyCode::usd(),
                as_of: ts(2026, 1, day),
                stale: false,
            },
        );
    }
    let start = ts(2026, 1, 9);
    let end = ts(2026, 1, 10);
    let mut engine_start = LedgerEngine::new(CurrencyCode::usd());
    let before: Vec<EconomicEvent> = events
        .iter()
        .filter(|e| e.occurred_at < start)
        .cloned()
        .collect();
    apply_effective(&mut engine_start, &before, &usd(), &aapl());
    let mut engine_end = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine_end, &events, &usd(), &aapl());
    let in_period: Vec<EconomicEvent> = events
        .iter()
        .filter(|e| e.occurred_at >= start && e.occurred_at < end)
        .cloned()
        .collect();
    let pnl = period_pnl(
        &engine_start,
        &engine_end,
        &in_period,
        &[AccountId("a".into())],
        start,
        end,
        &prices,
        &usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(
        pnl.realized_pnl,
        Some(dec!(0)),
        "AC-02 sale is outside the period"
    );
    assert_eq!(pnl.fees_valued, Some(dec!(0)));
    assert_eq!(pnl.income, Some(dec!(0)));
    // Start: 1438 + 6×105 (latest price at start is day 9 → 105) = 2068;
    // end uses the same price → period PnL 0.
    assert_eq!(pnl.total_pnl, Some(dec!(0)));
}

// ---- F-07: valuation lists missing inputs and keeps closed positions out ---------

#[test]
fn r1_a_05_closed_position_without_price_keeps_total_valued() {
    let events = vec![
        deposit("a", "dep", ts(2026, 2, 1), 1, "USD", dec!(1000)),
        buy_aapl("b", ts(2026, 2, 2), 2, dec!(1), dec!(100)),
        sell_aapl("s", ts(2026, 2, 3), 3, dec!(1), dec!(110)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &usd(), &aapl());
    let v = value_workspace(
        &engine,
        ts(2026, 2, 4),
        &PriceMap::default(),
        &usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(v.total_market_value, Some(dec!(1010)));
    assert_eq!(v.quality, Quality::Complete);
}

#[test]
fn r1_a_09_missing_price_lists_missing_input_and_known_part() {
    let events = vec![
        deposit("a", "dep", ts(2026, 2, 1), 1, "USD", dec!(1000)),
        buy_aapl("b", ts(2026, 2, 2), 2, dec!(1), dec!(100)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &usd(), &aapl());
    let v = value_workspace(
        &engine,
        ts(2026, 2, 4),
        &PriceMap::default(),
        &usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(v.total_market_value, None);
    assert_eq!(v.quality, Quality::Partial);
    assert_eq!(v.known_market_value, dec!(900), "cash part stays visible");
    assert!(
        v.missing_inputs.iter().any(|m| m.contains("AAPL")),
        "missing price is listed: {:?}",
        v.missing_inputs
    );
}

#[test]
fn r1_a_06_period_pnl_counts_only_scope_accounts() {
    // Account b sits in the same engine but outside the scope [a]: its
    // balance must not enter a's net worth (cross-account isolation).
    let events = vec![
        deposit("a", "dep-a", ts(2026, 5, 2), 1, "USD", dec!(100)),
        deposit("b", "dep-b", ts(2026, 5, 2), 2, "USD", dec!(5000)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &usd(), &InstrumentMap::default());
    let empty = LedgerEngine::new(CurrencyCode::usd());
    let pnl = period_pnl(
        &empty,
        &engine,
        &events,
        &[AccountId("a".into())],
        ts(2026, 5, 1),
        ts(2026, 6, 1),
        &PriceMap::default(),
        &usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(pnl.end_net_worth, Some(dec!(100)));
    assert_eq!(pnl.net_external_flow, Some(dec!(100)));
    assert_eq!(pnl.total_pnl, Some(dec!(0)));
}

#[test]
fn r1_a_07_unknown_cost_sale_posts_balanced_without_realized() {
    // Opening 10 AAPL with unknown cost, sell 4 @ 100: cash +400 is booked,
    // the journal balances through the 待补全成本 equity row, and realized
    // PnL stays unknown.
    let events = vec![
        ev(
            "a",
            "open",
            ts(2026, 2, 1),
            1,
            EventPayload::OpeningPosition {
                asset: AssetId("AAPL".into()),
                quantity: dec!(10),
                cost: Some(OpeningCost::Unknown),
            },
        ),
        sell_aapl("s", ts(2026, 2, 2), 2, dec!(4), dec!(100)),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &usd(), &aapl());
    assert_all_posted_and_balanced(&out);
    let a = &engine.accounts[&AccountId("a".into())];
    assert_eq!(a.cash_of(&AssetId("USD".into())), dec!(400));
    let h = a.holding(&AssetId("AAPL".into())).unwrap();
    assert_eq!(h.quantity(), dec!(6));
    assert_eq!(h.realized_pnl, None);
    assert_eq!(h.unknown_cost_disposals, 1);
}

#[test]
fn r1_a_06_in_transit_position_is_valued_at_market_not_at_cost() {
    // 1 BTC with known cost 10000 leaves "ex" for "wallet"; while it is in
    // transit the portfolio must keep its market value (50000), not drop to
    // the historical cost and jump back on arrival (F-13 review).
    use delta_core::valuation::value_accounts;
    let open_at = ts(2026, 3, 1);
    let out_at = ts(2026, 3, 2);
    let events = vec![
        ev(
            "ex",
            "open-btc",
            open_at,
            1,
            EventPayload::OpeningPosition {
                asset: AssetId("BTC".into()),
                quantity: dec!(1),
                cost: Some(OpeningCost::Known {
                    total: dec!(10000),
                    currency: CurrencyCode::usd(),
                }),
            },
        ),
        ev(
            "ex",
            "btc-out",
            out_at,
            2,
            EventPayload::Transfer {
                kind: TransferKind::Out,
                group: TransferGroupId("move".into()),
                counterparty: AccountId("wallet".into()),
                asset: AssetId("BTC".into()),
                principal: dec!(1),
                fee: None,
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let out = apply_effective(&mut engine, &events, &usd(), &InstrumentMap::default());
    assert_all_posted_and_balanced(&out);
    let mut prices = PriceMap::default();
    prices.insert(
        AssetId("BTC".into()),
        PriceQuote {
            price: dec!(50000),
            currency: CurrencyCode::usd(),
            as_of: out_at,
            stale: false,
        },
    );
    let both = |id: &AccountId| id.0 == "ex" || id.0 == "wallet";
    let v = value_accounts(
        &engine,
        out_at,
        &prices,
        &usd(),
        &CurrencyCode::usd(),
        &both,
    )
    .unwrap();
    assert_eq!(v.total_market_value, Some(dec!(50000)));
    let row = v.positions.iter().find(|p| p.asset.0 == "BTC").unwrap();
    assert_eq!(row.carrying_cost, Some(dec!(10000)));
    assert_eq!(row.unrealized_pnl, Some(dec!(40000)));
    // Without a price the in-transit row is missing, not silently at cost.
    let none = PriceMap::default();
    let v = value_accounts(&engine, out_at, &none, &usd(), &CurrencyCode::usd(), &both).unwrap();
    assert_eq!(v.total_market_value, None);
    assert!(v.missing_inputs.iter().any(|m| m.contains("BTC")));
}
