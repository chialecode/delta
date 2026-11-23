//! Self-contained synthetic market fixture generator for M4 smoke, benchmarks, and headless tests.

use std::fs;
use std::path::Path;

use chrono::{Datelike as _, Days, NaiveDate, TimeZone as _, Utc, Weekday};
use delta_app::ErrorEnvelope;
use serde_json::json;

/// Writes a self-contained `market_data/` fixture directory consumable by
/// `AppService::sync_from_fixtures` / `AppHost::sync_from_fixtures`:
/// - `OKX:BTC/USDT`: `okx_days` continuous 24/7 UTC finalized daily candles (`"1"`),
///   supporting 1,600+ bar replay windows (`warmup=200`, `history=1000`, `training=340`, `reveal=60`).
/// - `NASDAQ:AAPL`: up to 62 continuous US regular trading days (`2024-09-03..2024-11-27`,
///   zero XNAS market holidays in window) cross-validated identically across Yahoo + EastMoney.
pub fn write_synthetic_training_fixtures(
    fixtures_root: &Path,
    okx_days: usize,
) -> Result<(), ErrorEnvelope> {
    let md_dir = fixtures_root.join("market_data");
    fs::create_dir_all(&md_dir)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_DIR_FAILED", e.to_string(), false))?;

    // 1. OKX BTC/USDT continuous 24/7 finalized daily bars starting 2020-01-01
    let start_okx = NaiveDate::from_ymd_opt(2020, 1, 1).expect("valid date");
    let mut okx_rows = Vec::with_capacity(okx_days);
    let mut prev_close = 28_000.0_f64;
    for idx in 0..okx_days {
        let day = start_okx
            .checked_add_days(Days::new(idx as u64))
            .expect("valid day");
        let ts_ms = Utc
            .from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("valid midnight"))
            .timestamp_millis();
        let wave = ((idx as f64) * 0.17).sin() * 240.0 + ((idx as f64) * 0.03).cos() * 410.0;
        let close = (28_000.0 + (idx as f64) * 12.5 + wave).max(1_000.0);
        let open = prev_close;
        let high = open.max(close) + 180.0 + ((idx % 7) as f64) * 15.0;
        let low = (open.min(close) - 160.0 - ((idx % 5) as f64) * 12.0).max(500.0);
        let vol = 1_200.0 + ((idx % 19) as f64) * 85.0;
        let quote_vol = vol * close;
        prev_close = close;
        okx_rows.push(json!([
            ts_ms.to_string(),
            format!("{open:.2}"),
            format!("{high:.2}"),
            format!("{low:.2}"),
            format!("{close:.2}"),
            format!("{vol:.2}"),
            format!("{quote_vol:.2}"),
            format!("{quote_vol:.2}"),
            "1"
        ]));
    }
    // OKX returns newest-first in raw payload (parser sorts chronologically)
    okx_rows.reverse();
    let okx_json = serde_json::to_string(&json!({
        "code": "0",
        "msg": "",
        "data": okx_rows,
    }))
    .map_err(|e| ErrorEnvelope::new("FIXTURE_SERIALIZE_FAILED", e.to_string(), false))?;
    fs::write(md_dir.join("okx_btc_usdt_candles.json"), okx_json)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    // 2. Yahoo + EastMoney NASDAQ:AAPL continuous trading window (2024-09-03..2024-11-27 = 62 XNAS days)
    let mut eq_dates = Vec::new();
    let mut cur = NaiveDate::from_ymd_opt(2024, 9, 3).expect("valid start date");
    let end_eq = NaiveDate::from_ymd_opt(2024, 11, 27).expect("valid end date");
    while cur <= end_eq {
        if !matches!(cur.weekday(), Weekday::Sat | Weekday::Sun) {
            eq_dates.push(cur);
        }
        cur = cur.checked_add_days(Days::new(1)).expect("valid next day");
    }
    let mut timestamps = Vec::with_capacity(eq_dates.len());
    let mut opens = Vec::with_capacity(eq_dates.len());
    let mut highs = Vec::with_capacity(eq_dates.len());
    let mut lows = Vec::with_capacity(eq_dates.len());
    let mut closes = Vec::with_capacity(eq_dates.len());
    let mut volumes = Vec::with_capacity(eq_dates.len());
    let mut em_klines = Vec::with_capacity(eq_dates.len());

    let mut eq_prev = 218.0_f64;
    for (i, d) in eq_dates.iter().enumerate() {
        let ts = Utc
            .from_utc_datetime(&d.and_hms_opt(14, 30, 0).expect("valid open time"))
            .timestamp();
        let o = ((eq_prev * 100.0).round()) / 100.0;
        let c = (((218.0 + (i as f64) * 0.45 + ((i as f64) * 0.35).sin() * 3.2) * 100.0).round())
            / 100.0;
        let h = ((o.max(c) + 2.25) * 100.0).round() / 100.0;
        let l = ((o.min(c) - 2.00) * 100.0).round() / 100.0;
        let v = 45_000_000_i64 + (i as i64) * 150_000;
        eq_prev = c;
        timestamps.push(ts);
        opens.push(o);
        highs.push(h);
        lows.push(l);
        closes.push(c);
        volumes.push(v);
        em_klines.push(format!(
            "{d},{o:.2},{c:.2},{h:.2},{l:.2},{v},9800000000.00,2.10,0.50,1.10,0.85"
        ));
    }
    let yahoo_json = serde_json::to_string(&json!({
        "chart": {
            "result": [{
                "meta": {
                    "currency": "USD",
                    "symbol": "AAPL",
                    "exchangeName": "NMS",
                    "instrumentType": "EQUITY"
                },
                "timestamp": timestamps,
                "events": {},
                "indicators": {
                    "quote": [{
                        "open": opens,
                        "high": highs,
                        "low": lows,
                        "close": closes.clone(),
                        "volume": volumes
                    }],
                    "adjclose": [{ "adjclose": closes }]
                }
            }],
            "error": null
        }
    }))
    .map_err(|e| ErrorEnvelope::new("FIXTURE_SERIALIZE_FAILED", e.to_string(), false))?;
    fs::write(md_dir.join("yahoo_aapl_2020_split.json"), yahoo_json)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    let em_json = serde_json::to_string(&json!({
        "rc": 0,
        "rt": 17,
        "svr": 182994511,
        "lt": 1,
        "full": 1,
        "data": {
            "code": "AAPL",
            "market": 105,
            "name": "苹果",
            "decimal": 2,
            "dktotal": em_klines.len(),
            "preKPrice": 218.0,
            "klines": em_klines
        }
    }))
    .map_err(|e| ErrorEnvelope::new("FIXTURE_SERIALIZE_FAILED", e.to_string(), false))?;
    fs::write(md_dir.join("eastmoney_aapl_2020_raw.json"), em_json)
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    // 3. Minimal FX fixtures required by `sync_from_fixtures`
    fs::write(
        md_dir.join("coinbase_usdt_usd_candles.json"),
        r#"[[1735689600,0.9998,1.0001,0.9999,1.0000,150000000.0]]"#,
    )
    .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;
    fs::write(
        md_dir.join("bitstamp_usdc_usd_ohlc.json"),
        r#"{"data":{"pair":"USDC/USD","ohlc":[{"timestamp":"1735689600","open":"1.0000","high":"1.0001","low":"0.9999","close":"1.0000","volume":"9500000.00"}]}}"#,
    )
    .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;
    fs::write(
        md_dir.join("frankfurter_usd_fx.json"),
        r#"{"amount":1.0,"base":"USD","start_date":"2025-01-02","end_date":"2025-01-02","rates":{"2025-01-02":{"CNY":7.265,"EUR":0.958,"HKD":7.7812}}}"#,
    )
    .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;
    fs::write(md_dir.join("ledger_fx_synthetic.json"), "[]")
        .map_err(|e| ErrorEnvelope::new("FIXTURE_WRITE_FAILED", e.to_string(), false))?;

    Ok(())
}
