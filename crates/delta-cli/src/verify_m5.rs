//! `verify-stage --stage M5 --plan-version 1.0 [--ui-smoke] [--ui-bench] [--live-data]` (`S-57`).
//!
//! Executes the M5 mechanical gates (docs, diff, fmt, clippy, workspace tests, dependency audit),
//! the architecture/hygiene check, an in-gate cross-process `delta-cli training` probe against the
//! loopback market fixture (prepare-data, practice account, quick fill, modify, undo, advance and
//! three-way replay — `M5-K03`), maps every automated case of `docs/delivery/m5-stage-cases.json`
//! to captured test results, optionally runs the release hidden-window `--m5-ui-smoke` and the
//! `--m5-ui-bench` continuous-playback benchmark (`M5-K02`), optionally prepares real public BTC
//! data into a temporary library (`--live-data`, `M5-K01` / `M5-L-01`, declared separately from
//! recorded fixtures), and closes with quietness/data isolation (`D-25`, `D-26`) and
//! same-fingerprint `M1`..`M4` regression verification (`M5-A-16`).

use super::{
    verify::{
        directory_snapshot, failure, owned_test_directories, record_tests, source_fingerprint,
    },
    verify_m4::M4QuietnessMonitor,
    CaseResultRecord, PocResultRecord, VerifyStageReport,
};
use chrono::{TimeZone, Utc};
use delta_app::{
    count_visible_windows_for_current_process, current_process_priority,
    market_fixture::MarketFixtureServer, observed_window_details, resolve_app_paths,
    resolve_user_documents_root, run_quiet_command, run_quiet_command_with_timeout,
    sanitize_terminal_output, AppHost, AppService, BuildChannel, ErrorEnvelope, OrderStatus,
    PracticeAccountDto, PrepareTrainingDataRequest, PrepareTrainingDataSummaryDto, QuietJobObject,
    ResolvedAppPaths, SimOrderType, TrainingReplayMode, TrainingReplayResultDto,
    TrainingRunSummaryDto, TrainingWorkspaceSnapshotDto, EXECUTION_SPEED_OPT_OUT,
};
use delta_data::{CandidatePoolQuery, CandidateQualityFilter, SqliteStore};
use delta_desktop::{M5_K02_INTERVAL_P95_BUDGET_MS, M5_K02_STEP_P95_BUDGET_MS};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

const STAGE: &str = "M5";
const PLAN_VERSION: &str = "1.0";
/// `M5-K01` limits on real public sources.
const K01_FIRST_PREPARE_LIMIT_S: f64 = 900.0;
const K01_INCREMENTAL_LIMIT_S: f64 = 30.0;
const K01_POOL_LIMIT_S: f64 = 2.0;

const M5_CASE_IDS: [&str; 19] = [
    "M5-A-01", "M5-A-02", "M5-A-03", "M5-A-04", "M5-A-05", "M5-A-06", "M5-A-07", "M5-A-08",
    "M5-A-09", "M5-A-10", "M5-A-11", "M5-A-12", "M5-A-13", "M5-A-14", "M5-A-15", "M5-A-16",
    "M5-A-17", "M5-L-01", "M5-M-01",
];

fn ensure(condition: bool, message: impl std::fmt::Display) -> Result<(), ErrorEnvelope> {
    if condition {
        Ok(())
    } else {
        Err(failure("M5_CHECK", message))
    }
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn temp_paths(
    probe_root: &Path,
    data_dir: &Path,
    tag: &str,
) -> Result<ResolvedAppPaths, ErrorEnvelope> {
    let mut paths = resolve_app_paths(Some(BuildChannel::Source), Some(data_dir))
        .map_err(|e| failure("m5_paths", e))?;
    paths.documents_dir = probe_root.join("docs");
    paths.default_export_dir = paths.documents_dir.join("exports");
    paths.default_backup_dir = paths.documents_dir.join("backups");
    paths.credential_prefix = format!("delta-test-m5-{tag}-{}", std::process::id());
    Ok(paths)
}

// ---------------------------------------------------------------------------
// Independent validation of the release UI smoke and the `M5-K02` raw samples
// ---------------------------------------------------------------------------

/// Nearest-rank percentile in milliseconds over microsecond samples (same rule as the bench).
fn percentile_ms(samples_us: &[u64], percent: usize) -> f64 {
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    sorted
        .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
        .map_or(0.0, |us| *us as f64 / 1000.0)
}

pub(crate) fn validate_m5_ui_smoke(smoke: &Value) -> Result<(), String> {
    let checks: [(&str, bool); 10] = [
        ("status == passed", smoke["status"] == "passed"),
        (
            "build_profile == release",
            smoke["build_profile"] == "release",
        ),
        (
            "native_window_created",
            smoke["native_window_created"] == true,
        ),
        ("visible_windows == 0", smoke["visible_windows"] == 0),
        ("focus_stolen == false", smoke["focus_stolen"] == false),
        ("practice_account", smoke["practice_account"] == true),
        (
            "quick_fill_then_undo",
            smoke["quick_fill_then_undo"] == true,
        ),
        (
            "played_steps > 0",
            smoke["played_steps"].as_i64().is_some_and(|n| n > 0),
        ),
        (
            "chart_paints > 0",
            smoke["chart_paints"].as_u64().is_some_and(|n| n > 0),
        ),
        (
            "finished and revealed",
            smoke["finished_status"] == "Finished"
                && smoke["revealed_instrument"] == "BINANCE:BTC/USDT",
        ),
    ];
    match checks.iter().find(|(_, ok)| !ok) {
        Some((name, _)) => Err(format!("m5 ui smoke check failed: {name} ({smoke})")),
        None => Ok(()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValidatedM5Bench {
    pub step_p50_ms: f64,
    pub step_p95_ms: f64,
    pub step_p99_ms: f64,
    pub interval_p50_ms: f64,
    pub interval_p95_ms: f64,
    pub interval_p99_ms: f64,
    pub measured_steps: usize,
}

pub(crate) fn validate_m5_playback_bench(bench: &Value) -> Result<ValidatedM5Bench, String> {
    if bench["status"] != "passed" || bench["passed"] != true {
        return Err("benchmark self-reported non-passing status".into());
    }
    if bench["build_profile"] != "release" {
        return Err(format!(
            "release build required, got {}",
            bench["build_profile"]
        ));
    }
    if bench["power_throttling"] != EXECUTION_SPEED_OPT_OUT {
        return Err(format!(
            "execution_speed_opt_out required, got {}",
            bench["power_throttling"]
        ));
    }
    if bench["visible_windows"] != 0 || bench["focus_stolen"] != false {
        return Err("benchmark showed a window or changed the foreground".into());
    }
    let at_least = |key: &str, min: u64| bench[key].as_u64().is_some_and(|n| n >= min);
    if bench["timeframe"] != "1h"
        || !at_least("visible_bars", 1000)
        || !at_least("drawings", 20)
        || !at_least("order_lines", 5)
        || bench["play_interval_ms"] != 150
    {
        return Err("benchmark scene is below the M5-K02 precondition (1h, 1,000 bars, 20 drawings, 5 lines, 150 ms)".into());
    }
    let samples = |key: &str| -> Vec<u64> {
        bench[key]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default()
    };
    let steps = samples("step_times_us");
    let intervals = samples("interval_times_us");
    if steps.len() < 300
        || intervals.len() < 300
        || bench["measured_steps"].as_u64() != Some(steps.len() as u64)
    {
        return Err(format!(
            "need >= 300 raw step and interval samples, got {} / {}",
            steps.len(),
            intervals.len()
        ));
    }
    if bench["native_chart_paints"].as_u64().unwrap_or(0) < steps.len() as u64 {
        return Err("every measured step must paint the native chart".into());
    }
    let v = ValidatedM5Bench {
        step_p50_ms: percentile_ms(&steps, 50),
        step_p95_ms: percentile_ms(&steps, 95),
        step_p99_ms: percentile_ms(&steps, 99),
        interval_p50_ms: percentile_ms(&intervals, 50),
        interval_p95_ms: percentile_ms(&intervals, 95),
        interval_p99_ms: percentile_ms(&intervals, 99),
        measured_steps: steps.len(),
    };
    if v.step_p95_ms > M5_K02_STEP_P95_BUDGET_MS {
        return Err(format!(
            "recomputed step p95 {:.3} ms > {M5_K02_STEP_P95_BUDGET_MS} ms",
            v.step_p95_ms
        ));
    }
    if v.interval_p95_ms > M5_K02_INTERVAL_P95_BUDGET_MS {
        return Err(format!(
            "recomputed interval p95 {:.3} ms > {M5_K02_INTERVAL_P95_BUDGET_MS} ms",
            v.interval_p95_ms
        ));
    }
    let reported = bench["step_p95_ms"].as_f64().unwrap_or(f64::NAN);
    if (reported - v.step_p95_ms).abs() > 0.01 {
        return Err(format!(
            "reported step p95 {reported} differs from recomputed {:.3}",
            v.step_p95_ms
        ));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// In-gate cross-process `delta-cli training` + `AppHost` probe (`M5-K03`)
// ---------------------------------------------------------------------------

struct M5ProbeEvidence {
    run_id: String,
    final_state_hash: String,
    checkpoint_started_seq: i64,
    replay_steps: usize,
    prepared_bars: usize,
    undo_restored: bool,
    elapsed_ms: u64,
}

fn run_in_gate_m5_probe(
    root: &Path,
    temp: &Path,
    gate_env: &BTreeMap<String, String>,
) -> Result<M5ProbeEvidence, ErrorEnvelope> {
    let started = Instant::now();
    let probe_root = temp.join("m5-probe");
    let data_dir = probe_root.join("data");
    fs::create_dir_all(&data_dir).map_err(|e| failure("m5_probe", e))?;
    let exe = env::current_exe().map_err(|e| failure("m5_probe_exe", e))?;
    let paths = temp_paths(&probe_root, &data_dir, "probe")?;
    let fixture = MarketFixtureServer::start(Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap())
        .map_err(|e| failure("m5_probe_fixture", e))?;

    let mut env_with_docs = gate_env.clone();
    env_with_docs.insert(
        "DELTA_DOCUMENTS_DIR".into(),
        probe_root.join("docs").display().to_string(),
    );
    let run_cli = |sub_args: &[&str]| -> Result<String, ErrorEnvelope> {
        let mut full: Vec<String> = vec!["training".into()];
        full.extend(sub_args.iter().map(|s| (*s).to_string()));
        full.push("--data-dir".into());
        full.push(data_dir.display().to_string());
        let res = run_quiet_command_with_timeout(
            exe.as_os_str(),
            full,
            Some(root),
            &env_with_docs,
            Duration::from_secs(120),
        )
        .map_err(|e| failure("m5_probe_cli", e))?;
        ensure(
            res.status_code == Some(0) && !res.timed_out,
            format!(
                "delta-cli training {sub_args:?} failed ({:?}): {}",
                res.status_code, res.stderr
            ),
        )?;
        Ok(res.stdout)
    };
    let parse = |raw: &str| -> Result<Value, ErrorEnvelope> {
        serde_json::from_str(raw).map_err(|e| failure("m5_probe_parse", e))
    };

    // 1. Empty library -> prepare BTC/USDT daily data from the recorded loopback fixture.
    let mut prep: Vec<String> = vec!["prepare-data".into(), "--timeframe".into(), "1d".into()];
    for (k, v) in fixture.base_urls() {
        prep.push("--base-url".into());
        prep.push(format!("{k}={v}"));
    }
    let prep_refs: Vec<&str> = prep.iter().map(String::as_str).collect();
    let summary: PrepareTrainingDataSummaryDto =
        serde_json::from_str(&run_cli(&prep_refs)?).map_err(|e| failure("m5_probe_parse", e))?;
    ensure(
        summary.failed == 0 && !summary.published_versions.is_empty(),
        format!("fixture prepare-data failed: {summary:?}"),
    )?;
    let prepared_bars = summary.batches.iter().map(|b| b.bars_committed).sum();
    let status = parse(&run_cli(&["prepare-data", "--status"])?)?;
    ensure(
        status["needs_preparation"] == false,
        format!("daily data not startable after preparation: {status}"),
    )?;

    // 2. Practice-account run with quick fill, limit order, modify, undo and advances.
    let created: TrainingRunSummaryDto = serde_json::from_str(&run_cli(&[
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
        "11",
        "--start",
    ])?)
    .map_err(|e| failure("m5_probe_parse", e))?;
    ensure(
        created.practice_account_id.as_deref() == Some("PRACTICE:USDT"),
        "run is not funded from the USDT practice account",
    )?;
    let run = created.run_id.clone();
    let show = |run: &str| -> Result<TrainingWorkspaceSnapshotDto, ErrorEnvelope> {
        serde_json::from_str(&run_cli(&["show", "--run-id", run])?)
            .map_err(|e| failure("m5_probe_parse", e))
    };
    run_cli(&[
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
    ])?;
    let snap = show(&run)?;
    let close = snap
        .chart_slice
        .bars
        .last()
        .map(|b| b.close)
        .ok_or_else(|| failure("m5_probe", "no visible bar"))?;
    let limit = (close * Decimal::new(80, 2)).round_dp(2);
    let modified = (close * Decimal::new(85, 2)).round_dp(2);
    run_cli(&[
        "order",
        "limit",
        "--run-id",
        &run,
        "--side",
        "buy",
        "--qty",
        "0.01",
        "--tif",
        "gtc",
        "--limit-price",
        &limit.to_string(),
    ])?;
    let order_id = show(&run)?
        .all_orders
        .iter()
        .find(|o| {
            matches!(o.order_type, SimOrderType::Limit { .. })
                && matches!(o.status, OrderStatus::Accepted | OrderStatus::Active)
        })
        .map(|o| o.order_id.clone())
        .ok_or_else(|| failure("m5_probe", "limit order not found"))?;
    run_cli(&[
        "modify",
        "--run-id",
        &run,
        "--order-id",
        &order_id,
        "--limit-price",
        &modified.to_string(),
        "--qty",
        "0.02",
    ])?;
    let before_quick = show(&run)?;
    run_cli(&[
        "order",
        "market",
        "--run-id",
        &run,
        "--side",
        "sell",
        "--qty",
        "0.005",
        "--fill-rule",
        "visible-close",
    ])?;
    run_cli(&["undo", "--run-id", &run])?;
    let after_undo = show(&run)?;
    let undo_restored = after_undo.account == before_quick.account
        && after_undo.position == before_quick.position
        && after_undo.fills == before_quick.fills;
    ensure(
        undo_restored,
        "undo did not restore cash/position/fills exactly",
    )?;
    run_cli(&["advance", "--run-id", &run, "--steps", "25"])?;
    // Undo after an advance must be refused.
    {
        let mut full: Vec<String> = ["training", "undo", "--run-id", &run, "--data-dir"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        full.push(data_dir.display().to_string());
        let res = run_quiet_command_with_timeout(
            exe.as_os_str(),
            full,
            Some(root),
            &env_with_docs,
            Duration::from_secs(60),
        )
        .map_err(|e| failure("m5_probe_cli", e))?;
        ensure(
            res.status_code != Some(0),
            "undo crossed an advance boundary",
        )?;
    }
    run_cli(&[
        "order",
        "market",
        "--run-id",
        &run,
        "--side",
        "sell",
        "--qty",
        "0.005",
        "--fill-rule",
        "visible-close",
    ])?;
    run_cli(&["advance", "--run-id", &run, "--steps", "3"])?;

    // 3. Three-way replay: CLI scratch + CLI checkpoint (separate processes) + in-process host.
    let cli_scratch: TrainingReplayResultDto = serde_json::from_str(&run_cli(&[
        "replay", "--run-id", &run, "--mode", "scratch",
    ])?)
    .map_err(|e| failure("m5_probe_parse", e))?;
    let cli_ckpt: TrainingReplayResultDto = serde_json::from_str(&run_cli(&[
        "replay",
        "--run-id",
        &run,
        "--mode",
        "checkpoint",
    ])?)
    .map_err(|e| failure("m5_probe_parse", e))?;
    let cli_show = show(&run)?;
    let cli_account: PracticeAccountDto =
        serde_json::from_str(&run_cli(&["account", "show", "--currency", "USDT"])?)
            .map_err(|e| failure("m5_probe_parse", e))?;
    let host = AppHost::from_service(AppService::open(paths.clone(), "USD", "USD", "UTC", true)?);
    let host_snap = host.get_training_workspace_snapshot(&run, None, 320)?;
    let scratch = host.replay_training_run(&run, TrainingReplayMode::Scratch)?;
    let ckpt = host.replay_training_run(&run, TrainingReplayMode::FromCheckpoint)?;
    let host_account = host.show_practice_account("USDT")?;
    host.shutdown()?;
    ensure(
        cli_show.run == host_snap.run
            && cli_show.account == host_snap.account
            && cli_show.position == host_snap.position
            && cli_show.all_orders == host_snap.all_orders
            && cli_show.fills == host_snap.fills
            && cli_account == host_account,
        "CLI and AppHost DTOs diverged on the same run/account",
    )?;
    ensure(
        scratch.matches_persisted_hashes
            && ckpt.matches_persisted_hashes
            && cli_scratch.matches_persisted_hashes
            && cli_ckpt.matches_persisted_hashes
            && ckpt.started_from_seq > 0
            && [&ckpt, &cli_scratch, &cli_ckpt].iter().all(|r| {
                r.final_state_hash == scratch.final_state_hash && r.report == scratch.report
            })
            && cli_scratch.step_hashes == scratch.step_hashes,
        "three-way replay hashes or reports diverged",
    )?;

    // 4. Abandon settles the run back into the practice account (conservation).
    run_cli(&["abandon", "--run-id", &run])?;
    let settled: PracticeAccountDto =
        serde_json::from_str(&run_cli(&["account", "show", "--currency", "USDT"])?)
            .map_err(|e| failure("m5_probe_parse", e))?;
    ensure(
        settled.open_run_id.is_none()
            && settled.allocated_to_open_run.is_zero()
            && settled.equity == settled.available,
        format!("abandoned run was not settled into the practice account: {settled:?}"),
    )?;

    Ok(M5ProbeEvidence {
        run_id: run,
        final_state_hash: scratch.final_state_hash,
        checkpoint_started_seq: ckpt.started_from_seq,
        replay_steps: scratch.step_hashes.len(),
        prepared_bars,
        undo_restored,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// `--live-data`: real public BTC preparation in a temporary library (`M5-K01`, `M5-L-01`)
// ---------------------------------------------------------------------------

fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path)
        .map(|it| {
            it.filter_map(Result::ok)
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        dir_size(&p)
                    } else {
                        e.metadata().map(|m| m.len()).unwrap_or(0)
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// Returns `(status, metrics, detail)`; `status` is `passed`, `failed` or `not-run`.
fn run_live_data_probe(temp: &Path) -> (String, BTreeMap<String, Value>, String) {
    let mut metrics = BTreeMap::new();
    metrics.insert(
        "data_source".into(),
        json!("REAL public services: data.binance.vision, data-api.binance.vision, www.okx.com (not recorded fixtures)"),
    );
    metrics.insert(
        "build_profile".into(),
        json!(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }),
    );
    let probe_root = temp.join("m5-live-data");
    let data_dir = probe_root.join("data");
    let outcome = (|| -> Result<(String, String), ErrorEnvelope> {
        fs::create_dir_all(&data_dir).map_err(|e| failure("live_probe", e))?;
        let paths = temp_paths(&probe_root, &data_dir, "live")?;
        let size_before = dir_size(&data_dir);
        let mut svc = AppService::open(paths.clone(), "USD", "USD", "UTC", true)?;

        let first_start = Instant::now();
        let first = svc.prepare_training_data(&PrepareTrainingDataRequest::default())?;
        let first_s = first_start.elapsed().as_secs_f64();
        let size_after_first = dir_size(&data_dir);

        let incr_start = Instant::now();
        let incremental = svc.prepare_training_data(&PrepareTrainingDataRequest::default())?;
        let incr_s = incr_start.elapsed().as_secs_f64();
        let status = svc.training_data_status(delta_app::DEFAULT_TRAINING_INSTRUMENT, true)?;
        drop(svc);
        let all_network = !first.batches.is_empty()
            && first.batches.iter().all(|b| {
                b.error
                    .as_ref()
                    .is_some_and(|e| e.code == "network_unavailable")
            });
        if all_network {
            return Err(ErrorEnvelope::new(
                "network_unavailable",
                first.batches[0]
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_default(),
                true,
            ));
        }

        let batches: Vec<Value> = first
            .batches
            .iter()
            .map(|b| {
                json!({
                    "instrument": b.instrument_id,
                    "timeframe": b.timeframe.as_str(),
                    "status": b.status,
                    "bars": b.bars_committed,
                    "first_open_at": b.first_open_at,
                    "last_close_at": b.last_close_at,
                    "archive_months": b.archive_months,
                    "rest_pages": b.rest_pages,
                    "intraday_gaps": b.intraday_gaps,
                    "quality_issues": b.quality_issues,
                    "cross_source_compared": b.cross_source_compared,
                    "cross_source_deviations": b.cross_source_deviations,
                    "max_cross_source_deviation_bps": b.max_cross_source_deviation_bps,
                    "elapsed_ms": b.elapsed_ms as u64,
                    "error": b.error.as_ref().map(|e| format!("{}: {}", e.code, e.message)),
                })
            })
            .collect();
        let incremental_added: usize = incremental.batches.iter().map(|b| b.bars_committed).sum();

        // 1h candidate pool build on the freshly prepared library.
        let version = status
            .dataset_version
            .ok_or_else(|| failure("live_probe", "no published dataset version"))?;
        let store = SqliteStore::open_or_init(&data_dir, "USD", "USD", "UTC")
            .map_err(|e| failure("live_probe_store", e))?;
        let pool_start = Instant::now();
        let pool = store
            .query_training_candidate_pool(&CandidatePoolQuery {
                dataset_version: version,
                market_category: "crypto_spot".into(),
                instrument_id: Some(delta_app::DEFAULT_TRAINING_INSTRUMENT.into()),
                start_date: None,
                end_date: None,
                warmup_bars: 0,
                history_len: 120,
                train_len: 250,
                reveal_len: 60,
                timeframe: "1h".into(),
                quality_filter: CandidateQualityFilter::PriceAffectingOnly,
            })
            .map_err(|e| failure("live_probe_pool", e))?;
        let pool_s = pool_start.elapsed().as_secs_f64();
        drop(store);

        metrics.insert("first_prepare_s".into(), json!(first_s));
        metrics.insert("incremental_s".into(), json!(incr_s));
        metrics.insert("incremental_bars_added".into(), json!(incremental_added));
        metrics.insert("pool_1h_build_s".into(), json!(pool_s));
        metrics.insert("pool_1h_candidates".into(), json!(pool.candidates.len()));
        metrics.insert(
            "db_growth_bytes".into(),
            json!(size_after_first.saturating_sub(size_before)),
        );
        metrics.insert("batches".into(), json!(batches));
        metrics.insert("failed_batches".into(), json!(first.failed));
        metrics.insert(
            "timeframe_status".into(),
            serde_json::to_value(&status.timeframes).unwrap_or(Value::Null),
        );

        let daily_ready = !status.needs_preparation;
        let all_ready = status
            .timeframes
            .iter()
            .all(|t| t.candidate_ready == Some(true));
        let within = first_s <= K01_FIRST_PREPARE_LIMIT_S
            && incr_s <= K01_INCREMENTAL_LIMIT_S
            && pool_s <= K01_POOL_LIMIT_S;
        let detail = format!(
            "REAL public sources: first prepare {first_s:.1} s (<= {K01_FIRST_PREPARE_LIMIT_S} s), incremental {incr_s:.1} s (<= {K01_INCREMENTAL_LIMIT_S} s, +{incremental_added} bars), 1h pool {pool_s:.3} s (<= {K01_POOL_LIMIT_S} s, {} candidates), failed batches {}, db growth {} bytes, daily startable={daily_ready}, all timeframes startable={all_ready}",
            pool.candidates.len(),
            first.failed,
            size_after_first.saturating_sub(size_before)
        );
        let ok = first.failed == 0 && incremental.failed == 0 && daily_ready && all_ready && within;
        Ok((if ok { "passed" } else { "failed" }.into(), detail))
    })();
    match outcome {
        Ok((status, detail)) => (status, metrics, detail),
        Err(e) if e.code == "network_unavailable" => (
            "not-run".into(),
            metrics,
            format!("real public sources unreachable ({}: {}); engineering acceptance relies on recorded fixtures", e.code, e.message),
        ),
        Err(e) => (
            "failed".into(),
            metrics,
            format!("live preparation failed: {}: {}", e.code, e.message),
        ),
    }
}

// ---------------------------------------------------------------------------
// Architecture / hygiene (`M5-G-07`) and same-fingerprint regressions (`M5-A-16`)
// ---------------------------------------------------------------------------

fn run_m5_architecture_check(root: &Path) -> Result<String, ErrorEnvelope> {
    let cases: Value = serde_json::from_slice(
        &fs::read(root.join("docs/delivery/m5-stage-cases.json"))
            .map_err(|e| failure("M5_G07", e))?,
    )
    .map_err(|e| failure("M5_G07", e))?;
    let ids: Vec<&str> = cases["cases"]
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["id"].as_str()).collect())
        .unwrap_or_default();
    ensure(
        cases["stage"] == STAGE && cases["planVersion"] == PLAN_VERSION && ids == M5_CASE_IDS,
        format!("m5-stage-cases.json header or ids mismatch: {ids:?}"),
    )?;
    let verify_rs = fs::read_to_string(root.join("crates/delta-cli/src/verify.rs"))
        .map_err(|e| failure("M5_G07", e))?;
    ensure(
        verify_rs.contains("docs/delivery/m5-stage-cases.json"),
        "source_fingerprint must include m5-stage-cases.json",
    )?;
    // Truncation stays in the host: the desktop never touches storage or the core engine.
    let mut desktop = Vec::new();
    super::verify::source_files(&root.join("crates/delta-desktop/src"), &mut desktop)
        .map_err(|e| failure("M5_G07", e))?;
    for file in &desktop {
        let text = fs::read_to_string(file).map_err(|e| failure("M5_G07", e))?;
        for forbidden in ["delta_core::", "delta_data::", "rusqlite::", "from bar"] {
            ensure(
                !text
                    .to_ascii_lowercase()
                    .contains(&forbidden.to_ascii_lowercase()),
                format!(
                    "delta-desktop {:?} references '{forbidden}'",
                    file.file_name()
                ),
            )?;
        }
    }
    // m5-sim-2 additions keep Decimal-only money in the core engine.
    let sim = fs::read_to_string(root.join("crates/delta-core/src/sim.rs"))
        .map_err(|e| failure("M5_G07", e))?;
    ensure(
        sim.contains("m4-sim-1") && sim.contains("m5-sim-2"),
        "sim.rs must keep both engine versions",
    )?;
    let account_report = fs::read_to_string(root.join("crates/delta-core/src/account_report.rs"))
        .map_err(|e| failure("M5_G07", e))?;
    for (n, line) in account_report.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        ensure(
            !(t.contains("f64") || t.contains("f32") || t.contains("HashMap")),
            format!("account_report.rs:{} uses float/HashMap", n + 1),
        )?;
    }
    Ok(format!(
        "{} cases in m5-stage-cases.json; fingerprint covers m5 cases, zip and sig fixtures; {} delta-desktop files free of core/storage access; account_report Decimal-only",
        ids.len(),
        desktop.len()
    ))
}

fn verify_prior_stage_regressions(root: &Path, fingerprint: &str) -> Result<String, ErrorEnvelope> {
    let mut summaries = Vec::new();
    for (dir, stage, plan) in [
        ("m1", "M1", "2.6"),
        ("m2", "M2", "1.0"),
        ("m3", "M3", "1.0"),
        ("m4", "M4", "1.0"),
    ] {
        let path = root.join(format!(".local/reports/{dir}/verify-stage-report.json"));
        let raw = fs::read(&path).map_err(|e| {
            failure(
                "REGRESSION_MISSING",
                format!(
                    "{stage} report missing at {} ({e}); run verify-stage --stage {stage} first",
                    path.display()
                ),
            )
        })?;
        let rep: Value = serde_json::from_slice(&raw).map_err(|e| failure("REGRESSION_JSON", e))?;
        ensure(
            rep["stage"] == stage
                && rep["plan_version"] == plan
                && rep["overall_status"] == "passed"
                && rep["source_fingerprint"] == fingerprint,
            format!(
                "{stage} regression mismatch: stage={} plan={} status={} fp={} (expected {fingerprint})",
                rep["stage"], rep["plan_version"], rep["overall_status"], rep["source_fingerprint"]
            ),
        )?;
        let case_passed = |id: &str| {
            rep["case_results"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|c| c["case_id"] == id && c["status"] == "passed")
            })
        };
        let required: &[&str] = match stage {
            "M2" => &["M2-A-20"],
            "M3" => &["M3-A-03"],
            "M4" => &["M4-A-17", "M4-A-18"],
            _ => &[],
        };
        ensure(
            required.iter().all(|id| case_passed(id)),
            format!("{stage} regression must include passed {required:?} (UI smoke/bench/installer flags)"),
        )?;
        if stage == "M2" {
            ensure(
                rep["poc_results"].as_array().is_some_and(|a| {
                    a.iter()
                        .any(|p| p["id"] == "M2-K06" && p["status"] == "passed")
                }),
                "M2 regression must include passed --ui-smoke (M2-K06)",
            )?;
        }
        if stage == "M3" {
            ensure(
                rep["case_results"].as_array().is_some_and(|a| {
                    a.iter().any(|c| {
                        c["case_id"] == "M3-A-03"
                            && c["evidence"]
                                .as_str()
                                .is_some_and(|e| e.contains("installed --ui-smoke passed"))
                    })
                }),
                "M3 regression must include --installer and --ui-smoke (M3-A-03)",
            )?;
        }
        summaries.push(format!(
            "{stage} v{plan} passed @ {}",
            rep["executed_at"].as_str().unwrap_or("?")
        ));
    }
    Ok(summaries.join("; "))
}

fn record_mapped_case(
    report: &mut VerifyStageReport,
    id: &str,
    package: &str,
    names: &[&str],
    extra: Option<&str>,
) {
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| report.test_results.get(*n).is_none_or(|s| s != "passed"))
        .collect();
    let mut evidence = format!("M5-G-05 captured test results: {}", names.join(", "));
    if !missing.is_empty() {
        evidence.push_str(&format!("; NOT PASSED: {}", missing.join(", ")));
    }
    if let Some(extra) = extra {
        evidence.push_str("; ");
        evidence.push_str(extra);
    }
    report.case_results.push(CaseResultRecord {
        case_id: id.into(),
        work_package: package.into(),
        tier: "required".into(),
        status: if missing.is_empty() {
            "passed"
        } else {
            "failed"
        }
        .into(),
        elapsed_ms: 0,
        evidence,
    });
}

fn run_logged(
    root: &Path,
    logs: &Path,
    report: &mut VerifyStageReport,
    key: &str,
    program: &OsStr,
    args: Vec<String>,
    env: &BTreeMap<String, String>,
) -> Result<delta_app::QuietCommandResult, ErrorEnvelope> {
    let res = run_quiet_command(program, args, Some(root), env).map_err(|e| failure(key, e))?;
    let path = logs.join(format!("{key}.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&res).map_err(|e| failure(key, e))?,
    )
    .map_err(|e| failure(key, e))?;
    report.log_files.insert(key.into(), rel(root, &path));
    Ok(res)
}

// ---------------------------------------------------------------------------
// Main entrypoint
// ---------------------------------------------------------------------------

pub(crate) fn run_verify_stage_m5(
    ui_smoke: bool,
    ui_bench: bool,
    live_data: bool,
) -> Result<ExitCode, ErrorEnvelope> {
    let root = env::current_dir().map_err(|e| failure("cwd", e))?;
    let fingerprint = source_fingerprint(&root)?;
    let run_id = format!("{}-{}", std::process::id(), Utc::now().timestamp_millis());
    let temp = env::temp_dir().join(format!("delta_m5_verify_{run_id}"));
    fs::create_dir(&temp).map_err(|e| failure("temp", e))?;
    let reports = root.join(".local/reports/m5");
    let logs = reports.join(format!("logs-{run_id}"));
    fs::create_dir_all(&logs).map_err(|e| failure("logs", e))?;

    let appdata = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let user_docs = resolve_user_documents_root().ok();
    let mut watched: Vec<PathBuf> = appdata
        .iter()
        .flat_map(|p| [p.join("delta"), p.join("delta-dev")])
        .collect();
    if let Some(docs) = &user_docs {
        watched.push(docs.join("delta"));
        watched.push(docs.join("delta-dev"));
    }
    let before = directory_snapshot(&watched).map_err(|e| failure("path_snapshot", e))?;
    let test_dirs_before = owned_test_directories().map_err(|e| failure("temp_snapshot", e))?;
    let stamp =
        |p: Option<PathBuf>| p.map(|p| (p.exists(), p.metadata().and_then(|m| m.modified()).ok()));
    let release_before = stamp(appdata.as_ref().map(|p| p.join("delta")));
    let dev_before = stamp(appdata.as_ref().map(|p| p.join("delta-dev")));

    let jobs = env::var("DELTA_GATE_JOBS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(1)
        });

    let mut report = VerifyStageReport {
        stage: STAGE.into(),
        plan_version: PLAN_VERSION.into(),
        executed_at: Utc::now().to_rfc3339(),
        live_mode: live_data,
        source_fingerprint: fingerprint,
        visible_window_count: 0,
        window_details: Vec::new(),
        temp_residual_dirs: 0,
        outside_temp_created_dirs: 0,
        path_observation_scope: "directory metadata under real LOCALAPPDATA/delta, delta-dev and Documents/delta, delta-dev; all gate, probe, live-data and test roots isolated in TEMP; EnumWindows + GetForegroundWindow quietness monitor active".into(),
        release_data_dir_touched: false,
        poc_results: Vec::new(),
        case_results: Vec::new(),
        live_probes: Vec::new(),
        overall_status: "running".into(),
        gate_jobs: jobs,
        process_priority: current_process_priority(),
        test_results: BTreeMap::new(),
        log_files: BTreeMap::new(),
        errors: Vec::new(),
    };

    let monitor = M4QuietnessMonitor::start(&root);
    let mut ui_smoke_summary: Option<String> = None;
    let mut probe_summary: Option<String> = None;

    let execution = catch_unwind(AssertUnwindSafe(|| -> Result<(), ErrorEnvelope> {
        let job = QuietJobObject::new().map_err(|e| failure("job_object", e))?;
        job.assign_current_process()
            .map_err(|e| failure("job_object", e))?;
        let gate_env = BTreeMap::from([
            (
                "DELTA_DATA_DIR".into(),
                temp.join("gate-data").display().to_string(),
            ),
            (
                "DELTA_DOCUMENTS_DIR".into(),
                temp.join("gate-documents").display().to_string(),
            ),
            (
                "LOCALAPPDATA".into(),
                temp.join("gate-localappdata").display().to_string(),
            ),
            (
                "DELTA_TEST_ROOT".into(),
                temp.join("tests").display().to_string(),
            ),
            ("CARGO_BUILD_JOBS".into(), jobs.to_string()),
            ("CARGO_INCREMENTAL".into(), "0".into()),
            ("PYTHONUTF8".into(), "1".into()),
            ("RUST_TEST_THREADS".into(), jobs.to_string()),
        ]);

        // 1. Mechanical gates `M5-G-01`..`M5-G-06`.
        let gates: [(&str, &str, Vec<&str>); 6] = [
            ("M5-G-01", "python", vec!["scripts/check_docs.py"]),
            ("M5-G-02", "git", vec!["diff", "--check"]),
            ("M5-G-03", "cargo", vec!["fmt", "--all", "--check"]),
            (
                "M5-G-04",
                "cargo",
                vec![
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            ("M5-G-05", "cargo", vec!["test", "--workspace", "--locked"]),
            ("M5-G-06", "python", vec!["scripts/deps.py", "--audit"]),
        ];
        for (id, program, args) in gates {
            let started = Instant::now();
            let res = run_logged(
                &root,
                &logs,
                &mut report,
                id,
                OsStr::new(program),
                args.iter().map(|s| s.to_string()).collect(),
                &gate_env,
            )?;
            if id == "M5-G-05" {
                record_tests(&res.stdout, &mut report.test_results);
            }
            let passed = res.status_code == Some(0) && !res.timed_out;
            // `deps.py` exits 2 when the network sources are unreachable: not-run, never passed.
            let not_run = id == "M5-G-06" && res.status_code == Some(2) && !res.timed_out;
            let mut evidence = format!(
                "{program} {}; exit={:?}; log {}",
                args.join(" "),
                res.status_code,
                report.log_files[id]
            );
            if id == "M5-G-06" {
                let last_line = |text: &str| {
                    text.lines()
                        .rfind(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .to_string()
                };
                evidence.push_str(&format!("; audit: {}", last_line(&res.stdout)));
                // Inventory of newer versions (D-35): reported, never a failure by itself.
                let inv = run_logged(
                    &root,
                    &logs,
                    &mut report,
                    "deps-inventory",
                    OsStr::new("python"),
                    vec!["scripts/deps.py".into()],
                    &gate_env,
                )?;
                evidence.push_str(&format!(
                    "; inventory exit={:?}: {}",
                    inv.status_code,
                    last_line(&inv.stdout)
                ));
            }
            report.case_results.push(CaseResultRecord {
                case_id: id.into(),
                work_package: if id == "M5-G-06" { "M5-W08" } else { "M5-W07" }.into(),
                tier: "required".into(),
                status: if passed {
                    "passed"
                } else if not_run {
                    "not-run"
                } else {
                    "failed"
                }
                .into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                evidence,
            });
            if !passed && !not_run {
                return Err(failure(
                    "gate_failed",
                    format!("{id}: see {}", report.log_files[id]),
                ));
            }
        }

        // 2. `M5-G-07` architecture & hygiene.
        let g07 = Instant::now();
        let arch = run_m5_architecture_check(&root)?;
        report.case_results.push(CaseResultRecord {
            case_id: "M5-G-07".into(),
            work_package: "M5-W07".into(),
            tier: "required".into(),
            status: "passed".into(),
            elapsed_ms: g07.elapsed().as_millis() as u64,
            evidence: arch,
        });

        // 3. In-gate cross-process probe (`M5-K03`).
        let probe = run_in_gate_m5_probe(&root, &temp, &gate_env)?;
        probe_summary = Some(format!(
            "in-gate cross-process probe on recorded loopback fixture ({} ms): prepare-data {} bars, run {}, quick fill + limit + modify + undo (restored={}) + 28 advances; CLI scratch/checkpoint + host scratch/checkpoint state_hash {} identical over {} steps (checkpoint seq {}); undo after advance refused; CLI/AppHost run+account DTO parity; abandon settled",
            probe.elapsed_ms,
            probe.prepared_bars,
            probe.run_id,
            probe.undo_restored,
            probe.final_state_hash,
            probe.replay_steps,
            probe.checkpoint_started_seq
        ));
        let k03_tests = [
            "sim::tests::m5_modify_and_undo_restore_exact_state_and_never_cross_an_advance",
            "sim::tests::m5_undo_after_an_earlier_undo_keeps_surviving_ids_exact",
            "sim::tests::m4_k02_deterministic_state_hash_replay_and_checkpoint_equivalence",
            "training_m5::tests::m5_genuine_m4_library_upgrades_and_old_run_hashes_are_unchanged",
        ];
        let k03_ok = k03_tests
            .iter()
            .all(|t| report.test_results.get(*t).is_some_and(|s| s == "passed"));
        report.poc_results.push(PocResultRecord {
            id: "M5-K03".into(),
            title: "Undo and old-engine replay determinism (two processes, checkpoint, scratch)"
                .into(),
            status: if k03_ok { "passed" } else { "failed" }.into(),
            metrics: BTreeMap::from([
                ("final_state_hash".into(), json!(probe.final_state_hash)),
                (
                    "checkpoint_started_seq".into(),
                    json!(probe.checkpoint_started_seq),
                ),
                ("replay_steps".into(), json!(probe.replay_steps)),
                (
                    "data_source".into(),
                    json!("recorded loopback market fixture (synthetic, Binance/OKX-shaped)"),
                ),
            ]),
            detail: format!(
                "{}; genuine M4 schema-5 m4-sim-1 runs replay with unchanged hashes ({})",
                probe_summary.as_deref().unwrap_or_default(),
                k03_tests[3]
            ),
        });

        // 4. Release build + hidden-window UI smoke / playback bench.
        let release_exe = if ui_smoke || ui_bench {
            let res = run_logged(
                &root,
                &logs,
                &mut report,
                "cargo-build-release-desktop",
                OsStr::new("cargo"),
                [
                    "build",
                    "--release",
                    "-p",
                    "delta-desktop",
                    "--bin",
                    "delta",
                    "--locked",
                ]
                .map(String::from)
                .to_vec(),
                &gate_env,
            )?;
            ensure(
                res.status_code == Some(0) && !res.timed_out,
                format!("release desktop build failed: {}", res.stderr),
            )?;
            let exe = root.join("target/release/delta.exe");
            ensure(exe.is_file(), format!("missing {}", exe.display()))?;
            Some(exe)
        } else {
            None
        };
        if ui_smoke {
            let exe = release_exe.as_ref().unwrap();
            let res = run_logged(
                &root,
                &logs,
                &mut report,
                "m5-ui-smoke",
                exe.as_os_str(),
                vec![
                    "--m5-ui-smoke".into(),
                    "--data-dir".into(),
                    temp.join("m5-ui-smoke").display().to_string(),
                ],
                &gate_env,
            )?;
            fs::write(reports.join("m5-ui-smoke.json"), &res.stdout)
                .map_err(|e| failure("smoke_report", e))?;
            ensure(
                res.status_code == Some(0) && !res.timed_out,
                format!("delta.exe --m5-ui-smoke failed: {}", res.stderr),
            )?;
            let v: Value =
                serde_json::from_str(&res.stdout).map_err(|e| failure("smoke_report", e))?;
            validate_m5_ui_smoke(&v).map_err(|m| failure("m5_ui_smoke_invalid", m))?;
            ui_smoke_summary = Some(format!(
                "release --m5-ui-smoke passed (hidden native window, {} chart paints, {} played steps, quick fill+undo, finish+reveal, visible_windows=0, focus_stolen=false; recorded loopback fixture)",
                v["chart_paints"], v["played_steps"]
            ));
        }

        // 5. Mapped cases.
        let probe_note = probe_summary.clone().unwrap_or_default();
        record_mapped_case(
            &mut report,
            "M5-A-01",
            "M5-W01",
            &[
                "training_m5::tests::m5_thin_slice_empty_library_prepares_btc_daily_and_starts_practice_run",
                "training_m5::tests::m5_prepare_failures_retry_and_incremental_update",
                "regression::monthly_archive_backfill_verifies_checksum_before_publishing",
            ],
            Some(&probe_note),
        );
        record_mapped_case(
            &mut report,
            "M5-A-02",
            "M5-W01",
            &[
                "regression::stablecoin_outage_keeps_crypto_history_and_m5_pool_ignores_valuation_issues",
                "regression::stablecoin_quote_leg_is_settled_once_from_cash",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-03",
            "M5-W01",
            &[
                "regression::stablecoin_quote_leg_is_settled_once_from_cash",
                "regression::stablecoin_outage_keeps_crypto_history_and_m5_pool_ignores_valuation_issues",
                "m4_schema::tests::test_m4_a02_candidate_pool_exclusions_determinism_and_redraw_rules",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-04",
            "M5-W02",
            &[
                "training_m5::tests::m5_intraday_runs_are_aligned_and_day_orders_expire_at_utc_day_end",
                "timeframe::tests::parse_and_alignment",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-05",
            "M5-W02",
            &[
                "training_m5::tests::m5_aggregated_view_matches_hand_calc_and_never_leaks_future_sentinel",
                "timeframe::tests::aggregate_marks_the_forming_bar_incomplete",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-06",
            "M5-W04",
            &[
                "sim::tests::m5_quick_fill_at_visible_close_is_hand_checkable",
                "sim::tests::m5_quick_fill_rounds_adversely_and_rejects_without_shrinking",
                "training_m5::tests::m5_quick_fill_modify_undo_and_settlement_are_hand_checkable",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-07",
            "M5-W04,M5-W05",
            &[
                "sim::tests::m5_modify_and_undo_restore_exact_state_and_never_cross_an_advance",
                "training_m5::tests::m5_quick_fill_modify_undo_and_settlement_are_hand_checkable",
                "training_workbench::tests::proportions_floor_to_lot_and_drag_respects_lock",
                "training_workbench_ui::tests::test_m5_w05_headless_workbench_flow",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-08",
            "M5-W04",
            &k03_tests,
            Some(&probe_note),
        );
        record_mapped_case(
            &mut report,
            "M5-A-09",
            "M5-W02,M5-W04",
            &[
                "training_m5::tests::m5_intraday_runs_are_aligned_and_day_orders_expire_at_utc_day_end",
                "sim::tests::m4_a_05_stop_oco_path_ordering_ambiguous_fills_and_attached_brackets",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-10",
            "M5-W03",
            &[
                "training_m5::tests::m5_abandon_restart_and_delete_keep_practice_account_conserved",
                "training_m5::tests::m5_quick_fill_modify_undo_and_settlement_are_hand_checkable",
            ],
            Some("in-gate probe: abandon settled the run back into PRACTICE:USDT (equity == available, no open allocation)"),
        );
        record_mapped_case(
            &mut report,
            "M5-A-11",
            "M5-W03",
            &[
                "account_report::tests::account_report_is_hand_checkable",
                "training_m5::tests::m5_quick_fill_modify_undo_and_settlement_are_hand_checkable",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-12",
            "M5-W03",
            &[
                "training_m5::tests::m5_genuine_m4_library_upgrades_and_old_run_hashes_are_unchanged",
                "training_m5::tests::m5_abandon_restart_and_delete_keep_practice_account_conserved",
                "training::tests::test_m4_a11_schema4_migration_backup_ledger_isolation_export_and_delete",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M5-A-13",
            "M5-W05",
            &[
                "training_workbench_ui::tests::test_m5_w05_headless_workbench_flow",
                "training_workbench::tests::no_key_or_click_ever_trades",
                "training_workbench::tests::focused_input_swallows_workbench_keys",
                "training_workbench::tests::draft_options_follow_line_position",
                "training_workbench::tests::drawings_need_their_anchor_count_and_measure_has_no_dates",
            ],
            ui_smoke_summary.as_deref(),
        );
        record_mapped_case(
            &mut report,
            "M5-A-14",
            "M5-W06",
            &[
                "training_cmd::tests::test_m5_cli_practice_account_quick_fill_undo_drawings_and_parity",
                "training_m5::tests::m5_quick_fill_modify_undo_and_settlement_are_hand_checkable",
            ],
            Some("in-gate probe: CLI show/account == AppHost workspace/account DTOs on the same run"),
        );
        let audit_note = report
            .case_results
            .iter()
            .find(|c| c.case_id == "M5-G-06")
            .map(|c| c.evidence.clone());
        record_mapped_case(
            &mut report,
            "M5-A-17",
            "M5-W08",
            &[
                "providers::archive_member_tests::traversal_absolute_symlink_and_directory_members_are_refused",
                "providers::archive_member_tests::plain_csv_member_is_read",
                "regression::monthly_archive_backfill_verifies_checksum_before_publishing",
                "regression::strict_headers_missing_ids_and_multi_account_flex_are_handled_explicitly",
                "service::export::tests::test_m2_a17_backup_restore_export_delete_and_log_redaction",
                "regression::recursive_backup_includes_settings_and_detects_tampering",
                "packaging::tests::installer_rejects_wrong_identity_before_creating_target",
                "installer::tests::package_build_keeps_only_the_newest_installer_of_the_identity",
                "update::tests::m3_a07_signature_downgrade_and_transport_policy",
                "update::tests::published_v0_1_1_manifest_signature_still_verifies",
            ],
            audit_note.as_deref(),
        );

        // 6. `M5-K02` / `M5-A-15` continuous playback benchmark.
        if ui_bench {
            let started = Instant::now();
            let exe = release_exe.as_ref().unwrap();
            let res = run_logged(
                &root,
                &logs,
                &mut report,
                "m5-ui-bench",
                exe.as_os_str(),
                vec![
                    "--m5-ui-bench".into(),
                    "--data-dir".into(),
                    temp.join("m5-ui-bench").display().to_string(),
                ],
                &gate_env,
            )?;
            let raw_path = reports.join("m5-k02-bench.json");
            fs::write(&raw_path, &res.stdout).map_err(|e| failure("bench_report", e))?;
            let raw_rel = rel(&root, &raw_path);
            let parsed: Result<Value, String> = serde_json::from_str(&res.stdout)
                .map_err(|e| format!("bench output is not JSON: {e}; stderr={}", res.stderr));
            let validated = parsed.as_ref().map_err(Clone::clone).and_then(|v| {
                if res.status_code == Some(0) && !res.timed_out {
                    validate_m5_playback_bench(v)
                } else {
                    Err(format!(
                        "delta.exe --m5-ui-bench exited {:?}",
                        res.status_code
                    ))
                }
            });
            match validated {
                Ok(v) => {
                    let bench = parsed.unwrap_or_default();
                    let detail = format!(
                        "release hidden HWND, 1h run, 1,000 visible bars + MA/MACD/RSI + 20 drawings + 5 order lines, {} steps at 150 ms: step p50/p95/p99 = {:.3}/{:.3}/{:.3} ms (p95 <= {M5_K02_STEP_P95_BUDGET_MS}), interval p50/p95/p99 = {:.3}/{:.3}/{:.3} ms (p95 <= {M5_K02_INTERVAL_P95_BUDGET_MS}); phase p95 {}; cpu {}; gpu {}; raw samples {raw_rel}",
                        v.measured_steps, v.step_p50_ms, v.step_p95_ms, v.step_p99_ms,
                        v.interval_p50_ms, v.interval_p95_ms, v.interval_p99_ms,
                        bench["phase_p95_ms"], bench["cpu"], bench["gpu"]
                    );
                    report.poc_results.push(PocResultRecord {
                        id: "M5-K02".into(),
                        title:
                            "Continuous playback next-bar latency (release, 300 steps at 150 ms)"
                                .into(),
                        status: "passed".into(),
                        metrics: BTreeMap::from([
                            ("step_p95_ms".into(), json!(v.step_p95_ms)),
                            ("interval_p95_ms".into(), json!(v.interval_p95_ms)),
                            ("measured_steps".into(), json!(v.measured_steps)),
                            ("raw_report".into(), json!(raw_rel)),
                        ]),
                        detail: detail.clone(),
                    });
                    report.case_results.push(CaseResultRecord {
                        case_id: "M5-A-15".into(),
                        work_package: "M5-W05,M5-W07".into(),
                        tier: "required".into(),
                        status: "passed".into(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        evidence: detail,
                    });
                }
                Err(reason) => {
                    report.poc_results.push(PocResultRecord {
                        id: "M5-K02".into(),
                        title:
                            "Continuous playback next-bar latency (release, 300 steps at 150 ms)"
                                .into(),
                        status: "failed".into(),
                        metrics: BTreeMap::new(),
                        detail: format!("FAILED: {reason} (raw {raw_rel})"),
                    });
                    report.case_results.push(CaseResultRecord {
                        case_id: "M5-A-15".into(),
                        work_package: "M5-W05,M5-W07".into(),
                        tier: "required".into(),
                        status: "failed".into(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        evidence: format!("FAILED: {reason} (raw {raw_rel})"),
                    });
                }
            }
        } else {
            report.poc_results.push(PocResultRecord {
                id: "M5-K02".into(),
                title: "Continuous playback next-bar latency (release, 300 steps at 150 ms)".into(),
                status: "not-run".into(),
                metrics: BTreeMap::new(),
                detail: "requires --ui-bench".into(),
            });
            report.case_results.push(CaseResultRecord {
                case_id: "M5-A-15".into(),
                work_package: "M5-W05,M5-W07".into(),
                tier: "required".into(),
                status: "not-run".into(),
                elapsed_ms: 0,
                evidence: "pass --ui-bench for the release continuous-playback measurement".into(),
            });
        }

        // 7. `--live-data`: real public sources (`M5-K01`, `M5-L-01`), declared separately.
        if live_data {
            let started = Instant::now();
            let (status, metrics, detail) = run_live_data_probe(&temp);
            fs::write(
                reports.join("m5-live-data.json"),
                serde_json::to_vec_pretty(
                    &json!({"status": status, "detail": detail, "metrics": metrics}),
                )
                .map_err(|e| failure("live_report", e))?,
            )
            .map_err(|e| failure("live_report", e))?;
            report.poc_results.push(PocResultRecord {
                id: "M5-K01".into(),
                title: "BTC/USDT 1d/4h/1h full-history preparation on REAL public sources (temporary library)".into(),
                status: status.clone(),
                metrics,
                detail: detail.clone(),
            });
            report.case_results.push(CaseResultRecord {
                case_id: "M5-L-01".into(),
                work_package: "M5-W01".into(),
                tier: "live".into(),
                status,
                elapsed_ms: started.elapsed().as_millis() as u64,
                evidence: format!("{detail}; raw .local/reports/m5/m5-live-data.json"),
            });
        }
        Ok(())
    }));

    match execution {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report
            .errors
            .push(format!("{}: {}", error.code, error.message)),
        Err(payload) => report.errors.push(format!(
            "gate panic: {}",
            payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic")
        )),
    }

    // Quietness, isolation and regressions (`M5-A-16`).
    let a16 = Instant::now();
    let (window_samples, window_findings) = monitor.finish();
    if let Err(error) = fs::remove_dir_all(&temp) {
        report
            .errors
            .push(format!("temporary cleanup failed: {error}"));
    }
    report.temp_residual_dirs = usize::from(temp.exists())
        + owned_test_directories()
            .map_err(|e| failure("temp_snapshot", e))?
            .difference(&test_dirs_before)
            .count();
    let after = directory_snapshot(&watched).map_err(|e| failure("path_snapshot", e))?;
    report.outside_temp_created_dirs = after.difference(&before).count();
    let release_after = stamp(appdata.as_ref().map(|p| p.join("delta")));
    let dev_after = stamp(appdata.as_ref().map(|p| p.join("delta-dev")));
    report.release_data_dir_touched = release_before != release_after;
    let dev_touched = dev_before != dev_after;
    report.visible_window_count = count_visible_windows_for_current_process();
    report.window_details = observed_window_details();
    let fingerprint_unchanged = source_fingerprint(&root)? == report.source_fingerprint;

    let quiet = (|| -> Result<(), ErrorEnvelope> {
        ensure(
            !cfg!(windows) || report.process_priority == "below_normal",
            "process priority is not below_normal",
        )?;
        ensure(
            report.visible_window_count == 0 && window_findings.is_empty(),
            format!("visible or foreground windows detected: {window_findings:?}"),
        )?;
        ensure(
            report.outside_temp_created_dirs == 0
                && !report.release_data_dir_touched
                && !dev_touched,
            "developer delta/delta-dev data or document directories changed",
        )?;
        ensure(
            report.temp_residual_dirs == 0,
            "test temporary directories remain",
        )?;
        ensure(
            fingerprint_unchanged,
            "source_fingerprint changed during gate",
        )?;
        Ok(())
    })();
    let a16_case = |status: &str, evidence: String| CaseResultRecord {
        case_id: "M5-A-16".into(),
        work_package: "M5-W07".into(),
        tier: "required".into(),
        status: status.into(),
        elapsed_ms: a16.elapsed().as_millis() as u64,
        evidence,
    };
    let a16_record = match quiet {
        Err(err) => a16_case("failed", format!("FAILED {}: {}", err.code, err.message)),
        Ok(()) if ui_smoke && ui_bench => {
            match verify_prior_stage_regressions(&root, &report.source_fingerprint) {
                Ok(summary) => a16_case(
                    "passed",
                    format!(
                        "below_normal priority; {window_samples} quietness samples with 0 visible/foreground windows; LOCALAPPDATA and Documents delta/delta-dev untouched; 0 temp residuals; fingerprint stable; same-fingerprint regressions: {summary}"
                    ),
                ),
                Err(err) => a16_case("failed", format!("FAILED {}: {}", err.code, err.message)),
            }
        }
        Ok(()) => a16_case(
            "not-run",
            format!("quietness ({window_samples} samples) and isolation passed; closing M5-A-16 requires --ui-smoke --ui-bench and same-fingerprint M1..M4 reports"),
        ),
    };
    if a16_record.status == "failed" {
        report
            .errors
            .push(format!("M5-A-16: {}", a16_record.evidence));
    }
    report.case_results.push(a16_record);

    if !live_data {
        report.case_results.push(CaseResultRecord {
            case_id: "M5-L-01".into(),
            work_package: "M5-W01".into(),
            tier: "live".into(),
            status: "not-run".into(),
            elapsed_ms: 0,
            evidence: "pass --live-data to prepare real public BTC data in a temporary library"
                .into(),
        });
    }
    report.case_results.push(CaseResultRecord {
        case_id: "M5-M-01".into(),
        work_package: "M5-W05".into(),
        tier: "manual".into(),
        status: "not-run".into(),
        elapsed_ms: 0,
        evidence: "Manual check USER-ACTIONS H-06 (hands-on training workbench on the installed dev build)".into(),
    });

    let failed = !report.errors.is_empty()
        || report
            .case_results
            .iter()
            .any(|c| c.status == "failed" && c.tier != "live")
        || report
            .poc_results
            .iter()
            .any(|p| p.status == "failed" && p.id != "M5-K01");
    let required_not_run = report
        .case_results
        .iter()
        .any(|c| c.tier == "required" && c.status == "not-run")
        || report
            .poc_results
            .iter()
            .any(|p| p.status == "not-run" && p.id != "M5-K01");
    report.overall_status = if failed {
        "failed"
    } else if required_not_run {
        "not-run"
    } else {
        "passed"
    }
    .into();

    let text = serde_json::to_string_pretty(&report).map_err(|e| failure("report", e))?;
    fs::write(reports.join("verify-stage-report.json"), &text).map_err(|e| failure("report", e))?;
    println!("{}", sanitize_terminal_output(&text));
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(step_us: u64, interval_us: u64) -> Value {
        json!({
            "status": "passed", "passed": true, "build_profile": "release",
            "power_throttling": EXECUTION_SPEED_OPT_OUT, "visible_windows": 0, "focus_stolen": false,
            "timeframe": "1h", "visible_bars": 1000, "drawings": 20, "order_lines": 5,
            "play_interval_ms": 150, "measured_steps": 300, "native_chart_paints": 300,
            "step_times_us": vec![step_us; 300], "interval_times_us": vec![interval_us; 300],
            "step_p95_ms": step_us as f64 / 1000.0,
        })
    }

    #[test]
    fn m5_bench_validation_recomputes_and_rejects_slow_dev_or_short_samples() {
        let ok = validate_m5_playback_bench(&bench(60_000, 155_000)).unwrap();
        assert_eq!(ok.step_p95_ms, 60.0);
        assert_eq!(ok.interval_p95_ms, 155.0);
        assert!(validate_m5_playback_bench(&bench(120_000, 155_000)).is_err());
        assert!(validate_m5_playback_bench(&bench(60_000, 250_000)).is_err());
        let mut dev = bench(60_000, 155_000);
        dev["build_profile"] = json!("dev");
        assert!(validate_m5_playback_bench(&dev).is_err());
        let mut short = bench(60_000, 155_000);
        short["step_times_us"] = json!(vec![60_000u64; 100]);
        assert!(validate_m5_playback_bench(&short).is_err());
        let mut lied = bench(60_000, 155_000);
        lied["step_p95_ms"] = json!(10.0);
        assert!(validate_m5_playback_bench(&lied).is_err());
        let mut small = bench(60_000, 155_000);
        small["drawings"] = json!(3);
        assert!(validate_m5_playback_bench(&small).is_err());
    }

    #[test]
    fn m5_smoke_validation_requires_hidden_release_flow() {
        let good = json!({
            "status": "passed", "build_profile": "release", "native_window_created": true,
            "visible_windows": 0, "focus_stolen": false, "practice_account": true,
            "quick_fill_then_undo": true, "played_steps": 5, "chart_paints": 11,
            "finished_status": "Finished", "revealed_instrument": "BINANCE:BTC/USDT",
        });
        assert!(validate_m5_ui_smoke(&good).is_ok());
        for (k, v) in [
            ("visible_windows", json!(1)),
            ("focus_stolen", json!(true)),
            ("build_profile", json!("dev")),
            ("quick_fill_then_undo", json!(false)),
        ] {
            let mut bad = good.clone();
            bad[k] = v;
            assert!(validate_m5_ui_smoke(&bad).is_err(), "{k}");
        }
    }

    #[test]
    fn m5_cases_file_matches_the_gate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let summary = run_m5_architecture_check(&root).unwrap();
        assert!(summary.starts_with("19 cases"), "{summary}");
    }
}
