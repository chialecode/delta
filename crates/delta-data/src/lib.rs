pub mod calendar;
pub mod importers;
pub mod m2_schema;
pub mod m4_schema;
pub mod providers;
pub mod registry;
pub mod storage;

pub use calendar::*;
pub use importers::*;
pub use m2_schema::*;
pub use m4_schema::*;
pub use providers::*;
pub use registry::*;
pub use storage::*;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CannedHttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl CannedHttpResponse {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8".to_string(),
            body: body.into().into_bytes(),
        }
    }

    pub fn bytes(status: u16, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
        }
    }
}

/// Loopback-only HTTP server strictly bound to `127.0.0.1:0` per S-10 (`D-25`).
/// Never binds `0.0.0.0` or `[::]` so Windows Firewall prompts can never be triggered.
pub struct LoopbackTestServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    routes: Arc<Mutex<BTreeMap<String, Vec<CannedHttpResponse>>>>,
    request_counts: Arc<Mutex<BTreeMap<String, usize>>>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackTestServer {
    pub fn start() -> Result<Self, String> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let listener = TcpListener::bind(addr).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let local_addr = listener.local_addr().map_err(|e| e.to_string())?;
        assert!(
            local_addr.ip().is_loopback(),
            "LoopbackTestServer must only bind to 127.0.0.1"
        );
        let base_url = format!("http://127.0.0.1:{}", local_addr.port());
        let stop = Arc::new(AtomicBool::new(false));
        let routes: Arc<Mutex<BTreeMap<String, Vec<CannedHttpResponse>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let request_counts: Arc<Mutex<BTreeMap<String, usize>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        let stop_clone = Arc::clone(&stop);
        let routes_clone = Arc::clone(&routes);
        let counts_clone = Arc::clone(&request_counts);

        let handle = thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        let mut buf = [0u8; 8192];
                        if let Ok(n) = stream.read(&mut buf) {
                            let req_str = String::from_utf8_lossy(&buf[..n]);
                            let first_line = req_str.lines().next().unwrap_or("");
                            let target = first_line.split_whitespace().nth(1).unwrap_or("/");
                            let path_only = target.split('?').next().unwrap_or("/");

                            let resp = {
                                let mut counts = counts_clone.lock().unwrap();
                                let mut r_map = routes_clone.lock().unwrap();

                                let matched_key = if r_map.contains_key(target) {
                                    Some(target.to_string())
                                } else if r_map.contains_key(path_only) {
                                    Some(path_only.to_string())
                                } else {
                                    r_map
                                        .keys()
                                        .find(|k| target.starts_with(k.as_str()))
                                        .cloned()
                                };

                                if let Some(key) = matched_key {
                                    let idx = *counts.get(&key).unwrap_or(&0);
                                    counts.insert(key.clone(), idx + 1);
                                    let seq = r_map.get_mut(&key).unwrap();
                                    let pick = if idx < seq.len() {
                                        seq[idx].clone()
                                    } else {
                                        seq.last().unwrap().clone()
                                    };
                                    pick
                                } else {
                                    CannedHttpResponse::json(
                                        404,
                                        format!(r#"{{"error":"no route for {target}"}}"#),
                                    )
                                }
                            };

                            let status_text = match resp.status {
                                200 => "OK",
                                401 => "Unauthorized",
                                403 => "Forbidden",
                                404 => "Not Found",
                                429 => "Too Many Requests",
                                500 => "Internal Server Error",
                                502 => "Bad Gateway",
                                503 => "Service Unavailable",
                                _ => "Status",
                            };
                            let header = format!(
                                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                resp.status,
                                status_text,
                                resp.content_type,
                                resp.body.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&resp.body);
                            let _ = stream.flush();
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            base_url,
            stop,
            routes,
            request_counts,
            handle: Some(handle),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn register_route(&self, path_or_prefix: impl Into<String>, response: CannedHttpResponse) {
        self.routes
            .lock()
            .unwrap()
            .insert(path_or_prefix.into(), vec![response]);
    }

    pub fn register_sequence(
        &self,
        path_or_prefix: impl Into<String>,
        responses: Vec<CannedHttpResponse>,
    ) {
        self.routes
            .lock()
            .unwrap()
            .insert(path_or_prefix.into(), responses);
    }

    pub fn request_count(&self, path_or_prefix: &str) -> usize {
        *self
            .request_counts
            .lock()
            .unwrap()
            .get(path_or_prefix)
            .unwrap_or(&0)
    }
}

impl Drop for LoopbackTestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use delta_core::{
        parse_decimal_lossless, AdjustmentKind, BarRecord, CalendarSessionKind, CorporateActionType,
    };
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn sha256_bytes_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    #[test]
    fn test_poc_d02_xnys_calendar_1990_to_2027_day_by_day() {
        let root = repo_root();
        let csv_1990_2023 =
            std::fs::read_to_string(root.join("tests/fixtures/calendar/xnys_1990_2023.csv"))
                .expect("read xnys_1990_2023.csv");
        let csv_2023_2027 =
            std::fs::read_to_string(root.join("tests/fixtures/calendar/xnys_2023_2027.csv"))
                .expect("read xnys_2023_2027.csv");

        let mut expected_sessions: BTreeMap<NaiveDate, (String, String)> = BTreeMap::new();
        for content in [&csv_1990_2023, &csv_2023_2027] {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with(',') || line.starts_with("date") {
                    continue;
                }
                let cols: Vec<&str> = line.split(',').collect();
                if cols.len() >= 3 {
                    let date_part = cols[0].get(0..10).unwrap_or(cols[0]);
                    if let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
                        expected_sessions.insert(d, (cols[1].to_string(), cols[2].to_string()));
                    }
                }
            }
        }

        let start = NaiveDate::from_ymd_opt(1990, 1, 2).unwrap();
        let end = NaiveDate::from_ymd_opt(2027, 12, 31).unwrap();
        let sessions = generate_calendar_range("XNYS", start, end);
        let nasdaq_sessions = generate_calendar_range("XNAS", start, end);
        assert_eq!(sessions.len(), nasdaq_sessions.len());
        for (nyse, nasdaq) in sessions.iter().zip(&nasdaq_sessions) {
            assert_eq!(
                (nyse.session_date, nyse.kind, nyse.open_at, nyse.close_at),
                (
                    nasdaq.session_date,
                    nasdaq.kind,
                    nasdaq.open_at,
                    nasdaq.close_at
                )
            );
        }

        let mut mismatches = Vec::new();
        for s in &sessions {
            let is_trading = matches!(
                s.kind,
                CalendarSessionKind::Regular | CalendarSessionKind::EarlyClose
            );
            match expected_sessions.get(&s.session_date) {
                Some((exp_open, exp_close)) => {
                    if !is_trading {
                        mismatches.push(format!(
                            "{}: expected trading day ({exp_open}..{exp_close}), got {:?}",
                            s.session_date, s.kind
                        ));
                    } else {
                        let got_open = s.open_at.unwrap().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                        let got_close =
                            s.close_at.unwrap().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                        if &got_open != exp_open || &got_close != exp_close {
                            mismatches.push(format!(
                                "{}: expected ({exp_open}, {exp_close}), got ({got_open}, {got_close})",
                                s.session_date
                            ));
                        }
                    }
                }
                None => {
                    if is_trading {
                        mismatches.push(format!(
                            "{}: expected closed/holiday, got trading day ({:?}..{:?})",
                            s.session_date, s.open_at, s.close_at
                        ));
                    }
                }
            }
        }

        assert!(
            mismatches.is_empty(),
            "XNYS calendar mismatches ({}): {:?}",
            mismatches.len(),
            &mismatches[..mismatches.len().min(10)]
        );

        // Verify out-of-range dates return CalendarUnknown (S-06)
        let before = query_calendar_session("XNYS", NaiveDate::from_ymd_opt(1989, 12, 29).unwrap());
        assert_eq!(before.kind, CalendarSessionKind::CalendarUnknown);
        let after = query_calendar_session("XNYS", NaiveDate::from_ymd_opt(2028, 1, 3).unwrap());
        assert_eq!(after.kind, CalendarSessionKind::CalendarUnknown);
    }

    #[test]
    fn test_s08_yahoo_restore_raw_and_cross_validation_over_loopback() {
        let root = repo_root();
        let server = LoopbackTestServer::start().unwrap();
        let yahoo_json = std::fs::read_to_string(
            root.join("tests/fixtures/market_data/yahoo_aapl_2020_split.json"),
        )
        .unwrap();
        let em_json = std::fs::read_to_string(
            root.join("tests/fixtures/market_data/eastmoney_aapl_2020_raw.json"),
        )
        .unwrap();
        let alpaca_json = std::fs::read_to_string(
            root.join("tests/fixtures/market_data/alpaca_aapl_corporate_actions.json"),
        )
        .unwrap();

        server.register_sequence(
            "/v8/finance/chart/AAPL",
            vec![
                CannedHttpResponse::json(429, r#"{"error":"rate limit"}"#),
                CannedHttpResponse::json(200, yahoo_json),
            ],
        );
        server.register_route(
            "/api/qt/stock/kline/get",
            CannedHttpResponse::json(200, em_json),
        );
        server.register_route(
            "/v1/corporate-actions",
            CannedHttpResponse::json(200, alpaca_json),
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = build_http_client(5000).unwrap();

        let yahoo = YahooChartV8Provider {
            base_url: server.base_url().to_string(),
        };
        let em = EastMoneyUsEquityProvider {
            base_url: server.base_url().to_string(),
        };
        let alpaca = AlpacaEquityProvider {
            base_url: server.base_url().to_string(),
        };

        let start = NaiveDate::from_ymd_opt(2020, 8, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2020, 9, 1).unwrap();

        let (y_bars, y_actions, _) = rt
            .block_on(yahoo.fetch_chart_v8(
                &client,
                "INST:EQUITY:XNAS:AAPL",
                "XNAS",
                "AAPL",
                1596240000,
                1598918400,
                1,
            ))
            .unwrap();
        assert_eq!(server.request_count("/v8/finance/chart/AAPL"), 2);

        // Check S-08 restoration: 2020-08-28 close must be 499.23, volume 469074800
        let bar_0828 = y_bars
            .iter()
            .find(|b| b.session_date == NaiveDate::from_ymd_opt(2020, 8, 28).unwrap())
            .expect("2020-08-28 bar");
        assert_eq!(bar_0828.adjustment, AdjustmentKind::Raw);
        assert_eq!(bar_0828.close, parse_decimal_lossless("499.23").unwrap());
        assert_eq!(
            bar_0828.volume,
            parse_decimal_lossless("469074800").unwrap()
        );

        // Check 2020-08-07 dividend restored from 0.205 * 4 = 0.82
        let div_0807 = y_actions
            .iter()
            .find(|a| a.action_type == CorporateActionType::CashDividend)
            .expect("2020-08-07 dividend");
        assert_eq!(
            div_0807.ratio_or_amount,
            parse_decimal_lossless("0.82").unwrap()
        );

        let (em_bars, _) = rt
            .block_on(em.fetch_kline_raw(
                &client,
                "INST:EQUITY:XNAS:AAPL",
                "XNAS",
                "105.AAPL",
                "20200801",
                "20200901",
                1,
            ))
            .unwrap();
        let (alpaca_actions, _) = rt
            .block_on(alpaca.fetch_corporate_actions_v1(
                &client,
                "test-key",
                "test-secret",
                "INST:EQUITY:XNAS:AAPL",
                "AAPL",
                start,
                end,
                1,
            ))
            .unwrap();
        assert_eq!(alpaca_actions.len(), 2);

        let outcome = cross_validate_bars_and_actions(
            "INST:EQUITY:XNAS:AAPL",
            &em_bars,
            &y_bars,
            &y_actions,
            1,
        );
        assert!(
            outcome.issues.is_empty(),
            "Expected 0 cross-validation issues, got: {:?}",
            outcome.issues
        );

        // Now simulate missing split in corporate_actions when primary has 499.23 -> 129.04 drop
        let missing_split_outcome =
            cross_validate_bars_and_actions("INST:EQUITY:XNAS:AAPL", &em_bars, &y_bars, &[], 1);
        assert!(
            missing_split_outcome
                .issues
                .iter()
                .any(|i| i.kind == "missing_corporate_action"),
            "Expected missing_corporate_action issue"
        );
    }

    #[test]
    fn test_binance_archive_ms_and_us_timestamps_and_checksum() {
        let csv_content = concat!(
            "1735603200000,92650.0,94200.0,91800.0,93429.2,8120.5,1735689599999,756000000.0,350000,4000.0,372000000.0,0\n",
            "1740787200000000,84350.0,86500.0,83800.0,86020.5,9450.125,1740873599999999,805200000.0,410000,4800.0,410000000.0,0\n"
        );

        let mut zip_buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_buf);
            let mut zip = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("BTCUSDT-1d-2025-03.csv", options).unwrap();
            zip.write_all(csv_content.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let zip_sha = sha256_bytes_hex(&zip_buf);
        let checksum_body = format!("{zip_sha}  BTCUSDT-1d-2025-03.zip\n");

        let bars = BinanceSpotProvider::verify_and_parse_archive(
            "INST:CRYPTO:SPOT:BTC-USDT",
            &zip_buf,
            &checksum_body,
            true,
            1,
        )
        .unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(
            bars[0].session_date,
            NaiveDate::from_ymd_opt(2024, 12, 31).unwrap()
        );
        assert_eq!(
            bars[1].session_date,
            NaiveDate::from_ymd_opt(2025, 3, 1).unwrap()
        );
    }

    #[test]
    fn test_s09_storage_version_intervals_and_backup_restore() {
        let tmp_dir = std::env::temp_dir().join(format!("delta_data_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let mut store = SqliteStore::open_or_init(&tmp_dir, "USD", "USD", "UTC").unwrap();

        let d1 = NaiveDate::from_ymd_opt(2020, 8, 27).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2020, 8, 28).unwrap();
        let open_d1 = chrono::DateTime::parse_from_rfc3339("2020-08-27T13:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let close_d1 = chrono::DateTime::parse_from_rfc3339("2020-08-27T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let open_d2 = chrono::DateTime::parse_from_rfc3339("2020-08-28T13:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let close_d2 = chrono::DateTime::parse_from_rfc3339("2020-08-28T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let bars_v1 = vec![
            BarRecord {
                id: "BAR:1".to_string(),
                instrument_id: "INST:EQUITY:XNAS:AAPL".to_string(),
                timeframe: "1d".to_string(),
                session: "regular".to_string(),
                session_date: d1,
                open_at: open_d1,
                close_at: close_d1,
                open: parse_decimal_lossless("508.57").unwrap(),
                high: parse_decimal_lossless("509.94").unwrap(),
                low: parse_decimal_lossless("495.33").unwrap(),
                close: parse_decimal_lossless("500.04").unwrap(),
                volume: parse_decimal_lossless("38888100").unwrap(),
                source: "eastmoney".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: 1,
                valid_to_version: None,
                raw_payload_hash: None,
            },
            BarRecord {
                id: "BAR:2".to_string(),
                instrument_id: "INST:EQUITY:XNAS:AAPL".to_string(),
                timeframe: "1d".to_string(),
                session: "regular".to_string(),
                session_date: d2,
                open_at: open_d2,
                close_at: close_d2,
                open: parse_decimal_lossless("504.05").unwrap(),
                high: parse_decimal_lossless("505.77").unwrap(),
                low: parse_decimal_lossless("498.31").unwrap(),
                close: parse_decimal_lossless("499.00").unwrap(),
                volume: parse_decimal_lossless("469074800").unwrap(),
                source: "eastmoney".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: 1,
                valid_to_version: None,
                raw_payload_hash: None,
            },
        ];

        let dsv1 = store
            .create_dataset_version("eastmoney", "unofficial_personal", "{}", "exact", "exact")
            .unwrap();
        let (ins1, rev1) = store
            .commit_bars_and_checkpoint(&bars_v1, dsv1.version_num, None)
            .unwrap();
        assert_eq!(ins1, 2);
        assert!(rev1.is_empty());

        // Second sync revises d2 close from 499.00 -> 499.23; d1 is unchanged
        let mut bars_v2 = bars_v1.clone();
        bars_v2[1].close = parse_decimal_lossless("499.23").unwrap();
        let dsv2 = store
            .create_dataset_version("eastmoney", "unofficial_personal", "{}", "warning", "exact")
            .unwrap();
        let (ins2, rev2) = store
            .commit_bars_and_checkpoint(&bars_v2, dsv2.version_num, None)
            .unwrap();
        assert_eq!(ins2, 1);
        assert_eq!(rev2.len(), 1);
        assert_eq!(rev2[0].kind, "data_revision");

        let total_rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM bar", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_rows, 3); // 2 original + 1 revised row, not 4!

        let at_v1 = store
            .query_bars_at_version(
                "INST:EQUITY:XNAS:AAPL",
                "1d",
                AdjustmentKind::Raw,
                Some("eastmoney"),
                dsv1.version_num,
                None,
            )
            .unwrap();
        assert_eq!(at_v1[1].close, parse_decimal_lossless("499.00").unwrap());

        let at_v2 = store
            .query_bars_at_version(
                "INST:EQUITY:XNAS:AAPL",
                "1d",
                AdjustmentKind::Raw,
                Some("eastmoney"),
                dsv2.version_num,
                None,
            )
            .unwrap();
        assert_eq!(at_v2[1].close, parse_decimal_lossless("499.23").unwrap());

        // Backup & restore verification
        let backup_dir = tmp_dir.join("backup_snap");
        let manifest = store.create_consistent_backup(&backup_dir).unwrap();
        assert!(manifest.file_hashes.contains_key("delta.sqlite"));

        let restore_dir = tmp_dir.join("restored_data");
        let restored = SqliteStore::restore_consistent_backup(&backup_dir, &restore_dir).unwrap();
        let restored_v2 = restored
            .query_bars_at_version(
                "INST:EQUITY:XNAS:AAPL",
                "1d",
                AdjustmentKind::Raw,
                Some("eastmoney"),
                dsv2.version_num,
                None,
            )
            .unwrap();
        assert_eq!(
            restored_v2[1].close,
            parse_decimal_lossless("499.23").unwrap()
        );

        drop(restored);
        drop(store);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_statement_importers_and_header_mismatch() {
        let root = repo_root();
        let tmp_dir =
            std::env::temp_dir().join(format!("delta_import_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let mut store = SqliteStore::open_or_init(&tmp_dir, "USD", "USD", "UTC").unwrap();
        let rates: Vec<delta_core::FxRateRecord> = serde_json::from_str(
            &std::fs::read_to_string(
                root.join("tests/fixtures/market_data/ledger_fx_synthetic.json"),
            )
            .unwrap(),
        )
        .unwrap();
        store
            .create_dataset_version(
                "synthetic_fixture",
                "public_open",
                "{}",
                "exact",
                "reconstructed_history",
            )
            .unwrap();
        store.upsert_fx_rates(&rates, None).unwrap();
        store.publish_dataset_version(1).unwrap();
        let futu_path = root.join("tests/fixtures/statements/futu_cn_sample.csv");

        // Wrong preset on Futu CSV must fail header verification
        let bad = preview_statement_import(&store, "ACC-FUTU", StatementPreset::OkxCsv, &futu_path);
        assert!(bad.is_err());

        let opening = preview_statement_import(
            &store,
            "ACC-FUTU",
            StatementPreset::GenericCsv,
            &root.join("tests/fixtures/statements/sample_opening_cash.csv"),
        )
        .unwrap();
        commit_import_preview(&mut store, &opening).unwrap();
        // Correct preset succeeds
        let preview =
            preview_statement_import(&store, "ACC-FUTU", StatementPreset::FutuCsv, &futu_path)
                .unwrap();
        assert_eq!(preview.error_count, 0);
        assert_eq!(preview.valid_count, 3);

        let commit = commit_import_preview(&mut store, &preview).unwrap();
        assert!(!commit.idempotent_skipped);
        assert_eq!(commit.committed_events, 3);

        // Repeat preview + commit is idempotent
        let preview_again =
            preview_statement_import(&store, "ACC-FUTU", StatementPreset::FutuCsv, &futu_path)
                .unwrap();
        assert!(preview_again.already_imported_batch_id.is_some());
        let commit_again = commit_import_preview(&mut store, &preview_again).unwrap();
        assert!(commit_again.idempotent_skipped);

        // Test OKX, Generic, IBKR Flex XML, Binance, and extension presets
        for (acc, preset, rel_path, expected_valid) in [
            (
                "ACC-FUTU-EN",
                StatementPreset::FutuCsv,
                "tests/fixtures/statements/futu_en_sample.csv",
                3usize,
            ),
            (
                "ACC-OKX",
                StatementPreset::OkxCsv,
                "tests/fixtures/statements/okx_billing_sample.csv",
                4usize,
            ),
            (
                "ACC-GEN",
                StatementPreset::GenericCsv,
                "tests/fixtures/statements/generic_full_lifecycle.csv",
                7usize,
            ),
            (
                "ACC-IBKR",
                StatementPreset::IbkrFlexXml,
                "tests/fixtures/statements/ibkr_flex_sample.xml",
                5usize,
            ),
            (
                "ACC-BN",
                StatementPreset::BinanceCsv,
                "tests/fixtures/statements/binance_trade_history.csv",
                4usize,
            ),
            (
                "ACC-TIGER",
                StatementPreset::TigerCsv,
                "tests/fixtures/statements/tiger_sample.csv",
                2usize,
            ),
            (
                "ACC-LB",
                StatementPreset::LongbridgeCsv,
                "tests/fixtures/statements/longbridge_sample.csv",
                2usize,
            ),
            (
                "ACC-SCHWAB",
                StatementPreset::SchwabCsv,
                "tests/fixtures/statements/schwab_sample.csv",
                3usize,
            ),
            (
                "ACC-EM",
                StatementPreset::EastMoneyCsv,
                "tests/fixtures/statements/eastmoney_sample.csv",
                2usize,
            ),
            (
                "ACC-IBCSV",
                StatementPreset::IbkrActivityCsv,
                "tests/fixtures/statements/ibkr_activity_sample.csv",
                2usize,
            ),
        ] {
            let opening = preview_statement_import(
                &store,
                acc,
                StatementPreset::GenericCsv,
                &root.join("tests/fixtures/statements/sample_opening_cash.csv"),
            )
            .unwrap();
            commit_import_preview(&mut store, &opening).unwrap();
            let p = preview_statement_import(&store, acc, preset, &root.join(rel_path)).unwrap();
            assert_eq!(p.error_count, 0, "error in {rel_path}: {:?}", p.rows);
            assert_eq!(
                p.valid_count, expected_valid,
                "unexpected valid_count in {rel_path}"
            );
            let c = commit_import_preview(&mut store, &p).unwrap();
            assert_eq!(c.committed_events, expected_valid);
        }

        drop(store);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
