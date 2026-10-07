//! Transfer pairing (AC-04), corporate actions (AC-05), crypto multi-asset
//! semantics with third-asset fees (AC-22) — hand-computed expectations.

use chrono::{TimeZone, Utc};
use delta_core::events::{EconomicEvent, EventPayload, Fee, TransferKind};
use delta_core::ids::{AccountId, AssetId, EventId, InstrumentId, TransferGroupId};
use delta_core::ledger::{apply_effective, FxResolver, IdentityFx, InstrumentMap, LedgerEngine};
use delta_core::money::CurrencyCode;
use delta_core::valuation::PriceMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
    Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
}

fn ev(
    acc: &str,
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
    seq: i64,
    payload: EventPayload,
) -> EconomicEvent {
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
        payload,
    }
}

fn fx_all_one(book: &str) -> IdentityFx {
    IdentityFx {
        currency: CurrencyCode(book.into()),
    }
}

/// FX resolver treating USDT as exactly 1 USD only when registered (tests use
/// explicit偏离 rates instead; here every registered currency is 1:1 to book).
/// For the USDT-decimals case see `r1_a_07_usdt_deviation`.
struct TableFx {
    #[allow(dead_code)]
    book: CurrencyCode,
    /// (base, quote) -> rate: 1 base = rate quote.
    rates: Vec<((CurrencyCode, CurrencyCode), Decimal)>,
}

impl FxResolver for TableFx {
    fn rate(
        &self,
        base: &CurrencyCode,
        quote: &CurrencyCode,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Option<(Decimal, chrono::DateTime<chrono::Utc>)> {
        if base == quote {
            return Some((Decimal::ONE, _at));
        }
        self.rates
            .iter()
            .find(|((b, q), _)| b == base && q == quote)
            .map(|(_, r)| (*r, _at))
    }
}

#[test]
fn r1_a_06_ac04_transfer_principal_no_gain_fee_single_expense() {
    // A 转出本金 998、手续费 2（现金共减少 1000）→ B 收到 998。
    // 组合视角：本金内部流转无收益；费用 −2。单账户：A 流出 1000，B 流入 998。
    let events = vec![
        ev(
            "a",
            "dep",
            ts(2026, 3, 1),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
        ev(
            "a",
            "out",
            ts(2026, 3, 2),
            2,
            EventPayload::Transfer {
                kind: TransferKind::Out,
                group: TransferGroupId("g1".into()),
                counterparty: AccountId("b".into()),
                asset: AssetId("USD".into()),
                principal: dec!(998),
                fee: Some(Fee {
                    asset: AssetId("USD".into()),
                    amount: dec!(2),
                    category: "transfer".into(),
                }),
            },
        ),
        ev(
            "b",
            "in",
            ts(2026, 3, 2),
            3,
            EventPayload::Transfer {
                kind: TransferKind::In,
                group: TransferGroupId("g1".into()),
                counterparty: AccountId("a".into()),
                asset: AssetId("USD".into()),
                principal: dec!(998),
                fee: None,
            },
        ),
    ];
    let fx = fx_all_one("USD");
    let mut engine = LedgerEngine::new(CurrencyCode::usd()).with_cash_asset(AssetId("USD".into()));
    apply_effective(&mut engine, &events, &fx, &InstrumentMap::default());
    let a = &engine.accounts[&AccountId("a".into())];
    let b = &engine.accounts[&AccountId("b".into())];
    assert_eq!(a.cash_of(&AssetId("USD".into())), dec!(0));
    assert_eq!(b.cash_of(&AssetId("USD".into())), dec!(998));
    // 单账户 A：净外部流 = 入金 +1000 − 本金转出 998 = +2（手续费 2 是支出，
    // 走费用账户而不是资金流）。
    let flows =
        delta_core::valuation::extract_external_flows(&events, &|acc: &AccountId| acc.0 == "a");
    let net = delta_core::valuation::flows_in_reporting(&flows, &fx, &CurrencyCode::usd()).unwrap();
    assert_eq!(net, Some(dec!(2)));
    // 组合视角（a+b）：两端都在范围内 → 本金不算外部流；费用仍计入支出
    let flows_p = delta_core::valuation::extract_external_flows(&events, &|_| true);
    let net_p =
        delta_core::valuation::flows_in_reporting(&flows_p, &fx, &CurrencyCode::usd()).unwrap();
    assert_eq!(net_p, Some(dec!(1000))); // 初始入金 1000，转账为内部
    assert_eq!(
        b.fees.valued_fees.get(&AssetId("USD".into())),
        None,
        "fee is charged on the outbound account"
    );
    assert_eq!(
        a.fees.valued_fees.get(&AssetId("USD".into())),
        Some(&dec!(2))
    );
}

#[test]
fn r1_a_06_transfer_without_pair_is_pending_not_fabricated() {
    let events = vec![ev(
        "b",
        "in",
        ts(2026, 3, 2),
        1,
        EventPayload::Transfer {
            kind: TransferKind::In,
            group: TransferGroupId("ghost".into()),
            counterparty: AccountId("a".into()),
            asset: AssetId("USD".into()),
            principal: dec!(500),
            fee: None,
        },
    )];
    let fx = fx_all_one("USD");
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &fx, &InstrumentMap::default());
    let b = &engine.accounts[&AccountId("b".into())];
    assert_eq!(
        b.cash_of(&AssetId("USD".into())),
        dec!(0),
        "no phantom cash"
    );
    assert_eq!(b.pending.len(), 1);
    assert!(b.pending[0].reason.contains("no paired outbound"));
}

#[test]
fn r1_a_08_ac05_split_doubles_quantity_total_cost_unchanged() {
    let mut instruments = InstrumentMap::default();
    instruments
        .0
        .insert(InstrumentId("NASDAQ:XYZ".into()), AssetId("XYZ".into()));
    let events = vec![
        ev(
            "a",
            "dep",
            ts(2026, 3, 1),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
        ev(
            "a",
            "buy",
            ts(2026, 3, 2),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("NASDAQ:XYZ".into()),
                quantity: dec!(10),
                price: dec!(50),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
        ev(
            "a",
            "split",
            ts(2026, 3, 10),
            3,
            EventPayload::StockSplit {
                instrument: InstrumentId("NASDAQ:XYZ".into()),
                ratio_num: 2,
                ratio_den: 1,
            },
        ),
    ];
    let fx = fx_all_one("USD");
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &fx, &instruments);
    let holding = engine.accounts[&AccountId("a".into())]
        .holding(&AssetId("XYZ".into()))
        .unwrap();
    assert_eq!(holding.quantity(), dec!(20));
    assert_eq!(holding.carrying_cost(), Some(dec!(500)), "总成本不变");
}

#[test]
fn r1_a_08_ac05_cash_dividend_recorded_once_on_pay_date() {
    let mut instruments = InstrumentMap::default();
    instruments
        .0
        .insert(InstrumentId("NASDAQ:XYZ".into()), AssetId("XYZ".into()));
    let mut events = vec![
        ev(
            "a",
            "dep",
            ts(2026, 3, 1),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
        ev(
            "a",
            "buy",
            ts(2026, 3, 2),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("NASDAQ:XYZ".into()),
                quantity: dec!(10),
                price: dec!(50),
                quote_currency: CurrencyCode::usd(),
                fees: vec![],
            },
        ),
        ev(
            "a",
            "div",
            ts(2026, 4, 1),
            3,
            EventPayload::DividendCash {
                cash_asset: AssetId("USD".into()),
                amount: dec!(5),
                instrument: InstrumentId("NASDAQ:XYZ".into()),
            },
        ),
    ];
    // 同一分红重复导入（同 source_ref、不同事件 id）→ Corrections 以
    // correction_group 去重：第二轮导入带相同组号与更高 revision。
    events[2].correction_group = Some("div-2026-q1".into());
    let dup = EconomicEvent {
        id: EventId("div-dup".into()),
        correction_group: Some("div-2026-q1".into()),
        revision: 1,
        ..events[2].clone()
    };
    let mut all = events.clone();
    all.push(dup);
    let fx = fx_all_one("USD");
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &all, &fx, &instruments);
    let ledger = &engine.accounts[&AccountId("a".into())];
    // 现金 = 1000 − 500 + 5（分红只记一次）
    assert_eq!(ledger.cash_of(&AssetId("USD".into())), dec!(505));
    assert_eq!(
        ledger.income.dividends.get(&AssetId("USD".into())),
        Some(&dec!(5))
    );
}

#[test]
fn r1_a_07_ac22_crypto_third_asset_fee_and_lot_move() {
    // BTC/USDT 买入 0.5 BTC @ 20000 USDT，手续费 0.001 BNB（值 60 USDT）
    // 再把 0.5 BTC 转到钱包账户 B。
    let mut instruments = InstrumentMap::default();
    instruments.0.insert(
        InstrumentId("BINANCE:BTCUSDT".into()),
        AssetId("BTC".into()),
    );
    instruments.0.insert(
        InstrumentId("BINANCE:BNBUSDT".into()),
        AssetId("BNB".into()),
    );
    let fx = TableFx {
        book: CurrencyCode::usd(),
        rates: vec![
            (
                (CurrencyCode("USDT".into()), CurrencyCode::usd()),
                dec!(1), // 稳定币按真实汇率（此处 1）
            ),
            (
                (CurrencyCode("BNB".into()), CurrencyCode("USDT".into())),
                dec!(60000), // 1 BNB = 60000 USDT
            ),
        ],
    };
    let events = vec![
        ev(
            "ex",
            "dep",
            ts(2026, 4, 1),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USDT".into()),
                amount: dec!(20000),
            },
        ),
        ev(
            "ex",
            "bnbbuy",
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
            "btcbuy",
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
        ev(
            "ex",
            "out",
            ts(2026, 4, 3),
            4,
            EventPayload::Transfer {
                kind: TransferKind::Out,
                group: TransferGroupId("btcmove".into()),
                counterparty: AccountId("wallet".into()),
                asset: AssetId("BTC".into()),
                principal: dec!(0.5),
                fee: None,
            },
        ),
        ev(
            "wallet",
            "in",
            ts(2026, 4, 3),
            5,
            EventPayload::Transfer {
                kind: TransferKind::In,
                group: TransferGroupId("btcmove".into()),
                counterparty: AccountId("ex".into()),
                asset: AssetId("BTC".into()),
                principal: dec!(0.5),
                fee: None,
            },
        ),
    ];
    let mut engine = LedgerEngine::new(CurrencyCode::usd()).with_cash_asset(AssetId("USDT".into()));
    apply_effective(&mut engine, &events, &fx, &instruments);
    let ex = &engine.accounts[&AccountId("ex".into())];
    let wallet = &engine.accounts[&AccountId("wallet".into())];

    // BNB 持仓：0.01 − 0.001 = 0.009；费用价值 0.001×60000 = 60 USD
    let bnb = ex.holding(&AssetId("BNB".into())).unwrap();
    assert_eq!(bnb.quantity(), dec!(0.009));
    // BTC 已全部转出
    assert_eq!(
        ex.holding(&AssetId("BTC".into())).unwrap().quantity(),
        dec!(0)
    );
    // 钱包获得 0.5 BTC，成本批次随迁（费用已含在买入成本 20000×0.5+60=10060）
    let btc = wallet.holding(&AssetId("BTC".into())).unwrap();
    assert_eq!(btc.quantity(), dec!(0.5));
    // 成本以 USDT 记（20120 USDT），转 USD 1:1 → 搬移不产生收益
    assert_eq!(btc.carrying_cost(), Some(dec!(10060)));
    // USDT 现金手算：20000 − 600（买 BNB）− 10000（买 BTC 本金）= 9400；
    // BNB 手续费只减少 BNB，不再从 USDT 重复扣（F-02）。全部事件正常入账。
    assert_eq!(ex.cash_of(&AssetId("USDT".into())), dec!(9400));
    assert!(ex.pending.is_empty(), "{:?}", ex.pending);
    assert!(wallet.pending.is_empty(), "{:?}", wallet.pending);
}

#[test]
fn r1_a_07_usdt_deviation_valued_by_real_fx() {
    // USDT 价格偏离 1 USD（0.98），估值必须用真实汇率而非假设 1:1。
    let fx = TableFx {
        book: CurrencyCode::usd(),
        rates: vec![(
            (CurrencyCode("USDT".into()), CurrencyCode::usd()),
            dec!(0.98),
        )],
    };
    let events = vec![ev(
        "a",
        "dep",
        ts(2026, 4, 1),
        1,
        EventPayload::CashDeposit {
            asset: AssetId("USDT".into()),
            amount: dec!(1000),
        },
    )];
    let mut engine = LedgerEngine::new(CurrencyCode::usd()).with_cash_asset(AssetId("USDT".into()));
    apply_effective(&mut engine, &events, &fx, &InstrumentMap::default());
    let v = delta_core::valuation::value_workspace(
        &engine,
        ts(2026, 4, 2),
        &PriceMap::default(),
        &fx,
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(v.total_market_value, Some(dec!(980)));
}

#[test]
fn r1_a_07_missing_fx_puts_event_into_pending_not_zero() {
    // 缺 BNB→USDT 汇率时，含第三资产费用的事件整体待入账。
    let mut instruments = InstrumentMap::default();
    instruments.0.insert(
        InstrumentId("BINANCE:BTCUSDT".into()),
        AssetId("BTC".into()),
    );
    let fx = TableFx {
        book: CurrencyCode::usd(),
        rates: vec![(
            (CurrencyCode("USDT".into()), CurrencyCode::usd()),
            Decimal::ONE,
        )],
        // BNB rate intentionally missing
    };
    let events = vec![
        ev(
            "ex",
            "dep",
            ts(2026, 4, 1),
            1,
            EventPayload::CashDeposit {
                asset: AssetId("USDT".into()),
                amount: dec!(10000),
            },
        ),
        ev(
            "ex",
            "buy",
            ts(2026, 4, 2),
            2,
            EventPayload::Buy {
                instrument: InstrumentId("BINANCE:BTCUSDT".into()),
                quantity: dec!(0.1),
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
    apply_effective(&mut engine, &events, &fx, &instruments);
    let ex = &engine.accounts[&AccountId("ex".into())];
    // 缺本位币折算 → 进入待入账区，不能只减现金或忽略费用
    assert!(ex.holding(&AssetId("BTC".into())).is_none());
    assert_eq!(ex.pending.len(), 1);
    assert!(ex.pending[0].reason.contains("missing price for fee asset"));
}

#[test]
fn r1_a_09_ac07_deposit_only_period_pnl_is_zero_not_100pct() {
    // 期初 1000，期中入金 1000，市场不变 → 区间损益 0，不得显示 100% 收益。
    let events = vec![
        ev(
            "a",
            "open",
            ts(2026, 4, 30),
            1,
            EventPayload::OpeningPosition {
                asset: AssetId("USD".into()),
                quantity: dec!(1000),
                cost: None,
            },
        ),
        ev(
            "a",
            "dep",
            ts(2026, 5, 15),
            2,
            EventPayload::CashDeposit {
                asset: AssetId("USD".into()),
                amount: dec!(1000),
            },
        ),
    ];
    let fx = fx_all_one("USD");
    let mut engine = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(&mut engine, &events, &fx, &InstrumentMap::default());
    let start_events: Vec<EconomicEvent> = events
        .iter()
        .filter(|e| e.occurred_at < ts(2026, 5, 1))
        .cloned()
        .collect();
    let mut engine_start = LedgerEngine::new(CurrencyCode::usd());
    apply_effective(
        &mut engine_start,
        &start_events,
        &fx,
        &InstrumentMap::default(),
    );
    let pnl = delta_core::pnl::period_pnl(
        &engine_start,
        &engine,
        &events,
        &[AccountId("a".into())],
        ts(2026, 5, 1),
        ts(2026, 6, 1),
        &PriceMap::default(),
        &fx,
        &CurrencyCode::usd(),
    )
    .unwrap();
    assert_eq!(pnl.start_net_worth, Some(dec!(1000)));
    assert_eq!(pnl.end_net_worth, Some(dec!(2000)));
    assert_eq!(pnl.net_external_flow, Some(dec!(1000)));
    assert_eq!(pnl.total_pnl, Some(dec!(0)), "损益必须为 0");
    assert_eq!(pnl.quality, delta_core::valuation::Quality::Complete);
}
