//! Conservation property tests (financial-engine §9): for any sequence of
//! valid trades, cash + holdings value changes exactly match flows, fees and
//! realized PnL; full liquidation conserves total cost.

use chrono::{TimeZone, Utc};
use delta_core::events::{EconomicEvent, EventPayload, Fee};
use delta_core::ids::{AccountId, AssetId, EventId, InstrumentId};
use delta_core::ledger::{apply_effective, IdentityFx, InstrumentMap, LedgerEngine};
use delta_core::money::CurrencyCode;
use proptest::prelude::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(y: i32, m: u32, d: u32, seq: i64) -> chrono::DateTime<chrono::Utc> {
    let base = Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap();
    base + chrono::Duration::seconds(seq)
}

fn ev(
    id: &str,
    at: chrono::DateTime<chrono::Utc>,
    seq: i64,
    payload: EventPayload,
) -> EconomicEvent {
    EconomicEvent {
        id: EventId(id.into()),
        account_id: AccountId("a".into()),
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
    m.0.insert(InstrumentId("X:TEST".into()), AssetId("TEST".into()));
    m
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_buy_sell_conservation(
        buys in prop::collection::vec((1u32..100, 1u32..200), 1..8),
        sells in prop::collection::vec(1u32..50, 0..6),
        prices in prop::collection::vec(1u32..300, 1..8),
    ) {
        let mut events = Vec::new();
        let mut seq = 0;
        let mut day = 1;
        events.push(ev("dep", ts(2026, 6, day, seq), seq, EventPayload::CashDeposit {
            asset: AssetId("USD".into()),
            amount: dec!(1000000),
        }));
        // Buys at the given prices.
        for (i, (qty, _price)) in buys.iter().enumerate() {
            seq += 1; day += 1;
            events.push(ev(format!("b{i}").as_str(), ts(2026, 6, day, seq), seq, EventPayload::Buy {
                instrument: InstrumentId("X:TEST".into()),
                quantity: Decimal::from(*qty),
                price: Decimal::from(prices[i % prices.len()]),
                quote_currency: CurrencyCode::usd(),
                fees: vec![Fee { asset: AssetId("USD".into()), amount: Decimal::ONE, category: "c".into() }],
            }));
        }
        // Sells: track remaining quantity so we never oversell (oversell is a
        // rejected event, not a conservation violation).
        let total_bought: Decimal = buys.iter().map(|(q, _)| Decimal::from(*q)).sum();
        let mut remaining = total_bought;
        for (i, qty) in sells.iter().enumerate() {
            let q = Decimal::from(*qty).min(remaining);
            if q.is_zero() { break; }
            seq += 1; day += 1;
            events.push(ev(format!("s{i}").as_str(), ts(2026, 6, day, seq), seq, EventPayload::Sell {
                instrument: InstrumentId("X:TEST".into()),
                quantity: q,
                price: Decimal::from(prices[(i + 1) % prices.len()]),
                quote_currency: CurrencyCode::usd(),
                fees: vec![Fee { asset: AssetId("USD".into()), amount: Decimal::ONE, category: "c".into() }],
            }));
            remaining -= q;
        }

        let fx = IdentityFx { currency: CurrencyCode::usd() };
        let mut engine = LedgerEngine::new(CurrencyCode::usd());
        let outcomes = apply_effective(&mut engine, &events, &fx, &instruments());
        // No pending: the scenario is always fundable and non-overselling.
        for (id, o) in &outcomes {
            if let delta_core::ledger::ApplyOutcome::Pending { pending } = o {
                prop_assert!(false, "unexpected pending for {id}: {}", pending.reason);
            }
        }

        let ledger = &engine.accounts[&AccountId("a".into())];
        let holding = ledger.holding(&AssetId("TEST".into())).unwrap();
        // Quantity conservation: remaining equals bought minus sold.
        let sold: Decimal = sells.iter()
            .scan(total_bought, |rem, q| {
                let take = Decimal::from(*q).min(*rem);
                *rem -= take;
                Some(take)
            })
            .sum();
        prop_assert_eq!(holding.quantity(), total_bought - sold);
        // Every posted transaction balances to zero in book currency.
        for (id, o) in &outcomes {
            if let delta_core::ledger::ApplyOutcome::Posted { transaction } = o {
                prop_assert!(transaction.balances_to_zero(), "{id} unbalanced");
            }
        }
        // Cash = initial − buys total − fees + sells net.
        let buy_spend: Decimal = buys.iter().enumerate()
            .map(|(i, (q, _))| Decimal::from(*q) * Decimal::from(prices[i % prices.len()]) + Decimal::ONE)
            .sum();
        let sell_gain: Decimal = sells.iter()
            .enumerate()
            .scan(total_bought, |rem, (i, q)| {
                let take = Decimal::from(*q).min(*rem);
                *rem -= take;
                Some((i, take))
            })
            .filter(|(_, take)| *take > Decimal::ZERO)
            .map(|(i, q)| q * Decimal::from(prices[(i + 1) % prices.len()]) - Decimal::ONE)
            .sum();
        prop_assert_eq!(
            ledger.cash_of(&AssetId("USD".into())),
            dec!(1000000) - buy_spend + sell_gain
        );
        // Cost conservation: realized = Σ net proceeds − relieved cost, so
        // carrying cost + relieved cost = total bought cost (fees included).
        // Partial allocations keep high precision (financial-engine §2), so
        // the only allowed gap is Decimal's 28-digit summation residue.
        let realized = holding.realized_pnl.unwrap_or_default();
        let relieved = sell_gain - realized;
        let gap = (holding.carrying_cost().unwrap() + relieved - buy_spend).abs();
        prop_assert!(gap <= dec!(0.000000000000000001), "cost not conserved, gap {gap}");
        if holding.quantity().is_zero() {
            // Full liquidation: the last take absorbs the residue exactly.
            prop_assert_eq!(holding.carrying_cost().unwrap(), Decimal::ZERO);
        }
    }
}
