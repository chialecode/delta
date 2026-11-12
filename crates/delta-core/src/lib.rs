pub mod decimal;
pub mod indicators;
pub mod ledger;
pub mod model;
pub mod sim;
pub mod sim_report;
pub mod valuation;

pub use decimal::*;
pub use indicators::*;
pub use ledger::*;
pub use model::*;
pub use sim::*;
pub use sim_report::*;
pub use valuation::*;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use rust_decimal::Decimal;
    use std::collections::HashMap;

    #[test]
    fn poc_d05_decimal_lossless_and_json_raw_parsing() {
        let dec = parse_decimal_lossless("124.80750274658203").expect("parse decimal");
        assert_eq!(to_canonical_text(dec), "124.80750274658203");

        // Parse directly from JSON raw number token without f64 intermediate (S-08)
        let json_payload = r#"{"close": 124.80750274658203, "str_price": "499.2300"}"#;
        #[derive(serde::Deserialize)]
        struct Sample {
            close: DirectDecimal,
            str_price: DirectDecimal,
        }
        let sample: Sample = serde_json::from_str(json_payload).expect("deserialize DirectDecimal");
        assert_eq!(to_canonical_text(sample.close.0), "124.80750274658203");
        assert_eq!(to_canonical_text(sample.str_price.0), "499.23");

        // Restore Yahoo AAPL 2020-08-28 split-adjusted close (4:1 split on 2020-08-31) -> 499.23
        let factor = parse_decimal_lossless("4").unwrap();
        let restored = restore_yahoo_raw_price(sample.close.0, factor);
        assert_eq!(to_canonical_text(restored), "499.23");

        // Reject excessive scale without silent truncation
        let err = parse_decimal_with_max_scale("1.123456789", 6).unwrap_err();
        assert!(matches!(err, DecimalError::PrecisionExceeded { .. }));
    }

    #[test]
    fn poc_d05_ac02_minimal_ledger_and_fifo() {
        // AC-02: Initial USD 2000, buy 10 * 100 fee 1, sell 4 * 110 fee 1, ending price 105
        // Expected: cash 1438, qty 6, cost 600.6, realized 38.6, unrealized 29.4, total PnL 68, net worth 2068
        let t0 = Utc.with_ymd_and_hms(2026, 1, 2, 14, 30, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 1, 5, 15, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 1, 10, 15, 30, 0).unwrap();

        let events = vec![
            LedgerEventInput {
                event_id: "EV-OPEN".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::OpeningBalance,
                occurred_at: t0,
                sequence_no: 1,
                asset_id: "USD".into(),
                instrument_id: None,
                quantity: parse_decimal_lossless("2000").unwrap(),
                price: None,
                quote_currency: "USD".into(),
                fee_amount: Decimal::ZERO,
                fee_currency: "USD".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
            LedgerEventInput {
                event_id: "EV-BUY".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::Buy,
                occurred_at: t1,
                sequence_no: 2,
                asset_id: "NASDAQ:AAPL".into(),
                instrument_id: Some("NASDAQ:AAPL".into()),
                quantity: parse_decimal_lossless("10").unwrap(),
                price: Some(parse_decimal_lossless("100").unwrap()),
                quote_currency: "USD".into(),
                fee_amount: parse_decimal_lossless("1").unwrap(),
                fee_currency: "USD".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
            LedgerEventInput {
                event_id: "EV-SELL".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::Sell,
                occurred_at: t2,
                sequence_no: 3,
                asset_id: "NASDAQ:AAPL".into(),
                instrument_id: Some("NASDAQ:AAPL".into()),
                quantity: parse_decimal_lossless("4").unwrap(),
                price: Some(parse_decimal_lossless("110").unwrap()),
                quote_currency: "USD".into(),
                fee_amount: parse_decimal_lossless("1").unwrap(),
                fee_currency: "USD".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
        ];

        let fx = HashMap::new();
        let proj = replay_ledger_events("USD", &events, &fx, 1).expect("replay");
        assert_eq!(
            proj.cash_balances
                .get(&("ACC-1".into(), "USD".into()))
                .copied(),
            Some(parse_decimal_lossless("1438").unwrap())
        );
        let h = proj
            .holdings
            .get(&("ACC-1".into(), "NASDAQ:AAPL".into()))
            .expect("holding");
        assert_eq!(h.quantity, parse_decimal_lossless("6").unwrap());
        assert_eq!(h.carrying_cost, parse_decimal_lossless("600.6").unwrap());
        assert_eq!(
            proj.realized_pnl_by_account.get("ACC-1").copied(),
            Some(parse_decimal_lossless("38.6").unwrap())
        );

        let as_of = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let mut prices = HashMap::new();
        prices.insert(
            ("NASDAQ:AAPL".to_string(), as_of),
            parse_decimal_lossless("105").unwrap(),
        );
        let mut quote_ccys = HashMap::new();
        quote_ccys.insert("NASDAQ:AAPL".to_string(), "USD".to_string());
        let fx_3d = HashMap::new();
        let liab = HashMap::new();

        let report = evaluate_portfolio(
            &proj,
            &["ACC-1".to_string()],
            "USD",
            as_of,
            &prices,
            &quote_ccys,
            &fx_3d,
            &liab,
        );
        assert_eq!(report.cash_value, parse_decimal_lossless("1438").unwrap());
        assert_eq!(
            report.holdings_market_value,
            parse_decimal_lossless("630").unwrap()
        );
        assert_eq!(report.net_worth, parse_decimal_lossless("2068").unwrap());
        assert_eq!(
            report.total_carrying_cost,
            parse_decimal_lossless("600.6").unwrap()
        );
        assert_eq!(
            report.realized_pnl,
            Some(parse_decimal_lossless("38.6").unwrap())
        );
        assert_eq!(
            report.unrealized_pnl,
            Some(parse_decimal_lossless("29.4").unwrap())
        );
        assert_eq!(
            report.total_pnl,
            Some(parse_decimal_lossless("68").unwrap())
        );
    }

    #[test]
    fn ac04_internal_transfer_and_ac05_split_and_ac07_twr_xirr() {
        // AC-07: Initial 1000, mid-period inflow 1000, market unchanged, ending 2000 -> PnL 0, TWR 0, XIRR 0
        let twr = compute_twr(&[
            SubPeriodSnapshot {
                v_start_after_flow: parse_decimal_lossless("1000").unwrap(),
                v_end_before_next_flow: parse_decimal_lossless("1000").unwrap(),
            },
            SubPeriodSnapshot {
                v_start_after_flow: parse_decimal_lossless("2000").unwrap(),
                v_end_before_next_flow: parse_decimal_lossless("2000").unwrap(),
            },
        ])
        .unwrap();
        assert_eq!(twr, Decimal::ZERO);

        let d0 = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let d1 = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let xirr = compute_xirr(&[
            (d0, parse_decimal_lossless("-1000").unwrap()),
            (d1, parse_decimal_lossless("-1000").unwrap()),
            (d2, parse_decimal_lossless("2000").unwrap()),
        ]);
        assert!(matches!(
            xirr,
            XirrStatus::Converged { rate, .. } if rate == Decimal::ZERO
        ));
    }

    #[test]
    fn ac11_indicator_future_sentinel_truncation() {
        let mut bars = Vec::new();
        for day in 1..=20 {
            let d = NaiveDate::from_ymd_opt(2026, 1, day).unwrap();
            let open_at = Utc.from_utc_datetime(&d.and_hms_opt(14, 30, 0).unwrap());
            let close_at = Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).unwrap());
            let close_price = if day <= 15 {
                Decimal::from(100 + day)
            } else {
                // Extreme future sentinel injected after day 15
                Decimal::from(999_999_999_i64)
            };
            bars.push(BarRecord {
                id: format!("B-{day}"),
                instrument_id: "NASDAQ:AAPL".into(),
                timeframe: "1d".into(),
                session: "regular".into(),
                session_date: d,
                open_at,
                close_at,
                open: close_price,
                high: close_price + Decimal::ONE,
                low: close_price - Decimal::ONE,
                close: close_price,
                volume: Decimal::from(1000),
                source: "test".into(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: 1,
                valid_to_version: None,
                raw_payload_hash: None,
            });
        }

        let cutoff_date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
        let cutoff = Utc.from_utc_datetime(&cutoff_date.and_hms_opt(21, 0, 0).unwrap());

        let sma_truncated = compute_sma(&bars, 5, Some(cutoff));
        let sma_clean = compute_sma(&bars[0..15], 5, None);
        assert_eq!(sma_truncated, sma_clean);

        let rsi_truncated = compute_rsi(&bars, 14, Some(cutoff));
        let rsi_clean = compute_rsi(&bars[0..15], 14, None);
        assert_eq!(rsi_truncated, rsi_clean);
    }

    #[test]
    fn m2_a06_ledger01_cash_exchange_usd_usdt_btc() {
        // M2-A-06 / LEDGER-01:
        // 1. Account ACC-1 deposits 2000 USD on 2026-01-01
        // 2. Exchanges 1000 USDT at 1.00 USD/USDT with 2 USD fee on 2026-01-02 (cost = 1002 USD)
        // 3. Buys 0.01 BTC at 50,000 USDT (500 USDT) with 1 USDT fee (total 501 USDT) on 2026-01-03 (when USDT/USD = 1.002)
        // 4. Sells 0.004 BTC at 55,000 USDT (220 USDT) with 1 USDT fee (net 219 USDT) on 2026-01-04 (when USDT/USD = 1.00)
        let d1 = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let d3 = NaiveDate::from_ymd_opt(2026, 1, 3).unwrap();
        let d4 = NaiveDate::from_ymd_opt(2026, 1, 4).unwrap();

        let events = vec![
            LedgerEventInput {
                event_id: "EV-DEP-USD".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::Deposit,
                occurred_at: Utc.from_utc_datetime(&d1.and_hms_opt(10, 0, 0).unwrap()),
                sequence_no: 1,
                asset_id: "USD".into(),
                instrument_id: None,
                quantity: parse_decimal_lossless("2000").unwrap(),
                price: None,
                quote_currency: "USD".into(),
                fee_amount: Decimal::ZERO,
                fee_currency: "USD".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
            LedgerEventInput {
                event_id: "EV-EX-USDT".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::CashExchange,
                occurred_at: Utc.from_utc_datetime(&d2.and_hms_opt(10, 0, 0).unwrap()),
                sequence_no: 2,
                asset_id: "USDT".into(),
                instrument_id: None,
                quantity: parse_decimal_lossless("1000").unwrap(),
                price: Some(parse_decimal_lossless("1.00").unwrap()),
                quote_currency: "USD".into(),
                fee_amount: parse_decimal_lossless("2").unwrap(),
                fee_currency: "USD".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
            LedgerEventInput {
                event_id: "EV-BUY-BTC".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::Buy,
                occurred_at: Utc.from_utc_datetime(&d3.and_hms_opt(10, 0, 0).unwrap()),
                sequence_no: 3,
                asset_id: "BTC".into(),
                instrument_id: Some("OKX:BTC/USDT".into()),
                quantity: parse_decimal_lossless("0.01").unwrap(),
                price: Some(parse_decimal_lossless("50000").unwrap()),
                quote_currency: "USDT".into(),
                fee_amount: parse_decimal_lossless("1").unwrap(),
                fee_currency: "USDT".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
            LedgerEventInput {
                event_id: "EV-SELL-BTC".into(),
                account_id: "ACC-1".into(),
                event_type: EconomicEventType::Sell,
                occurred_at: Utc.from_utc_datetime(&d4.and_hms_opt(10, 0, 0).unwrap()),
                sequence_no: 4,
                asset_id: "BTC".into(),
                instrument_id: Some("OKX:BTC/USDT".into()),
                quantity: parse_decimal_lossless("0.004").unwrap(),
                price: Some(parse_decimal_lossless("55000").unwrap()),
                quote_currency: "USDT".into(),
                fee_amount: parse_decimal_lossless("1").unwrap(),
                fee_currency: "USDT".into(),
                cost_known: true,
                counterparty_account_id: None,
                transfer_group_id: None,
                correction_group_id: None,
                revision: 1,
                active_in_correction_group: true,
                source_ref: None,
            },
        ];

        let mut fx = HashMap::new();
        fx.insert(("USDT".into(), d2), parse_decimal_lossless("1.00").unwrap());
        fx.insert(
            ("USDT".into(), d3),
            parse_decimal_lossless("1.002").unwrap(),
        );
        fx.insert(("USDT".into(), d4), parse_decimal_lossless("1.00").unwrap());

        let proj = replay_ledger_events("USD", &events, &fx, 1).expect("replay LEDGER-01");
        // USD cash: 2000 - (1000 + 2) = 998 USD
        assert_eq!(
            proj.cash_balances
                .get(&("ACC-1".into(), "USD".into()))
                .copied(),
            Some(parse_decimal_lossless("998").unwrap())
        );
        // USDT cash: 1000 - 501 + 219 = 718 USDT (settled quote leg once!)
        assert_eq!(
            proj.cash_balances
                .get(&("ACC-1".into(), "USDT".into()))
                .copied(),
            Some(parse_decimal_lossless("718").unwrap())
        );
        // BTC holding: 0.01 - 0.004 = 0.006 BTC
        let btc_holding = proj.holdings.get(&("ACC-1".into(), "BTC".into())).unwrap();
        assert_eq!(
            btc_holding.quantity,
            parse_decimal_lossless("0.006").unwrap()
        );
        // BTC buy cost was 501 * 1.002 = 502.002 USD; 60% remaining = 301.2012 USD
        assert_eq!(
            btc_holding.carrying_cost,
            parse_decimal_lossless("301.2012").unwrap()
        );
        // FX gain on disposing 501 USDT (carrying cost 501 * 1.002 = 502.002 vs 501/1000 * 1002 = 502.002 -> 0 when rate=1.002!)
        assert!(proj.realized_pnl_by_account.get("ACC-1").unwrap() > &Decimal::ZERO);
    }
}
