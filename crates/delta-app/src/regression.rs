use super::*;
use chrono::{NaiveDate, TimeZone, Utc};
use delta_core::{
    parse_decimal_lossless as dec, AdjustmentKind, EconomicEventType, LedgerEventInput,
    QualityStatus,
};
use delta_data::{CannedHttpResponse, LoopbackTestServer, StatementPreset};
use rust_decimal::Decimal;
use std::{collections::BTreeMap, fs, path::PathBuf};

static NEXT_SANDBOX_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        apply_process_quiet_guards();
        let id = NEXT_SANDBOX_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "delta-regression-{}-{}-{}",
            std::process::id(),
            id,
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn app(&self) -> AppService {
        let data = self.0.join("data");
        let docs = self.0.join("docs");
        AppService::open(
            ResolvedAppPaths {
                channel: BuildChannel::Source,
                data_dir: data,
                documents_dir: docs.clone(),
                default_export_dir: docs.join("exports"),
                default_backup_dir: docs.join("backups"),
                credential_prefix: "delta-dev".into(),
            },
            "USD",
            "USD",
            "UTC",
            true,
        )
        .unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        for _ in 0..40 {
            if fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}
fn event(id: &str, kind: EconomicEventType, qty: &str, ccy: &str, day: u32) -> LedgerEventInput {
    LedgerEventInput {
        event_id: id.into(),
        account_id: "A".into(),
        event_type: kind,
        occurred_at: Utc.with_ymd_and_hms(2026, 1, day, 12, 0, 0).unwrap(),
        sequence_no: day as i64,
        asset_id: ccy.into(),
        instrument_id: None,
        quantity: dec(qty).unwrap(),
        price: None,
        quote_currency: ccy.into(),
        fee_amount: Decimal::ZERO,
        fee_currency: ccy.into(),
        cost_known: true,
        counterparty_account_id: None,
        transfer_group_id: None,
        correction_group_id: None,
        revision: 1,
        active_in_correction_group: true,
        source_ref: None,
    }
}

#[test]
fn missing_fx_and_opening_cash_are_rejected_and_interest_fee_are_applied() {
    let mut events = vec![event(
        "deposit",
        EconomicEventType::Deposit,
        "1000",
        "HKD",
        2,
    )];
    assert!(delta_core::replay_ledger_events("USD", &events, &Default::default(), 1).is_err());
    events[0].quote_currency = "USD".into();
    events[0].asset_id = "USD".into();
    events.push(event(
        "interest",
        EconomicEventType::Interest,
        "10",
        "USD",
        3,
    ));
    events.push(event("fee", EconomicEventType::FeeOnly, "2", "USD", 4));
    let projection =
        delta_core::replay_ledger_events("USD", &events, &Default::default(), 1).unwrap();
    assert_eq!(
        projection.cash_balances[&("A".into(), "USD".into())],
        dec("1008").unwrap()
    );
    let report = delta_core::evaluate_portfolio(
        &projection,
        &["A".into()],
        "USD",
        NaiveDate::from_ymd_opt(2026, 1, 4).unwrap(),
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(report.total_pnl, Some(dec("8").unwrap()));
}

#[test]
fn foreign_cash_fifo_and_remaining_fx_gain_match_nav() {
    let events = vec![
        event("in", EconomicEventType::Deposit, "1000", "HKD", 2),
        event("out", EconomicEventType::Withdrawal, "500", "HKD", 3),
    ];
    let mut rates = std::collections::HashMap::new();
    rates.insert(
        ("HKD".into(), NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
        dec("0.1").unwrap(),
    );
    rates.insert(
        ("HKD".into(), NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()),
        dec("0.12").unwrap(),
    );
    let p = delta_core::replay_ledger_events("USD", &events, &rates, 1).unwrap();
    assert_eq!(p.realized_pnl_by_account["A"], dec("10").unwrap());
    assert_eq!(
        p.cash_carrying_costs[&("A".into(), "HKD".into())],
        dec("50").unwrap()
    );
}

#[test]
fn same_preview_replay_is_idempotent_and_modified_preview_is_rejected() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let path = fixture_root().join("statements/sample_opening_cash.csv");
    let (preview, _) = app
        .import_statement("A", StatementPreset::GenericCsv, &path, false)
        .unwrap();
    let mut changed = preview.clone();
    changed.rows[0].normalized.as_mut().unwrap().quantity = dec("123").unwrap();
    assert!(delta_data::commit_import_preview(app.store_mut(), &changed).is_err());
    let first = delta_data::commit_import_preview(app.store_mut(), &preview).unwrap();
    let second = delta_data::commit_import_preview(app.store_mut(), &preview).unwrap();
    assert!(second.idempotent_skipped);
    assert_eq!(first.ledger_revision, second.ledger_revision);
}

#[test]
fn fixture_failure_does_not_publish_version_or_checkpoint() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    assert!(app.sync_from_fixtures(&sandbox.0.join("missing")).is_err());
    let count: i64 = app
        .store()
        .conn()
        .query_row("SELECT COUNT(*) FROM dataset_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn multi_page_sync_retries_429_and_persists_the_subscription_id() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let server = LoopbackTestServer::start().unwrap();
    let start = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let mut rows = Vec::new();
    let mut fx = Vec::new();
    for i in 0..102 {
        let timestamp = (start + chrono::Duration::days(i))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        rows.push(serde_json::json!([
            timestamp * 1000,
            "100",
            "110",
            "90",
            "105",
            "20",
            timestamp * 1000 + 86399999,
            "2000",
            100,
            "10",
            "1000",
            "0"
        ]));
        fx.push(serde_json::json!([
            timestamp, "0.999", "1.001", "1.0", "0.9998", "100"
        ]));
    }
    server.register_sequence(
        "/api/v3/klines",
        vec![
            CannedHttpResponse::json(429, "{}"),
            CannedHttpResponse::json(200, serde_json::to_string(&rows[..100]).unwrap()),
            CannedHttpResponse::json(200, serde_json::to_string(&rows[100..]).unwrap()),
        ],
    );
    server.register_route(
        "/products/USDT-USD/candles",
        CannedHttpResponse::json(200, serde_json::to_string(&fx).unwrap()),
    );
    let mut urls = BTreeMap::new();
    urls.insert("binance".into(), server.base_url().into());
    urls.insert("binance_archive".into(), server.base_url().into());
    urls.insert("bitstamp".into(), server.base_url().into());
    urls.insert("coinbase".into(), server.base_url().into());
    let request = SyncRequest {
        start,
        end: start + chrono::Duration::days(102),
        instruments: vec!["BINANCE:BTC/USDT".into()],
        fx_pairs: vec![],
        use_configured_credentials: false,
        base_urls: urls,
    };
    let result = app.sync_market_data(&request).unwrap();
    assert_eq!(result.bars_committed, 102);
    let cp = app
        .store()
        .get_sync_checkpoint("binance", "bars", "BINANCE:BTC/USDT:1d:raw")
        .unwrap()
        .unwrap();
    assert_eq!(cp.cursor, (start + chrono::Duration::days(101)).to_string());
    let bars = app
        .store()
        .query_bars_at_version("BINANCE:BTC/USDT", "1d", AdjustmentKind::Raw, None, 1, None)
        .unwrap();
    assert_eq!(bars.len(), 102);
    let again = app.sync_market_data(&request).unwrap();
    assert_eq!(again.bars_committed, 0);
}

#[test]
fn backup_manifest_without_database_cannot_initialize_an_empty_replacement() {
    let sandbox = Sandbox::new();
    let app = sandbox.app();
    let backup = sandbox.0.join("backup");
    let mut manifest = app.create_backup(&backup).unwrap();
    manifest.file_hashes.remove("delta.sqlite");
    fs::write(
        backup.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let target = sandbox.0.join("restore");
    assert!(AppService::restore_backup(&backup, &target).is_err());
    assert!(!target.exists());
}

#[test]
fn report_cash_flows_produce_zero_returns_for_unchanged_market() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let csv = sandbox.0.join("flows.csv");
    fs::write(&csv,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,deposit,2026-01-01 00:00:00,UTC,USD,buy,1000,1,USD,0,USD,D1\nA,deposit,2026-01-02 00:00:00,UTC,USD,buy,1000,1,USD,0,USD,D2\n").unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &csv, true)
        .unwrap();
    let report = app
        .evaluate_and_explain(NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(), 0, None)
        .unwrap();
    assert_eq!(report.valuation.net_worth, dec("2000").unwrap());
    assert_eq!(report.valuation.total_pnl, Some(Decimal::ZERO));
    assert_eq!(report.twr, Some(Decimal::ZERO));
    assert!(report.twr_method.starts_with("daily_approximation"));
    assert!(report.xirr_status.contains("Converged"));
    assert_eq!(report.max_drawdown, Some(Decimal::ZERO));
}

#[test]
fn historical_indicators_ignore_future_sentinel_and_published_versions_are_immutable() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let sync = app.sync_from_fixtures(&fixture_root()).unwrap();
    let date = NaiveDate::from_ymd_opt(2020, 8, 28).unwrap();
    let before = app.evaluate_and_explain(date, 1, None).unwrap();
    let bars = app
        .store()
        .query_bars_at_version(
            "NASDAQ:AAPL",
            "1d",
            AdjustmentKind::Raw,
            Some("yahoo"),
            1,
            None,
        )
        .unwrap();
    assert!(!bars.is_empty());
    assert!(app
        .store_mut()
        .commit_bars_and_checkpoint(&bars, 1, None)
        .is_err());
    let version = app
        .store_mut()
        .create_dataset_version(
            "sentinel",
            "unofficial_personal",
            "{}",
            "exact",
            "reconstructed_history",
        )
        .unwrap();
    let mut sentinel = bars[0].clone();
    sentinel.session_date = NaiveDate::from_ymd_opt(2020, 9, 1).unwrap();
    sentinel.open_at = sentinel
        .session_date
        .and_hms_opt(13, 30, 0)
        .unwrap()
        .and_utc();
    sentinel.close_at = sentinel
        .session_date
        .and_hms_opt(20, 0, 0)
        .unwrap()
        .and_utc();
    sentinel.open = dec("999999").unwrap();
    sentinel.high = sentinel.open;
    sentinel.low = sentinel.open;
    sentinel.close = sentinel.open;
    app.store_mut()
        .commit_bars_and_checkpoint(&[sentinel], version.version_num, None)
        .unwrap();
    app.store_mut()
        .publish_dataset_version(version.version_num)
        .unwrap();
    let after = app.evaluate_and_explain(date, 2, None).unwrap();
    assert_eq!(before.sample_sma_2, after.sample_sma_2);
    assert_eq!(before.sample_rsi_2, after.sample_rsi_2);
    assert_ne!(sync.dataset_version.manifest_hash, version.manifest_hash);
}

#[test]
fn third_asset_fee_reduces_one_lot_and_corrections_preserve_journals() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    app.sync_from_fixtures(&fixture_root()).unwrap();
    app.import_statement(
        "A",
        StatementPreset::OkxCsv,
        &fixture_root().join("statements/okx_billing_sample.csv"),
        true,
    )
    .unwrap();
    let events = delta_data::load_all_ledger_events(app.store()).unwrap();
    let fx = delta_data::load_book_fx_lookup(app.store(), "USD").unwrap();
    let p = delta_core::replay_ledger_events(
        "USD",
        &events,
        &fx,
        app.store()
            .conn()
            .query_row("SELECT ledger_revision FROM library", [], |r| r.get(0))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        p.holdings[&("A".into(), "OKB".into())].quantity,
        dec("8.5").unwrap()
    );
    assert!(!p.cash_balances.contains_key(&("A".into(), "OKB".into())));
    assert_eq!(
        p.cash_balances[&("A".into(), "USDT".into())],
        dec("24700").unwrap()
    );
    for tx in &p.transactions {
        tx.verify_balanced().unwrap();
    }
    let first = events
        .iter()
        .find(|e| e.event_type == EconomicEventType::Deposit)
        .unwrap();
    let original_tx: String = app
        .store()
        .conn()
        .query_row(
            "SELECT id FROM journal_transaction WHERE event_id=?1",
            [&first.event_id],
            |r| r.get(0),
        )
        .unwrap();
    app.amend_event(
        &first.event_id,
        dec("51000").unwrap(),
        first.price,
        Decimal::ZERO,
    )
    .unwrap();
    let retained: bool = app
        .store()
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM journal_transaction WHERE id=?1)",
            [&original_tx],
            |r| r.get(0),
        )
        .unwrap();
    assert!(retained);
    let reversal: i64 = app
        .store()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM journal_transaction WHERE reversal_of=?1",
            [original_tx],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reversal, 1);
}

#[test]
fn registered_symbol_survives_reopen_and_auto_subscribes_after_import() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let id = app
        .register_market_instrument("NASDAQ", "GOOG", false)
        .unwrap();
    assert_eq!(app.resolve_market_symbol("US.GOOG").unwrap(), id);
    app.import_statement(
        "A",
        StatementPreset::GenericCsv,
        &fixture_root().join("statements/sample_opening_cash.csv"),
        true,
    )
    .unwrap();
    let path = sandbox.0.join("trade.csv");
    fs::write(&path,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,buy,2026-01-02 15:00:00,UTC,GOOG,buy,10,100,USD,1,USD,GOOG1\n").unwrap();
    let (_, receipt) = app
        .import_statement("A", StatementPreset::GenericCsv, &path, true)
        .unwrap();
    assert!(receipt.unwrap().auto_subscribed_instruments.contains(&id));
    drop(app);
    let reopened = sandbox.app();
    assert_eq!(reopened.resolve_market_symbol("GOOG.US").unwrap(), id);
}

#[test]
fn cancelled_sync_publishes_no_version() {
    let sandbox = Sandbox::new();
    let app = sandbox.app();
    let request = SyncRequest {
        start: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        end: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
        instruments: vec!["BINANCE:BTC/USDT".into()],
        fx_pairs: vec![],
        use_configured_credentials: false,
        base_urls: BTreeMap::new(),
    };
    let id = app.start_market_sync(request, std::time::Duration::ZERO);
    let snapshot = app
        .jobs()
        .wait_job(id, std::time::Duration::from_secs(1))
        .unwrap();
    assert!(matches!(snapshot.state, JobState::TimedOut(_)));
    // The worker may still be closing its SQLite savepoint. The lock lease outlives AppService.
    let start = std::time::Instant::now();
    loop {
        let count: i64 = app
            .store()
            .conn()
            .query_row("SELECT COUNT(*) FROM dataset_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        if start.elapsed() > std::time::Duration::from_millis(50) {
            break;
        }
        std::thread::yield_now();
    }
}

#[test]
fn in_flight_sync_cancellation_releases_resources_and_delete_restore_keep_identity() {
    use std::io::{Read as _, Write as _};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let sandbox = Sandbox::new();
    let app = sandbox.app();
    let data_dir = app.paths().data_dir.clone();
    let host = AppHost::from_service(app);
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (entered, received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = [0_u8; 8192];
        let _ = socket.read(&mut request).unwrap();
        entered.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(5));
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    });
    let request = SyncRequest {
        start: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        end: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
        instruments: vec!["NASDAQ:AAPL".into()],
        fx_pairs: vec![],
        use_configured_credentials: false,
        base_urls: BTreeMap::from([
            ("yahoo".into(), base_url.clone()),
            ("eastmoney".into(), base_url),
        ]),
    };
    let job = host
        .start_market_sync(request, Duration::from_secs(30))
        .unwrap();
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let progress = host.poll_job_throttled(job).unwrap();
    assert_eq!(progress.progress.records_published, 0);
    assert!(!progress.worker_finished);
    let start = Instant::now();
    assert!(matches!(
        host.cancel_job(job).unwrap().state,
        JobState::Cancelled(_)
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    let finished = host.wait_job_finished(job, Duration::from_secs(5)).unwrap();
    assert!(finished.worker_finished);
    assert!(matches!(finished.state, JobState::Cancelled(_)));
    assert!(start.elapsed() < Duration::from_secs(5));
    release.send(()).unwrap();
    server.join().unwrap();
    let published = host
        .execute(|service| Ok(service.latest_dataset_version()))
        .unwrap();
    assert_eq!(published, 0);
    let backup = sandbox.0.join("backup-after-cancel");
    host.create_backup(backup.clone()).unwrap();
    let preview = host
        .execute(|service| service.preview_library_deletion())
        .unwrap();
    assert_eq!(
        AppService::delete_library_from_preview(&preview, "wrong-id")
            .unwrap_err()
            .code,
        "DELETE_CONFIRMATION"
    );
    assert!(data_dir.join("delta.sqlite").is_file());
    host.shutdown().unwrap();
    assert!(host.execute(|_| Ok(())).is_err());
    assert!(
        AppService::delete_library_from_preview(&preview, &preview.library_id)
            .unwrap()
            .deleted
    );
    assert!(!data_dir.exists());
    AppService::restore_backup(&backup, &data_dir).unwrap();
    let restored = sandbox.app();
    assert_eq!(
        restored.preview_library_deletion().unwrap().library_id,
        preview.library_id
    );
}

#[test]
fn real_sync_job_reports_actual_prepared_and_published_records() {
    let sandbox = Sandbox::new();
    let server = LoopbackTestServer::start().unwrap();
    server.register_route("/v8/finance/chart/AAPL", CannedHttpResponse::json(200,
        r#"{"chart":{"result":[{"timestamp":[1598535000,1598621400],"indicators":{"quote":[{"open":[100,105],"high":[100,105],"low":[100,105],"close":[100,105],"volume":[100,100]}]}}]}}"#));
    server.register_route("/api/qt/stock/kline/get", CannedHttpResponse::json(200,
        r#"{"data":{"klines":["2020-08-27,100,100,100,100,100","2020-08-28,105,105,105,105,100"]}}"#));
    let host = AppHost::from_service(sandbox.app());
    let request = SyncRequest {
        start: NaiveDate::from_ymd_opt(2020, 8, 27).unwrap(),
        end: NaiveDate::from_ymd_opt(2020, 8, 29).unwrap(),
        instruments: vec!["NASDAQ:AAPL".into()],
        fx_pairs: vec![],
        use_configured_credentials: false,
        base_urls: BTreeMap::from([
            ("yahoo".into(), server.base_url().into()),
            ("eastmoney".into(), server.base_url().into()),
        ]),
    };
    let job = host
        .start_market_sync(request, std::time::Duration::from_secs(10))
        .unwrap();
    let result = host
        .wait_job_finished(job, std::time::Duration::from_secs(5))
        .unwrap();
    assert!(matches!(result.state, JobState::Succeeded));
    assert_eq!(result.progress.records_prepared, 2);
    assert_eq!(result.progress.records_published, 2);
    assert!(result.worker_finished);
    host.shutdown().unwrap();
}

#[test]
fn frozen_empty_scope_and_zero_market_version_never_expand_to_new_data() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let scope = AnalysisScope::all_accounts(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(), "USD");
    let frozen = app.resolve_and_validate_scope(&scope).unwrap();
    assert!(frozen.versions_frozen);
    assert!(frozen.resolved_accounts.is_empty());
    app.store_mut()
        .upsert_account("A", "later account", "broker", "manual", None, "active")
        .unwrap();
    app.sync_from_fixtures(&fixture_root()).unwrap();
    let still_frozen = app.resolve_and_validate_scope(&frozen).unwrap();
    assert!(still_frozen.resolved_accounts.is_empty());
    assert_eq!(still_frozen.dataset_version, 0);
    let slice = app
        .load_scoped_kline_window_slice(
            &still_frozen,
            "NASDAQ:AAPL",
            "1d",
            AdjustmentKind::Raw,
            &KlineViewport::new(0, 100),
            100,
        )
        .unwrap();
    assert!(slice.bars.is_empty());
}

#[test]
fn journal_scope_enforces_accounts_review_period_cutoff_and_permission_changes() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    seed_ac02_fixture_in_service(&mut app).unwrap();
    app.store_mut()
        .upsert_account("B", "other account", "broker", "manual", None, "active")
        .unwrap();
    for (id, account, date) in [
        ("note:A", "ACC-1", "2026-01-10"),
        ("note:B", "B", "2026-01-10"),
        ("note:future", "ACC-1", "2026-01-20"),
        ("note:old", "ACC-1", "2025-12-31"),
    ] {
        app.store_mut()
            .save_note_revision(
                id,
                "review",
                "post_trade",
                true,
                "account-scoped review",
                &StructuredNoteFields::default(),
                &[],
            )
            .unwrap();
        app.store_mut()
            .add_note_link(id, "account", account, 1, None, "owner")
            .unwrap();
        app.store_mut()
            .add_note_link(id, "review_date", date, 1, None, "scope")
            .unwrap();
        app.store()
            .conn()
            .execute(
                "UPDATE note_revision SET saved_at='2026-01-11T00:00:00Z' WHERE note_id=?1",
                rusqlite::params![id],
            )
            .unwrap();
    }
    let mut scope = AnalysisScope::for_accounts(
        vec!["ACC-1".into()],
        NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
        "USD",
    );
    scope.start_date = Some(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    scope.visible_until = Some(Utc.with_ymd_and_hms(2026, 1, 15, 23, 59, 59).unwrap());
    let result = app.query_notes(&scope, "", true, "journal", 0).unwrap();
    assert_eq!(
        result
            .value
            .iter()
            .map(|note| note.id.as_str())
            .collect::<Vec<_>>(),
        vec!["note:A"]
    );
    for denied in ["note:B", "note:future", "note:old"] {
        assert!(app.get_scoped_note(&scope, denied).is_err());
    }
    let frozen = result.scope;
    app.store()
        .conn()
        .execute("UPDATE note SET allow_model_access=0 WHERE id='note:A'", [])
        .unwrap();
    assert!(app.resolve_and_validate_scope(&frozen).is_err());
    assert!(app
        .query_notes(&scope, "", true, "journal", 0)
        .unwrap()
        .value
        .is_empty());
    app.store()
        .conn()
        .execute("UPDATE note SET allow_model_access=1 WHERE id='note:A'", [])
        .unwrap();
    app.store()
        .conn()
        .execute(
            "UPDATE note_revision SET saved_at='2026-01-16T00:00:00Z' WHERE note_id='note:A'",
            [],
        )
        .unwrap();
    assert!(app
        .query_notes(&scope, "", true, "journal", 0)
        .unwrap()
        .value
        .is_empty());
}

#[test]
fn source_business_ids_deduplicate_across_files_and_revisions_require_correction() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let header="account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\n";
    let row = "A,deposit,2026-01-02 00:00:00,UTC,USD,buy,1000,1,USD,0,USD,D1\n";
    let first = sandbox.0.join("first.csv");
    fs::write(&first, format!("{header}{row}")).unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &first, true)
        .unwrap();
    let overlap = sandbox.0.join("overlap.csv");
    fs::write(
        &overlap,
        format!("{header}{row}A,deposit,2026-01-03 00:00:00,UTC,USD,buy,1,1,USD,0,USD,D2\n"),
    )
    .unwrap();
    let (preview, receipt) = app
        .import_statement("A", StatementPreset::GenericCsv, &overlap, true)
        .unwrap();
    assert_eq!(receipt.unwrap().committed_events, 1);
    assert_eq!(preview.rows[0].validation_state, "source_duplicate");
    let changed = sandbox.0.join("changed.csv");
    fs::write(
        &changed,
        format!("{header}{}", row.replace(",1000,", ",2000,")),
    )
    .unwrap();
    assert!(app
        .import_statement("A", StatementPreset::GenericCsv, &changed, true)
        .is_err());
}

#[test]
fn distinct_same_timestamp_trades_can_be_confirmed_from_held_rows() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    app.import_statement(
        "A",
        StatementPreset::GenericCsv,
        &fixture_root().join("statements/sample_opening_cash.csv"),
        true,
    )
    .unwrap();
    let text="account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,buy,2026-01-02 15:00:00,UTC,AAPL,buy,10,100,USD,1,USD,\n";
    let first = sandbox.0.join("first.csv");
    fs::write(&first, text).unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &first, true)
        .unwrap();
    let second = sandbox.0.join("second.csv");
    fs::write(&second, format!("{text}\n")).unwrap();
    let (preview, receipt) = app
        .import_statement("A", StatementPreset::GenericCsv, &second, true)
        .unwrap();
    assert_eq!(preview.suspected_duplicate_count, 1);
    assert_eq!(receipt.unwrap().committed_events, 0);
    let row_id = format!("ROW:{}:1", preview.batch_id);
    app.resolve_import_row(&row_id, true).unwrap();
    let events = delta_data::load_all_ledger_events(app.store()).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == EconomicEventType::Buy)
            .count(),
        2
    );
    let revision = app.resolve_import_row(&row_id, true).unwrap();
    assert!(revision > 1);
}

#[test]
fn past_balance_snapshot_ignores_later_deposit() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let csv = sandbox.0.join("flows.csv");
    fs::write(&csv,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,deposit,2026-01-01 00:00:00,UTC,USD,buy,1000,1,USD,0,USD,D1\nA,deposit,2026-01-02 00:00:00,UTC,USD,buy,1000,1,USD,0,USD,D2\n").unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &csv, true)
        .unwrap();
    let issues = app
        .reconcile_account(
            "A",
            &[delta_data::AccountBalanceSnapshotDto {
                asset_id: "USD".into(),
                quantity: dec("1000").unwrap(),
                as_of: Utc.with_ymd_and_hms(2026, 1, 1, 20, 0, 0).unwrap(),
            }],
        )
        .unwrap();
    assert!(issues.is_empty());
}

#[test]
fn internal_cash_transfer_preserves_principal_and_charges_fee_once() {
    let mut out = event("out", EconomicEventType::TransferOut, "998", "USD", 3);
    out.fee_amount = dec("2").unwrap();
    out.counterparty_account_id = Some("B".into());
    let mut incoming = event("in", EconomicEventType::TransferIn, "998", "USD", 3);
    incoming.account_id = "B".into();
    incoming.counterparty_account_id = Some("A".into());
    let events = vec![
        event("deposit", EconomicEventType::Deposit, "1000", "USD", 2),
        out,
        incoming,
    ];
    let p = delta_core::replay_ledger_events("USD", &events, &Default::default(), 1).unwrap();
    let report = delta_core::evaluate_portfolio(
        &p,
        &["A".into(), "B".into()],
        "USD",
        NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(),
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(report.net_worth, dec("998").unwrap());
    assert_eq!(report.net_external_cash_flow, dec("1000").unwrap());
    assert_eq!(report.total_pnl, Some(dec("-2").unwrap()));
}

#[test]
fn ac02_application_import_sync_report_and_export_match_hand_calculation() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let statement = sandbox.0.join("ac02.csv");
    fs::write(&statement,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,deposit,2020-08-27 10:00:00,UTC,USD,buy,2000,1,USD,0,USD,D\nA,buy,2020-08-27 15:00:00,UTC,AAPL,buy,10,100,USD,1,USD,B\nA,sell,2020-08-28 15:00:00,UTC,AAPL,sell,4,110,USD,1,USD,S\n").unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &statement, true)
        .unwrap();
    let server = LoopbackTestServer::start().unwrap();
    server.register_route("/v8/finance/chart/AAPL", CannedHttpResponse::json(200,r#"{"chart":{"result":[{"timestamp":[1598535000,1598621400],"indicators":{"quote":[{"open":[100,105],"high":[100,105],"low":[100,105],"close":[100,105],"volume":[100,100]}]}}]}}"#));
    server.register_route("/api/qt/stock/kline/get", CannedHttpResponse::json(200,r#"{"data":{"klines":["2020-08-27,100,100,100,100,100","2020-08-28,105,105,105,105,100"]}}"#));
    let request = SyncRequest {
        start: NaiveDate::from_ymd_opt(2020, 8, 27).unwrap(),
        end: NaiveDate::from_ymd_opt(2020, 8, 29).unwrap(),
        instruments: vec![],
        fx_pairs: vec![],
        use_configured_credentials: false,
        base_urls: BTreeMap::from([
            ("yahoo".into(), server.base_url().into()),
            ("eastmoney".into(), server.base_url().into()),
        ]),
    };
    let sync = app.sync_market_data(&request).unwrap();
    assert_eq!(sync.bars_committed, 2);
    let report = app
        .evaluate_and_explain(
            NaiveDate::from_ymd_opt(2020, 8, 28).unwrap(),
            sync.dataset_version.version_num,
            None,
        )
        .unwrap();
    assert_eq!(report.valuation.cash_value, dec("1438").unwrap());
    assert_eq!(report.valuation.total_carrying_cost, dec("600.6").unwrap());
    assert_eq!(report.valuation.realized_pnl, Some(dec("38.6").unwrap()));
    assert_eq!(report.valuation.unrealized_pnl, Some(dec("29.4").unwrap()));
    assert_eq!(report.valuation.net_worth, dec("2068").unwrap());
    assert_eq!(report.valuation.total_pnl, Some(dec("68").unwrap()));
    assert_eq!(report.twr, Some(dec("0.034").unwrap()));
    let export = sandbox.0.join("export");
    let manifest = app.export_library(&export).unwrap();
    assert_eq!(manifest.files.len(), 8);
    for (name, hash) in &manifest.files {
        assert_eq!(*hash, delta_data::sha256_file(&export.join(name)).unwrap());
    }
    assert!(export.join("fx_rates.csv").is_file());
    assert!(app.export_library(&export).is_err());
    let scope: String = app
        .store()
        .conn()
        .query_row("SELECT scope_json FROM analysis_report", [], |r| r.get(0))
        .unwrap();
    assert!(scope.contains("fifo") && scope.contains("m1-ledger-3") && scope.contains("yahoo"));
}

#[test]
fn foreign_report_currency_uses_historical_flows_and_explains_translation() {
    let events = vec![
        event("deposit", EconomicEventType::Deposit, "1000", "HKD", 2),
        event("withdrawal", EconomicEventType::Withdrawal, "500", "HKD", 3),
    ];
    let mut book = std::collections::HashMap::new();
    let mut rates = std::collections::HashMap::new();
    for (day, hkd, eur) in [(2, "0.1", "0.9"), (3, "0.12", "0.8"), (4, "0.12", "0.7")] {
        let date = NaiveDate::from_ymd_opt(2026, 1, day).unwrap();
        book.insert(("HKD".into(), date), dec(hkd).unwrap());
        rates.insert(("HKD".into(), "USD".into(), date), dec(hkd).unwrap());
        rates.insert(("USD".into(), "EUR".into(), date), dec(eur).unwrap());
    }
    let projection = delta_core::replay_ledger_events("USD", &events, &book, 1).unwrap();
    let report = delta_core::evaluate_portfolio(
        &projection,
        &["A".into()],
        "EUR",
        NaiveDate::from_ymd_opt(2026, 1, 4).unwrap(),
        &Default::default(),
        &Default::default(),
        &rates,
        &Default::default(),
    );
    assert_eq!(report.net_worth, dec("42").unwrap());
    assert_eq!(report.net_external_cash_flow, dec("42").unwrap());
    assert_eq!(report.total_pnl, Some(Decimal::ZERO));
    assert_eq!(
        report.scope_and_currency_effect_pnl,
        Some(dec("-14").unwrap())
    );
}

#[test]
fn internal_foreign_cash_preserves_fifo_cost_and_value_while_in_transit() {
    let mut outgoing = event("out", EconomicEventType::TransferOut, "998", "HKD", 3);
    outgoing.fee_amount = dec("2").unwrap();
    outgoing.counterparty_account_id = Some("B".into());
    let mut incoming = event("in", EconomicEventType::TransferIn, "998", "HKD", 4);
    incoming.account_id = "B".into();
    incoming.counterparty_account_id = Some("A".into());
    let events = vec![
        event("deposit", EconomicEventType::Deposit, "1000", "HKD", 2),
        outgoing,
        incoming,
    ];
    let book = std::collections::HashMap::from([
        (
            ("HKD".into(), NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
            dec("0.1").unwrap(),
        ),
        (
            ("HKD".into(), NaiveDate::from_ymd_opt(2026, 1, 3).unwrap()),
            dec("0.12").unwrap(),
        ),
        (
            ("HKD".into(), NaiveDate::from_ymd_opt(2026, 1, 4).unwrap()),
            dec("0.13").unwrap(),
        ),
    ]);
    let before = delta_core::replay_ledger_events("USD", &events[..2], &book, 1).unwrap();
    assert_eq!(before.in_transit_assets.len(), 1);
    assert_eq!(
        before.in_transit_assets[0].carrying_cost,
        dec("99.8").unwrap()
    );
    assert!(!before
        .cash_balances
        .contains_key(&("B".into(), "HKD".into())));
    let after = delta_core::replay_ledger_events("USD", &events, &book, 1).unwrap();
    assert!(after.in_transit_assets.is_empty());
    assert_eq!(
        after.cash_carrying_costs[&("B".into(), "HKD".into())],
        dec("99.8").unwrap()
    );
    assert_eq!(after.realized_pnl_by_account["A"], dec("0.04").unwrap());
    let rates = std::collections::HashMap::from([(
        (
            "HKD".into(),
            "USD".into(),
            NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(),
        ),
        dec("0.12").unwrap(),
    )]);
    let report = delta_core::evaluate_portfolio(
        &before,
        &["A".into(), "B".into()],
        "USD",
        NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(),
        &Default::default(),
        &Default::default(),
        &rates,
        &Default::default(),
    );
    assert_eq!(report.net_worth, dec("119.76").unwrap());
    assert_eq!(report.total_pnl, Some(dec("19.76").unwrap()));
    assert_eq!(report.scope_and_currency_effect_pnl, Some(Decimal::ZERO));
}

#[test]
fn position_transfer_arrival_keeps_original_lot_date_and_partial_scope_flow() {
    let mut opening = event("opening", EconomicEventType::OpeningBalance, "10", "USD", 2);
    opening.asset_id = "NASDAQ:AAPL".into();
    opening.instrument_id = Some(opening.asset_id.clone());
    opening.price = Some(dec("100").unwrap());
    let mut outgoing = event("out", EconomicEventType::PositionTransferOut, "6", "USD", 3);
    outgoing.asset_id = opening.asset_id.clone();
    outgoing.instrument_id = opening.instrument_id.clone();
    outgoing.counterparty_account_id = Some("B".into());
    let mut incoming = outgoing.clone();
    incoming.event_id = "in".into();
    incoming.event_type = EconomicEventType::PositionTransferIn;
    incoming.account_id = "B".into();
    incoming.counterparty_account_id = Some("A".into());
    incoming.occurred_at = Utc.with_ymd_and_hms(2026, 1, 4, 12, 0, 0).unwrap();
    let events = vec![opening.clone(), outgoing, incoming];
    let before =
        delta_core::replay_ledger_events("USD", &events[..2], &Default::default(), 1).unwrap();
    assert_eq!(
        before.holdings[&("A".into(), "NASDAQ:AAPL".into())].quantity,
        dec("4").unwrap()
    );
    assert!(!before
        .holdings
        .contains_key(&("B".into(), "NASDAQ:AAPL".into())));
    assert_eq!(before.in_transit_assets[0].quantity, dec("6").unwrap());
    let after = delta_core::replay_ledger_events("USD", &events, &Default::default(), 1).unwrap();
    let lot = after
        .open_lots
        .iter()
        .find(|lot| lot.account_id == "B")
        .unwrap();
    assert_eq!(lot.acquired_at, opening.occurred_at);
    assert_eq!(lot.remaining_cost_book, dec("600").unwrap());
    let day = NaiveDate::from_ymd_opt(2026, 1, 4).unwrap();
    let report = delta_core::evaluate_portfolio(
        &after,
        &["B".into()],
        "USD",
        day,
        &std::collections::HashMap::from([(("NASDAQ:AAPL".into(), day), dec("110").unwrap())]),
        &std::collections::HashMap::from([("NASDAQ:AAPL".into(), "USD".into())]),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(report.net_external_cash_flow, dec("660").unwrap());
    assert_eq!(report.total_pnl, Some(Decimal::ZERO));
    assert_eq!(
        report.scope_and_currency_effect_pnl,
        Some(dec("-60").unwrap())
    );
}

#[test]
fn event_book_fx_does_not_change_when_historical_market_fx_is_revised() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
    let mut first_rate = delta_core::FxRateRecord {
        id: "FX:HKD:1".into(),
        base_currency: "HKD".into(),
        quote_currency: "USD".into(),
        rate: dec("0.1").unwrap(),
        effective_date: date,
        effective_at: date.and_hms_opt(0, 0, 0).unwrap().and_utc(),
        observed_at: Utc::now(),
        source: "frankfurter".into(),
        quality: QualityStatus::Exact,
        carried_from_date: None,
        dataset_version: 1,
    };
    app.store_mut()
        .create_dataset_version(
            "frankfurter",
            "public_open",
            "{}",
            "exact",
            "reconstructed_history",
        )
        .unwrap();
    app.store_mut()
        .upsert_fx_rates(&[first_rate.clone()], None)
        .unwrap();
    app.store_mut().publish_dataset_version(1).unwrap();
    let first = sandbox.0.join("deposit.csv");
    fs::write(&first,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,deposit,2026-01-02 12:00:00,UTC,HKD,buy,1000,1,HKD,0,HKD,D\n").unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &first, true)
        .unwrap();
    app.store_mut()
        .create_dataset_version(
            "frankfurter",
            "public_open",
            "{}",
            "exact",
            "reconstructed_history",
        )
        .unwrap();
    first_rate.dataset_version = 2;
    first_rate.rate = dec("0.2").unwrap();
    assert!(app
        .store_mut()
        .upsert_fx_rates(&[first_rate.clone()], None)
        .is_err());
    first_rate.id = "FX:HKD:2".into();
    app.store_mut()
        .upsert_fx_rates(&[first_rate], None)
        .unwrap();
    app.store_mut().publish_dataset_version(2).unwrap();
    let second = sandbox.0.join("interest.csv");
    fs::write(&second,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,interest,2026-01-02 13:00:00,UTC,HKD,buy,10,1,HKD,0,HKD,I\n").unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &second, true)
        .unwrap();
    let report = app.evaluate_and_explain(date, 2, None).unwrap();
    assert_eq!(report.valuation.net_external_cash_flow, dec("100").unwrap());
    assert_eq!(report.valuation.net_worth, dec("202").unwrap());
    assert_eq!(report.valuation.total_pnl, Some(dec("102").unwrap()));
    assert_eq!(
        report.valuation.scope_and_currency_effect_pnl,
        Some(Decimal::ZERO)
    );
}

#[test]
fn malformed_final_flags_and_timestamps_are_rejected() {
    assert!(delta_data::OkxSpotProvider::parse_candles_json(
        "OKX:BTC/USDT",
        r#"{"code":"0","data":[["1","1","1","1","1","1"]]}"#,
        None,
        1
    )
    .is_err());
    assert!(delta_data::StablecoinFxProvider::parse_kraken_ohlc(
        "USDC",
        r#"{"error":[],"result":{"USDCUSD":[["bad","1","1","1","1"]]}}"#,
        1
    )
    .is_err());
    assert!(delta_data::BinanceSpotProvider::parse_binance_timestamp(i64::MIN).is_err());
}

#[test]
fn missing_book_fx_is_synced_before_import_commits() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let server = LoopbackTestServer::start().unwrap();
    server.register_route(
        "/v1/2026-01-02..2026-01-03",
        CannedHttpResponse::json(200, r#"{"base":"USD","rates":{"2026-01-02":{"HKD":10}}}"#),
    );
    let file = sandbox.0.join("foreign.csv");
    fs::write(&file,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,deposit,2026-01-02 12:00:00,UTC,HKD,buy,1000,1,HKD,0,HKD,D\n").unwrap();
    let request = SyncRequest {
        start: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
        end: NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(),
        instruments: vec![],
        fx_pairs: vec![("HKD".into(), "USD".into())],
        use_configured_credentials: false,
        base_urls: BTreeMap::from([("frankfurter".into(), server.base_url().into())]),
    };
    let (_, receipt) = app
        .import_statement_with_sync("A", StatementPreset::GenericCsv, &file, true, Some(request))
        .unwrap();
    assert_eq!(receipt.unwrap().committed_events, 1);
    let binding: String = app
        .store()
        .conn()
        .query_row("SELECT rate FROM event_book_fx", [], |r| r.get(0))
        .unwrap();
    assert_eq!(binding, "0.1");
    assert_eq!(server.request_count("/v1/2026-01-02..2026-01-03"), 1);
}

#[test]
fn monthly_archive_backfill_verifies_checksum_before_publishing() {
    use std::io::Write;
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let server = LoopbackTestServer::start().unwrap();
    let mut archive = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut archive));
        zip.start_file(
            "BTCUSDT-1d-2025-03.csv",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"1740787200000000,100,110,90,105,20,1740873599999999,1000,1,1,1,0\n")
            .unwrap();
        zip.finish().unwrap();
    }
    use sha2::{Digest, Sha256};
    let path = "/data/spot/monthly/klines/BTCUSDT/1d/BTCUSDT-1d-2025-03.zip";
    server.register_route(
        path,
        CannedHttpResponse::bytes(200, "application/zip", archive.clone()),
    );
    server.register_route(
        format!("{path}.CHECKSUM"),
        CannedHttpResponse::json(
            200,
            format!(
                "{} BTCUSDT-1d-2025-03.zip",
                hex::encode(Sha256::digest(&archive))
            ),
        ),
    );
    server.register_route(
        "/products/USDT-USD/candles",
        CannedHttpResponse::json(200, "[[1740787200,0.99,1.01,1,0.9998,100]]"),
    );
    server.register_route(
        "/api/v2/ohlc/usdtusd/",
        CannedHttpResponse::json(
            200,
            r#"{"data":{"ohlc":[{"timestamp":"1740787200","close":"0.9998"}]}}"#,
        ),
    );
    let request = SyncRequest {
        start: NaiveDate::from_ymd_opt(2025, 3, 1).unwrap(),
        end: NaiveDate::from_ymd_opt(2025, 3, 2).unwrap(),
        instruments: vec!["BINANCE:BTC/USDT".into()],
        fx_pairs: vec![],
        use_configured_credentials: false,
        base_urls: BTreeMap::from([
            ("binance".into(), server.base_url().into()),
            ("binance_archive".into(), server.base_url().into()),
            ("coinbase".into(), server.base_url().into()),
            ("bitstamp".into(), server.base_url().into()),
        ]),
    };
    let result = app.sync_market_data(&request).unwrap();
    assert_eq!(result.bars_committed, 1);
    assert_eq!(server.request_count(path), 1);
    assert_eq!(server.request_count("/api/v3/klines"), 0);
    let other = Sandbox::new();
    let mut other_app = other.app();
    server.register_route(
        format!("{path}.CHECKSUM"),
        CannedHttpResponse::json(200, "bad-checksum"),
    );
    assert!(other_app.sync_market_data(&request).is_err());
    let versions: i64 = other_app
        .store()
        .conn()
        .query_row("SELECT COUNT(*) FROM dataset_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(versions, 0);
}

#[test]
fn recursive_backup_includes_settings_and_detects_tampering() {
    let sandbox = Sandbox::new();
    let app = sandbox.app();
    let data = app.paths().data_dir.clone();
    fs::create_dir_all(data.join("attachments/nested")).unwrap();
    fs::write(
        data.join("attachments/nested/note.txt"),
        "synthetic attachment",
    )
    .unwrap();
    fs::write(data.join("settings.json"), "{}").unwrap();
    let backup = sandbox.0.join("backup");
    let manifest = app.create_backup(&backup).unwrap();
    assert!(manifest
        .file_hashes
        .contains_key("attachments/nested/note.txt"));
    assert!(manifest.file_hashes.contains_key("settings.json"));
    let restored = sandbox.0.join("restored");
    AppService::restore_backup(&backup, &restored).unwrap();
    assert_eq!(
        fs::read_to_string(restored.join("attachments/nested/note.txt")).unwrap(),
        "synthetic attachment"
    );
    fs::write(backup.join("settings.json"), "tampered").unwrap();
    let rejected = sandbox.0.join("rejected");
    assert!(AppService::restore_backup(&backup, &rejected).is_err());
    assert!(!rejected.exists());
}

#[test]
fn repeated_company_action_versions_and_sources_do_not_multiply_adjustment() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    app.sync_from_fixtures(&fixture_root()).unwrap();
    let bars = app
        .store()
        .query_bars_at_version(
            "NASDAQ:AAPL",
            "1d",
            AdjustmentKind::Raw,
            Some("yahoo"),
            1,
            None,
        )
        .unwrap();
    let mut actions = app
        .store()
        .query_corporate_actions("NASDAQ:AAPL", Some(1))
        .unwrap();
    let first =
        delta_core::derive_adjusted_bars(&bars, &actions, AdjustmentKind::SplitAdjusted).unwrap();
    actions.extend(actions.clone());
    let again =
        delta_core::derive_adjusted_bars(&bars, &actions, AdjustmentKind::SplitAdjusted).unwrap();
    assert_eq!(first, again);
    let before = first
        .iter()
        .find(|bar| bar.session_date == NaiveDate::from_ymd_opt(2020, 8, 28).unwrap())
        .unwrap();
    assert_eq!(before.close, dec("124.8075").unwrap());
}

#[test]
fn independent_funding_presets_commit_cash_income_fees_and_withdrawals() {
    for (preset, header, rows) in [
        (
            StatementPreset::FutuCsv,
            "Account,Type,Time,Amount,Currency,Fee,TransactionID",
            vec![
                "S,deposit,2026-01-02 10:00:00,100,USD,0,D",
                "S,interest,2026-01-03 10:00:00,5,USD,0,I",
                "S,dividend,2026-01-04 10:00:00,3,USD,0,V",
                "S,fee,2026-01-05 10:00:00,2,USD,0,F",
                "S,withdrawal,2026-01-06 10:00:00,10,USD,1,W",
            ],
        ),
        (
            StatementPreset::OkxCsv,
            "account,type,timestamp,amount,currency,fee,billid",
            vec![
                "S,deposit,2026-01-02 10:00:00,100,USD,0,D",
                "S,interest,2026-01-03 10:00:00,5,USD,0,I",
                "S,dividend,2026-01-04 10:00:00,3,USD,0,V",
                "S,fee,2026-01-05 10:00:00,2,USD,0,F",
                "S,withdrawal,2026-01-06 10:00:00,10,USD,1,W",
            ],
        ),
        (
            StatementPreset::BinanceCsv,
            "account,type,date(utc),amount,coin,fee,transactionid",
            vec![
                "S,deposit,2026-01-02 10:00:00,100,USD,0,D",
                "S,interest,2026-01-03 10:00:00,5,USD,0,I",
                "S,dividend,2026-01-04 10:00:00,3,USD,0,V",
                "S,fee,2026-01-05 10:00:00,2,USD,0,F",
                "S,withdrawal,2026-01-06 10:00:00,10,USD,1,W",
            ],
        ),
    ] {
        let sandbox = Sandbox::new();
        let mut app = sandbox.app();
        let file = sandbox.0.join("funding.csv");
        fs::write(&file, format!("{header}\n{}\n", rows.join("\n"))).unwrap();
        let (_, receipt) = app.import_statement("A", preset, &file, true).unwrap();
        assert_eq!(receipt.unwrap().committed_events, 6);
        let report = app
            .evaluate_and_explain(NaiveDate::from_ymd_opt(2026, 1, 6).unwrap(), 0, None)
            .unwrap();
        assert_eq!(report.valuation.net_worth, dec("95").unwrap());
        assert_eq!(report.valuation.net_external_cash_flow, dec("90").unwrap());
        assert_eq!(report.valuation.total_pnl, Some(dec("5").unwrap()));
        assert_eq!(
            report.valuation.scope_and_currency_effect_pnl,
            Some(Decimal::ZERO)
        );
    }
}

#[test]
fn crypto_funding_keeps_unknown_history_after_full_disposal_until_cost_correction() {
    let sandbox = Sandbox::new();
    let mut app = sandbox.app();
    let funding = sandbox.0.join("funding.csv");
    fs::write(
        &funding,
        "account,type,timestamp,amount,currency,billid\nS,deposit,2026-01-02 10:00:00,1,BTC,D\n",
    )
    .unwrap();
    app.import_statement("A", StatementPreset::OkxCsv, &funding, true)
        .unwrap();
    let opening = delta_data::load_all_ledger_events(app.store())
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(opening.event_type, EconomicEventType::OpeningBalance);
    assert_eq!(opening.asset_id, "BTC");
    assert!(!opening.cost_known);
    let sale = sandbox.0.join("sale.csv");
    fs::write(&sale,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,sell,2026-01-03 10:00:00,UTC,OKX:BTC/USDT,sell,1,100,USD,0,USD,S\n").unwrap();
    app.import_statement("A", StatementPreset::GenericCsv, &sale, true)
        .unwrap();
    let date = NaiveDate::from_ymd_opt(2026, 1, 3).unwrap();
    let report = app.evaluate_and_explain(date, 0, None).unwrap();
    assert_eq!(report.valuation.net_worth, dec("100").unwrap());
    assert_eq!(report.valuation.total_pnl, None);
    app.amend_event(
        &opening.event_id,
        Decimal::ONE,
        Some(dec("90").unwrap()),
        Decimal::ZERO,
    )
    .unwrap();
    let corrected = app.evaluate_and_explain(date, 0, None).unwrap();
    assert_eq!(corrected.valuation.total_pnl, Some(dec("10").unwrap()));
}

#[test]
fn asset_fee_disposes_one_fifo_slice_and_preserves_unknown_cost_flag() {
    for known in [true, false] {
        let mut opening = event("opening", EconomicEventType::OpeningBalance, "1", "USD", 2);
        opening.asset_id = "BTC".into();
        opening.instrument_id = Some("OKX:BTC/USDT".into());
        opening.cost_known = known;
        opening.price = known.then(|| dec("100").unwrap());
        let mut fee = event("fee", EconomicEventType::FeeOnly, "0.1", "BTC", 3);
        fee.asset_id = "BTC".into();
        let rates = std::collections::HashMap::from([(
            ("BTC".into(), fee.occurred_at.date_naive()),
            dec("120").unwrap(),
        )]);
        let projection =
            delta_core::replay_ledger_events("USD", &[opening, fee], &rates, 1).unwrap();
        assert_eq!(
            projection.holdings[&("A".into(), "BTC".into())].quantity,
            dec("0.9").unwrap()
        );
        assert_eq!(projection.allocations.len(), 1);
        assert_eq!(projection.fee_expense_by_account["A"], dec("12").unwrap());
        if known {
            assert_eq!(projection.realized_pnl_by_account["A"], dec("2").unwrap());
        } else {
            assert!(projection
                .unknown_cost_assets
                .contains(&("A".into(), "BTC".into())));
        }
        for tx in projection.transactions {
            tx.verify_balanced().unwrap();
        }
    }
}

#[test]
fn external_destination_is_excluded_and_registered_empty_account_keeps_internal_transit() {
    for internal in [false, true] {
        let sandbox = Sandbox::new();
        let mut app = sandbox.app();
        if internal {
            app.store().conn().execute("INSERT INTO account(id,name,type,provider,status,created_at) VALUES ('B','B','cash','synthetic','active','2026-01-01')",[]).unwrap();
        }
        let file = sandbox.0.join("transfer.csv");
        fs::write(&file,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id,counterparty_account\nA,deposit,2026-01-02 10:00:00,UTC,USD,,100,,USD,0,USD,D,\nA,transfer_out,2026-01-03 10:00:00,UTC,USD,,40,,USD,0,USD,T,B\n").unwrap();
        app.import_statement("A", StatementPreset::GenericCsv, &file, true)
            .unwrap();
        let report = app
            .evaluate_and_explain(NaiveDate::from_ymd_opt(2026, 1, 3).unwrap(), 0, None)
            .unwrap();
        assert_eq!(
            report.valuation.net_worth,
            if internal {
                dec("100").unwrap()
            } else {
                dec("60").unwrap()
            }
        );
        assert_eq!(report.valuation.total_pnl, Some(Decimal::ZERO));
        assert_eq!(
            report.valuation.account_scope.contains(&"B".into()),
            internal
        );
    }
}

#[test]
fn all_indicators_match_independent_hand_values_and_ignore_future_sentinel() {
    use delta_core::*;
    let mut bars = Vec::new();
    for (day, close) in [(2, 10), (3, 12), (4, 14), (5, 16), (6, 18)] {
        let at = Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap();
        bars.push(BarRecord {
            id: format!("b{day}"),
            instrument_id: "SYNTHETIC".into(),
            timeframe: "1d".into(),
            session: "utc_24_7".into(),
            session_date: at.date_naive(),
            open_at: at,
            close_at: at + chrono::Duration::days(1),
            open: Decimal::from(close),
            high: Decimal::from(close + 1),
            low: Decimal::from(close - 1),
            close: Decimal::from(close),
            volume: Decimal::ONE,
            source: "synthetic".into(),
            adjustment: AdjustmentKind::Raw,
            is_final: true,
            valid_from_version: 1,
            valid_to_version: None,
            raw_payload_hash: None,
        });
    }
    let approx = |got: Option<f64>, want: f64| {
        assert!((got.unwrap() - want).abs() < 1e-10, "{got:?} != {want}")
    };
    approx(compute_sma(&bars, 3, None).last().unwrap().value, 16.0);
    approx(compute_ema(&bars, 3, None).last().unwrap().value, 16.0);
    approx(compute_rsi(&bars, 2, None).last().unwrap().value, 100.0);
    let macd = compute_macd(&bars, 2, 3, 2, None);
    approx(macd[3].macd, 1.0);
    approx(macd[3].signal, 1.0);
    approx(macd[4].histogram, 0.0);
    let atr = compute_atr(&bars, 2, None);
    approx(atr[1].value, 2.5);
    approx(atr[4].value, 2.9375);
    let bb = compute_bollinger_bands(&bars, 2, 2.0, None);
    approx(bb.last().unwrap().middle, 17.0);
    approx(bb.last().unwrap().upper, 19.0);
    approx(bb.last().unwrap().lower, 15.0);
    let before = bars.clone();
    let cutoff = before.last().unwrap().close_at;
    let mut future = before.last().unwrap().clone();
    future.open_at = cutoff;
    future.close_at = cutoff + chrono::Duration::days(1);
    future.session_date = cutoff.date_naive();
    future.close = Decimal::MAX;
    future.high = Decimal::MAX;
    bars.push(future);
    assert_eq!(
        compute_sma(&bars, 3, Some(cutoff)),
        compute_sma(&before, 3, None)
    );
    assert_eq!(
        compute_ema(&bars, 3, Some(cutoff)),
        compute_ema(&before, 3, None)
    );
    assert_eq!(
        compute_macd(&bars, 2, 3, 2, Some(cutoff)),
        compute_macd(&before, 2, 3, 2, None)
    );
    assert_eq!(
        compute_rsi(&bars, 2, Some(cutoff)),
        compute_rsi(&before, 2, None)
    );
    assert_eq!(
        compute_atr(&bars, 2, Some(cutoff)),
        compute_atr(&before, 2, None)
    );
    assert_eq!(
        compute_bollinger_bands(&bars, 2, 2.0, Some(cutoff)),
        compute_bollinger_bands(&before, 2, 2.0, None)
    );
    assert!(compute_macd(&before, 0, 3, 2, None).is_empty());
}

#[test]
fn recorded_account_providers_normalize_fills_balances_and_completed_funding() {
    use delta_data::AccountProvider;
    for (provider, body) in [
        (
            "okx",
            r#"{"as_of":"2026-01-04T00:00:00Z","balances":[{"ccy":"USDT","cashBal":"102"}],"fills":[{"instId":"BTC-USDT","side":"buy","fillSz":"1","fillPx":"100","fee":"-0.1","feeCcy":"OKB","tradeId":"T","ts":"1767355200000"}],"deposits":[{"ccy":"USDT","amt":"200","txId":"D","ts":"1767351600000","state":"2"}],"withdrawals":[{"ccy":"BTC","amt":"0.1","fee":"0.01","wdId":"W","ts":"1767441600000","state":"2"}]}"#,
        ),
        (
            "binance",
            r#"{"as_of":"2026-01-04T00:00:00Z","balances":[{"asset":"USDT","free":"100","locked":"2"}],"fills":[{"symbol":"BTCUSDT","isBuyer":true,"qty":"1","price":"100","commission":"0.1","commissionAsset":"BNB","id":123,"time":1767355200000}],"deposits":[{"coin":"USDT","amount":"200","txId":"D","insertTime":1767351600000,"status":1}],"withdrawals":[{"coin":"BTC","amount":"0.1","transactionFee":"0.01","id":"W","applyTime":"2026-01-03T12:00:00Z","status":6}]}"#,
        ),
    ] {
        let fixture =
            delta_data::FixtureAccountProvider::from_recorded_response(provider, "A", body)
                .unwrap();
        assert!(fixture.fetch_account_page("B", None).is_err());
        let page = fixture.fetch_account_page("A", None).unwrap();
        assert_eq!(page.balances[0].quantity, dec("102").unwrap());
        assert_eq!(page.events.len(), 4);
        assert!(page
            .events
            .iter()
            .any(|e| e.event_type == EconomicEventType::Buy
                && e.quantity == Decimal::ONE
                && e.fee_amount == dec("0.1").unwrap()));
        assert!(page
            .events
            .iter()
            .any(|e| e.event_type == EconomicEventType::Deposit && e.quote_currency == "USDT"));
        assert!(
            page.events
                .iter()
                .any(|e| e.event_type == EconomicEventType::PositionTransferOut
                    && e.asset_id == "BTC")
        );
        assert!(page
            .events
            .iter()
            .any(|e| e.event_type == EconomicEventType::FeeOnly
                && e.quantity == dec("0.01").unwrap()));
    }
}

#[test]
fn non_usd_book_currency_triangulates_and_freezes_event_fx() {
    let sandbox = Sandbox::new();
    let mut store =
        delta_data::SqliteStore::open_or_init(&sandbox.0.join("eur-data"), "EUR", "EUR", "UTC")
            .unwrap();
    store
        .create_dataset_version(
            "synthetic",
            "public_open",
            "{}",
            "exact",
            "reconstructed_history",
        )
        .unwrap();
    let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
    let at = date.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let rates: [delta_core::FxRateRecord; 2] =
        [("HKD", "10"), ("EUR", "0.8")].map(|(quote, rate)| delta_core::FxRateRecord {
            id: quote.into(),
            base_currency: "USD".into(),
            quote_currency: quote.into(),
            rate: dec(rate).unwrap(),
            effective_date: date,
            effective_at: at,
            observed_at: at,
            source: "synthetic".into(),
            quality: QualityStatus::Exact,
            carried_from_date: None,
            dataset_version: 1,
        });
    store.upsert_fx_rates(&rates, None).unwrap();
    store.publish_dataset_version(1).unwrap();
    let file = sandbox.0.join("eur.csv");
    fs::write(&file,"account,event_type,occurred_at,timezone,symbol,side,quantity,price,quote_currency,fee_amount,fee_currency,source_id\nA,deposit,2026-01-02 12:00:00,UTC,HKD,,100,,HKD,0,HKD,D\n").unwrap();
    let preview =
        delta_data::preview_statement_import(&store, "A", StatementPreset::GenericCsv, &file)
            .unwrap();
    delta_data::commit_import_preview(&mut store, &preview).unwrap();
    let binding: String = store
        .conn()
        .query_row("SELECT rate FROM event_book_fx", [], |row| row.get(0))
        .unwrap();
    assert_eq!(binding, "0.08");
    let replay = delta_core::replay_ledger_events_with_bindings(
        "EUR",
        &delta_data::load_all_ledger_events(&store).unwrap(),
        &delta_data::load_book_fx_lookup(&store, "EUR").unwrap(),
        &delta_data::load_event_book_fx(&store).unwrap(),
        1,
    )
    .unwrap();
    assert_eq!(replay.external_inflows_by_account["A"], dec("8").unwrap());
    let synthetic_credential_url = ["https://", "user:", "password@", "example.com"].concat();
    assert!(delta_data::validate_provider_base_url(&synthetic_credential_url).is_err());
    assert!(delta_data::validate_provider_base_url("https://example.com?token=synthetic").is_err());
    assert!(delta_data::validate_provider_base_url("http://127.0.0.1:12345").is_ok());
}

#[test]
fn public_sessions_require_lock_and_cannot_impersonate_release_channel() {
    let sandbox = Sandbox::new();
    let app = sandbox.app();
    let paths = app.paths().clone();
    assert!(AppService::open(paths.clone(), "USD", "USD", "UTC", false).is_err());
    assert!(AppService::open(paths.clone(), "USD", "USD", "UTC", true).is_err());
    let mut forged = paths;
    forged.channel = BuildChannel::Release;
    forged.data_dir = sandbox.0.join("forged-release");
    assert!(AppService::open(forged, "USD", "USD", "UTC", true).is_err());
    assert!(!sandbox.0.join("forged-release").exists());
}

#[test]
fn strict_headers_missing_ids_and_multi_account_flex_are_handled_explicitly() {
    let sandbox = Sandbox::new();
    let app = sandbox.app();
    let file = sandbox.0.join("input.csv");
    fs::write(&file,"date(utc),pair,side,price,executed,amount,fee,fee coin\n2026-01-02 10:00:00,BTCUSDT,buy,100,1,100,0,USDT\n").unwrap();
    let first =
        delta_data::preview_statement_import(app.store(), "A", StatementPreset::BinanceCsv, &file)
            .unwrap();
    assert_eq!(first.error_count, 0);
    assert_eq!(first.rows[0].normalized.as_ref().unwrap().source_ref, None);
    fs::write(&file,"date(utc),pair,side,price,executed,amount,fee,fee coin,PRICE\n2026-01-02 10:00:00,BTCUSDT,buy,100,1,100,0,USDT,100\n").unwrap();
    assert!(delta_data::preview_statement_import(
        app.store(),
        "A",
        StatementPreset::BinanceCsv,
        &file
    )
    .is_err());
    let xml = sandbox.0.join("input.xml");
    fs::write(&xml,r#"<FlexQueryResponse><FlexStatement accountId="U1"><Trade dateTime="2026-01-02 10:00:00" symbol="AAPL" buySell="BUY" quantity="1" tradePrice="100" currency="USD"/></FlexStatement><FlexStatement accountId="U2"/></FlexQueryResponse>"#).unwrap();
    assert!(delta_data::preview_statement_import(
        app.store(),
        "A",
        StatementPreset::IbkrFlexXml,
        &xml
    )
    .is_err());
}

#[test]
fn opening_cash_requires_its_own_currency_and_known_openings_need_unit_cost() {
    let day = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
    let mut rates = std::collections::HashMap::new();
    rates.insert(("HKD".to_string(), day), dec("0.1").unwrap());
    // 1000 HKD quoted in USD would otherwise be booked as 1000 USD.
    let mut mismatched = event(
        "opening",
        EconomicEventType::OpeningBalance,
        "1000",
        "HKD",
        2,
    );
    mismatched.quote_currency = "USD".into();
    assert!(delta_core::replay_ledger_events("USD", &[mismatched], &rates, 1).is_err());
    let own = event(
        "opening",
        EconomicEventType::OpeningBalance,
        "1000",
        "HKD",
        2,
    );
    let projection = delta_core::replay_ledger_events("USD", &[own], &rates, 1).unwrap();
    assert_eq!(
        projection.external_inflows_by_account["A"],
        dec("100").unwrap()
    );
    assert_eq!(
        projection.cash_carrying_costs[&("A".into(), "HKD".into())],
        dec("100").unwrap()
    );
    // An unregistered position with a declared known cost must not get a zero cost.
    let mut position = event("opening", EconomicEventType::OpeningBalance, "5", "USD", 2);
    position.asset_id = "UNREGISTERED".into();
    assert!(delta_core::replay_ledger_events("USD", &[position], &rates, 1).is_err());
}

#[test]
fn stablecoin_quote_leg_is_settled_once_from_cash() {
    let mut rates = std::collections::HashMap::new();
    for day in 1..=3 {
        rates.insert(
            (
                "USDT".to_string(),
                NaiveDate::from_ymd_opt(2026, 1, day).unwrap(),
            ),
            dec("1").unwrap(),
        );
    }
    let mut usd = event("usd", EconomicEventType::Deposit, "100", "USD", 1);
    usd.sequence_no = 1;
    let mut usdt = event("usdt", EconomicEventType::Deposit, "100", "USDT", 1);
    usdt.sequence_no = 2;
    let mut convert = event("convert", EconomicEventType::Buy, "100", "USD", 2);
    convert.asset_id = "USDT".into();
    convert.price = Some(dec("1").unwrap());
    let mut btc = event("btc", EconomicEventType::Buy, "1", "USDT", 3);
    btc.asset_id = "BTC".into();
    btc.price = Some(dec("100").unwrap());
    let events = vec![usd, usdt, convert.clone(), btc.clone()];
    let projection = delta_core::replay_ledger_events("USD", &events, &rates, 1).unwrap();
    let usdt_cash = projection.cash_balances[&("A".into(), "USDT".into())];
    let usdt_held = projection
        .holdings
        .get(&("A".into(), "USDT".into()))
        .map(|h| h.quantity)
        .unwrap_or_default();
    // 200 USDT acquired, 100 spent: exactly 100 must remain.
    assert_eq!(usdt_cash + usdt_held, dec("100").unwrap());
    // Without settling cash the purchase is rejected instead of silently consuming a lot.
    let mut usd_only = event("usd", EconomicEventType::Deposit, "100", "USD", 1);
    usd_only.sequence_no = 1;
    assert!(delta_core::replay_ledger_events("USD", &[usd_only, convert, btc], &rates, 1).is_err());
}

#[test]
fn source_channel_cannot_target_release_user_file_root() {
    let documents = crate::paths::system_documents_root().unwrap();
    for target in [
        documents.join("delta"),
        documents.join("delta").join("exports"),
    ] {
        assert!(
            crate::paths::ensure_channel_data_isolation(&target, BuildChannel::Source).is_err()
        );
    }
    assert!(crate::paths::ensure_channel_data_isolation(
        &documents.join("delta-dev").join("exports"),
        BuildChannel::Source
    )
    .is_ok());
}
