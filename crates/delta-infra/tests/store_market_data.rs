//! Market-data store regressions found in the R1 A review: timestamps with
//! offsets must compare chronologically (no future leak), valuation reads raw
//! closes only, and FX lookups report the rate's own effective time.

use chrono::{TimeZone, Utc};
use delta_core::ids::AssetId;
use delta_core::ledger::FxResolver;
use delta_core::money::CurrencyCode;
use delta_core::valuation::PriceSource;
use delta_infra::sqlite::store::Library;
use rust_decimal_macros::dec;

fn library() -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::create(&dir.path().join("lib.sqlite"), "USD").unwrap();
    lib.ensure_asset("AAPL", "equity").unwrap();
    lib.ensure_asset("USD", "fiat").unwrap();
    lib.ensure_instrument("XNAS:AAPL", "AAPL", "USD", "XNAS")
        .unwrap();
    (dir, lib)
}

#[allow(clippy::too_many_arguments)]
fn bar(
    lib: &Library,
    date: &str,
    open_at: &str,
    close_at: &str,
    close: &str,
    adj: &str,
    ver: &str,
) {
    lib.upsert_bar(
        "XNAS:AAPL",
        date,
        open_at,
        close_at,
        close,
        close,
        close,
        close,
        "100",
        "test",
        adj,
        ver,
    )
    .unwrap();
}

#[test]
fn r1_a_10_offset_close_time_is_not_visible_before_it_happens() {
    let (_d, lib) = library();
    bar(
        &lib,
        "2026-01-29",
        "2026-01-29T09:30:00-05:00",
        "2026-01-29T16:00:00-05:00",
        "100",
        "raw",
        "v1",
    );
    // Closes at 21:00Z; as text "16:00-05:00" sorts before "20:00Z".
    bar(
        &lib,
        "2026-01-30",
        "2026-01-30T09:30:00-05:00",
        "2026-01-30T16:00:00-05:00",
        "999",
        "raw",
        "v1",
    );
    let prices = lib.price_source();
    let aapl = AssetId("AAPL".into());

    let before_close = Utc.with_ymd_and_hms(2026, 1, 30, 20, 0, 0).unwrap();
    let q = prices.price(&aapl, before_close).unwrap();
    assert_eq!(q.price, dec!(100), "a bar is not visible before its close");

    let at_close = Utc.with_ymd_and_hms(2026, 1, 30, 21, 0, 0).unwrap();
    let q = prices.price(&aapl, at_close).unwrap();
    assert_eq!(q.price, dec!(999), "a bar is visible exactly at its close");
    assert_eq!(q.as_of, at_close);
}

#[test]
fn r1_a_10_valuation_ignores_adjusted_series() {
    let (_d, lib) = library();
    bar(
        &lib,
        "2026-01-30",
        "2026-01-30T14:30:00Z",
        "2026-01-30T21:00:00Z",
        "100",
        "raw",
        "v1",
    );
    // The adjusted series extends one session past the raw series.
    bar(
        &lib,
        "2026-01-31",
        "2026-01-31T14:30:00Z",
        "2026-01-31T21:00:00Z",
        "50",
        "adjusted",
        "v9",
    );
    let at = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
    let q = lib
        .price_source()
        .price(&AssetId("AAPL".into()), at)
        .unwrap();
    assert_eq!(q.price, dec!(100));
}

#[test]
fn r1_a_10_fx_reports_effective_time_and_respects_offsets() {
    let (_d, lib) = library();
    lib.upsert_fx(
        "EUR",
        "USD",
        "1.10",
        "2026-01-30T00:00:00+01:00",
        "2026-01-30T00:00:00Z",
        "test",
    )
    .unwrap();
    lib.upsert_fx(
        "EUR",
        "USD",
        "1.20",
        "2026-01-31T08:00:00+08:00",
        "2026-01-31T00:00:00Z",
        "test",
    )
    .unwrap();
    let fx = lib.fx_resolver();
    let eur = CurrencyCode("EUR".into());
    let usd = CurrencyCode::usd();

    // 2026-01-31T00:00Z is exactly the second rate's effective time.
    let at = Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).unwrap();
    let (rate, effective) = fx.rate(&eur, &usd, at).unwrap();
    assert_eq!(rate, dec!(1.20));
    assert_eq!(effective, at);

    let earlier = Utc.with_ymd_and_hms(2026, 1, 30, 12, 0, 0).unwrap();
    let (rate, effective) = fx.rate(&eur, &usd, earlier).unwrap();
    assert_eq!(rate, dec!(1.10));
    assert_eq!(
        effective,
        Utc.with_ymd_and_hms(2026, 1, 29, 23, 0, 0).unwrap()
    );

    // Inverse direction keeps the same effective time.
    let (_, effective) = fx.rate(&usd, &eur, earlier).unwrap();
    assert_eq!(
        effective,
        Utc.with_ymd_and_hms(2026, 1, 29, 23, 0, 0).unwrap()
    );
}

#[test]
fn r1_a_10_malformed_timestamp_is_rejected() {
    let (_d, lib) = library();
    let err = lib.upsert_bar(
        "XNAS:AAPL",
        "2026-01-30",
        "yesterday",
        "2026-01-30T21:00:00Z",
        "1",
        "1",
        "1",
        "1",
        "1",
        "test",
        "raw",
        "v1",
    );
    assert!(err.is_err());
}
