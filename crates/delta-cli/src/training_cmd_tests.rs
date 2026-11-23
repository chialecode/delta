use super::*;
use chrono::{Datelike, Days, NaiveDate, Utc};
use delta_app::{
    apply_process_quiet_guards, run_quiet_command_with_timeout, seed_ac02_fixture_in_service,
    AdjustmentKind, AppHost, ArchiveTrainingNotesResultDto, BarRecord, CorporateActionType,
    DeleteTrainingRunPreview, FeeProfileDto, OrderStatus, PlanExecutionStatus, QuietCommandResult,
    ResolvedAppPaths, SimRejectCode, TrainingNoteDto, TrainingOrderPreviewDto,
    TrainingReplayResultDto, TrainingReportDto, TrainingRevealDto, TrainingRunSummaryDto,
    TrainingStepReceiptDto, TrainingWorkspaceSnapshotDto, UserSelfEvaluationDto,
    HIDDEN_INSTRUMENT_ID,
};
use delta_core::{parse_decimal_lossless, to_canonical_text, CalendarSessionKind};
use delta_data::{query_calendar_session, SqliteStore};
use rusqlite::params;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_CLI_SANDBOX_ID: AtomicU64 = AtomicU64::new(1);

struct CliTrainingSandbox {
    root: PathBuf,
    credential_prefix: String,
}

impl CliTrainingSandbox {
    fn new(label: &str) -> Self {
        apply_process_quiet_guards();
        let id = NEXT_CLI_SANDBOX_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "delta-m4-cli-{label}-{}-{id}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        fs::create_dir_all(&root).unwrap();
        let credential_prefix = format!("delta-test-m4-cli-{label}-{}-{id}", std::process::id());
        Self {
            root,
            credential_prefix,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn app_paths(&self) -> ResolvedAppPaths {
        let mut paths =
            resolve_app_paths(Some(BuildChannel::Source), Some(&self.data_dir())).unwrap();
        paths.documents_dir = self.root.join("docs");
        paths.default_export_dir = paths.documents_dir.join("exports");
        paths.default_backup_dir = paths.documents_dir.join("backups");
        paths.credential_prefix = self.credential_prefix.clone();
        paths
    }

    fn open_service(&self) -> AppService {
        AppService::open(self.app_paths(), "USD", "USD", "UTC", true).unwrap()
    }

    fn open_host(&self) -> AppHost {
        AppHost::from_service(self.open_service())
    }

    fn run_cli_ok<I, S>(&self, sub_args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let res = self.run_cli_subprocess(sub_args);
        assert_eq!(
            res.status_code,
            Some(0),
            "expected CLI exit 0 for {:?}, got {:?}; stderr:\n{}\nstdout:\n{}",
            res.args,
            res.status_code,
            res.stderr,
            res.stdout
        );
        assert!(!res.timed_out, "CLI subprocess timed out");
        res.stdout.trim().to_string()
    }

    fn run_cli_subprocess<I, S>(&self, sub_args: I) -> QuietCommandResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut full_args: Vec<String> = vec!["training".to_string()];
        for a in sub_args {
            full_args.push(a.as_ref().to_string());
        }
        full_args.push("--data-dir".to_string());
        full_args.push(self.data_dir().to_string_lossy().to_string());

        let exe = std::env::current_exe().expect("test binary path");
        let mut extra_env = BTreeMap::new();
        extra_env.insert(
            "DELTA_M4_CLI_SUBPROCESS_ARGS_JSON".to_string(),
            serde_json::to_string(&full_args).unwrap(),
        );
        let mut res = run_quiet_command_with_timeout(
            exe.as_os_str(),
            [
                OsStr::new("--quiet"),
                OsStr::new("--exact"),
                OsStr::new("training_cmd::tests::subprocess_cli_entry_worker"),
                OsStr::new("--nocapture"),
            ],
            Some(&self.root),
            &extra_env,
            Duration::from_secs(30),
        )
        .expect("spawn CLI subprocess worker");
        if let Some((_, body)) = res.stdout.split_once(SUBPROCESS_STDOUT_MARKER) {
            res.stdout = body.to_string();
        }
        res
    }
}

const SUBPROCESS_STDOUT_MARKER: &str = "<<<DELTA_CLI_SUBPROCESS_STDOUT_START>>>\n";

impl Drop for CliTrainingSandbox {
    fn drop(&mut self) {
        for _ in 0..40 {
            if fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Subprocess entrypoint exercising the real `main.rs` `dispatch_cli` path under OS
/// `Stdio::null()` (`stdin().is_terminal() == false`) via `run_quiet_command_with_timeout`.
#[test]
fn subprocess_cli_entry_worker() {
    let Ok(args_json) = std::env::var("DELTA_M4_CLI_SUBPROCESS_ARGS_JSON") else {
        return;
    };
    apply_process_quiet_guards();
    print!("{SUBPROCESS_STDOUT_MARKER}");
    let _ = std::io::stdout().flush();
    let cli_args: Vec<String> =
        serde_json::from_str(&args_json).expect("valid DELTA_M4_CLI_SUBPROCESS_ARGS_JSON");
    let code = match crate::dispatch_cli(&cli_args) {
        Ok(_) => 0,
        Err(err) => {
            let formatted = to_sanitized_pretty_json(&err).unwrap_or_else(|_| {
                sanitize_terminal_output(&format!(
                    r#"{{"code":"{}","message":"{}"}}"#,
                    err.code, err.message
                ))
            });
            eprintln!("{}", sanitize_terminal_output(&formatted));
            1
        }
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

fn trading_days_from(venue: &str, start: NaiveDate, count: usize) -> Vec<NaiveDate> {
    let mut out = Vec::with_capacity(count);
    let mut cur = start;
    while out.len() < count {
        let sess = query_calendar_session(venue, cur);
        if matches!(
            sess.kind,
            CalendarSessionKind::Regular
                | CalendarSessionKind::EarlyClose
                | CalendarSessionKind::Crypto24x7
        ) {
            out.push(cur);
        }
        cur = cur.checked_add_days(Days::new(1)).unwrap();
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn make_equity_bar_ohlcv(
    instrument_id: &str,
    d: NaiveDate,
    open_px: &str,
    high_px: &str,
    low_px: &str,
    close_px: &str,
    vol: &str,
    dsv: i64,
) -> BarRecord {
    let sess = query_calendar_session("XNAS", d);
    BarRecord {
        id: format!("BAR:{instrument_id}:1d:raw:yahoo:{d}:{dsv}"),
        instrument_id: instrument_id.to_string(),
        timeframe: "1d".to_string(),
        session: sess.kind.as_str().to_string(),
        session_date: d,
        open_at: sess.open_at.expect("equity open_at"),
        close_at: sess.close_at.expect("equity close_at"),
        open: parse_decimal_lossless(open_px).unwrap(),
        high: parse_decimal_lossless(high_px).unwrap(),
        low: parse_decimal_lossless(low_px).unwrap(),
        close: parse_decimal_lossless(close_px).unwrap(),
        volume: parse_decimal_lossless(vol).unwrap(),
        source: "yahoo".to_string(),
        adjustment: AdjustmentKind::Raw,
        is_final: true,
        valid_from_version: dsv,
        valid_to_version: None,
        raw_payload_hash: None,
    }
}

fn make_equity_bar(instrument_id: &str, d: NaiveDate, close_px: &str, dsv: i64) -> BarRecord {
    let px = parse_decimal_lossless(close_px).unwrap();
    let hi = to_canonical_text(px + Decimal::from(3));
    let lo = to_canonical_text((px - Decimal::from(2)).max(Decimal::ONE));
    make_equity_bar_ohlcv(
        instrument_id,
        d,
        close_px,
        &hi,
        &lo,
        close_px,
        "120000",
        dsv,
    )
}

fn assert_workspace_snapshot_parity(
    cli_json: &str,
    cli: &TrainingWorkspaceSnapshotDto,
    ui: &TrainingWorkspaceSnapshotDto,
) {
    assert_eq!(cli.run, ui.run);
    assert_eq!(cli.account, ui.account);
    assert_eq!(cli.position, ui.position);
    assert_eq!(cli.open_orders, ui.open_orders);
    assert_eq!(cli.all_orders, ui.all_orders);
    assert_eq!(cli.fills, ui.fills);
    assert_eq!(cli.notes, ui.notes);
    assert_eq!(cli.self_evaluation, ui.self_evaluation);
    assert_eq!(cli.viewport, ui.viewport);
    assert_eq!(cli.bar_labels, ui.bar_labels);
    assert_eq!(cli.chart_slice.instrument_id, ui.chart_slice.instrument_id);
    assert_eq!(cli.chart_slice.venue, ui.chart_slice.venue);
    assert_eq!(cli.chart_slice.bars, ui.chart_slice.bars);
    assert_eq!(
        cli.chart_slice
            .markers
            .iter()
            .map(|m| &m.marker)
            .collect::<Vec<_>>(),
        ui.chart_slice
            .markers
            .iter()
            .map(|m| &m.marker)
            .collect::<Vec<_>>()
    );
    assert_eq!(cli_json, to_sanitized_pretty_json(ui).unwrap());
}

// ========================================================================
// 1. M4-A-15: CLI and UI (`AppHost`) DTO Parity on the Same Run
// ========================================================================

#[test]
fn test_m4_a15_cli_and_ui_dto_parity_on_same_run() {
    let sandbox = CliTrainingSandbox::new("a15_parity");
    let ledger_rev_before: i64;
    let seeded_dsv: i64;

    {
        let mut svc = sandbox.open_service();
        // Seed real ledger events first so we also prove training CLI leaves ledger_revision untouched
        let _ = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let aapl = svc
            .register_market_instrument("NASDAQ", "AAPL", false)
            .unwrap();

        let mut store = SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").ok();
        drop(store.take());

        // Seed 60 continuous XNAS trading sessions in 2024 with deterministic trajectory
        // designed to trigger entry fill, bracket TP fill, limit fill, and ending open position.
        let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(), 60);
        let mut bars = Vec::with_capacity(days.len());
        // Allocate a fresh published dataset_version via sync/SQLite before CLI run
        drop(svc);

        let mut direct_store =
            SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
        ledger_rev_before = direct_store
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        seeded_dsv = direct_store
            .create_dataset_version("m4-a15-fixture", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;

        // Warmup (10) + History (25) = indices 0..35; Training (12) = indices 35..47; Reveal (6) = 47..53
        for (idx, &d) in days.iter().enumerate() {
            let bar = match idx {
                // Step 1 (idx 35): Market buy + bracket fills around 150
                35 => make_equity_bar_ohlcv(
                    &aapl, d, "150.00", "153.00", "149.00", "152.00", "150000", seeded_dsv,
                ),
                // Step 2 (idx 36): Inside bar; low limit buy @ 90 stays open for mid-run check & cancel
                36 => make_equity_bar_ohlcv(
                    &aapl, d, "152.50", "156.00", "151.00", "155.00", "150000", seeded_dsv,
                ),
                // Step 3 (idx 37): Surge hits attached bracket take-profit @ 175 -> closes Trade Group #1!
                37 => make_equity_bar_ohlcv(
                    &aapl, d, "160.00", "178.00", "159.00", "174.00", "220000", seeded_dsv,
                ),
                // Step 4 (idx 38): Pullback fills limit buy of 6 shares @ 168
                38 => make_equity_bar_ohlcv(
                    &aapl, d, "170.00", "172.00", "165.00", "169.00", "180000", seeded_dsv,
                ),
                // Step 5 (idx 39): Rally leaves open position in profit at finish
                39 => make_equity_bar_ohlcv(
                    &aapl, d, "171.00", "182.00", "170.00", "180.00", "190000", seeded_dsv,
                ),
                _ => make_equity_bar(&aapl, d, &(145 + (idx % 9) as i64).to_string(), seeded_dsv),
            };
            bars.push(bar);
        }
        direct_store
            .commit_bars_and_checkpoint(&bars, seeded_dsv, None)
            .unwrap();
        direct_store.publish_dataset_version(seeded_dsv).unwrap();
    }

    // 1. Inspect fee-profiles via CLI
    let fps_out = sandbox.run_cli_ok(["fee-profiles"]);
    let fps: Vec<FeeProfileDto> = serde_json::from_str(&fps_out).unwrap();
    assert!(fps
        .iter()
        .any(|f| f.record.id == BUILTIN_FEE_US_EQUITY_PER_SHARE_ID));

    // 2. Create training run through `delta-cli training create` pinned to exact segment
    let start_from =
        trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(), 60)[10];
    let end_to = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(), 60)[52];
    let create_out = sandbox.run_cli_ok([
        "create",
        "--run-id",
        "TRN-A15-PARITY",
        "--market",
        "us_equity_etf",
        "--instrument",
        "NASDAQ:AAPL",
        "--from",
        &start_from.to_string(),
        "--to",
        &end_to.to_string(),
        "--warmup-bars",
        "10",
        "--history-bars",
        "25",
        "--training-bars",
        "12",
        "--reveal-bars",
        "6",
        "--seed",
        "20261112",
        "--dataset-version",
        &seeded_dsv.to_string(),
        "--initial-cash",
        "100000",
        "--fee-profile",
        BUILTIN_FEE_US_EQUITY_PER_SHARE_ID,
        "--hidden",
        "--start",
    ]);
    let created: TrainingRunSummaryDto = serde_json::from_str(&create_out).unwrap();
    assert_eq!(created.run_id, "TRN-A15-PARITY");
    assert_eq!(created.status, SimRunStatus::Active);
    assert_eq!(created.dataset_version, seeded_dsv);
    assert_eq!(created.engine_version, delta_app::SIM_ENGINE_VERSION_M5);
    assert_eq!(
        created.practice_account_id, None,
        "--initial-cash keeps independent capital"
    );
    assert_eq!(
        created.fee_profile_version_id,
        BUILTIN_FEE_US_EQUITY_PER_SHARE_ID
    );
    assert!(created.is_identity_masked);

    // 3. Preview + Submit Market Buy (10 shares) with attached TP=175 / SL=135 bracket
    let preview_out = sandbox.run_cli_ok([
        "order",
        "--run-id",
        "TRN-A15-PARITY",
        "--preview",
        "--type",
        "market",
        "--side",
        "buy",
        "--tif",
        "day",
        "--qty",
        "10",
        "--take-profit",
        "175",
        "--stop-loss",
        "135",
    ]);
    let preview_dto: TrainingOrderPreviewDto = serde_json::from_str(&preview_out).unwrap();
    assert!(preview_dto.valid);
    assert!(preview_dto.preview.is_some());

    let submit_entry_out = sandbox.run_cli_ok([
        "order",
        "market",
        "--run-id",
        "TRN-A15-PARITY",
        "--side",
        "buy",
        "--tif",
        "day",
        "--qty",
        "10",
        "--take-profit",
        "175",
        "--stop-loss",
        "135",
    ]);
    let entry_rec: TrainingStepReceiptDto = serde_json::from_str(&submit_entry_out).unwrap();
    assert_eq!(entry_rec.seq, 1);

    // Advance step 1 -> fills entry market buy & activates TP/SL bracket legs
    let adv1_out = sandbox.run_cli_ok(["advance", "--run-id", "TRN-A15-PARITY", "--steps", "1"]);
    let adv1: TrainingStepReceiptDto = serde_json::from_str(&adv1_out).unwrap();
    assert_eq!(adv1.run.current_step_index, 1);

    // Submit GTC Limit Buy @ 90 (stays open on step 2 so we can verify open-order parity & cancel)
    let _ = sandbox.run_cli_ok([
        "order",
        "--run-id",
        "TRN-A15-PARITY",
        "--type",
        "limit",
        "--side",
        "buy",
        "--tif",
        "gtc",
        "--qty",
        "5",
        "--limit-price",
        "90",
    ]);
    let _ = sandbox.run_cli_ok(["advance", "--run-id", "TRN-A15-PARITY"]);

    // 4. MID-RUN PARITY CHECK: compare `delta-cli training show` JSON against `AppHost` desktop DTO
    let cli_mid_show_out = sandbox.run_cli_ok(["show", "--run-id", "TRN-A15-PARITY"]);
    let cli_mid_show: TrainingWorkspaceSnapshotDto =
        serde_json::from_str(&cli_mid_show_out).unwrap();
    {
        let host = sandbox.open_host();
        let ui_mid_show = host
            .get_training_workspace_snapshot("TRN-A15-PARITY", None, 320)
            .unwrap();
        assert!(cli_mid_show.position.is_some());
        assert_eq!(cli_mid_show.position, ui_mid_show.position);
        assert_eq!(
            cli_mid_show.account.cash_balance,
            ui_mid_show.account.cash_balance
        );
        assert_eq!(
            cli_mid_show.account.available_cash,
            ui_mid_show.account.available_cash
        );
        assert_eq!(
            cli_mid_show.account.reserved_cash,
            ui_mid_show.account.reserved_cash
        );
        assert_eq!(cli_mid_show.open_orders, ui_mid_show.open_orders);
        assert_eq!(cli_mid_show.all_orders, ui_mid_show.all_orders);
        assert_eq!(cli_mid_show.fills, ui_mid_show.fills);
        assert_workspace_snapshot_parity(&cli_mid_show_out, &cli_mid_show, &ui_mid_show);
    }

    // Cancel the open standalone GTC limit buy (`limit_price == 90`)
    let open_limit_id = cli_mid_show
        .open_orders
        .iter()
        .find(|o| o.side == OrderSide::Buy && o.order_type.limit_price() == Some(Decimal::from(90)))
        .expect("open limit buy @ 90")
        .order_id
        .clone();
    let cancel_out = sandbox.run_cli_ok([
        "cancel",
        "--run-id",
        "TRN-A15-PARITY",
        "--order-id",
        &open_limit_id,
    ]);
    let cancel_rec: TrainingStepReceiptDto = serde_json::from_str(&cancel_out).unwrap();
    assert!(cancel_rec.seq > adv1.seq);

    // Add training note linked to the filled entry order
    let entry_order_id = cli_mid_show.fills[0].order_id.clone();
    let note_out = sandbox.run_cli_ok([
        "note",
        "--run-id",
        "TRN-A15-PARITY",
        "--rationale",
        "按交易计划持有等待 175 止盈触发",
        "--emotion",
        "冷静",
        "--linked-order-id",
        &entry_order_id,
    ]);
    let note_dto: TrainingNoteDto = serde_json::from_str(&note_out).unwrap();
    assert_eq!(
        note_dto.linked_order_id.as_deref(),
        Some(entry_order_id.as_str())
    );

    // Advance step 3 -> hits bracket TP @ 175, closing Trade Group #1!
    let _ = sandbox.run_cli_ok(["advance", "--run-id", "TRN-A15-PARITY"]);

    // Submit Limit Buy of 6 shares @ 168 and advance 2 steps so we also hold an open position at finish
    let _ = sandbox.run_cli_ok([
        "order",
        "--run-id",
        "TRN-A15-PARITY",
        "--type",
        "limit",
        "--side",
        "buy",
        "--tif",
        "gtc",
        "--qty",
        "6",
        "--limit-price",
        "168",
    ]);
    let _ = sandbox.run_cli_ok(["advance", "--run-id", "TRN-A15-PARITY", "--steps", "2"]);

    // Pause -> Resume -> Finish via CLI
    let paused: TrainingRunSummaryDto =
        serde_json::from_str(&sandbox.run_cli_ok(["pause", "--run-id", "TRN-A15-PARITY"])).unwrap();
    assert_eq!(paused.status, SimRunStatus::Paused);
    let resumed: TrainingRunSummaryDto =
        serde_json::from_str(&sandbox.run_cli_ok(["resume", "--run-id", "TRN-A15-PARITY"]))
            .unwrap();
    assert_eq!(resumed.status, SimRunStatus::Active);

    let fin_out = sandbox.run_cli_ok(["finish", "--run-id", "TRN-A15-PARITY"]);
    let fin_rec: TrainingStepReceiptDto = serde_json::from_str(&fin_out).unwrap();
    assert_eq!(fin_rec.run.status, SimRunStatus::Finished);

    // 5. POST-FINISH PARITY CHECK: compare `delta-cli training show` & `training report`
    //    against `AppHost::get_training_workspace_snapshot` & `AppHost::get_training_report`!
    let cli_show_out = sandbox.run_cli_ok(["show", "--run-id", "TRN-A15-PARITY"]);
    let cli_show: TrainingWorkspaceSnapshotDto = serde_json::from_str(&cli_show_out).unwrap();

    let cli_report_out = sandbox.run_cli_ok(["report", "--run-id", "TRN-A15-PARITY"]);
    let cli_report: TrainingReportDto = serde_json::from_str(&cli_report_out).unwrap();

    {
        let host = sandbox.open_host();
        let ui_show = host
            .get_training_workspace_snapshot("TRN-A15-PARITY", None, 320)
            .unwrap();
        let ui_report = host.get_training_report("TRN-A15-PARITY").unwrap();

        // Exact field-by-field & whole-DTO equality (`M4-A-15`)
        assert!(cli_show.position.is_some());
        assert_eq!(
            cli_show.position.as_ref().unwrap().quantity,
            Decimal::from(6)
        );
        assert_eq!(cli_show.position, ui_show.position);
        assert_eq!(cli_show.account.cash_balance, ui_show.account.cash_balance);
        assert_eq!(
            cli_show.account.available_cash,
            ui_show.account.available_cash
        );
        assert_eq!(cli_show.account.total_equity, ui_show.account.total_equity);
        assert_eq!(
            cli_show.account.total_fees_quote,
            ui_show.account.total_fees_quote
        );
        assert_eq!(cli_show.all_orders, ui_show.all_orders);
        assert_eq!(cli_show.fills, ui_show.fills);
        assert_eq!(cli_show.notes, ui_show.notes);
        assert_eq!(cli_show.run.dataset_version, ui_show.run.dataset_version);
        assert_eq!(cli_show.run.engine_version, ui_show.run.engine_version);
        assert_eq!(
            cli_show.run.fee_profile_version_id,
            ui_show.run.fee_profile_version_id
        );
        assert_workspace_snapshot_parity(&cli_show_out, &cli_show, &ui_show);

        assert_eq!(cli_report.report.closed_trade_groups.len(), 1);
        assert_eq!(
            cli_report.report.plan_execution.status,
            PlanExecutionStatus::Computed
        );
        assert_eq!(
            cli_report.report.dataset_version,
            ui_report.report.dataset_version
        );
        assert_eq!(
            cli_report.report.engine_version,
            ui_report.report.engine_version
        );
        assert_eq!(
            cli_report.report.fee_profile_version_id,
            ui_report.report.fee_profile_version_id
        );
        assert!(!cli_report.report.limitations.items.is_empty());
        assert_eq!(cli_report.report.benchmark, ui_report.report.benchmark);
        assert_eq!(cli_report, ui_report);
        assert_eq!(
            cli_report_out,
            to_sanitized_pretty_json(&ui_report).unwrap()
        );
    }

    // 6. Replay (`scratch` & `checkpoint`), Retry, Archive Note, and Delete `--preview` via CLI
    let replay_scratch: TrainingReplayResultDto = serde_json::from_str(&sandbox.run_cli_ok([
        "replay",
        "--run-id",
        "TRN-A15-PARITY",
        "--mode",
        "scratch",
    ]))
    .unwrap();
    let replay_cp: TrainingReplayResultDto = serde_json::from_str(&sandbox.run_cli_ok([
        "replay",
        "--run-id",
        "TRN-A15-PARITY",
        "--mode",
        "checkpoint",
    ]))
    .unwrap();
    assert!(replay_scratch.matches_persisted_hashes);
    assert!(replay_cp.matches_persisted_hashes);
    assert_eq!(replay_scratch.final_state_hash, replay_cp.final_state_hash);
    assert_eq!(replay_scratch.report, cli_report);
    assert_eq!(replay_cp.report, cli_report);

    let retried: TrainingRunSummaryDto = serde_json::from_str(&sandbox.run_cli_ok([
        "retry",
        "--run-id",
        "TRN-A15-PARITY",
        "--new-run-id",
        "TRN-A15-CHILD",
    ]))
    .unwrap();
    assert_eq!(retried.run_id, "TRN-A15-CHILD");
    assert_eq!(retried.parent_run_id.as_deref(), Some("TRN-A15-PARITY"));

    // Archive note to journal after finish and verify real ledger_revision is completely untouched
    let archived: ArchiveTrainingNotesResultDto = serde_json::from_str(&sandbox.run_cli_ok([
        "note",
        "--run-id",
        "TRN-A15-PARITY",
        "--archive",
        &note_dto.note_id,
    ]))
    .unwrap();
    assert_eq!(archived.archived_notes.len(), 1);

    let del_preview: DeleteTrainingRunPreview = serde_json::from_str(&sandbox.run_cli_ok([
        "delete",
        "--run-id",
        "TRN-A15-PARITY",
        "--preview",
    ]))
    .unwrap();
    assert_eq!(del_preview.run_id, "TRN-A15-PARITY");
    assert_eq!(del_preview.child_run_ids, vec!["TRN-A15-CHILD".to_string()]);
    assert_eq!(del_preview.fill_count, 3);
    assert_eq!(del_preview.note_count, 1);
    assert_eq!(del_preview.archived_journal_note_count, 1);

    // Original run report & state remain completely unchanged after replay, retry, archive, and delete --preview
    let cli_report_after: TrainingReportDto =
        serde_json::from_str(&sandbox.run_cli_ok(["report", "--run-id", "TRN-A15-PARITY"]))
            .unwrap();
    assert_eq!(cli_report_after.report, cli_report.report);

    let store_check = SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
    let ledger_rev_after: i64 = store_check
        .conn()
        .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        ledger_rev_after, ledger_rev_before,
        "simulated training CLI must never alter real ledger_revision"
    );
}

// ========================================================================
// 2. Hidden Mode & Future Sentinel Isolation Across CLI JSON Outputs
// ========================================================================

#[test]
fn test_m4_cli_hidden_mode_and_future_sentinel_isolation() {
    let sandbox = CliTrainingSandbox::new("sentinel_hidden");
    let nvda_id: String;
    let dsv: i64;
    let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2021, 5, 3).unwrap(), 55);

    {
        let mut svc = sandbox.open_service();
        nvda_id = svc
            .register_market_instrument("NASDAQ", "NVDA", false)
            .unwrap();
        drop(svc);

        let mut store =
            SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
        dsv = store
            .create_dataset_version("m4-cli-sentinel", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let bars: Vec<BarRecord> = days
            .iter()
            .enumerate()
            .map(|(i, &d)| make_equity_bar(&nvda_id, d, &(220 + (i % 13) as i64).to_string(), dsv))
            .collect();
        store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();
        store.publish_dataset_version(dsv).unwrap();
    }

    // Warmup 10 (0..10) + History 20 (10..30) + Training 8 (30..38) + Reveal 6 (38..44); extra tail 44..55
    let _ = sandbox.run_cli_ok([
        "create",
        "--run-id",
        "TRN-CLI-HIDDEN",
        "--market",
        "us_equity_etf",
        "--instrument",
        "NVDA",
        "--from",
        &days[10].to_string(),
        "--to",
        &days[43].to_string(),
        "--warmup-bars",
        "10",
        "--history-bars",
        "20",
        "--training-bars",
        "8",
        "--reveal-bars",
        "6",
        "--seed",
        "909090",
        "--hidden",
        "--start",
    ]);

    // Overwrite ALL bars & CAs strictly after the current clock (leaving steps 1..2 clean first? No:
    // overwrite IMMEDIATELY right after the initial clock first, test CLI show/list/order, then restore
    // steps 1..8 + reveal 6 while keeping sentinels strictly after reveal_end_at AND after step 2!)
    {
        let store = SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
        let sess = store.get_training_session("TRN-CLI-HIDDEN").unwrap();
        let cur_vis = sess.visible_until;

        store
            .conn()
            .execute(
                "UPDATE bar
                 SET open = '999999999', high = '999999999', low = '999999999',
                     close = '999999999', volume = '888888888'
                 WHERE instrument_id = ?1 AND close_at > ?2",
                params![nvda_id, cur_vis.to_rfc3339()],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE bar SET open = 'POISON_FUTURE_BAR'
                 WHERE instrument_id = ?1 AND session_date = ?2",
                params![nvda_id, days[50].to_string()],
            )
            .unwrap();
        // Unfinalized bar before clock (`is_final = 0`)
        store
            .conn()
            .execute(
                "INSERT INTO bar (
                    id, instrument_id, timeframe, session, session_date, open_at, close_at,
                    open, high, low, close, volume, source, adjustment, is_final,
                    valid_from_version, valid_to_version, raw_payload_hash
                 ) VALUES (
                    'BAR:UNFINAL:CLI', ?1, '1d', 'intraday', '2021-05-20',
                    '2021-05-20T14:00:00Z', '2021-05-20T15:00:00Z',
                    '666666666', '666666666', '666666666', '666666666', '666666666',
                    'preview', 'raw', 0, ?2, NULL, NULL
                 )",
                params![nvda_id, dsv],
            )
            .unwrap();
        // Future corporate action split & dividend right after current clock
        let future_ex = days[31].to_string();
        store
            .conn()
            .execute(
                "INSERT INTO corporate_action (
                    id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
                 ) VALUES ('CA:CLI:SPLIT', ?1, ?2, ?3, ?3, '999999', NULL, NULL, 'yahoo', ?4, NULL)",
                params![nvda_id, CorporateActionType::Split.as_str(), future_ex, dsv],
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO corporate_action (
                    id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
                 ) VALUES ('CA:CLI:DIV', ?1, ?2, ?3, ?3, '777777', 'USD', NULL, 'yahoo', ?4, NULL)",
                params![
                    nvda_id,
                    CorporateActionType::CashDividend.as_str(),
                    future_ex,
                    dsv
                ],
            )
            .unwrap();
    }

    let assert_zero_sentinels_and_masked = |label: &str, json_str: &str| {
        for sentinel in [
            "999999999",
            "888888888",
            "777777",
            "666666666",
            "999999",
            "POISON_FUTURE_BAR",
        ] {
            assert!(
                !json_str.contains(sentinel),
                "{label} leaked sentinel '{sentinel}':\n{json_str}"
            );
        }
        for real_leak in ["NVDA", "NASDAQ:NVDA", "NASDAQ", "2021-"] {
            assert!(
                !json_str.contains(real_leak),
                "{label} leaked hidden identity/calendar date '{real_leak}':\n{json_str}"
            );
        }
    };

    let list_json = sandbox.run_cli_ok(["list"]);
    assert_zero_sentinels_and_masked("training list (hidden active)", &list_json);

    let show_json = sandbox.run_cli_ok(["show", "--run-id", "TRN-CLI-HIDDEN"]);
    assert_zero_sentinels_and_masked("training show (hidden active)", &show_json);
    let snap: TrainingWorkspaceSnapshotDto = serde_json::from_str(&show_json).unwrap();
    assert_eq!(snap.run.instrument_id, HIDDEN_INSTRUMENT_ID);
    assert!(snap.run.is_identity_masked);
    assert!(snap.run.visible_until.is_none());
    assert!(snap.run.history_start_date.is_none());
    assert!(snap.run.train_start_date.is_none());
    assert!(snap.run.train_end_date.is_none());
    for bar in &snap.chart_slice.bars {
        assert_eq!(bar.instrument_id, HIDDEN_INSTRUMENT_ID);
        assert_eq!(bar.session_date.year(), 1);
    }

    let order_prev_json = sandbox.run_cli_ok([
        "order",
        "--run-id",
        "TRN-CLI-HIDDEN",
        "--preview",
        "--type",
        "market",
        "--side",
        "buy",
        "--qty",
        "5",
    ]);
    assert_zero_sentinels_and_masked("training order --preview (hidden)", &order_prev_json);

    // Reveal before finish must be denied
    let premature_reveal = sandbox.run_cli_subprocess(["reveal", "--run-id", "TRN-CLI-HIDDEN"]);
    assert_ne!(premature_reveal.status_code, Some(0));
    assert!(premature_reveal
        .stderr
        .contains("REVEAL_BEFORE_FINISH_DENIED"));

    // Restore valid prices for training+reveal window (indices 30..44) while keeping
    // sentinels (`999999999`, `POISON_FUTURE_BAR`) strictly beyond `reveal_end_at` (indices 45..55)!
    {
        let store = SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
        store
            .conn()
            .execute(
                "DELETE FROM corporate_action WHERE id IN ('CA:CLI:SPLIT', 'CA:CLI:DIV')",
                [],
            )
            .unwrap();
        for (i, &d) in days[30..44].iter().enumerate() {
            let px = (225 + (i % 11) as i64).to_string();
            let hi = (228 + (i % 11) as i64).to_string();
            let lo = (223 + (i % 11) as i64).to_string();
            store
                .conn()
                .execute(
                    "UPDATE bar SET open = ?1, high = ?2, low = ?3, close = ?1, volume = '120000'
                     WHERE instrument_id = ?4 AND session_date = ?5",
                    params![px, hi, lo, nvda_id, d.to_string()],
                )
                .unwrap();
        }
    }

    let _ = sandbox.run_cli_ok(["advance", "--run-id", "TRN-CLI-HIDDEN", "--steps", "2"]);
    let fin_json = sandbox.run_cli_ok(["finish", "--run-id", "TRN-CLI-HIDDEN"]);
    assert_zero_sentinels_and_masked("training finish (before reveal)", &fin_json);

    let unrevealed_rep_json = sandbox.run_cli_ok(["report", "--run-id", "TRN-CLI-HIDDEN"]);
    assert_zero_sentinels_and_masked("training report (before reveal)", &unrevealed_rep_json);

    // Now explicitly reveal!
    let reveal_json = sandbox.run_cli_ok(["reveal", "--run-id", "TRN-CLI-HIDDEN"]);
    for sentinel in ["999999999", "888888888", "666666666", "POISON_FUTURE_BAR"] {
        assert!(
            !reveal_json.contains(sentinel),
            "reveal leaked post-reveal sentinel '{sentinel}'"
        );
    }
    let revealed: TrainingRevealDto = serde_json::from_str(&reveal_json).unwrap();
    assert_eq!(revealed.real_instrument_id, "NASDAQ:NVDA");
    assert_eq!(revealed.real_symbol, "NVDA");
    assert_eq!(revealed.venue, "NASDAQ");
    assert!(!revealed.run.is_identity_masked);
    assert!(revealed.run.revealed);
    assert_eq!(revealed.post_finish_bars.len(), 6);
    assert_eq!(revealed.train_start_date.year(), 2021);

    let show_after_reveal: TrainingWorkspaceSnapshotDto =
        serde_json::from_str(&sandbox.run_cli_ok(["show", "--run-id", "TRN-CLI-HIDDEN"])).unwrap();
    assert_eq!(show_after_reveal.run.instrument_id, "NASDAQ:NVDA");
    assert_eq!(show_after_reveal.run.display_symbol, "NVDA");
    assert!(!show_after_reveal.run.is_identity_masked);
}

// ========================================================================
// 3. Non-TTY `training delete` Fail-Fast & Control Character Escaping
// ========================================================================

#[test]
fn test_m4_cli_delete_non_tty_requires_yes_and_escapes_control_chars() {
    let sandbox = CliTrainingSandbox::new("delete_ctrl");
    let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(), 50);

    {
        let mut svc = sandbox.open_service();
        let msft = svc
            .register_market_instrument("NASDAQ", "MSFT", false)
            .unwrap();
        drop(svc);

        let mut store =
            SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
        let dsv = store
            .create_dataset_version("m4-cli-del", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let bars: Vec<BarRecord> = days
            .iter()
            .enumerate()
            .map(|(i, &d)| make_equity_bar(&msft, d, &(300 + (i % 10) as i64).to_string(), dsv))
            .collect();
        store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();
        store.publish_dataset_version(dsv).unwrap();
    }

    for run_id in ["TRN-DEL-TARGET", "TRN-DEL-KEEP"] {
        let _ = sandbox.run_cli_ok([
            "create",
            "--run-id",
            run_id,
            "--market",
            "us_equity_etf",
            "--history-bars",
            "15",
            "--training-bars",
            "10",
            "--reveal-bars",
            "5",
            "--seed",
            "12345",
            "--independent",
            "--start",
        ]);
    }

    // Add order, fill, and note containing BEL (`\x07`), ANSI CSI (`\x1b[31m`), and DEL (`\x7f`)
    let _ = sandbox.run_cli_ok([
        "order",
        "market",
        "--run-id",
        "TRN-DEL-TARGET",
        "--side",
        "buy",
        "--qty",
        "2",
    ]);
    let _ = sandbox.run_cli_ok(["advance", "--run-id", "TRN-DEL-TARGET"]);

    let raw_rationale = "Alert\x07Bell and \x1b[31mANSI_RED\x1b[0m plus \x7fDEL";
    let in_proc_note = match execute_training_cli(
        &[
            "training".to_string(),
            "note".to_string(),
            "--run-id".to_string(),
            "TRN-DEL-TARGET".to_string(),
            "--rationale".to_string(),
            raw_rationale.to_string(),
            "--emotion".to_string(),
            "警觉\x07".to_string(),
        ],
        Some(BuildChannel::Source),
        Some(&sandbox.data_dir()),
        false,
    )
    .unwrap()
    {
        TrainingCliOutcome::Json(s) => s,
        TrainingCliOutcome::HelpPrinted => panic!("expected JSON"),
    };
    assert!(!in_proc_note.contains('\x07'));
    assert!(!in_proc_note.contains('\x1b'));
    assert!(!in_proc_note.contains('\x7f'));
    assert!(in_proc_note.contains("\\u0007"));
    assert!(in_proc_note.contains("\\u001b"));
    assert!(in_proc_note.contains("\\u007f"));
    let parsed_note: TrainingNoteDto = serde_json::from_str(&in_proc_note).unwrap();
    assert_eq!(parsed_note.rationale, raw_rationale);

    let show_sub_out = sandbox.run_cli_ok(["show", "--run-id", "TRN-DEL-TARGET"]);
    assert!(!show_sub_out.contains('\x07'));
    assert!(!show_sub_out.contains('\x1b'));
    assert!(!show_sub_out.contains('\x7f'));
    assert!(show_sub_out.contains("\\u0007"));
    assert!(show_sub_out.contains("\\u001b"));
    let _: TrainingWorkspaceSnapshotDto = serde_json::from_str(&show_sub_out).unwrap();

    // Error output with control chars in unknown run_id is also sanitized on stderr
    let bad_err = sandbox.run_cli_subprocess(["show", "--run-id", "BAD\x07\x1b[31mID"]);
    assert_ne!(bad_err.status_code, Some(0));
    assert!(!bad_err.stderr.contains('\x07'));
    assert!(!bad_err.stderr.contains('\x1b'));

    // 1. Empty-stdin / non-TTY `training delete` WITHOUT `--yes` or `--preview` MUST fail fast!
    let t0 = Instant::now();
    let refused = sandbox.run_cli_subprocess(["delete", "--run-id", "TRN-DEL-TARGET"]);
    let elapsed = t0.elapsed();
    assert!(!refused.timed_out);
    assert_ne!(refused.status_code, Some(0));
    assert!(
        elapsed < Duration::from_secs(3),
        "non-TTY delete without --yes took {:?}, expected prompt fail-fast",
        elapsed
    );
    assert!(
        refused.stderr.contains("confirmation_required_non_tty"),
        "expected confirmation_required_non_tty in stderr, got:\n{}",
        refused.stderr
    );

    // 2. `--preview` returns counts and leaves `TRN-DEL-TARGET` completely intact
    let preview_out = sandbox.run_cli_ok(["delete", "--run-id", "TRN-DEL-TARGET", "--preview"]);
    let preview: DeleteTrainingRunPreview = serde_json::from_str(&preview_out).unwrap();
    assert_eq!(preview.run_id, "TRN-DEL-TARGET");
    assert_eq!(preview.order_count, 1);
    assert_eq!(preview.fill_count, 1);
    assert_eq!(preview.note_count, 1);

    let still_there: TrainingWorkspaceSnapshotDto =
        serde_json::from_str(&sandbox.run_cli_ok(["show", "--run-id", "TRN-DEL-TARGET"])).unwrap();
    assert_eq!(still_there.fills.len(), 1);

    // 3. `--yes` deletes ONLY `TRN-DEL-TARGET`, leaving `TRN-DEL-KEEP` untouched!
    let deleted_out = sandbox.run_cli_ok(["delete", "--run-id", "TRN-DEL-TARGET", "--yes"]);
    let deleted: DeleteTrainingRunPreview = serde_json::from_str(&deleted_out).unwrap();
    assert_eq!(deleted, preview);

    let target_gone = sandbox.run_cli_subprocess(["show", "--run-id", "TRN-DEL-TARGET"]);
    assert_ne!(target_gone.status_code, Some(0));
    assert!(target_gone.stderr.contains("RUN_NOT_FOUND"));

    let remaining_list: Vec<TrainingRunSummaryDto> =
        serde_json::from_str(&sandbox.run_cli_ok(["list"])).unwrap();
    assert_eq!(remaining_list.len(), 1);
    assert_eq!(remaining_list[0].run_id, "TRN-DEL-KEEP");
}

// ========================================================================
// 4. Lifecycle (`prepare-only` -> `redraw` -> `start`), OCO/Stop Orders,
//    Tick/Lot Rejections, Self-Eval & Evidence Open
// ========================================================================

#[test]
fn test_m4_cli_lifecycle_oco_stop_rejections_and_evidence() {
    let sandbox = CliTrainingSandbox::new("lifecycle_oco");
    let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 7, 1).unwrap(), 65);

    {
        let mut svc = sandbox.open_service();
        let qqq = svc
            .register_market_instrument("NASDAQ", "QQQ", true)
            .unwrap();
        drop(svc);

        let mut store =
            SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
        let dsv = store
            .create_dataset_version("m4-cli-oco", "public_open", "{}", "exact", "exact")
            .unwrap()
            .version_num;
        let bars: Vec<BarRecord> = days
            .iter()
            .enumerate()
            .map(|(i, &d)| make_equity_bar(&qqq, d, &(400 + (i % 15) as i64).to_string(), dsv))
            .collect();
        store.commit_bars_and_checkpoint(&bars, dsv, None).unwrap();
        store.publish_dataset_version(dsv).unwrap();
    }

    // Create with `--prepare-only`, redraw before start, then start, and verify redraw after start is refused
    let prep: TrainingRunSummaryDto = serde_json::from_str(&sandbox.run_cli_ok([
        "create",
        "--run-id",
        "TRN-CLI-LIFE",
        "--market",
        "us_equity_etf",
        "--history-bars",
        "15",
        "--training-bars",
        "12",
        "--reveal-bars",
        "5",
        "--seed",
        "77",
        "--no-hidden",
        "--prepare-only",
    ]))
    .unwrap();
    assert_eq!(prep.status, SimRunStatus::Preparing);
    assert!(!prep.hidden_mode);

    let redrawn: TrainingRunSummaryDto =
        serde_json::from_str(&sandbox.run_cli_ok(["redraw", "--run-id", "TRN-CLI-LIFE"])).unwrap();
    assert_eq!(redrawn.redraw_count, 1);

    let started: TrainingRunSummaryDto =
        serde_json::from_str(&sandbox.run_cli_ok(["start", "--run-id", "TRN-CLI-LIFE"])).unwrap();
    assert_eq!(started.status, SimRunStatus::Active);

    let redraw_after = sandbox.run_cli_subprocess(["redraw", "--run-id", "TRN-CLI-LIFE"]);
    assert_ne!(redraw_after.status_code, Some(0));
    assert!(redraw_after.stderr.contains("REDRAW_FORBIDDEN_AFTER_START"));

    // Invalid lot size (`0.5` shares on US equity `lot_size = 1`) preview returns nearest valid 1
    let bad_lot_prev: TrainingOrderPreviewDto = serde_json::from_str(&sandbox.run_cli_ok([
        "order",
        "--run-id",
        "TRN-CLI-LIFE",
        "--preview",
        "--type",
        "market",
        "--side",
        "buy",
        "--qty",
        "1.5",
    ]))
    .unwrap();
    assert!(!bad_lot_prev.valid);
    let rej = bad_lot_prev.rejection.unwrap();
    assert_eq!(rej.code, SimRejectCode::InvalidLotSize);
    assert_eq!(rej.nearest_lower, Some(Decimal::ONE));
    assert_eq!(rej.nearest_upper, Some(Decimal::from(2)));

    // Submit stop-market buy above current price or market buy -> advance -> submit OCO exit
    let _ = sandbox.run_cli_ok([
        "order",
        "market",
        "--run-id",
        "TRN-CLI-LIFE",
        "--side",
        "buy",
        "--qty",
        "4",
    ]);
    let _ = sandbox.run_cli_ok(["advance", "--run-id", "TRN-CLI-LIFE"]);

    let oco_rec: TrainingStepReceiptDto = serde_json::from_str(&sandbox.run_cli_ok([
        "order",
        "oco",
        "--run-id",
        "TRN-CLI-LIFE",
        "--qty",
        "4",
        "--take-profit",
        "450",
        "--stop-loss",
        "350",
    ]))
    .unwrap();
    assert_eq!(oco_rec.seq, 3);

    let snap_limited: TrainingWorkspaceSnapshotDto = serde_json::from_str(&sandbox.run_cli_ok([
        "show",
        "--run-id",
        "TRN-CLI-LIFE",
        "--bars-limit",
        "8",
    ]))
    .unwrap();
    assert_eq!(snap_limited.chart_slice.bars.len(), 8);
    assert_eq!(
        snap_limited
            .open_orders
            .iter()
            .filter(|o| o.status == OrderStatus::Accepted || o.status == OrderStatus::Active)
            .count(),
        2
    );

    // Finish + Reveal via `finish --reveal`, save self-eval, and open simulated fill evidence
    let fin_rev: TrainingRevealDto = serde_json::from_str(&sandbox.run_cli_ok([
        "finish",
        "--run-id",
        "TRN-CLI-LIFE",
        "--reveal",
    ]))
    .unwrap();
    assert_eq!(fin_rev.run.status, SimRunStatus::Finished);
    assert!(fin_rev.run.revealed);

    let self_eval: UserSelfEvaluationDto = serde_json::from_str(&sandbox.run_cli_ok([
        "self-eval",
        "--run-id",
        "TRN-CLI-LIFE",
        "--decision-score",
        "4",
        "--discipline-score",
        "5",
        "--summary",
        "严格设置 OCO 止盈止损",
    ]))
    .unwrap();
    assert_eq!(self_eval.decision_score, 4);

    let fill_id = snap_limited.fills[0].fill_id.clone();
    let ev_val: serde_json::Value = serde_json::from_str(&sandbox.run_cli_ok([
        "evidence",
        "--run-id",
        "TRN-CLI-LIFE",
        "--evidence-id",
        &fill_id,
    ]))
    .unwrap();
    assert_eq!(ev_val["kind"], "sim_fill");
}

/// `S-55` / `M5-W06`: the M5 CLI commands drive the same service as the desktop; the
/// account, analysis and run numbers printed by the CLI equal the host DTOs.
#[test]
fn test_m5_cli_practice_account_quick_fill_undo_drawings_and_parity() {
    use chrono::TimeZone;
    use delta_app::market_fixture::MarketFixtureServer;
    use delta_app::{PracticeAccountDto, TrainingDrawingDto, TrainingHistoryRowDto};
    let sandbox = CliTrainingSandbox::new("m5");
    let fixture =
        MarketFixtureServer::start(Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap()).unwrap();
    let mut prep = vec![
        "prepare-data".to_string(),
        "--timeframe".into(),
        "1d".into(),
    ];
    for (k, v) in fixture.base_urls() {
        prep.push("--base-url".into());
        prep.push(format!("{k}={v}"));
    }
    let out = sandbox.run_cli_ok(prep);
    let summary: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(summary["failed"], 0, "{out}");
    let status: serde_json::Value =
        serde_json::from_str(&sandbox.run_cli_ok(["prepare-data", "--status"])).unwrap();
    assert_eq!(status["needs_preparation"], false);

    let created: TrainingRunSummaryDto = serde_json::from_str(&sandbox.run_cli_ok([
        "create",
        "--market",
        "crypto_spot",
        "--instrument",
        "BINANCE:BTC/USDT",
        "--timeframe",
        "1d",
        "--amount",
        "60000",
        "--seed",
        "5",
        "--start",
    ]))
    .unwrap();
    assert_eq!(
        created.practice_account_id.as_deref(),
        Some("PRACTICE:USDT")
    );
    assert_eq!(created.initial_cash, Decimal::from(60_000));
    let run = created.run_id.clone();

    let r: TrainingStepReceiptDto = serde_json::from_str(&sandbox.run_cli_ok([
        "order",
        "market",
        "--run-id",
        &run,
        "--side",
        "buy",
        "--qty",
        "0.01",
        "--fill-rule",
        "visible-close",
    ]))
    .unwrap();
    assert!(r
        .events
        .iter()
        .any(|e| matches!(e, delta_app::SimEvent::OrderFilled { .. })));
    let _ = sandbox.run_cli_ok(["undo", "--run-id", &run]);
    let drawing: TrainingDrawingDto = serde_json::from_str(&sandbox.run_cli_ok([
        "drawing",
        "add",
        "--run-id",
        &run,
        "--kind",
        "horizontal",
        "--anchor",
        "3:50000",
    ]))
    .unwrap();
    let listed: Vec<TrainingDrawingDto> =
        serde_json::from_str(&sandbox.run_cli_ok(["drawing", "list", "--run-id", &run])).unwrap();
    assert_eq!(listed, vec![drawing.clone()]);
    let _ = sandbox.run_cli_ok([
        "drawing",
        "delete",
        "--run-id",
        &run,
        "--drawing-id",
        &drawing.drawing_id,
    ]);
    // Withdrawal above the available balance is refused through the CLI too.
    let refused = sandbox.run_cli_subprocess([
        "account",
        "transfer",
        "--currency",
        "USDT",
        "--withdraw",
        "40001",
    ]);
    assert_ne!(refused.status_code, Some(0));
    assert!(
        refused.stderr.contains("WITHDRAWAL_EXCEEDS_AVAILABLE"),
        "{}",
        refused.stderr
    );
    let _ = sandbox.run_cli_ok(["abandon", "--run-id", &run]);

    let cli_account: PracticeAccountDto =
        serde_json::from_str(&sandbox.run_cli_ok(["account", "show", "--currency", "USDT"]))
            .unwrap();
    let cli_analysis: serde_json::Value =
        serde_json::from_str(&sandbox.run_cli_ok(["account", "analysis", "--currency", "USDT"]))
            .unwrap();
    let cli_history: Vec<TrainingHistoryRowDto> =
        serde_json::from_str(&sandbox.run_cli_ok(["history"])).unwrap();
    let host = sandbox.open_host();
    assert_eq!(host.show_practice_account("USDT").unwrap(), cli_account);
    assert_eq!(
        serde_json::to_value(host.practice_account_analysis("USDT").unwrap()).unwrap(),
        cli_analysis
    );
    assert_eq!(host.training_history().unwrap(), cli_history);
    assert_eq!(
        cli_account.available,
        Decimal::from(100_000),
        "undo + abandon at cost"
    );
    assert_eq!(cli_history[0].status, SimRunStatus::Abandoned);
}

#[test]
fn test_cli_strict_flag_validation_and_no_premature_db_creation() {
    let sandbox = CliTrainingSandbox::new("strict_flags");
    let fresh_dir = sandbox.root.join("never-created-db");
    let s = |items: &[&str]| -> Vec<String> { items.iter().map(|v| (*v).to_string()).collect() };

    // Unknown training subcommand must fail WITHOUT initializing a SQLite DB on disk.
    let err = execute_training_cli(
        &s(&["training", "bogus"]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "invalid_subcommand");
    assert!(!fresh_dir.exists(), "invalid subcommand created data_dir");

    // `self-eval` with `--summary` but no `--decision-score` must error instead of silent GET.
    let err = execute_training_cli(
        &s(&[
            "training",
            "self-eval",
            "--run-id",
            "R1",
            "--summary",
            "note",
        ]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "missing_arg");
    assert!(!fresh_dir.exists());

    // `fee-profiles save` combining `--json` and individual field flags must error.
    let err = execute_training_cli(
        &s(&[
            "training",
            "fee-profiles",
            "save",
            "--json",
            "{}",
            "--name",
            "x",
        ]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "invalid_arg");

    // `show` with `--visible-start >= --visible-end` must error.
    let err = execute_training_cli(
        &s(&[
            "training",
            "show",
            "--run-id",
            "R1",
            "--visible-start",
            "50",
            "--visible-end",
            "20",
        ]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "invalid_arg");

    // `order oco` rejects `--fill-rule`.
    let err = execute_training_cli(
        &s(&[
            "training",
            "order",
            "oco",
            "--run-id",
            "R1",
            "--qty",
            "1",
            "--take-profit",
            "120",
            "--stop-loss",
            "80",
            "--fill-rule",
            "visible-close",
        ]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "INVALID_ORDER_PARAMS");

    // `note --archive` rejects `--linked-order-id`.
    let err = execute_training_cli(
        &s(&[
            "training",
            "note",
            "--run-id",
            "R1",
            "--archive",
            "--linked-order-id",
            "O1",
        ]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "invalid_arg");

    // `account transfer --deposit bad` names `--deposit`, not `--amount`.
    let err = execute_training_cli(
        &s(&[
            "training",
            "account",
            "transfer",
            "--deposit",
            "not-a-number",
        ]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert!(err.message.contains("--deposit"), "{}", err.message);

    // `prepare-data --status` rejects incompatible flags.
    let err = execute_training_cli(
        &s(&["training", "prepare-data", "--status", "--timeframe", "1d"]),
        Some(BuildChannel::Source),
        Some(&fresh_dir),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, "invalid_arg");

    // Top-level `library delete --yes --data-dir=<path>` parses `--data-dir=` cleanly and NEVER
    // touches the default library (missing `--data-dir` value errors cleanly too).
    let err = crate::dispatch_cli(&s(&["library", "delete", "--yes", "--data-dir"])).unwrap_err();
    assert_eq!(err.code, "missing_arg");
    let err = crate::dispatch_cli(&s(&[
        "library",
        "delete",
        "--yes",
        "--datadir",
        &fresh_dir.display().to_string(),
    ]))
    .unwrap_err();
    assert_eq!(err.code, "unknown_flag");
}
