//! M5 behaviour tests on loopback fixtures (synthetic/recorded data; no public network).

use super::*;
use crate::market_fixture::{FixtureFault, MarketFixtureServer};
use crate::process::apply_process_quiet_guards;
use crate::service::{
    PrepareTrainingDataRequest, PrepareTrainingDataSummaryDto, CROSSCHECK_TRAINING_INSTRUMENT,
    DEFAULT_TRAINING_INSTRUMENT,
};
use crate::training::{TrainingOrderRequestDto, TrainingReplayMode};
use crate::{BuildChannel, ResolvedAppPaths};
use chrono::{DateTime, Duration, NaiveDate, TimeZone};
use delta_core::{OrderSide, OrderStatus, SimOrderType, TimeInForce, Timeframe};
use rust_decimal::prelude::*;
use std::fs;
use std::path::PathBuf;

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) struct M5Sandbox(pub PathBuf);

impl M5Sandbox {
    pub(crate) fn new(label: &str) -> Self {
        apply_process_quiet_guards();
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "delta-m5-{label}-{}-{id}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn paths(&self) -> ResolvedAppPaths {
        let docs = self.0.join("docs");
        ResolvedAppPaths {
            channel: BuildChannel::Source,
            data_dir: self.0.join("data"),
            documents_dir: docs.clone(),
            default_export_dir: docs.join("exports"),
            default_backup_dir: docs.join("backups"),
            credential_prefix: format!(
                "delta-test-m5-{}",
                self.0.file_name().unwrap().to_string_lossy()
            ),
        }
    }

    pub(crate) fn app(&self) -> AppService {
        AppService::open(self.paths(), "USD", "USD", "UTC", true).unwrap()
    }
}

impl Drop for M5Sandbox {
    fn drop(&mut self) {
        for _ in 0..40 {
            if fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

pub(crate) fn fixture_end() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap()
}

fn d(raw: &str) -> Decimal {
    delta_core::parse_decimal_lossless(raw).unwrap()
}

fn ledger_revision(svc: &AppService) -> i64 {
    svc.store()
        .conn()
        .query_row("SELECT ledger_revision FROM library", [], |r| r.get(0))
        .unwrap()
}

fn practice_sum(svc: &AppService, account: &str) -> Decimal {
    svc.store()
        .list_practice_entries(account)
        .unwrap()
        .iter()
        .map(|e| e.amount)
        .sum()
}

fn quick(side: OrderSide, qty: Decimal) -> TrainingOrderRequestDto {
    TrainingOrderRequestDto::quick(side, qty)
}

pub(crate) fn prepare(
    svc: &mut AppService,
    fixture: &MarketFixtureServer,
    tfs: Vec<Timeframe>,
    from: Option<NaiveDate>,
) -> PrepareTrainingDataSummaryDto {
    svc.prepare_training_data(&PrepareTrainingDataRequest {
        timeframes: tfs,
        from,
        base_urls: fixture.base_urls(),
        ..Default::default()
    })
    .unwrap()
}

pub(crate) fn btc_daily_run(seed: u64, practice: bool) -> CreateTrainingRunRequest {
    CreateTrainingRunRequest {
        market_category: TrainingMarketCategory::CryptoSpot,
        instrument_id: Some(DEFAULT_TRAINING_INSTRUMENT.to_string()),
        seed: Some(seed),
        create_and_start: true,
        use_practice_account: practice,
        ..Default::default()
    }
}

fn last_close(svc: &AppService, run_id: &str) -> Decimal {
    let (_, st) = svc.read_verified_sim_state(run_id).unwrap();
    *st.last_close_by_instrument.values().next().unwrap()
}

/// `M5-A01` thin slice: an empty temporary library prepares Binance BTC/USDT daily history from
/// the (loopback) public sources and a default practice run starts on it.
#[test]
fn m5_thin_slice_empty_library_prepares_btc_daily_and_starts_practice_run() {
    let sandbox = M5Sandbox::new("thin");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    let before = svc
        .training_data_status(DEFAULT_TRAINING_INSTRUMENT, true)
        .unwrap();
    assert!(before.needs_preparation);

    let summary = prepare(&mut svc, &fixture, vec![Timeframe::D1], None);
    assert_eq!(summary.failed, 0, "{summary:#?}");
    assert_eq!(summary.published_versions.len(), 2);
    let binance = &summary.batches[0];
    assert_eq!(binance.instrument_id, DEFAULT_TRAINING_INSTRUMENT);
    assert!(binance.archive_months > 100, "{binance:?}");
    assert!(binance.bars_committed > 3000);
    let okx = &summary.batches[1];
    assert_eq!(okx.instrument_id, CROSSCHECK_TRAINING_INSTRUMENT);
    assert!(okx.cross_source_compared > 3000, "{okx:?}");
    assert_eq!(okx.cross_source_deviations, 0);

    let status = svc
        .training_data_status(DEFAULT_TRAINING_INSTRUMENT, true)
        .unwrap();
    assert!(!status.needs_preparation, "{status:#?}");

    // Stored daily bars equal the fixture's own aggregation (hand-checkable sample).
    let open = Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap();
    let expected = fixture
        .expected_bar(Timeframe::D1, open.timestamp_millis())
        .unwrap();
    let bars = svc
        .store()
        .query_bars_at_version(
            DEFAULT_TRAINING_INSTRUMENT,
            "1d",
            delta_core::AdjustmentKind::Raw,
            None,
            *summary.published_versions.iter().max().unwrap(),
            None,
        )
        .unwrap();
    let bar = bars.iter().find(|b| b.open_at == open).unwrap();
    assert_eq!(
        [bar.open, bar.high, bar.low, bar.close, bar.volume],
        expected
    );

    let ledger_before = ledger_revision(&svc);
    let run = svc.create_training_run(btc_daily_run(7, true)).unwrap();
    assert_eq!(run.engine_version, SIM_ENGINE_VERSION_M5);
    assert_eq!(run.timeframe, "1d");
    assert_eq!(run.practice_account_id.as_deref(), Some("PRACTICE:USDT"));
    assert_eq!(run.initial_cash, Decimal::from(100_000));
    assert!(run.fill_rule_version.unwrap().starts_with("m5-fill-1"));
    let account = svc.show_practice_account("USDT").unwrap();
    assert_eq!(account.available, Decimal::ZERO);
    assert_eq!(account.allocated_to_open_run, Decimal::from(100_000));
    assert_eq!(account.open_run_id.as_deref(), Some(run.run_id.as_str()));
    assert_eq!(ledger_revision(&svc), ledger_before);
    // AI review stays refused while the run is not finished.
    let scope = crate::host::AnalysisScope::for_training_run(&run.run_id);
    assert!(svc
        .query_training_ai_run_summary(&scope, "open", 1)
        .is_err());
}

/// `M5-A03`/`A04`: quick fill at the visible close, in-place modify and single undo are
/// hand-checkable; undo never crosses a bar advance; replay reproduces every hash.
#[test]
fn m5_quick_fill_modify_undo_and_settlement_are_hand_checkable() {
    let sandbox = M5Sandbox::new("trade");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    prepare(&mut svc, &fixture, vec![Timeframe::D1], None);
    let ledger_before = ledger_revision(&svc);
    let run = svc.create_training_run(btc_daily_run(11, true)).unwrap();
    let run_id = run.run_id.clone();

    // Second practice run on the same account is refused while this one is open.
    let busy = svc
        .create_training_run(btc_daily_run(12, true))
        .unwrap_err();
    assert_eq!(busy.code, "PRACTICE_ACCOUNT_BUSY");

    let close = last_close(&svc, &run_id);
    let qty = (Decimal::from(10_000) / close).round_dp_with_strategy(4, RoundingStrategy::ToZero);

    // ① quick buy fills now at the visible close (zero-cost profile: cost = qty × close).
    let r1 = svc
        .submit_training_order(&run_id, quick(OrderSide::Buy, qty))
        .unwrap();
    let snap = svc.show_training_run(&run_id).unwrap();
    assert_eq!(snap.fills.len(), 1);
    assert_eq!(snap.fills[0].fill_price, close);
    assert_eq!(
        snap.account.cash_balance,
        Decimal::from(100_000) - qty * close
    );
    assert_eq!(snap.run.current_step_index, 0, "quick fill keeps the clock");

    // ③ undo the quick fill: cash and fills restored.
    let undo = svc.undo_training_last(&run_id).unwrap();
    assert!(undo.seq > r1.seq);
    let snap = svc.show_training_run(&run_id).unwrap();
    assert!(snap.fills.is_empty());
    assert_eq!(snap.account.cash_balance, Decimal::from(100_000));
    assert_eq!(
        svc.undo_training_last(&run_id).unwrap_err().code,
        "UNDO_UNAVAILABLE"
    );

    // ② resting limit far below, modified in place, then the modify is undone.
    let limit = (close * d("0.5")).round_dp(2);
    svc.submit_training_order(
        &run_id,
        TrainingOrderRequestDto::single(
            OrderSide::Buy,
            SimOrderType::Limit { limit_price: limit },
            qty,
            TimeInForce::Gtc,
            None,
        ),
    )
    .unwrap();
    let order_id = svc.show_training_run(&run_id).unwrap().open_orders[0]
        .order_id
        .clone();
    let new_limit = (close * d("0.6")).round_dp(2);
    svc.modify_training_order(
        &run_id,
        ModifyTrainingOrderRequest {
            order_id: order_id.clone(),
            quantity: None,
            limit_price: Some(new_limit),
            stop_price: None,
        },
    )
    .unwrap();
    let snap = svc.show_training_run(&run_id).unwrap();
    assert_eq!(snap.open_orders.len(), 1);
    assert_eq!(snap.open_orders[0].order_id, order_id);
    assert_eq!(
        snap.open_orders[0].order_type,
        SimOrderType::Limit {
            limit_price: new_limit
        }
    );
    svc.undo_training_last(&run_id).unwrap();
    let snap = svc.show_training_run(&run_id).unwrap();
    assert_eq!(
        snap.open_orders[0].order_type,
        SimOrderType::Limit { limit_price: limit }
    );

    // Quick buy, then advance: the advance closes the undo window.
    svc.submit_training_order(&run_id, quick(OrderSide::Buy, qty))
        .unwrap();
    svc.advance_training_bar(&run_id).unwrap();
    assert_eq!(
        svc.undo_training_last(&run_id).unwrap_err().code,
        "UNDO_UNAVAILABLE"
    );

    // Several advances (past a periodic checkpoint), then quick sell everything.
    for _ in 0..21 {
        svc.advance_training_bar(&run_id).unwrap();
    }
    let pos_qty = svc
        .show_training_run(&run_id)
        .unwrap()
        .position
        .unwrap()
        .quantity;
    svc.submit_training_order(&run_id, quick(OrderSide::Sell, pos_qty))
        .unwrap();
    // Undo after a reopen still works (recovery starts from a bar-boundary checkpoint).
    drop(svc);
    let mut svc = sandbox.app();
    svc.undo_training_last(&run_id).unwrap();
    svc.submit_training_order(&run_id, quick(OrderSide::Sell, pos_qty))
        .unwrap();

    // Finish settles the account at the equity of the last visible close.
    let (_, st) = svc.read_verified_sim_state(&run_id).unwrap();
    let equity = st.total_equity_at_last_close();
    svc.finish_training_run(&run_id).unwrap();
    let account = svc.show_practice_account("USDT").unwrap();
    assert_eq!(account.available, equity);
    assert_eq!(practice_sum(&svc, "PRACTICE:USDT"), equity);
    assert!(account.open_run_id.is_none());

    // Withdrawal above the available balance is refused; a smaller one is accepted.
    let too_much = svc
        .transfer_practice_funds(
            "USDT",
            PracticeTransferKind::Withdrawal,
            equity + Decimal::ONE,
        )
        .unwrap_err();
    assert_eq!(too_much.code, "WITHDRAWAL_EXCEEDS_AVAILABLE");
    let after_w = svc
        .transfer_practice_funds(
            "USDT",
            PracticeTransferKind::Withdrawal,
            Decimal::from(1000),
        )
        .unwrap();
    assert_eq!(after_w.available, equity - Decimal::from(1000));

    // Replay from scratch and from a checkpoint reproduce every persisted hash (incl. undo).
    for mode in [
        TrainingReplayMode::Scratch,
        TrainingReplayMode::FromCheckpoint,
    ] {
        let replay = svc.replay_training_run(&run_id, mode).unwrap();
        assert!(replay.matches_persisted_hashes, "{mode:?}");
    }
    assert_eq!(ledger_revision(&svc), ledger_before);

    // `m5-account-report-1`: balance and PnL tie out with the entries; undo count is carried.
    let analysis = svc.practice_account_analysis("USDT").unwrap();
    assert_eq!(analysis.balance, practice_sum(&svc, "PRACTICE:USDT"));
    assert_eq!(analysis.cumulative_pnl, equity - Decimal::from(100_000));
    assert_eq!(analysis.net_deposits, Decimal::from(99_000));
    assert_eq!(analysis.ended_run_count, 1);
    assert_eq!(analysis.undo_count, 3);
    assert!(analysis.groups.sample_insufficient);
    let history = svc.training_history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].funding_label, "练习账户 USDT");
    assert_eq!(history[0].timeframe, "1d");
    // `S-55`: the post-run AI summary carries the timeframe and operation counts.
    let scope = crate::host::AnalysisScope::for_training_run(&run_id);
    let ai = svc.query_training_ai_run_summary(&scope, "m5", 1).unwrap();
    let json = serde_json::to_value(&ai).unwrap();
    let text = json.to_string();
    assert!(text.contains("\"undo_count\":3"), "{text}");
    // Counts are effective operations: undone quick fills and modifies are not counted.
    assert!(text.contains("\"quick_fill_count\":2"), "{text}");
    assert!(text.contains("\"modify_count\":0"), "{text}");
}

/// `S-52`: abandon also settles and cannot be resumed; restart keeps the abandoned run; delete
/// only appends a reversal entry.
#[test]
fn m5_abandon_restart_and_delete_keep_practice_account_conserved() {
    let sandbox = M5Sandbox::new("abandon");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    prepare(&mut svc, &fixture, vec![Timeframe::D1], None);
    let run = svc
        .create_training_run(CreateTrainingRunRequest {
            allocation: Some(Decimal::from(40_000)),
            ..btc_daily_run(21, true)
        })
        .unwrap();
    assert_eq!(run.initial_cash, Decimal::from(40_000));
    let close = last_close(&svc, &run.run_id);
    let qty = (Decimal::from(5_000) / close).round_dp_with_strategy(4, RoundingStrategy::ToZero);
    svc.submit_training_order(&run.run_id, quick(OrderSide::Buy, qty))
        .unwrap();
    for _ in 0..3 {
        svc.advance_training_bar(&run.run_id).unwrap();
    }
    let (_, st) = svc.read_verified_sim_state(&run.run_id).unwrap();
    let equity = st.total_equity_at_last_close();

    let restarted = svc.restart_training_run(&run.run_id).unwrap();
    let old = svc.get_training_run_summary(&run.run_id).unwrap();
    assert_eq!(old.status, SimRunStatus::Abandoned);
    assert!(svc.resume_training_run(&run.run_id).is_err());
    assert!(svc
        .submit_training_order(&run.run_id, quick(OrderSide::Buy, qty))
        .is_err());
    // Restart allocated everything available: 60,000 + the abandoned run's settlement.
    assert_eq!(restarted.initial_cash, Decimal::from(60_000) + equity);
    assert_eq!(
        restarted.practice_account_id.as_deref(),
        Some("PRACTICE:USDT")
    );
    assert_eq!(restarted.status, SimRunStatus::Active);
    // Abandoned runs can be revealed and reported like finished runs.
    svc.reveal_training_run(&run.run_id).unwrap();
    svc.get_training_report(&run.run_id).unwrap();

    // Finish the restarted run, then delete the abandoned one: only a reversal is appended.
    svc.finish_training_run(&restarted.run_id).unwrap();
    let before = practice_sum(&svc, "PRACTICE:USDT");
    let n_before = svc
        .store()
        .list_practice_entries("PRACTICE:USDT")
        .unwrap()
        .len();
    let preview = svc.preview_delete_training_run(&run.run_id).unwrap();
    assert_eq!(preview.practice_entry_count, 2);
    assert_eq!(
        preview.practice_reversal,
        Some(Decimal::from(40_000) - equity)
    );
    svc.delete_training_run(&run.run_id).unwrap();
    let entries = svc.store().list_practice_entries("PRACTICE:USDT").unwrap();
    assert_eq!(entries.len(), n_before + 1);
    assert_eq!(
        entries.last().unwrap().kind,
        PracticeEntryKind::DeleteReversal
    );
    assert_eq!(
        practice_sum(&svc, "PRACTICE:USDT"),
        before + Decimal::from(40_000) - equity
    );
    // The abandoned run was deleted; the restarted (finished) run remains in the analysis.
    let analysis = svc.practice_account_analysis("USDT").unwrap();
    assert_eq!(analysis.run_count, 1);
    assert_eq!(analysis.balance, practice_sum(&svc, "PRACTICE:USDT"));
}

/// `S-50`: 4h and 1h runs advance one aligned bar per step; DAY orders on intraday runs expire
/// at the end of the UTC day; US equity is daily only.
#[test]
fn m5_intraday_runs_are_aligned_and_day_orders_expire_at_utc_day_end() {
    let sandbox = M5Sandbox::new("intraday");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    let from = NaiveDate::from_ymd_opt(2026, 3, 1);
    let summary = prepare(&mut svc, &fixture, vec![Timeframe::H4, Timeframe::H1], from);
    assert_eq!(summary.failed, 0, "{summary:#?}");
    for tf in [Timeframe::H4, Timeframe::H1] {
        let run = svc
            .create_training_run(CreateTrainingRunRequest {
                timeframe: tf,
                warmup_bars: 50,
                history_bars: 60,
                training_bars: 80,
                reveal_bars: 10,
                hidden_mode: false,
                ..btc_daily_run(31, false)
            })
            .unwrap();
        assert_eq!(run.timeframe, tf.as_str());
        let before = run.visible_until.unwrap();
        assert_eq!(before.timestamp() % tf.duration().num_seconds(), 0);
        let close = last_close(&svc, &run.run_id);
        svc.submit_training_order(
            &run.run_id,
            TrainingOrderRequestDto::single(
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: (close * d("0.5")).round_dp(2),
                },
                d("0.01"),
                TimeInForce::Day,
                None,
            ),
        )
        .unwrap();
        let step = svc.advance_training_bar(&run.run_id).unwrap();
        let after = step.run.visible_until.unwrap();
        assert_eq!(after - before, tf.duration());
        // The order became eligible at `before`; it lives until that UTC day has ended.
        let day_end = before
            .date_naive()
            .succ_opt()
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let mut vis = after;
        while vis < day_end {
            let snap = svc.show_training_run(&run.run_id).unwrap();
            assert_eq!(snap.open_orders.len(), 1, "DAY order alive at {vis}");
            vis = svc
                .advance_training_bar(&run.run_id)
                .unwrap()
                .run
                .visible_until
                .unwrap();
        }
        let snap = svc.show_training_run(&run.run_id).unwrap();
        assert!(snap.open_orders.is_empty(), "expired at {vis}");
        assert!(snap
            .all_orders
            .iter()
            .any(|o| o.status == OrderStatus::Expired));
        svc.finish_training_run(&run.run_id).unwrap();
        let replay = svc
            .replay_training_run(&run.run_id, TrainingReplayMode::Scratch)
            .unwrap();
        assert!(replay.matches_persisted_hashes);
    }
    let err = svc
        .create_training_run(CreateTrainingRunRequest {
            market_category: TrainingMarketCategory::UsEquityEtf,
            timeframe: Timeframe::H4,
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.code, "TIMEFRAME_NOT_SUPPORTED");
}

/// `S-48`: checksum failure, archive gap, rate limit, retry and incremental update.
#[test]
fn m5_prepare_failures_retry_and_incremental_update() {
    let sandbox = M5Sandbox::new("prepfail");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    let from = NaiveDate::from_ymd_opt(2026, 1, 1);
    // A checksum mismatch fails the Binance batch without publishing it; OKX still publishes.
    fixture.inject(
        "BTCUSDT-1d-2026-01.zip.CHECKSUM",
        1,
        FixtureFault::ChecksumMismatch,
    );
    let failed = prepare(&mut svc, &fixture, vec![Timeframe::D1], from);
    assert_eq!(failed.failed, 1, "{failed:#?}");
    assert!(failed.batches[0].error.is_some());
    assert_eq!(failed.batches[0].dataset_version, None);
    assert_eq!(failed.published_versions.len(), 1);
    let cp = svc
        .store()
        .get_sync_checkpoint("binance", "bars", "BINANCE:BTC/USDT:1d:raw")
        .unwrap();
    assert!(cp.is_none(), "a failed batch must not move the watermark");

    // Retry: succeeds; a month without an archive falls back to REST; 429s are retried.
    fixture.clear_faults();
    fixture.mark_archive_unavailable("2026-02");
    fixture.inject("/api/v3/klines", 1, FixtureFault::RateLimited);
    let ok = prepare(&mut svc, &fixture, vec![Timeframe::D1], from);
    assert_eq!(ok.failed, 0, "{ok:#?}");
    assert!(ok.batches[0].bars_committed > 250);

    // Up to date: nothing new is published.
    let again = prepare(&mut svc, &fixture, vec![Timeframe::D1], from);
    assert!(
        again.batches.iter().all(|b| b.status == "up_to_date"),
        "{again:#?}"
    );
    assert!(again.published_versions.is_empty());

    // Two more days: only the new bars (plus the overlap) are fetched and committed.
    fixture.advance(Duration::days(2));
    let inc = prepare(&mut svc, &fixture, vec![Timeframe::D1], from);
    assert_eq!(inc.failed, 0, "{inc:#?}");
    assert!((2..=5).contains(&inc.batches[0].bars_committed), "{inc:#?}");
    assert_eq!(
        inc.batches[0].archive_months, 0,
        "incremental updates use REST"
    );
}

/// `S-48`: while a preparation batch is still downloading, the library stays writable, so a
/// timeframe that is already prepared can start and advance a run (the desktop enables those
/// buttons during preparation). A cancelled batch still publishes nothing.
#[test]
fn m5_runs_start_and_advance_while_a_prepare_batch_downloads() {
    let sandbox = M5Sandbox::new("prepwrite");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    let daily = prepare(&mut svc, &fixture, vec![Timeframe::D1], None);
    assert_eq!(daily.failed, 0, "{daily:#?}");
    let versions_before: i64 = svc
        .store()
        .conn()
        .query_row("SELECT COUNT(*) FROM dataset_version", [], |r| r.get(0))
        .unwrap();

    // ~110 archive months at 200 ms each: the 4h batch downloads for well over 5 s
    // (the SQLite busy timeout) while the run below is created and advanced.
    fixture.set_delay_ms(200);
    let job = svc.start_prepare_training_data(
        PrepareTrainingDataRequest {
            timeframes: vec![Timeframe::H4],
            base_urls: fixture.base_urls(),
            ..Default::default()
        },
        std::time::Duration::from_secs(600),
    );
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let started = std::time::Instant::now();
    let run = svc
        .create_training_run(btc_daily_run(11, true))
        .expect("a prepared timeframe starts while another batch downloads");
    svc.advance_training_bar(&run.run_id)
        .expect("a step commits while another batch downloads");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "writes waited on the preparation job: {:?}",
        started.elapsed()
    );
    let snap = svc.jobs().snapshot(job).unwrap();
    assert!(
        !snap.worker_finished,
        "the 4h batch must still be downloading"
    );

    svc.jobs().cancel_job(job).unwrap();
    let done = svc
        .jobs()
        .wait_finished(job, std::time::Duration::from_secs(120))
        .unwrap();
    assert!(done.worker_finished);
    let versions_after: i64 = svc
        .store()
        .conn()
        .query_row("SELECT COUNT(*) FROM dataset_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        versions_after, versions_before,
        "a cancelled batch creates no dataset version"
    );
    assert!(svc
        .store()
        .get_sync_checkpoint("binance", "bars", "BINANCE:BTC/USDT:4h:raw")
        .unwrap()
        .is_none());
}

fn load_m4_fixture_library(sandbox: &M5Sandbox) -> serde_json::Value {
    let dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/libraries/m4-schema5");
    let data = sandbox.paths().data_dir;
    fs::create_dir_all(&data).unwrap();
    let mut archive =
        zip::ZipArchive::new(fs::File::open(dir.join("m4-library.sqlite.zip")).unwrap()).unwrap();
    let mut entry = archive.by_name("delta.sqlite").unwrap();
    let mut out = fs::File::create(data.join("delta.sqlite")).unwrap();
    std::io::copy(&mut entry, &mut out).unwrap();
    serde_json::from_str(&fs::read_to_string(dir.join("expected.json")).unwrap()).unwrap()
}

/// `M5-A10`: a genuine M4 (`m4-sim-1`, schema 5) library upgrades to schema 6 with a backup
/// first; every old run replays to the identical persisted hashes; the paused M4 run keeps its
/// engine and independent capital; M5-only commands are refused on it.
#[test]
fn m5_genuine_m4_library_upgrades_and_old_run_hashes_are_unchanged() {
    let sandbox = M5Sandbox::new("m4lib");
    let expected = load_m4_fixture_library(&sandbox);
    let data = sandbox.paths().data_dir;

    // A failed migration rolls back and leaves the schema-5 library and its backup intact.
    delta_data::set_schema_6_migration_fault_injection_for_test(true);
    assert!(AppService::open(sandbox.paths(), "USD", "USD", "UTC", true).is_err());
    delta_data::set_schema_6_migration_fault_injection_for_test(false);
    assert_eq!(
        delta_data::SqliteStore::check_schema_compatibility_at_version(&data, 5).unwrap(),
        5
    );
    assert!(
        !delta_data::SqliteStore::list_pre_migration_backups(&data, 5)
            .unwrap()
            .is_empty()
    );

    let mut svc = sandbox.app();
    assert!(delta_data::SqliteStore::check_schema_compatibility_at_version(&data, 5).is_err());
    let runs = svc.list_training_runs().unwrap();
    assert_eq!(runs.len(), 3);
    for run in &runs {
        let exp = &expected[&run.run_id];
        assert_eq!(run.engine_version, "m4-sim-1");
        assert_eq!(run.timeframe, "1d");
        assert!(
            run.practice_account_id.is_none(),
            "M4 runs keep independent capital"
        );
        assert_eq!(
            run.latest_state_hash,
            exp["latest_state_hash"].as_str().unwrap()
        );
        assert_eq!(run.latest_seq, exp["latest_seq"].as_i64().unwrap());
        assert_eq!(
            run.candidate_pool_hash,
            exp["candidate_pool_hash"].as_str().unwrap()
        );
        if run.latest_seq > 0 {
            for mode in [
                TrainingReplayMode::Scratch,
                TrainingReplayMode::FromCheckpoint,
            ] {
                let replay = svc.replay_training_run(&run.run_id, mode).unwrap();
                assert!(replay.matches_persisted_hashes, "{} {mode:?}", run.run_id);
            }
        }
    }

    // The paused M4 run continues with `m4-sim-1`; M5-only commands are refused.
    let cr = "TRN-M4FX-CR";
    svc.resume_training_run(cr).unwrap();
    assert_eq!(
        svc.undo_training_last(cr).unwrap_err().code,
        "ENGINE_FEATURE_UNAVAILABLE"
    );
    let quick_err = svc
        .submit_training_order(cr, quick(OrderSide::Buy, d("0.01")))
        .unwrap_err();
    assert_eq!(quick_err.code, "ENGINE_FEATURE_UNAVAILABLE");
    svc.advance_training_bar(cr).unwrap();
    let summary = svc.get_training_run_summary(cr).unwrap();
    assert_eq!(summary.engine_version, "m4-sim-1");
    let replay = svc
        .replay_training_run(cr, TrainingReplayMode::Scratch)
        .unwrap();
    assert!(replay.matches_persisted_hashes);
    // A redraw of the not-started M4 run keeps its frozen M4 pool hash.
    svc.redraw_training_run("TRN-M4FX-PREP").unwrap();
    // No practice account was created by touching M4 runs.
    assert!(svc.list_practice_accounts().unwrap().is_empty());
    let history = svc.training_history().unwrap();
    assert!(history.iter().all(|h| h.funding_label == "独立资金（M4）"));
    assert_eq!(
        svc.practice_account_analysis("USDT").unwrap().run_count,
        0,
        "M4 runs never count toward the practice account"
    );
}

/// `S-50` sentinel: the higher-timeframe view never contains a future price, even when a future
/// base bar with an absurd sentinel price exists in the same dataset version.
#[test]
fn m5_aggregated_view_matches_hand_calc_and_never_leaks_future_sentinel() {
    let sandbox = M5Sandbox::new("agg");
    let fixture = MarketFixtureServer::start(fixture_end()).unwrap();
    let mut svc = sandbox.app();
    prepare(
        &mut svc,
        &fixture,
        vec![Timeframe::H1],
        NaiveDate::from_ymd_opt(2026, 6, 1),
    );
    let run = svc
        .create_training_run(CreateTrainingRunRequest {
            timeframe: Timeframe::H1,
            warmup_bars: 50,
            history_bars: 100,
            training_bars: 100,
            reveal_bars: 10,
            hidden_mode: true,
            ..btc_daily_run(41, false)
        })
        .unwrap();
    for _ in 0..5 {
        svc.advance_training_bar(&run.run_id).unwrap();
    }
    // Turn every future base bar into an absurd sentinel price (same version).
    let visible = svc
        .store()
        .query_run_visible_final_bars(&run.run_id, false)
        .unwrap();
    let last = visible.last().unwrap().clone();
    svc.store()
        .conn()
        .execute(
            "UPDATE bar SET high = '99999999', close = '99999999' WHERE instrument_id = ?1
             AND timeframe = '1h' AND open_at > ?2",
            rusqlite::params![last.instrument_id, last.close_at.to_rfc3339()],
        )
        .unwrap();
    for view in [Timeframe::H4, Timeframe::D1] {
        let agg = svc.get_training_aggregated_view(&run.run_id, view).unwrap();
        assert!(
            agg.candles.iter().all(|c| c.open_at.is_none()),
            "hidden run shows no dates"
        );
        assert!(agg.candles.iter().all(|c| c.high < d("99999999")));
        let forming = agg.candles.last().unwrap();
        // Hand-check the last (possibly forming) candle against the visible base bars.
        let bucket = view.floor(last.open_at);
        let members: Vec<_> = visible
            .iter()
            .filter(|b| view.floor(b.open_at) == bucket)
            .collect();
        assert_eq!(forming.base_bar_count, members.len());
        assert_eq!(forming.open, members[0].open);
        assert_eq!(forming.close, members.last().unwrap().close);
        assert_eq!(forming.high, members.iter().map(|b| b.high).max().unwrap());
        assert_eq!(forming.low, members.iter().map(|b| b.low).min().unwrap());
        assert_eq!(forming.complete, last.close_at >= bucket + view.duration());
    }
    assert_eq!(
        svc.get_training_aggregated_view(&run.run_id, Timeframe::H1)
            .unwrap_err()
            .code,
        "TIMEFRAME_VIEW_UNAVAILABLE"
    );
}
