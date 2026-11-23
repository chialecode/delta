use delta_core::TermsClass;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod account;
mod crypto;
mod equity;
mod fx;
mod http;

pub use account::{
    reconcile_balances, reconcile_balances_full_snapshot, AccountBalanceSnapshotDto,
    AccountProvider, AccountSyncPageDto, FixtureAccountProvider,
};
pub use crypto::{BinanceSpotProvider, OkxSpotProvider};
pub use equity::{
    cross_validate_bars_and_actions, AlpacaEquityProvider, CrossValidationOutcome,
    EastMoneyUsEquityProvider, TiingoEquityProvider, YahooChartV8Provider,
};
pub use fx::{FrankfurterFxProvider, StablecoinFxProvider};
pub use http::{
    build_http_client, get_bytes_with_retry, get_text_with_retry, provider_bar_id,
    validate_provider_base_url,
};

pub const DEFAULT_USER_AGENT: &str = "delta-data/0.1.0 (pure-rust; local-first)";

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http error from {provider} ({status:?}): {message}")]
    Http {
        provider: String,
        status: Option<u16>,
        message: String,
        retryable: bool,
    },
    #[error("rate limited by {provider} (HTTP 429)")]
    RateLimited { provider: String },
    #[error("forbidden endpoint '{0}': api.binance.com is restricted (HTTP 451); use data-api.binance.vision or data.binance.vision")]
    ForbiddenEndpoint(String),
    #[error("unsupported product '{product}' on {provider}: {reason}")]
    UnsupportedProduct {
        provider: String,
        product: String,
        reason: String,
    },
    #[error("missing API token for provider {0} (see ACT-03)")]
    MissingCredentials(String),
    #[error("archive checksum mismatch for {file}: expected {expected}, actual {actual}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    #[error("parse error in {provider}: {detail}")]
    Parse { provider: String, detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub provider: String,
    pub base_url: String,
    pub requires_key: bool,
    pub rate_limit_per_min: u32,
    pub terms_class: TermsClass,
    pub supports_equities: bool,
    pub supports_crypto: bool,
    pub supports_fx: bool,
    pub supports_corporate_actions: bool,
}

#[cfg(test)]
mod archive_member_tests {
    use super::http::{build_provider_url, read_single_csv_member, sanitize_untrusted_text};
    use super::{
        AlpacaEquityProvider, BinanceSpotProvider, FrankfurterFxProvider, OkxSpotProvider,
        StablecoinFxProvider, TiingoEquityProvider,
    };
    use delta_core::{CorporateActionType, Timeframe};
    use rust_decimal::Decimal;
    use std::io::Write as _;

    fn archive(build: impl FnOnce(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        build(&mut zip);
        zip.finish().unwrap().into_inner()
    }

    fn opts() -> zip::write::SimpleFileOptions {
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
    }

    #[test]
    fn plain_csv_member_is_read() {
        let bytes = archive(|z| {
            z.start_file("BTCUSDT-1d-2024-01.csv", opts()).unwrap();
            z.write_all(b"1,2,3\n").unwrap();
        });
        assert_eq!(read_single_csv_member(&bytes).unwrap(), "1,2,3\n");
    }

    #[test]
    fn traversal_absolute_symlink_and_directory_members_are_refused() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "parent traversal",
                archive(|z| {
                    z.start_file("../evil.csv", opts()).unwrap();
                    z.write_all(b"x").unwrap();
                }),
            ),
            (
                "nested traversal",
                archive(|z| {
                    z.start_file("a/../../evil.csv", opts()).unwrap();
                    z.write_all(b"x").unwrap();
                }),
            ),
            (
                "absolute path",
                archive(|z| {
                    z.start_file("/tmp/evil.csv", opts()).unwrap();
                    z.write_all(b"x").unwrap();
                }),
            ),
            (
                "symlink outside",
                archive(|z| {
                    z.add_symlink("link.csv", "../../outside/secret.csv", opts())
                        .unwrap();
                }),
            ),
            (
                "symlink inside",
                archive(|z| {
                    z.add_symlink("link.csv", "data.csv", opts()).unwrap();
                }),
            ),
            (
                "directory",
                archive(|z| {
                    z.add_directory("dir.csv/", opts()).unwrap();
                }),
            ),
            (
                "not csv",
                archive(|z| {
                    z.start_file("payload.exe", opts()).unwrap();
                    z.write_all(b"x").unwrap();
                }),
            ),
            (
                "two members",
                archive(|z| {
                    z.start_file("a.csv", opts()).unwrap();
                    z.write_all(b"x").unwrap();
                    z.start_file("b.csv", opts()).unwrap();
                    z.write_all(b"y").unwrap();
                }),
            ),
        ];
        for (label, bytes) in cases {
            assert!(
                read_single_csv_member(&bytes).is_err(),
                "{label} must be refused"
            );
        }
    }

    /// Real Binance `BTCUSDT-1h-2018-02` rows around the 2018-02-08 outage: a short aligned bar,
    /// then klines offset by 28m14.789s. Off-grid rows are skipped (no invented data); aligned
    /// rows are kept with their exact values.
    #[test]
    fn off_grid_archive_klines_are_skipped_not_fatal() {
        let csv = "\
1518048000000,7599.00000000,7844.00000000,7572.09000000,7784.02000000,1521.53731800,1518049694788,0,0,0,0,0
1518168494789,7789.90000000,8390.00000000,7789.90000000,8180.50000000,1451.89645000,1518172094788,0,0,0,0,0
1518325200000,8301.01000000,8395.00000000,8201.95000000,8219.98000000,1275.86692600,1518328799999,0,0,0,0,0
";
        let bars = BinanceSpotProvider::parse_archive_csv_tf(
            "BINANCE:BTC/USDT",
            csv,
            None,
            1,
            Timeframe::H1,
        )
        .unwrap();
        let opens: Vec<String> = bars.iter().map(|b| b.open_at.to_rfc3339()).collect();
        assert_eq!(
            opens,
            ["2018-02-08T00:00:00+00:00", "2018-02-11T05:00:00+00:00"]
        );
        assert_eq!(bars[0].close.to_string(), "7784.02");
    }

    #[test]
    fn test_data_b06_alpaca_reverse_splits_and_zero_rate_validation() {
        let payload = r#"{
            "corporate_actions": {
                "forward_splits": [{"ex_date": "2020-08-31", "new_rate": 4, "old_rate": 1}],
                "reverse_splits": [{"ex_date": "2023-05-01", "new_rate": 1, "old_rate": 10}],
                "cash_dividends": [{"ex_date": "2023-06-01", "payable_date": "2023-06-15", "rate": 0.24}]
            }
        }"#;
        let actions =
            AlpacaEquityProvider::parse_corporate_actions_v1("INST:EQUITY:XNAS:TEST", payload, 1)
                .unwrap();
        assert_eq!(actions.len(), 3);
        let rev = actions
            .iter()
            .find(|a| {
                a.action_type == CorporateActionType::Split && a.ex_date.to_string() == "2023-05-01"
            })
            .expect("reverse split must be parsed");
        assert_eq!(rev.ratio_or_amount, Decimal::new(1, 1));

        // Zero old_rate must return Parse error instead of panicking on division by zero.
        let bad_split = r#"{"corporate_actions":{"forward_splits":[{"ex_date":"2020-08-31","new_rate":4,"old_rate":0}]}}"#;
        assert!(AlpacaEquityProvider::parse_corporate_actions_v1(
            "INST:EQUITY:XNAS:TEST",
            bad_split,
            1
        )
        .is_err());

        // Non-positive dividend must be rejected.
        let bad_div =
            r#"{"corporate_actions":{"cash_dividends":[{"ex_date":"2023-06-01","rate":-0.1}]}}"#;
        assert!(AlpacaEquityProvider::parse_corporate_actions_v1(
            "INST:EQUITY:XNAS:TEST",
            bad_div,
            1
        )
        .is_err());
    }

    #[test]
    fn test_data_b07_zip_decompression_cap_rejects_oversized_deflate() {
        // Compress 17 MiB of zeros with Deflate into a tiny ZIP (~17 KiB) to verify actual cap.
        let deflate_opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let bytes = archive(|z| {
            z.start_file("bomb.csv", deflate_opts).unwrap();
            let chunk = vec![b'0'; 1024 * 1024];
            for _ in 0..17 {
                z.write_all(&chunk).unwrap();
            }
        });
        assert!(
            bytes.len() < 1024 * 1024,
            "compressed bomb should be well under 1 MiB"
        );
        assert!(read_single_csv_member(&bytes).is_err());
    }

    #[test]
    fn test_data_b12_fx_pair_quote_currency_and_positive_rate_checks() {
        let bs_json = r#"{"data":{"ohlc":[{"timestamp":"1704067200","close":"0.91"}]}}"#;
        let eur =
            StablecoinFxProvider::parse_bitstamp_ohlc_pair("USDC", "EUR", bs_json, 1).unwrap();
        assert_eq!(eur.len(), 1);
        assert_eq!(eur[0].base_currency, "USDC");
        assert_eq!(eur[0].quote_currency, "EUR");
        assert_eq!(eur[0].id, "FX:USDC:EUR:2024-01-01:bitstamp:1");

        let cb_json = r#"[[1704067200,0.9,0.92,0.91,0.915,100]]"#;
        let cb = StablecoinFxProvider::parse_coinbase_candles("USDT-EUR", cb_json, 1).unwrap();
        assert_eq!(cb[0].base_currency, "USDT");
        assert_eq!(cb[0].quote_currency, "EUR");

        // Zero / negative FX rates must be rejected across FX providers.
        let zero_bs = r#"{"data":{"ohlc":[{"timestamp":"1704067200","close":"0"}]}}"#;
        assert!(StablecoinFxProvider::parse_bitstamp_ohlc_pair("USDC", "USD", zero_bs, 1).is_err());
        let neg_cb = r#"[[1704067200,0.9,0.92,0.91,-1.0,100]]"#;
        assert!(StablecoinFxProvider::parse_coinbase_candles("USDT-USD", neg_cb, 1).is_err());
        let zero_ff = r#"{"base":"USD","date":"2024-01-02","rates":{"CNY":0}}"#;
        assert!(FrankfurterFxProvider::parse_fx_json(zero_ff, 1).is_err());
    }

    #[test]
    fn test_data_b20_url_encoding_case_insensitive_binance_and_sanitization() {
        // Case-insensitive forbidden api.binance.com check covers both REST and archive URLs.
        let bad_rest = BinanceSpotProvider {
            rest_base_url: "https://API.BINANCE.COM".into(),
            ..Default::default()
        };
        assert!(bad_rest.validate_base_url().is_err());
        let bad_arch = BinanceSpotProvider {
            archive_base_url: "https://Api.Binance.Com".into(),
            ..Default::default()
        };
        assert!(bad_arch.validate_base_url().is_err());

        // URL builder encodes special characters in path & query safely.
        let built = build_provider_url(
            "yahoo",
            "https://query1.finance.yahoo.com",
            &["v8", "finance", "chart", "BRK.B?injected=1"],
            &[("events", "div,split")],
            false,
        )
        .unwrap();
        assert!(built.contains("BRK.B%3Finjected=1"));

        // Sanitization strips terminal escape/control sequences.
        let sanitized = sanitize_untrusted_text("\x1b[31mFAKE\x07\nLINE", 64);
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\x07'));
        assert_eq!(sanitized, "[31mFAKE LINE");

        // OKX error messages are sanitized.
        let err = OkxSpotProvider::parse_candles_json(
            "OKX:BTC/USDT",
            r#"{"code":"51000","msg":"\u001b[2Jbad","data":[]}"#,
            None,
            1,
        )
        .unwrap_err();
        assert!(!err.to_string().contains('\x1b'));

        // Binance archive handles UTF-8 BOM and preserves microsecond precision in parse_binance_timestamp.
        let bom_csv = "\u{feff}open_time,open,high,low,close,volume,close_time\n1740787200000000,100,110,90,105,20,1740873599999999\n";
        let bars =
            BinanceSpotProvider::parse_archive_csv("BINANCE:BTC/USDT", bom_csv, None, 1).unwrap();
        assert_eq!(bars.len(), 1);
        let us_dt = BinanceSpotProvider::parse_binance_timestamp(1740873599999999).unwrap();
        assert_eq!(us_dt.timestamp_subsec_micros(), 999_999);

        // Tiingo parser returns bars sorted chronologically.
        let tiingo_json = r#"[
            {"date":"2025-01-03T00:00:00Z","open":102,"high":105,"low":101,"close":104,"volume":1000,"divCash":0,"splitFactor":1},
            {"date":"2025-01-02T00:00:00Z","open":100,"high":103,"low":99,"close":102,"volume":900,"divCash":0,"splitFactor":1}
        ]"#;
        let (t_bars, _) = TiingoEquityProvider::parse_daily_json(
            "INST:EQUITY:XNAS:AAPL",
            "XNAS",
            tiingo_json,
            None,
            1,
        )
        .unwrap();
        assert_eq!(t_bars.len(), 2);
        assert!(t_bars[0].open_at < t_bars[1].open_at);
    }
}
