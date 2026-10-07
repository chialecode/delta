//! R1-A-05 / AC-02 golden scenario with independent hand-computed
//! expectations (financial-engine §2 sample). Expectations come from the
//! contract document, never from the engine under test.

use chrono::{TimeZone, Utc};
use delta_core::events::{EconomicEvent, EventPayload, Fee};
use delta_core::ids::{AccountId, AssetId, EventId, InstrumentId};
use delta_core::ledger::{
    apply_effective, IdentityFx, InstrumentMap, LedgerCategory, LedgerEngine,
};
use delta_core::money::CurrencyCode;
use delta_core::pnl::period_pnl;
use delta_core::valuation::{PriceMap, PriceQuote};
use rust_decimal_macros::dec;

fn ts(y: i32, m: u32, d: u32, h: u32) -> chrono::DateTime<chrono::Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
}

fn ev(
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
    seq: i64,
    payload: EventPayload,
) -> EconomicEvent {
    EconomicEvent {
        id: EventId(id.into()),
        account_id: AccountId("acc-us".into()),
        occurred_at: at,
        recorded_at: at,
        seq,
        source_ref: None,
        correction_group: None,
        revision: 0,
        reverses: None,
        payload,
    }
}

fn instruments() -> InstrumentMap {
    let mut m = InstrumentMap::default();
    m.0.insert(InstrumentId("NASDAQ:AAPL".into()), AssetId("AAPL".into()));
    m
}

fn fx_usd() -> IdentityFx {
    IdentityFx {
        currency: CurrencyCode::usd(),
    }
}

/// AC-02 fixture: opening 2000; buy 10@100 fee 1; sell 4@110 fee 1; price 105.
pub fn ac02_events() -> Vec<EconomicEvent> {
    vec![
        ev(
            "ev-open",
            ts(2026, 1, 5, 14),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(2000),
            },
        ),
        ev(
            "ev-buy",
            ts(2026, 1, 6, 14),
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
            "ev-sell",
            ts(2026, 1, 8, 14),
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
    ]
}

pub fn ac02_prices() -> PriceMap {
    let mut p = PriceMap::default();
    p.insert(
        AssetId("AAPL".into()),
        PriceQuote {
            price: dec!(105),
            currency: CurrencyCode::usd(),
            as_of: ts(2026, 1, 9, 21),
            stale: false,
        },
    );
    p
}

#[test]
fn r1_a_05_ac02_golden_hand_computed_values() {
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let events = ac02_events();
    let outcomes = apply_effective(&mut engine, &events, &fx_usd(), &instruments());
    assert_eq!(outcomes.len(), 3, "all events post (no pending)");
    let ledger = &engine.accounts[&AccountId("acc-us".into())];

    // 现金 1438 = 2000 − 1001 + 439
    assert_eq!(ledger.cash_of(&AssetId("USD".into())), dec!(1438));
    let holding = ledger.holding(&AssetId("AAPL".into())).unwrap();
    // 剩余数量 6，剩余成本 600.6
    assert_eq!(holding.quantity(), dec!(6));
    assert_eq!(holding.carrying_cost(), Some(dec!(600.6)));
    // 已实现 38.6
    assert_eq!(holding.realized_pnl, Some(dec!(38.6)));
    assert_eq!(holding.realized_count, 1);
    // 费用解释项：共 2（已含在成本与净收入中，不重复扣）
    assert_eq!(
        ledger.fees.trade_fees.get(&AssetId("USD".into())),
        Some(&dec!(2))
    );

    // 期末价 105：未实现 = 6 × (105 − 100.1) = 29.4
    let at = ts(2026, 1, 9, 21);
    let v = delta_core::valuation::value_workspace(
        &engine,
        at,
        &ac02_prices(),
        &fx_usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    let aapl = v
        .positions
        .iter()
        .find(|p| p.asset == AssetId("AAPL".into()))
        .unwrap();
    assert_eq!(aapl.market_value, Some(dec!(630)));
    assert_eq!(aapl.unrealized_pnl, Some(dec!(29.4)));
    assert_eq!(v.total_market_value, Some(dec!(2068))); // 1438 + 630

    // 总损益 68 = 38.6 + 29.4；区间损益 = 2068 − 0 − 2000 = 68（期初为空仓 0）
    // 期初快照：仅应用期初之前的有效事件（此处为空）。
    let start_events: Vec<EconomicEvent> = events
        .iter()
        .filter(|e| e.occurred_at < ts(2026, 1, 1, 0))
        .cloned()
        .collect();
    let mut engine_start = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine_start, &start_events, &fx_usd(), &instruments());
    let pnl = period_pnl(
        &engine_start,
        &engine,
        &delta_core::pnl::effective(&events),
        &[AccountId("acc-us".into())],
        ts(2026, 1, 1, 0),
        ts(2026, 1, 10, 0),
        &ac02_prices(),
        &fx_usd(),
        &CurrencyCode::usd(),
    )
    .unwrap();
    // Start (2026-01-01) has no price and no position: net worth 0 (empty ledger).
    assert_eq!(pnl.end_net_worth, Some(dec!(2068)));
    assert_eq!(pnl.net_external_flow, Some(dec!(2000))); // initial deposit is external
    assert_eq!(pnl.total_pnl, Some(dec!(68)));
    assert_eq!(pnl.quality, delta_core::valuation::Quality::Complete);
    // 开市前无价格 → 期初净资产不可得；此处期初账本为空 → 0
    assert_eq!(pnl.start_net_worth, Some(dec!(0)));
}

#[test]
fn r1_a_05_ac02_double_entry_balances_and_categorizes() {
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let outcomes = apply_effective(&mut engine, &ac02_events(), &fx_usd(), &instruments());
    let mut buy_holding = None;
    for (_, o) in outcomes {
        match o {
            delta_core::ledger::ApplyOutcome::Posted { transaction } => {
                assert!(transaction.balances_to_zero(), "transaction must balance");
                for p in &transaction.postings {
                    if p.category == LedgerCategory::Holding && p.native_quantity > dec!(0) {
                        buy_holding = Some(p.book_amount);
                    }
                }
            }
            delta_core::ledger::ApplyOutcome::Pending { pending } => {
                panic!("unexpected pending: {}", pending.reason);
            }
        }
    }
    // 买入分录：借持仓成本 1001、贷现金 1001（费用计入成本）
    assert_eq!(buy_holding, Some(dec!(1001)));
}

#[test]
fn r1_a_05_multi_batch_full_sell_conserves_cost() {
    // 买10@100费1 → 卖4@110费1 → 卖6@120费2 → 清仓
    // 手算：现金 = 2000 − 1001 + 439 + 718 = 2156；累计已实现 = 156
    let mut events = ac02_events();
    events.push(ev(
        "ev-sell2",
        ts(2026, 1, 12, 14),
        4,
        EventPayload::Sell {
            instrument: InstrumentId("NASDAQ:AAPL".into()),
            quantity: dec!(6),
            price: dec!(120),
            quote_currency: CurrencyCode::usd(),
            fees: vec![Fee {
                asset: AssetId("USD".into()),
                amount: dec!(2),
                category: "commission".into(),
            }],
        },
    ));
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &fx_usd(), &instruments());
    let ledger = &engine.accounts[&AccountId("acc-us".into())];
    assert_eq!(ledger.cash_of(&AssetId("USD".into())), dec!(2156));
    let holding = ledger.holding(&AssetId("AAPL".into())).unwrap();
    assert_eq!(holding.quantity(), dec!(0));
    assert_eq!(holding.carrying_cost(), Some(dec!(0)));
    assert_eq!(holding.realized_pnl, Some(dec!(156)));
}

#[test]
fn r1_a_05_partial_sell_across_lots_allocates_fifo() {
    // 两笔买入（6@50 费 0.6、4@60 费 0.4），卖 8：先吃 6 股单位成本 50.1，
    // 再吃 2 股单位成本 60.1。成本 = 300.6 + 120.2 = 420.8
    // 先入金 1000：没有资金的买入应进入待入账区而不是凭空建仓（F-01）。
    let events = vec![
        ev(
            "dep",
            ts(2026, 2, 1, 10),
            0,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
        ev(
            "b1",
            ts(2026, 2, 1, 14),
            1,
            EventPayload::Buy {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(6),
                price: dec!(50),
                quote_currency: CurrencyCode::usd(),
                fees: vec![Fee {
                    asset: AssetId("USD".into()),
                    amount: dec!(0.6),
                    category: "commission".into(),
                }],
            },
        ),
        ev(
            "b2",
            ts(2026, 2, 2, 14),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(4),
                price: dec!(60),
                quote_currency: CurrencyCode::usd(),
                fees: vec![Fee {
                    asset: AssetId("USD".into()),
                    amount: dec!(0.4),
                    category: "commission".into(),
                }],
            },
        ),
        ev(
            "s1",
            ts(2026, 2, 3, 14),
            3,
            EventPayload::Sell {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(8),
                price: dec!(55),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &fx_usd(), &instruments());
    let ledger = &engine.accounts[&AccountId("acc-us".into())];
    let holding = ledger.holding(&AssetId("AAPL".into())).unwrap();
    assert_eq!(holding.quantity(), dec!(2));
    // 剩余 = 第二批残量成本：300.6+240.4 − 420.8 = 120.2
    assert_eq!(holding.carrying_cost(), Some(dec!(120.2)));
    assert_eq!(holding.realized_pnl, Some(dec!(19.2)));
    // 现金 = 1000 − 300.6 − 240.4 + 8×55 = 899，且无待入账事件
    assert_eq!(ledger.cash_of(&AssetId("USD".into())), dec!(899));
    assert!(ledger.pending.is_empty());
}

#[test]
fn r1_a_07_oversell_is_rejected() {
    let events = vec![
        ev(
            "o",
            ts(2026, 2, 1, 14),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
        ev(
            "b",
            ts(2026, 2, 1, 15),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(1),
                price: dec!(100),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
        ev(
            "s",
            ts(2026, 2, 2, 15),
            3,
            EventPayload::Sell {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(2),
                price: dec!(100),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    let outcomes = apply_effective(&mut engine, &events, &fx_usd(), &instruments());
    match &outcomes[2].1 {
        delta_core::ledger::ApplyOutcome::Pending { pending } => {
            assert!(pending.reason.contains("oversell"), "{}", pending.reason);
        }
        delta_core::ledger::ApplyOutcome::Posted { .. } => {
            panic!("oversell must be rejected");
        }
    }
}

#[test]
fn r1_a_07_unknown_cost_never_counts_as_zero_pnl() {
    let events = vec![
        ev(
            "open",
            ts(2026, 2, 1, 14),
            1,
            EventPayload::OpeningPosition {
                asset: AssetId("AAPL".into()),
                quantity: dec!(10),
                cost: Some(delta_core::events::OpeningCost::Unknown),
            },
        ),
        ev(
            "s",
            ts(2026, 2, 2, 15),
            2,
            EventPayload::Sell {
                instrument: InstrumentId("NASDAQ:AAPL".into()),
                quantity: dec!(4),
                price: dec!(100),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &fx_usd(), &instruments());
    let ledger = &engine.accounts[&AccountId("acc-us".into())];
    let holding = ledger.holding(&AssetId("AAPL".into())).unwrap();
    // 卖出照常入账（现金 +400），但已实现不得伪装为 0
    assert_eq!(ledger.cash_of(&AssetId("USD".into())), dec!(400));
    assert_eq!(holding.realized_pnl, None);
    assert_eq!(holding.unknown_cost_disposals, 1);
}
