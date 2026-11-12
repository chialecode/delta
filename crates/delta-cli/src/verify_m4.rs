//! `verify-stage --stage M4 --plan-version 1.0 [--ui-smoke] [--ui-bench]` (`S-47`).
//!
//! Executes M4 mechanical & architecture gates, M4-K01/M4-K02/M4-K03 spike checks,
//! all 20 cases from `docs/delivery/m4-stage-cases.json` (`M4-A-01..18`, `M4-M-01`, `M4-L-01`),
//! cross-process `delta-cli training` <-> `AppHost` sentinel/replay/DTO parity probes,
//! optional release hidden-window `--m4-ui-smoke` & `--m4-ui-bench` (`p95 <= 100.0 ms`),
//! quietness/data-isolation checks (`D-25`, `D-26`), and same-fingerprint `M1`/`M2`/`M3`
//! regression verification (`M4-A-18`).

use super::{
    verify::{
        directory_snapshot, failure, owned_test_directories, record_tests, source_files,
        source_fingerprint,
    },
    CaseResultRecord, PocResultRecord, VerifyStageReport,
};
use chrono::{Days, NaiveDate, Utc};
use delta_app::{
    count_visible_windows_for_current_process, current_process_priority, observed_window_details,
    resolve_app_paths, resolve_user_documents_root, run_quiet_command,
    run_quiet_command_with_timeout, sanitize_terminal_output, AdjustmentKind, AppHost, AppService,
    BarRecord, BuildChannel, CorporateActionType, ErrorEnvelope, QuietJobObject,
    TrainingReplayMode, TrainingReplayResultDto, TrainingReportDto, TrainingRevealDto,
    TrainingWorkspaceSnapshotDto, EXECUTION_SPEED_OPT_OUT, SIMULATED_TRAINING_BADGE,
};
use delta_core::{parse_decimal_lossless, CalendarSessionKind};
use delta_data::{query_calendar_session, SqliteStore};
use delta_desktop::M4_K03_P95_BUDGET_MS;
use rusqlite::params;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

fn ensure(condition: bool, message: impl std::fmt::Display) -> Result<(), ErrorEnvelope> {
    if condition {
        Ok(())
    } else {
        Err(failure("M4_CHECK", message))
    }
}

// ---------------------------------------------------------------------------
// Quietness monitor (`D-25`, `M4-A-18`): observes the gate process and any
// test/release child process spawned from the workspace `target/` directory.
// ---------------------------------------------------------------------------

struct M4QuietnessMonitor {
    stop: Arc<AtomicBool>,
    findings: Arc<Mutex<BTreeSet<String>>>,
    samples: Arc<Mutex<u64>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
fn process_image(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(process);
        (ok != 0).then(|| String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

#[cfg(windows)]
fn sample_m4_windows(target_prefix_lower: &str, findings: &Mutex<BTreeSet<String>>) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    struct VisitCtx<'a> {
        self_pid: u32,
        target_prefix_lower: &'a str,
        findings: &'a Mutex<BTreeSet<String>>,
    }

    fn describe_owned(hwnd: HWND, ctx: &VisitCtx<'_>) -> Option<String> {
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return None;
            }
            let image = process_image(pid).unwrap_or_default();
            let norm = image.replace('/', "\\").to_ascii_lowercase();
            let owned = pid == ctx.self_pid
                || (!ctx.target_prefix_lower.is_empty()
                    && norm.starts_with(ctx.target_prefix_lower));
            if !owned {
                return None;
            }
            let mut title = [0u16; 256];
            let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0);
            Some(format!(
                "pid={pid} {} \"{}\"",
                image.rsplit(['\\', '/']).next().unwrap_or(&image),
                String::from_utf16_lossy(&title[..length as usize])
            ))
        }
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = &*(lparam as *const VisitCtx<'_>);
        if delta_app::window_is_perceivable(hwnd as isize) {
            if let Some(text) = describe_owned(hwnd, ctx) {
                ctx.findings
                    .lock()
                    .unwrap()
                    .insert(format!("visible window: {text}"));
            }
        }
        1
    }

    let ctx = VisitCtx {
        self_pid: std::process::id(),
        target_prefix_lower,
        findings,
    };
    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.is_null() {
            if let Some(text) = describe_owned(foreground, &ctx) {
                findings
                    .lock()
                    .unwrap()
                    .insert(format!("foreground stolen: {text}"));
            }
        }
        EnumWindows(Some(visit), &ctx as *const VisitCtx<'_> as LPARAM);
    }
}

#[cfg(not(windows))]
fn sample_m4_windows(_target_prefix_lower: &str, _findings: &Mutex<BTreeSet<String>>) {}

impl M4QuietnessMonitor {
    fn start(root: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let findings = Arc::new(Mutex::new(BTreeSet::new()));
        let samples = Arc::new(Mutex::new(0u64));
        let target_prefix_lower = root
            .join("target")
            .display()
            .to_string()
            .replace('/', "\\")
            .trim_start_matches("\\\\?\\")
            .to_ascii_lowercase();
        let thread = {
            let (stop, findings, samples) = (
                Arc::clone(&stop),
                Arc::clone(&findings),
                Arc::clone(&samples),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    sample_m4_windows(&target_prefix_lower, &findings);
                    *samples.lock().unwrap() += 1;
                    std::thread::sleep(Duration::from_millis(150));
                }
            })
        };
        Self {
            stop,
            findings,
            samples,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> (u64, Vec<String>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let samples = *self.samples.lock().unwrap();
        let findings = self.findings.lock().unwrap().iter().cloned().collect();
        (samples, findings)
    }
}

// ---------------------------------------------------------------------------
// Independent validation of `M4-K03` / `M4-A-17` raw benchmark samples
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValidatedM4Bench {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub measured_steps: usize,
    pub native_chart_paints: u64,
}

pub(crate) fn validate_m4_next_bar_bench(bench: &Value) -> Result<ValidatedM4Bench, String> {
    if bench["status"].as_str() != Some("passed") || bench["passed"].as_bool() != Some(true) {
        return Err("benchmark self-reported non-passing status".into());
    }
    if bench["build_profile"].as_str() != Some("release") {
        return Err(format!(
            "release build required, got {:?}",
            bench["build_profile"]
        ));
    }
    if bench["power_throttling"].as_str() != Some(EXECUTION_SPEED_OPT_OUT) {
        return Err(format!(
            "execution_speed_opt_out required, got {:?}",
            bench["power_throttling"]
        ));
    }
    if bench["native_window_created"].as_bool() != Some(true) {
        return Err("native_window_created must be true".into());
    }
    if bench["visible_windows"].as_u64() != Some(0) {
        return Err(format!(
            "visible_windows must be 0, got {:?}",
            bench["visible_windows"]
        ));
    }
    if bench["focus_stolen"].as_bool() != Some(false) {
        return Err("focus_stolen must be false".into());
    }
    if bench["history_bars"].as_u64() != Some(1000)
        || bench["kline_visible_bars"].as_u64() != Some(1000)
    {
        return Err(format!(
            "expected 1000 history/visible bars, got {:?}/{:?}",
            bench["history_bars"], bench["kline_visible_bars"]
        ));
    }
    if bench["warmup_steps"].as_u64().is_none_or(|w| w < 20) {
        return Err(format!(
            "expected >= 20 warmup_steps, got {:?}",
            bench["warmup_steps"]
        ));
    }
    if bench["measured_steps"].as_u64() != Some(300) {
        return Err(format!(
            "expected 300 measured_steps, got {:?}",
            bench["measured_steps"]
        ));
    }
    let Some(native_chart_paints) = bench["native_chart_paints"].as_u64() else {
        return Err("missing native_chart_paints".into());
    };
    if native_chart_paints < 300 {
        return Err(format!(
            "native_chart_paints must be >= 300, got {native_chart_paints}"
        ));
    }
    let Some(us_arr) = bench["step_times_us"].as_array() else {
        return Err("missing step_times_us array".into());
    };
    let Some(ms_arr) = bench["step_times_ms"].as_array() else {
        return Err("missing step_times_ms array".into());
    };
    let Some(mut times_us) = us_arr
        .iter()
        .map(Value::as_u64)
        .collect::<Option<Vec<u64>>>()
    else {
        return Err("step_times_us contains non-u64 entry".into());
    };
    if times_us.len() != 300 || ms_arr.len() != 300 || times_us.contains(&0) {
        return Err(format!(
            "expected 300 non-zero raw samples, got us={} ms={}",
            times_us.len(),
            ms_arr.len()
        ));
    }

    times_us.sort_unstable();
    let pct_us = |percent: usize| -> u64 { times_us[(times_us.len() * percent).div_ceil(100) - 1] };
    let p50_ms = (pct_us(50) as f64) / 1000.0;
    let p95_ms = (pct_us(95) as f64) / 1000.0;
    let p99_ms = (pct_us(99) as f64) / 1000.0;
    let max_ms = (*times_us.last().unwrap_or(&0) as f64) / 1000.0;

    for (field, recomputed) in [
        ("p50_ms", p50_ms),
        ("p95_ms", p95_ms),
        ("p99_ms", p99_ms),
        ("max_ms", max_ms),
    ] {
        let Some(reported) = bench[field].as_f64() else {
            return Err(format!("missing numeric {field}"));
        };
        if (reported - recomputed).abs() > 1e-6 {
            return Err(format!(
                "{field} mismatch: reported {reported} vs recomputed {recomputed}"
            ));
        }
    }

    if p95_ms > M4_K03_P95_BUDGET_MS {
        return Err(format!(
            "p95_ms {p95_ms:.3} exceeds budget {:.1} ms",
            M4_K03_P95_BUDGET_MS
        ));
    }

    Ok(ValidatedM4Bench {
        p50_ms,
        p95_ms,
        p99_ms,
        max_ms,
        measured_steps: times_us.len(),
        native_chart_paints,
    })
}

pub(crate) fn validate_m4_ui_smoke(smoke: &Value) -> Result<(), String> {
    if smoke["status"].as_str() != Some("passed") {
        return Err(format!("smoke status != passed: {:?}", smoke["status"]));
    }
    if smoke["build_profile"].as_str() != Some("release") {
        return Err(format!(
            "smoke build_profile != release: {:?}",
            smoke["build_profile"]
        ));
    }
    if smoke["power_throttling"].as_str() != Some(EXECUTION_SPEED_OPT_OUT) {
        return Err(format!(
            "smoke power_throttling != execution_speed_opt_out: {:?}",
            smoke["power_throttling"]
        ));
    }
    if smoke["native_window_created"].as_bool() != Some(true) {
        return Err("smoke native_window_created != true".into());
    }
    if smoke["visible_windows"].as_u64() != Some(0) {
        return Err(format!(
            "smoke visible_windows != 0: {:?}",
            smoke["visible_windows"]
        ));
    }
    if smoke["focus_stolen"].as_bool() != Some(false) {
        return Err("smoke focus_stolen != false".into());
    }
    if smoke["simulated_badge"].as_str() != Some(SIMULATED_TRAINING_BADGE) {
        return Err(format!(
            "smoke simulated_badge mismatch: {:?}",
            smoke["simulated_badge"]
        ));
    }
    if smoke["workspace_chart_paints"]
        .as_u64()
        .is_none_or(|n| n == 0)
    {
        return Err(format!(
            "smoke workspace_chart_paints must be > 0, got {:?}",
            smoke["workspace_chart_paints"]
        ));
    }
    if smoke["revealed_post_finish_bars"].as_u64() != Some(60) {
        return Err(format!(
            "smoke revealed_post_finish_bars != 60: {:?}",
            smoke["revealed_post_finish_bars"]
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// In-gate live cross-process `delta-cli training` + `AppHost` verification
// ---------------------------------------------------------------------------

struct InGateE2eEvidence {
    run_id: String,
    candidate_pool_hash: String,
    final_state_hash: String,
    checkpoint_started_seq: i64,
    ledger_revision: i64,
    elapsed_ms: u64,
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
        cur = cur.checked_add_days(Days::new(1)).expect("valid date");
    }
    out
}

fn make_equity_bar(instrument_id: &str, d: NaiveDate, close_px: &str, dsv: i64) -> BarRecord {
    let sess = query_calendar_session("XNAS", d);
    let px = parse_decimal_lossless(close_px).expect("decimal");
    BarRecord {
        id: format!("BAR:{instrument_id}:1d:raw:yahoo:{d}:{dsv}"),
        instrument_id: instrument_id.to_string(),
        timeframe: "1d".to_string(),
        session: sess.kind.as_str().to_string(),
        session_date: d,
        open_at: sess.open_at.expect("equity open_at"),
        close_at: sess.close_at.expect("equity close_at"),
        open: px,
        high: px + Decimal::from(3),
        low: (px - Decimal::from(2)).max(Decimal::ONE),
        close: px,
        volume: Decimal::from(150_000),
        source: "yahoo".to_string(),
        adjustment: AdjustmentKind::Raw,
        is_final: true,
        valid_from_version: dsv,
        valid_to_version: None,
        raw_payload_hash: None,
    }
}

fn run_in_gate_m4_e2e_probe(
    root: &Path,
    temp: &Path,
    gate_env: &BTreeMap<String, String>,
) -> Result<InGateE2eEvidence, ErrorEnvelope> {
    let started = Instant::now();
    let probe_root = temp.join("m4-e2e-probe");
    let data_dir = probe_root.join("data");
    fs::create_dir_all(&data_dir).map_err(|e| failure("m4_e2e", e))?;
    let exe = env::current_exe().map_err(|e| failure("m4_e2e_exe", e))?;

    let mut paths = resolve_app_paths(Some(BuildChannel::Source), Some(&data_dir))
        .map_err(|e| failure("m4_e2e_paths", e))?;
    paths.documents_dir = probe_root.join("docs");
    paths.default_export_dir = paths.documents_dir.join("exports");
    paths.default_backup_dir = paths.documents_dir.join("backups");
    paths.credential_prefix = format!("delta-test-m4-gate-{}", std::process::id());

    let (rev_before, dsv) = {
        let aapl = {
            let mut svc = AppService::open(paths.clone(), "USD", "USD", "UTC", true)?;
            svc.register_market_instrument("NASDAQ", "AAPL", false)?
        };
        let mut store = SqliteStore::open_or_init(&data_dir, "USD", "USD", "UTC")
            .map_err(|e| failure("m4_e2e_store", e))?;
        let dsv = store
            .create_dataset_version(
                "gate_m4_e2e",
                "official_free_personal",
                "{}",
                "exact",
                "exact",
            )
            .map_err(|e| failure("m4_e2e_dsv", e))?
            .version_num;
        let days = trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(), 140);
        let bars: Vec<BarRecord> = days
            .iter()
            .enumerate()
            .map(|(idx, &d)| {
                let px = 160 + ((idx * 3) % 23) as i64;
                make_equity_bar(&aapl, d, &px.to_string(), dsv)
            })
            .collect();
        store
            .commit_bars_and_checkpoint(&bars, dsv, None)
            .map_err(|e| failure("m4_e2e_bars", e))?;
        store
            .publish_dataset_version(dsv)
            .map_err(|e| failure("m4_e2e_pub", e))?;
        let rev = store
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(|e| failure("m4_e2e_rev", e))?;
        (rev, dsv)
    };

    let run_cli = |sub_args: &[&str]| -> Result<String, ErrorEnvelope> {
        let mut full_args: Vec<String> = vec!["training".into()];
        full_args.extend(sub_args.iter().map(|s| (*s).to_string()));
        full_args.push("--data-dir".into());
        full_args.push(data_dir.display().to_string());
        let res = run_quiet_command_with_timeout(
            exe.as_os_str(),
            full_args,
            Some(root),
            gate_env,
            Duration::from_secs(30),
        )
        .map_err(|e| failure("m4_e2e_cli", e))?;
        ensure(
            res.status_code == Some(0) && !res.timed_out,
            format!(
                "delta-cli {:?} failed ({:?}): {}",
                sub_args, res.status_code, res.stderr
            ),
        )?;
        Ok(res.stdout)
    };

    // 1. Create hidden-mode run via real `delta-cli training create`
    let created_json = run_cli(&[
        "create",
        "--market",
        "us_equity_etf",
        "--instrument",
        "NASDAQ:AAPL",
        "--warmup-bars",
        "20",
        "--history-bars",
        "50",
        "--training-bars",
        "30",
        "--reveal-bars",
        "15",
        "--seed",
        "20261112",
        "--fee-profile",
        "us_equity_per_share_example",
        "--hidden",
        "--start",
    ])?;
    let created_val: Value =
        serde_json::from_str(&created_json).map_err(|e| failure("m4_e2e_json", e))?;
    let run_id = created_val["run_id"]
        .as_str()
        .ok_or_else(|| failure("m4_e2e_json", "missing run_id"))?
        .to_string();
    let candidate_pool_hash = created_val["candidate_pool_hash"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    ensure(
        !created_json.contains("AAPL") && !created_json.contains("2024-"),
        "hidden run create leaked symbol or date",
    )?;

    // 2. Inject future sentinel into bars & corporate actions strictly after visible_until (`M4-K01`)
    {
        let store = SqliteStore::open_or_init(&data_dir, "USD", "USD", "UTC")
            .map_err(|e| failure("m4_e2e_store", e))?;
        let session = store
            .get_training_session(&run_id)
            .map_err(|e| failure("m4_e2e_sess", e))?;
        let vis_str = session.visible_until.to_rfc3339();
        let mut stmt = store
            .conn()
            .prepare("SELECT session_date FROM bar WHERE instrument_id = 'NASDAQ:AAPL' AND close_at > ?1 ORDER BY close_at ASC")
            .map_err(|e| failure("m4_e2e_sql", e))?;
        let dates: Vec<String> = stmt
            .query_map(params![vis_str], |r| r.get::<_, String>(0))
            .map_err(|e| failure("m4_e2e_sql", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| failure("m4_e2e_sql", e))?;
        ensure(
            dates.len() >= 45,
            format!(
                "expected >= 45 future bars after visible_until, got {}",
                dates.len()
            ),
        )?;
        // Poison a bar strictly after the training + reveal window with extreme sentinels
        let poison_date_str = dates.last().unwrap().clone();
        store
            .conn()
            .execute(
                "UPDATE bar SET open='999999999', high='999999999', low='999999999', close='999999999', source='SENTINEL_FUTURE_SOURCE' WHERE instrument_id='NASDAQ:AAPL' AND session_date=?1",
                params![poison_date_str],
            )
            .map_err(|e| failure("m4_e2e_sql", e))?;
        store
            .conn()
            .execute(
                "INSERT INTO corporate_action (
                    id, instrument_id, action_type, ex_date, pay_date,
                    ratio_or_amount, currency, announced_at, source, factor_version, raw_payload_hash
                 ) VALUES (?1, 'NASDAQ:AAPL', ?2, ?3, ?3, '999999', 'USD', NULL, 'SENTINEL_CA_SOURCE', ?4, NULL)",
                params![
                    format!("CA:SENTINEL:{poison_date_str}"),
                    CorporateActionType::Split.as_str(),
                    poison_date_str,
                    dsv
                ],
            )
            .map_err(|e| failure("m4_e2e_ca", e))?;
    }

    let show_before = run_cli(&["show", "--run-id", &run_id])?;
    for forbidden in [
        "999999999",
        "SENTINEL_FUTURE_SOURCE",
        "SENTINEL_CA_SOURCE",
        "AAPL",
        "2024-",
    ] {
        ensure(
            !show_before.contains(forbidden),
            format!("CLI show leaked '{forbidden}' before reveal"),
        )?;
    }

    // 3. Submit market buy, advance 22 bars (past Periodic20 checkpoint), add note, finish
    let _ = run_cli(&[
        "order", "market", "--run-id", &run_id, "--side", "buy", "--qty", "10", "--tif", "day",
    ])?;
    let _ = run_cli(&["advance", "--run-id", &run_id, "--steps", "22"])?;
    let _ = run_cli(&[
        "note",
        "--run-id",
        &run_id,
        "--rationale",
        "M4 gate cross-process verification note",
        "--emotion",
        "calm",
    ])?;
    let finish_json = run_cli(&["finish", "--run-id", &run_id])?;
    ensure(
        !finish_json.contains("AAPL") && !finish_json.contains("999999999"),
        "pre-reveal finish leaked identity or sentinel",
    )?;

    // 4. Cross-process CLI vs in-process `AppHost` parity + 3-way deterministic replay (`M4-K02`, `M4-A-10`, `M4-A-15`)
    let cli_show_json = run_cli(&["show", "--run-id", &run_id])?;
    let cli_report_json = run_cli(&["report", "--run-id", &run_id])?;
    let cli_replay_json = run_cli(&["replay", "--run-id", &run_id, "--mode", "scratch"])?;

    let cli_snap: TrainingWorkspaceSnapshotDto =
        serde_json::from_str(&cli_show_json).map_err(|e| failure("m4_e2e_parse", e))?;
    let cli_rep: TrainingReportDto =
        serde_json::from_str(&cli_report_json).map_err(|e| failure("m4_e2e_parse", e))?;
    let cli_replay: TrainingReplayResultDto =
        serde_json::from_str(&cli_replay_json).map_err(|e| failure("m4_e2e_parse", e))?;

    let (final_state_hash, checkpoint_started_seq) = {
        let host =
            AppHost::from_service(AppService::open(paths.clone(), "USD", "USD", "UTC", true)?);
        let ui_snap = host.get_training_workspace_snapshot(&run_id, None, 320)?;
        let ui_rep = host.get_training_report(&run_id)?;
        let scratch = host.replay_training_run(&run_id, TrainingReplayMode::Scratch)?;
        let ckpt = host.replay_training_run(&run_id, TrainingReplayMode::FromCheckpoint)?;
        host.shutdown()?;

        ensure(
            cli_snap.run == ui_snap.run
                && cli_snap.account == ui_snap.account
                && cli_snap.position == ui_snap.position
                && cli_snap.all_orders == ui_snap.all_orders
                && cli_snap.fills == ui_snap.fills
                && cli_snap.notes == ui_snap.notes
                && cli_rep == ui_rep,
            "CLI and AppHost DTO values diverged on the same run",
        )?;
        ensure(
            scratch.matches_persisted_hashes
                && ckpt.matches_persisted_hashes
                && cli_replay.matches_persisted_hashes
                && ckpt.started_from_seq >= 20
                && scratch.final_state_hash == ckpt.final_state_hash
                && scratch.final_state_hash == cli_replay.final_state_hash
                && scratch.report == ckpt.report
                && scratch.report == cli_replay.report,
            "3-way deterministic replay hashes or reports diverged",
        )?;
        (scratch.final_state_hash, ckpt.started_from_seq)
    };

    // 5. Explicit `reveal` exposes real instrument & 15 post-finish bars while excluding poison bar beyond reveal_end
    let reveal_json = run_cli(&["reveal", "--run-id", &run_id])?;
    let revealed: TrainingRevealDto =
        serde_json::from_str(&reveal_json).map_err(|e| failure("m4_e2e_reveal", e))?;
    ensure(
        revealed.real_instrument_id == "NASDAQ:AAPL"
            && revealed.post_finish_bars.len() == 15
            && !reveal_json.contains("999999999")
            && !reveal_json.contains("SENTINEL_FUTURE_SOURCE"),
        "post-finish reveal mismatch or leaked beyond reveal_end_at",
    )?;

    // 6. Non-TTY `training delete` requires `--yes`, `--preview` does not mutate, `--yes` deletes cleanly; `ledger_revision` unchanged
    let non_tty_del = run_quiet_command_with_timeout(
        exe.as_os_str(),
        [
            "training",
            "delete",
            "--run-id",
            &run_id,
            "--data-dir",
            &data_dir.display().to_string(),
        ],
        Some(root),
        gate_env,
        Duration::from_secs(15),
    )
    .map_err(|e| failure("m4_e2e_del", e))?;
    ensure(
        non_tty_del.status_code != Some(0)
            && non_tty_del.stderr.contains("confirmation_required_non_tty"),
        "non-TTY training delete without --yes must fail with confirmation_required_non_tty",
    )?;
    let _ = run_cli(&["delete", "--run-id", &run_id, "--preview"])?;
    let _ = run_cli(&["delete", "--run-id", &run_id, "--yes"])?;

    {
        let svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
        ensure(
            svc.list_training_runs()?.is_empty(),
            "training run still present after explicit delete --yes",
        )?;
    }
    let rev_after = {
        let store = SqliteStore::open_or_init(&data_dir, "USD", "USD", "UTC")
            .map_err(|e| failure("m4_e2e_store", e))?;
        store
            .conn()
            .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(|e| failure("m4_e2e_rev", e))?
    };
    ensure(
        rev_before == rev_after,
        format!("real ledger_revision changed ({rev_before} -> {rev_after})"),
    )?;

    let _ = fs::remove_dir_all(&probe_root);
    Ok(InGateE2eEvidence {
        run_id,
        candidate_pool_hash,
        final_state_hash,
        checkpoint_started_seq,
        ledger_revision: rev_after,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// Architecture & boundary check (`M4-G-06`)
// ---------------------------------------------------------------------------

fn run_m4_architecture_and_hygiene_check(
    root: &Path,
    gate_env: &BTreeMap<String, String>,
) -> Result<(Vec<String>, usize), ErrorEnvelope> {
    ensure(
        !root.join("Packager.toml").exists(),
        "Packager.toml must be deleted in M4-W07",
    )?;
    let verify_rs = fs::read_to_string(root.join("crates/delta-cli/src/verify.rs"))
        .map_err(|e| failure("M4_G06", e))?;
    ensure(
        !verify_rs.contains("Packager.toml")
            && verify_rs.contains("docs/delivery/m4-stage-cases.json"),
        "verify.rs source_fingerprint must remove Packager.toml and include m4-stage-cases.json",
    )?;

    let cases_json: Value = serde_json::from_slice(
        &fs::read(root.join("docs/delivery/m4-stage-cases.json"))
            .map_err(|e| failure("M4_G06", e))?,
    )
    .map_err(|e| failure("M4_G06", e))?;
    let case_ids: Vec<String> = cases_json["cases"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ensure(
        cases_json["schemaVersion"] == 1
            && cases_json["stage"] == "M4"
            && cases_json["planVersion"] == "1.0"
            && case_ids.len() == 20,
        format!(
            "m4-stage-cases.json header or count invalid (found {} cases)",
            case_ids.len()
        ),
    )?;

    // 1. `delta-core` purity: 0 IO/UI crates; `sim.rs` & `sim_report.rs` 0 f64/f32/HashMap/HashSet
    let mut core_files = Vec::new();
    source_files(&root.join("crates/delta-core/src"), &mut core_files)
        .map_err(|e| failure("M4_G06", e))?;
    for file in &core_files {
        let text = fs::read_to_string(file).map_err(|e| failure("M4_G06", e))?;
        for forbidden in ["tokio", "reqwest", "gpui", "rusqlite"] {
            ensure(
                !text.contains(forbidden),
                format!(
                    "delta-core {:?} contains forbidden '{forbidden}'",
                    file.file_name()
                ),
            )?;
        }
        let fname = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(fname, "sim.rs" | "sim_report.rs") {
            for (lineno, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                for tok in ["f64", "f32", "HashMap", "HashSet"] {
                    ensure(
                        !trimmed.contains(tok),
                        format!("delta-core {fname}:{} contains '{tok}'", lineno + 1),
                    )?;
                }
            }
        }
    }

    // 2. `delta-app` boundary: 0 `pub async fn`; 0 raw `FROM bar` SQL in training/ai/host prod code (`S-37`)
    let mut app_files = Vec::new();
    source_files(&root.join("crates/delta-app/src"), &mut app_files)
        .map_err(|e| failure("M4_G06", e))?;
    for file in &app_files {
        let text = fs::read_to_string(file).map_err(|e| failure("M4_G06", e))?;
        ensure(
            !text.contains("pub async fn"),
            format!("delta-app {:?} leaks pub async fn", file.file_name()),
        )?;
        let fname = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !matches!(fname, "export.rs" | "sync.rs") {
            let prod = text
                .split_once("#[cfg(test)]")
                .map_or(text.as_str(), |(p, _)| p);
            ensure(
                !prod.to_ascii_lowercase().contains("from bar"),
                format!("delta-app {fname} queries `FROM bar` directly outside SqliteStore port"),
            )?;
        }
    }

    // 3. `delta-desktop` dependency boundary (`cargo metadata --locked` + source scan)
    let meta_res = run_quiet_command(
        OsStr::new("cargo"),
        ["metadata", "--locked", "--format-version", "1"],
        Some(root),
        gate_env,
    )
    .map_err(|e| failure("M4_G06_META", e))?;
    ensure(
        meta_res.status_code == Some(0) && !meta_res.timed_out,
        "cargo metadata --locked failed",
    )?;
    let meta_json: Value =
        serde_json::from_str(&meta_res.stdout).map_err(|e| failure("M4_G06_META", e))?;
    let packages = meta_json["packages"]
        .as_array()
        .ok_or_else(|| failure("M4_G06_META", "missing packages"))?;
    let desktop_pkg = packages
        .iter()
        .find(|p| p["name"].as_str() == Some("delta-desktop"))
        .ok_or_else(|| failure("M4_G06_META", "delta-desktop missing"))?;
    let desktop_deps: Vec<String> = desktop_pkg["dependencies"]
        .as_array()
        .map(|deps| {
            deps.iter()
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ensure(
        desktop_deps.iter().any(|d| d == "delta-app"),
        "delta-desktop must depend on delta-app",
    )?;
    for forbidden_dep in [
        "delta-data",
        "delta-core",
        "delta-model",
        "rusqlite",
        "tokio",
        "reqwest",
    ] {
        ensure(
            !desktop_deps.iter().any(|d| d == forbidden_dep),
            format!("delta-desktop directly depends on forbidden crate '{forbidden_dep}'"),
        )?;
    }
    let mut desktop_files = Vec::new();
    source_files(&root.join("crates/delta-desktop/src"), &mut desktop_files)
        .map_err(|e| failure("M4_G06", e))?;
    for file in &desktop_files {
        let text = fs::read_to_string(file).map_err(|e| failure("M4_G06", e))?;
        for forbidden_use in [
            "delta_core::",
            "delta_data::",
            "rusqlite::",
            "tokio::",
            "reqwest::",
        ] {
            ensure(
                !text.contains(forbidden_use),
                format!(
                    "delta-desktop {:?} references '{forbidden_use}'",
                    file.file_name()
                ),
            )?;
        }
    }

    // 4. Workspace-wide `Command::new` & `0.0.0.0` scan
    let forbidden_bind = ["0.0", ".0.0"].concat();
    let forbidden_cmd = ["Command", "::new"].concat();
    let mut all_crate_files = Vec::new();
    source_files(&root.join("crates"), &mut all_crate_files).map_err(|e| failure("M4_G06", e))?;
    let mut rs_scanned = 0usize;
    for file in &all_crate_files {
        if !file.extension().is_some_and(|e| e == "rs") {
            continue;
        }
        rs_scanned += 1;
        let is_process_rs = file
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("crates/delta-app/src/process.rs");
        let text = fs::read_to_string(file).map_err(|e| failure("M4_G06", e))?;
        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            ensure(
                !trimmed.contains(&forbidden_bind),
                format!(
                    "{:?}:{} non-loopback bind address found",
                    file.file_name(),
                    lineno + 1
                ),
            )?;
            if !is_process_rs {
                ensure(
                    !trimmed.contains(&forbidden_cmd),
                    format!(
                        "{:?}:{} unauthorized process spawn outside process.rs",
                        file.file_name(),
                        lineno + 1
                    ),
                )?;
            }
        }
    }

    Ok((desktop_deps, rs_scanned))
}

// ---------------------------------------------------------------------------
// Same-fingerprint M1 / M2 / M3 regression check (`M4-A-18`)
// ---------------------------------------------------------------------------

fn verify_prior_stage_regressions(
    root: &Path,
    expected_fingerprint: &str,
) -> Result<String, ErrorEnvelope> {
    let mut summaries = Vec::new();
    for (dir_stage, expected_stage, expected_plan) in [
        ("m1", "M1", "2.6"),
        ("m2", "M2", "1.0"),
        ("m3", "M3", "1.0"),
    ] {
        let path = root.join(format!(
            ".local/reports/{dir_stage}/verify-stage-report.json"
        ));
        let raw = fs::read(&path).map_err(|e| {
            failure(
                "REGRESSION_MISSING",
                format!(
                    "{expected_stage} report missing at {} ({e}); run verify-stage --stage {expected_stage} first",
                    path.display()
                ),
            )
        })?;
        let rep: Value = serde_json::from_slice(&raw).map_err(|e| failure("REGRESSION_JSON", e))?;
        ensure(
            rep["stage"].as_str() == Some(expected_stage)
                && rep["plan_version"].as_str() == Some(expected_plan)
                && rep["overall_status"].as_str() == Some("passed")
                && rep["source_fingerprint"].as_str() == Some(expected_fingerprint),
            format!(
                "{expected_stage} regression mismatch: stage={:?} plan={:?} status={:?} fp={:?} (expected fp={expected_fingerprint})",
                rep["stage"], rep["plan_version"], rep["overall_status"], rep["source_fingerprint"]
            ),
        )?;
        if expected_stage == "M2" {
            let has_bench = rep["case_results"].as_array().is_some_and(|arr| {
                arr.iter()
                    .any(|c| c["case_id"] == "M2-A-20" && c["status"] == "passed")
            });
            let has_smoke = rep["poc_results"].as_array().is_some_and(|arr| {
                arr.iter()
                    .any(|p| p["id"] == "M2-K06" && p["status"] == "passed")
            });
            ensure(
                has_bench && has_smoke,
                "M2 regression report must include passed --ui-smoke (M2-K06) and --ui-bench (M2-A-20)",
            )?;
        }
        if expected_stage == "M3" {
            let has_installer_and_smoke = rep["case_results"].as_array().is_some_and(|arr| {
                arr.iter().any(|c| {
                    c["case_id"] == "M3-A-03"
                        && c["status"] == "passed"
                        && c["evidence"]
                            .as_str()
                            .is_some_and(|ev| ev.contains("installed --ui-smoke passed"))
                })
            });
            ensure(
                has_installer_and_smoke,
                "M3 regression report must include passed --installer and --ui-smoke (M3-A-03)",
            )?;
        }
        summaries.push(format!(
            "{expected_stage} v{expected_plan} passed @ {}",
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
    extra_evidence: Option<&str>,
) {
    let all_passed = names
        .iter()
        .all(|n| report.test_results.get(*n).is_some_and(|s| s == "passed"));
    let status = if all_passed { "passed" } else { "failed" };
    let mut evidence = format!("M4-G-05 captured test results: {}", names.join(", "));
    if let Some(extra) = extra_evidence {
        evidence.push_str("; ");
        evidence.push_str(extra);
    }
    report.case_results.push(CaseResultRecord {
        case_id: id.into(),
        work_package: package.into(),
        tier: "required".into(),
        status: status.into(),
        elapsed_ms: 0,
        evidence,
    });
}

// ---------------------------------------------------------------------------
// Main `verify-stage --stage M4 --plan-version 1.0` entrypoint
// ---------------------------------------------------------------------------

pub(crate) fn run_verify_stage_m4(
    ui_smoke: bool,
    ui_bench: bool,
) -> Result<ExitCode, ErrorEnvelope> {
    let root = env::current_dir().map_err(|e| failure("cwd", e))?;
    let fingerprint = source_fingerprint(&root)?;
    let run_id = format!("{}-{}", std::process::id(), Utc::now().timestamp_millis());
    let temp = env::temp_dir().join(format!("delta_m4_verify_{run_id}"));
    fs::create_dir(&temp).map_err(|e| failure("temp", e))?;
    let reports = root.join(".local/reports/m4");
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
    let test_directories_before =
        owned_test_directories().map_err(|e| failure("temp_snapshot", e))?;
    let release_before = appdata
        .as_ref()
        .map(|p| p.join("delta"))
        .map(|p| (p.exists(), p.metadata().and_then(|m| m.modified()).ok()));

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
        stage: "M4".into(),
        plan_version: "1.0".into(),
        executed_at: Utc::now().to_rfc3339(),
        live_mode: false,
        source_fingerprint: fingerprint,
        visible_window_count: 0,
        window_details: Vec::new(),
        temp_residual_dirs: 0,
        outside_temp_created_dirs: 0,
        path_observation_scope: "directory metadata under real LOCALAPPDATA/delta, delta-dev and Documents/delta, delta-dev; all gate and test roots isolated in TEMP; EnumWindows + GetForegroundWindow quietness monitor active".into(),
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

        // 1. Fast-fail Mechanical Gates (`M4-G-01`..`M4-G-05`)
        for (id, program, args) in [
            ("M4-G-01", "python", vec!["scripts/check_docs.py"]),
            ("M4-G-02", "git", vec!["diff", "--check"]),
            ("M4-G-03", "cargo", vec!["fmt", "--all", "--check"]),
            (
                "M4-G-04",
                "cargo",
                vec![
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            ("M4-G-05", "cargo", vec!["test", "--workspace", "--locked"]),
        ] {
            let started = Instant::now();
            let result =
                run_quiet_command(OsStr::new(program), args.clone(), Some(&root), &gate_env)
                    .map_err(|e| failure("gate_command", e))?;
            let path = logs.join(format!("{id}.json"));
            fs::write(
                &path,
                serde_json::to_vec_pretty(&result).map_err(|e| failure("gate_log", e))?,
            )
            .map_err(|e| failure("gate_log", e))?;
            let rel_log = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string()
                .replace('\\', "/");
            report.log_files.insert(id.into(), rel_log.clone());
            if id == "M4-G-05" {
                record_tests(&result.stdout, &mut report.test_results);
            }
            let passed = result.status_code == Some(0) && !result.timed_out;
            report.case_results.push(CaseResultRecord {
                case_id: id.into(),
                work_package: "M4-W07".into(),
                tier: "required".into(),
                status: if passed { "passed" } else { "failed" }.into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                evidence: format!(
                    "{program} {}; exit={:?}; captured log {rel_log}",
                    args.join(" "),
                    result.status_code
                ),
            });
            if !passed {
                return Err(failure("gate_failed", format!("{id}: see {rel_log}")));
            }
        }

        // 2. `M4-G-06`: Architecture, SQL port, Packager.toml retirement & hygiene check
        let g06_start = Instant::now();
        let (desktop_deps, rs_scanned) = run_m4_architecture_and_hygiene_check(&root, &gate_env)?;
        report.case_results.push(CaseResultRecord {
            case_id: "M4-G-06".into(),
            work_package: "M4-W07".into(),
            tier: "required".into(),
            status: "passed".into(),
            elapsed_ms: g06_start.elapsed().as_millis() as u64,
            evidence: format!(
                "Packager.toml retired; 20 cases in m4-stage-cases.json; delta-core Decimal/BTree purity verified; delta-app 0 pub async fn & 0 raw FROM bar outside SqliteStore port; delta-desktop direct deps={desktop_deps:?}; {rs_scanned} Rust source files clean of unauthorized process spawns and non-loopback binds"
            ),
        });

        // 3. In-gate live cross-process `delta-cli training` + `AppHost` sentinel/replay/parity probe
        let e2e = run_in_gate_m4_e2e_probe(&root, &temp, &gate_env)?;

        // 4. Optional release `delta.exe` build + `--m4-ui-smoke`
        let release_delta_exe = if ui_smoke || ui_bench {
            let build_start = Instant::now();
            let build_res = run_quiet_command(
                OsStr::new("cargo"),
                [
                    "build",
                    "--release",
                    "-p",
                    "delta-desktop",
                    "--bin",
                    "delta",
                    "--locked",
                ],
                Some(&root),
                &gate_env,
            )
            .map_err(|e| failure("release_build_desktop", e))?;
            let build_log = logs.join("cargo-build-release-desktop.json");
            fs::write(
                &build_log,
                serde_json::to_vec_pretty(&build_res).map_err(|e| failure("gate_log", e))?,
            )
            .map_err(|e| failure("gate_log", e))?;
            report.log_files.insert(
                "cargo-build-release-desktop".into(),
                build_log
                    .strip_prefix(&root)
                    .unwrap_or(&build_log)
                    .display()
                    .to_string()
                    .replace('\\', "/"),
            );
            ensure(
                build_res.status_code == Some(0) && !build_res.timed_out,
                format!(
                    "cargo build --release -p delta-desktop failed ({} ms): {}",
                    build_start.elapsed().as_millis(),
                    build_res.stderr
                ),
            )?;
            let exe_path = root.join("target/release/delta.exe");
            ensure(
                exe_path.is_file(),
                format!("expected release binary at {}", exe_path.display()),
            )?;
            Some(exe_path)
        } else {
            None
        };

        if ui_smoke {
            let desktop_exe = release_delta_exe.as_ref().unwrap();
            let smoke_res = run_quiet_command(
                desktop_exe.as_os_str(),
                [
                    "--m4-ui-smoke",
                    "--data-dir",
                    &temp.join("m4-ui-smoke-data").display().to_string(),
                ],
                Some(&root),
                &gate_env,
            )
            .map_err(|e| failure("m4_ui_smoke", e))?;
            let log_path = logs.join("m4-ui-smoke.json");
            fs::write(
                &log_path,
                serde_json::to_vec_pretty(&smoke_res).map_err(|e| failure("smoke_log", e))?,
            )
            .map_err(|e| failure("smoke_log", e))?;
            let rel_log = log_path
                .strip_prefix(&root)
                .unwrap_or(&log_path)
                .display()
                .to_string()
                .replace('\\', "/");
            report.log_files.insert("m4-ui-smoke".into(), rel_log);
            let smoke_report_path = reports.join("m4-ui-smoke-report.json");
            fs::write(&smoke_report_path, &smoke_res.stdout)
                .map_err(|e| failure("smoke_report", e))?;
            ensure(
                smoke_res.status_code == Some(0) && !smoke_res.timed_out,
                format!("delta.exe --m4-ui-smoke failed: {}", smoke_res.stderr),
            )?;
            let smoke_val: Value =
                serde_json::from_str(&smoke_res.stdout).map_err(|e| failure("smoke_report", e))?;
            validate_m4_ui_smoke(&smoke_val).map_err(|msg| failure("m4_ui_smoke_invalid", msg))?;
            ui_smoke_summary = Some(format!(
                "release --m4-ui-smoke passed ({} chart paints, {} post-finish bars, visible_windows=0, focus_stolen=false, power_throttling={})",
                smoke_val["workspace_chart_paints"],
                smoke_val["revealed_post_finish_bars"],
                smoke_val["power_throttling"]
            ));
        }

        // 5. Record Spikes `M4-K01` and `M4-K02`
        let k01_tests = [
            "m4_schema::tests::test_m4_k01_and_a01_sql_truncation_with_sentinels_clock_monotonicity_and_reveal_gate",
            "training::tests::test_m4_a01_and_k01_replay_clock_truncation_with_future_sentinels",
            "training_cmd::tests::test_m4_cli_hidden_mode_and_future_sentinel_isolation",
        ];
        let k01_ok = k01_tests
            .iter()
            .all(|t| report.test_results.get(*t).is_some_and(|s| s == "passed"));
        let mut k01_metrics = BTreeMap::new();
        k01_metrics.insert("e2e_run_id".into(), json!(e2e.run_id));
        k01_metrics.insert("sentinel_leaks".into(), json!(0));
        k01_metrics.insert("hidden_identity_leaks".into(), json!(0));
        report.poc_results.push(PocResultRecord {
            id: "M4-K01".into(),
            title: "SQL/host truncated training data port with numeric & string future sentinels".into(),
            status: if k01_ok { "passed" } else { "failed" }.into(),
            metrics: k01_metrics,
            detail: "SQL close_at <= visible_until & effective_date(visible_until) truncation verified across chart slice, indicators, order validation, CLI show and AI tools with extreme 999999999 & poison-string sentinels.".into(),
        });

        let k02_tests = [
            "sim::tests::m4_k02_deterministic_state_hash_replay_and_checkpoint_equivalence",
            "m4_schema::tests::test_m4_a09_and_a10_atomic_step_rollback_checkpoint_recovery_damage_and_retry",
            "training::tests::test_m4_a09_and_k02_a10_pause_resume_subprocess_crash_recovery_replay_and_retry",
        ];
        let k02_ok = k02_tests
            .iter()
            .all(|t| report.test_results.get(*t).is_some_and(|s| s == "passed"));
        let mut k02_metrics = BTreeMap::new();
        k02_metrics.insert("final_state_hash".into(), json!(e2e.final_state_hash));
        k02_metrics.insert(
            "checkpoint_started_seq".into(),
            json!(e2e.checkpoint_started_seq),
        );
        k02_metrics.insert("three_way_hash_identical".into(), json!(true));
        report.poc_results.push(PocResultRecord {
            id: "M4-K02".into(),
            title: "3-way deterministic state_hash replay (Scratch / FromCheckpoint / second process)".into(),
            status: if k02_ok { "passed" } else { "failed" }.into(),
            metrics: k02_metrics,
            detail: format!(
                "Bit-for-bit identical canonical Decimal state_hash ({}) across scratch replay, checkpoint replay (seq={}) and second-process CLI replay.",
                e2e.final_state_hash, e2e.checkpoint_started_seq
            ),
        });

        // 6. Record Mapped Cases `M4-A-01` .. `M4-A-16`
        let e2e_note_a01 = format!(
            "in-gate cross-process sentinel probe ({} ms, 0 sentinel/identity leaks)",
            e2e.elapsed_ms
        );
        record_mapped_case(
            &mut report,
            "M4-A-01",
            "M4-W01",
            &k01_tests,
            Some(&e2e_note_a01),
        );
        record_mapped_case(
            &mut report,
            "M4-A-02",
            "M4-W01",
            &[
                "sim::tests::m4_a_02_deterministic_prng_and_rejection_sampling",
                "m4_schema::tests::test_m4_a02_candidate_pool_exclusions_determinism_and_redraw_rules",
                "training::tests::test_m4_a02_deterministic_sampling_candidate_pool_and_hidden_dto_scrubbing",
                "training_cmd::tests::test_m4_cli_hidden_mode_and_future_sentinel_isolation",
            ],
            Some(&format!("candidate_pool_hash={}", e2e.candidate_pool_hash)),
        );
        record_mapped_case(
            &mut report,
            "M4-A-03",
            "M4-W01,M4-W02",
            &[
                "sim::tests::m4_a_03_pit_split_adjustment_and_in_session_corporate_actions",
                "training::tests::test_m4_a03_recorded_yahoo_split_and_dividend_in_training_session",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-04",
            "M4-W02",
            &[
                "sim::tests::thin_slice_market_buy_and_sell_step_transition",
                "sim::tests::m4_a_04_market_limit_fills_gaps_and_tick_lot_validation",
                "training_cmd::tests::test_m4_cli_lifecycle_oco_stop_rejections_and_evidence",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-05",
            "M4-W02",
            &[
                "sim::tests::m4_a_05_stop_oco_path_ordering_ambiguous_fills_and_attached_brackets",
                "training_cmd::tests::test_m4_cli_lifecycle_oco_stop_rejections_and_evidence",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-06",
            "M4-W02",
            &["sim::tests::m4_a_06_reservations_insufficient_funds_at_fill_and_zero_volume_bars"],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-07",
            "M4-W02",
            &["sim::tests::m4_a_07_and_08_composable_fees_synthetic_partials_received_asset_and_versioning"],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-08",
            "M4-W02,M4-W03",
            &[
                "sim::tests::m4_a_07_and_08_composable_fees_synthetic_partials_received_asset_and_versioning",
                "m4_schema::tests::test_m4_a08_fee_profile_immutability_versioning_and_builtin_provenance",
                "training::tests::test_m4_a08_fee_profile_version_freezing_across_resume_and_replay",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-09",
            "M4-W03",
            &[
                "m4_schema::tests::test_m4_a09_and_a10_atomic_step_rollback_checkpoint_recovery_damage_and_retry",
                "training::tests::test_m4_a09_and_k02_a10_pause_resume_subprocess_crash_recovery_replay_and_retry",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-10",
            "M4-W03",
            &k02_tests,
            Some(&format!(
                "in-gate 3-way replay state_hash={} (checkpoint seq={})",
                e2e.final_state_hash, e2e.checkpoint_started_seq
            )),
        );
        record_mapped_case(
            &mut report,
            "M4-A-11",
            "M4-W03",
            &[
                "m4_schema::tests::test_m4_a11_and_a13_schema_5_migration_backup_ledger_isolation_delete_and_note_archive",
                "training::tests::test_m4_a11_schema4_migration_backup_ledger_isolation_export_and_delete",
                "training_cmd::tests::test_m4_cli_delete_non_tty_requires_yes_and_escapes_control_chars",
            ],
            Some(&format!(
                "in-gate ledger_revision={} invariant & non-TTY delete guard verified",
                e2e.ledger_revision
            )),
        );
        record_mapped_case(
            &mut report,
            "M4-A-12",
            "M4-W04",
            &[
                "sim_report::tests::m4_a_12_hand_calculated_report_scaled_entries_mae_mfe_plan_rate_and_open_pos",
                "sim_report::tests::m4_a_12_zero_losses_and_no_trades_explicit_ratio_states",
                "sim_report::tests::m4_a_12_in_trade_split_drawdown_and_zero_wins_metrics",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-13",
            "M4-W05",
            &[
                "m4_schema::tests::test_m4_a11_and_a13_schema_5_migration_backup_ledger_isolation_delete_and_note_archive",
                "training::tests::test_m4_a13_training_notes_without_orders_and_journal_archive",
            ],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-14",
            "M4-W05",
            &["training::tests::test_m4_a14_ai_review_finished_only_both_protocols_citations_injection_and_offline"],
            None,
        );
        record_mapped_case(
            &mut report,
            "M4-A-15",
            "M4-W06",
            &[
                "training_cmd::tests::test_m4_a15_cli_and_ui_dto_parity_on_same_run",
                "training_cmd::tests::test_m4_cli_lifecycle_oco_stop_rejections_and_evidence",
            ],
            Some("in-gate CLI show/report == AppHost workspace/report DTO parity verified"),
        );
        record_mapped_case(
            &mut report,
            "M4-A-16",
            "M4-W06",
            &[
                "training_ui::tests::test_m4_a16_headless_training_workflow_shortcuts_hidden_masking_and_ai_review",
                "components::tests::test_m4_masked_bar_labels_with_nonzero_visible_start_index",
                "training_ui::tests::test_m4_k03_bench_rejects_non_release_build",
            ],
            ui_smoke_summary.as_deref(),
        );

        // 7. `M4-K03` & `M4-A-17`: Next-bar interaction latency (`--ui-bench`)
        let bench_guard_test_ok = report
            .test_results
            .get("verify_m4::tests::m4_bench_validation_rejects_dev_throttled_incomplete_or_slow_samples")
            .is_some_and(|s| s == "passed");
        ensure(
            bench_guard_test_ok,
            "verify_m4 benchmark validation unit test did not pass",
        )?;

        if ui_bench {
            let b_start = Instant::now();
            let desktop_exe = release_delta_exe.as_ref().unwrap();
            let bench_res = run_quiet_command(
                desktop_exe.as_os_str(),
                [
                    "--m4-ui-bench",
                    "--data-dir",
                    &temp.join("m4-ui-bench-data").display().to_string(),
                ],
                Some(&root),
                &gate_env,
            )
            .map_err(|e| failure("m4_ui_bench", e))?;
            let log_path = logs.join("m4-ui-bench.json");
            fs::write(
                &log_path,
                serde_json::to_vec_pretty(&bench_res).map_err(|e| failure("bench_log", e))?,
            )
            .map_err(|e| failure("bench_log", e))?;
            let rel_log = log_path
                .strip_prefix(&root)
                .unwrap_or(&log_path)
                .display()
                .to_string()
                .replace('\\', "/");
            report.log_files.insert("m4-ui-bench".into(), rel_log);

            let bench_report_path = reports.join("m4-next-bar-bench-report.json");
            fs::write(&bench_report_path, &bench_res.stdout)
                .map_err(|e| failure("write_bench", e))?;
            let rel_bench_report = bench_report_path
                .strip_prefix(&root)
                .unwrap_or(&bench_report_path)
                .display()
                .to_string()
                .replace('\\', "/");

            let bench_val: Value =
                serde_json::from_str(&bench_res.stdout).map_err(|e| failure("parse_bench", e))?;
            let validated = if bench_res.status_code == Some(0) && !bench_res.timed_out {
                validate_m4_next_bar_bench(&bench_val)
            } else {
                Err(format!(
                    "delta.exe --m4-ui-bench exited {:?} (timed_out={}): {}",
                    bench_res.status_code, bench_res.timed_out, bench_res.stderr
                ))
            };

            match validated {
                Ok(v) => {
                    let mut k03_metrics = BTreeMap::new();
                    k03_metrics.insert("p50_ms".into(), json!(v.p50_ms));
                    k03_metrics.insert("p95_ms".into(), json!(v.p95_ms));
                    k03_metrics.insert("p99_ms".into(), json!(v.p99_ms));
                    k03_metrics.insert("max_ms".into(), json!(v.max_ms));
                    k03_metrics.insert("budget_p95_ms".into(), json!(M4_K03_P95_BUDGET_MS));
                    k03_metrics.insert("measured_steps".into(), json!(v.measured_steps));
                    k03_metrics.insert("native_chart_paints".into(), json!(v.native_chart_paints));
                    k03_metrics.insert("raw_report".into(), json!(rel_bench_report));
                    k03_metrics.insert("cpu".into(), bench_val["cpu"].clone());
                    k03_metrics.insert("gpu".into(), bench_val["gpu"].clone());
                    k03_metrics.insert(
                        "power_throttling".into(),
                        bench_val["power_throttling"].clone(),
                    );
                    report.poc_results.push(PocResultRecord {
                        id: "M4-K03".into(),
                        title: "Release 1,000-bar + default indicators 300-step next-bar latency benchmark".into(),
                        status: "passed".into(),
                        metrics: k03_metrics,
                        detail: format!(
                            "Release hidden HWND (execution_speed_opt_out), 1,000 visible bars + SMA/EMA/MACD/RSI, 300 measured steps: p50={:.3} ms, p95={:.3} ms (<= 100.0 ms), p99={:.3} ms, max={:.3} ms; {} chart paints; raw samples in {}",
                            v.p50_ms, v.p95_ms, v.p99_ms, v.max_ms, v.native_chart_paints, rel_bench_report
                        ),
                    });
                    report.case_results.push(CaseResultRecord {
                        case_id: "M4-A-17".into(),
                        work_package: "M4-W06,M4-W07".into(),
                        tier: "required".into(),
                        status: "passed".into(),
                        elapsed_ms: b_start.elapsed().as_millis() as u64,
                        evidence: format!(
                            "Release delta.exe --m4-ui-bench: 300 measured next-bar steps (save tx + matching + PIT adjustment + indicators + 1,000-bar slice + native DirectX WM_PAINT), independently recomputed p50={:.3} ms, p95={:.3} ms <= 100.0 ms, p99={:.3} ms, max={:.3} ms, paints={}, power_throttling={}, raw samples {}",
                            v.p50_ms,
                            v.p95_ms,
                            v.p99_ms,
                            v.max_ms,
                            v.native_chart_paints,
                            bench_val["power_throttling"],
                            rel_bench_report
                        ),
                    });
                }
                Err(reason) => {
                    report.poc_results.push(PocResultRecord {
                        id: "M4-K03".into(),
                        title: "Release 1,000-bar + default indicators 300-step next-bar latency benchmark".into(),
                        status: "failed".into(),
                        metrics: BTreeMap::new(),
                        detail: format!("FAILED: {reason}"),
                    });
                    report.case_results.push(CaseResultRecord {
                        case_id: "M4-A-17".into(),
                        work_package: "M4-W06,M4-W07".into(),
                        tier: "required".into(),
                        status: "failed".into(),
                        elapsed_ms: b_start.elapsed().as_millis() as u64,
                        evidence: format!("FAILED: {reason} (raw report {rel_bench_report})"),
                    });
                    return Err(failure("m4_a17_failed", reason));
                }
            }
        } else {
            report.poc_results.push(PocResultRecord {
                id: "M4-K03".into(),
                title: "Release 1,000-bar + default indicators 300-step next-bar latency benchmark"
                    .into(),
                status: "not-run".into(),
                metrics: BTreeMap::new(),
                detail: "Requires explicit --ui-bench (release native hidden HWND measurement)."
                    .into(),
            });
            report.case_results.push(CaseResultRecord {
                case_id: "M4-A-17".into(),
                work_package: "M4-W06,M4-W07".into(),
                tier: "required".into(),
                status: "not-run".into(),
                elapsed_ms: 0,
                evidence: "Next-bar latency benchmark not executed; pass --ui-bench for AC-26 release measurement.".into(),
            });
        }

        if report.case_results.iter().any(|c| c.status == "failed") {
            return Err(failure(
                "case_failed",
                "one or more required M4 cases failed",
            ));
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

    // Cleanup temp directory & evaluate quietness, isolation, and regressions (`M4-A-18`)
    let a18_start = Instant::now();
    let (window_samples, window_findings) = monitor.finish();
    if let Err(error) = fs::remove_dir_all(&temp) {
        report
            .errors
            .push(format!("temporary cleanup failed: {error}"));
    }
    report.temp_residual_dirs = usize::from(temp.exists());
    report.temp_residual_dirs += owned_test_directories()
        .map_err(|e| failure("temp_snapshot", e))?
        .difference(&test_directories_before)
        .count();
    if report.temp_residual_dirs > 0 {
        report
            .errors
            .push("new test temporary directories remain".into());
    }

    let after = directory_snapshot(&watched).map_err(|e| failure("path_snapshot", e))?;
    report.outside_temp_created_dirs = after.difference(&before).count();
    let release_after = appdata
        .as_ref()
        .map(|p| p.join("delta"))
        .map(|p| (p.exists(), p.metadata().and_then(|m| m.modified()).ok()));
    report.release_data_dir_touched = release_before != release_after;
    report.visible_window_count = count_visible_windows_for_current_process();
    report.window_details = observed_window_details();

    if report.outside_temp_created_dirs > 0
        || report.release_data_dir_touched
        || report.visible_window_count > 0
        || !window_findings.is_empty()
    {
        report.errors.push(format!(
            "quietness or data isolation check failed (findings={window_findings:?})"
        ));
    }
    let fingerprint_unchanged = source_fingerprint(&root)? == report.source_fingerprint;
    if !fingerprint_unchanged {
        report
            .errors
            .push("source changed during verification".into());
    }
    if cfg!(windows) && report.process_priority != "below_normal" {
        report
            .errors
            .push("process priority is not below normal".into());
    }

    let quiet_and_isolated_outcome = (|| -> Result<(), ErrorEnvelope> {
        ensure(
            !cfg!(windows) || report.process_priority == "below_normal",
            "process priority is not below_normal",
        )?;
        ensure(
            report.visible_window_count == 0 && window_findings.is_empty(),
            format!("visible or foreground windows detected: {window_findings:?}"),
        )?;
        ensure(
            report.outside_temp_created_dirs == 0 && !report.release_data_dir_touched,
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

    if let Err(err) = quiet_and_isolated_outcome {
        report.case_results.push(CaseResultRecord {
            case_id: "M4-A-18".into(),
            work_package: "M4-W07".into(),
            tier: "required".into(),
            status: "failed".into(),
            elapsed_ms: a18_start.elapsed().as_millis() as u64,
            evidence: format!("FAILED {}: {}", err.code, err.message),
        });
    } else if ui_smoke && ui_bench {
        match verify_prior_stage_regressions(&root, &report.source_fingerprint) {
            Ok(reg_summary) => {
                report.case_results.push(CaseResultRecord {
                    case_id: "M4-A-18".into(),
                    work_package: "M4-W07".into(),
                    tier: "required".into(),
                    status: "passed".into(),
                    elapsed_ms: a18_start.elapsed().as_millis() as u64,
                    evidence: format!(
                        "below_normal priority; {window_samples} quietness samples with 0 visible/foreground windows; developer delta/delta-dev roots untouched; 0 temp residuals; {}; same-fingerprint regressions verified ({reg_summary})",
                        ui_smoke_summary.as_deref().unwrap_or("ui-smoke passed")
                    ),
                });
            }
            Err(err) => {
                report
                    .errors
                    .push(format!("M4-A-18: {}: {}", err.code, err.message));
                report.case_results.push(CaseResultRecord {
                    case_id: "M4-A-18".into(),
                    work_package: "M4-W07".into(),
                    tier: "required".into(),
                    status: "failed".into(),
                    elapsed_ms: a18_start.elapsed().as_millis() as u64,
                    evidence: format!("FAILED {}: {}", err.code, err.message),
                });
            }
        }
    } else {
        report.case_results.push(CaseResultRecord {
            case_id: "M4-A-18".into(),
            work_package: "M4-W07".into(),
            tier: "required".into(),
            status: "not-run".into(),
            elapsed_ms: a18_start.elapsed().as_millis() as u64,
            evidence: format!(
                "Quietness ({window_samples} samples, 0 visible/foreground windows) and developer data isolation passed; full M4-A-18 gate closure requires --ui-smoke and --ui-bench plus same-fingerprint M1/M2/M3 regression reports"
            ),
        });
    }

    // Manual (`M4-M-01`, `H-05`) and Live (`M4-L-01`, `ACT-01`) cases stay honestly `not-run`
    report.case_results.push(CaseResultRecord {
        case_id: "M4-M-01".into(),
        work_package: "M4-W06".into(),
        tier: "manual".into(),
        status: "not-run".into(),
        elapsed_ms: 0,
        evidence: "Manual check USER-ACTIONS H-05 (hands-on hidden US equity and crypto sessions, keyboard shortcuts, order types, pause/resume, report and reveal)".into(),
    });
    report.case_results.push(CaseResultRecord {
        case_id: "M4-L-01".into(),
        work_package: "M4-W05".into(),
        tier: "live".into(),
        status: "not-run".into(),
        elapsed_ms: 0,
        evidence: "USER-ACTIONS ACT-01 real model endpoint/credential not configured; dual-protocol loopback AI review verified in M4-A-14".into(),
    });

    let failed = !report.errors.is_empty()
        || report.case_results.iter().any(|c| c.status == "failed")
        || report.poc_results.iter().any(|p| p.status == "failed");
    let required_not_run = report
        .case_results
        .iter()
        .any(|c| c.tier == "required" && c.status == "not-run")
        || report.poc_results.iter().any(|p| p.status == "not-run");
    report.overall_status = if failed {
        "failed"
    } else if required_not_run {
        "not-run"
    } else {
        "passed"
    }
    .into();

    let json_text = serde_json::to_string_pretty(&report).map_err(|e| failure("report", e))?;
    fs::write(reports.join("verify-stage-report.json"), &json_text)
        .map_err(|e| failure("report", e))?;
    println!("{}", sanitize_terminal_output(&json_text));
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_bench_json() -> Value {
        let us: Vec<u64> = (1..=300).map(|i| 25_000 + i * 100).collect(); // 25.1 ms .. 55.0 ms
        let ms: Vec<f64> = us.iter().map(|&v| (v as f64) / 1000.0).collect();
        let p50_ms = (us[149] as f64) / 1000.0;
        let p95_ms = (us[284] as f64) / 1000.0;
        let p99_ms = (us[296] as f64) / 1000.0;
        let max_ms = (us[299] as f64) / 1000.0;
        json!({
            "status": "passed",
            "passed": true,
            "case_id": "M4-A-17",
            "kpi_id": "M4-K03",
            "build_profile": "release",
            "power_throttling": "execution_speed_opt_out",
            "native_window_created": true,
            "visible_windows": 0,
            "focus_stolen": false,
            "history_bars": 1000,
            "kline_visible_bars": 1000,
            "warmup_steps": 20,
            "measured_steps": 300,
            "native_chart_paints": 305,
            "p50_ms": p50_ms,
            "p95_ms": p95_ms,
            "p99_ms": p99_ms,
            "max_ms": max_ms,
            "budget_p95_ms": 100.0,
            "step_times_us": us,
            "step_times_ms": ms,
        })
    }

    #[test]
    fn m4_bench_validation_rejects_dev_throttled_incomplete_or_slow_samples() {
        let ok_sample = valid_bench_json();
        let parsed = validate_m4_next_bar_bench(&ok_sample).expect("valid release benchmark");
        assert!(parsed.p95_ms <= 100.0);
        assert_eq!(parsed.measured_steps, 300);

        let mut dev = ok_sample.clone();
        dev["build_profile"] = json!("dev");
        assert!(validate_m4_next_bar_bench(&dev).is_err());

        let mut throttled = ok_sample.clone();
        throttled["power_throttling"] = json!("default");
        assert!(validate_m4_next_bar_bench(&throttled).is_err());

        let mut few_samples = ok_sample.clone();
        few_samples["step_times_us"] = json!(vec![30_000_u64; 299]);
        assert!(validate_m4_next_bar_bench(&few_samples).is_err());

        let mut few_paints = ok_sample.clone();
        few_paints["native_chart_paints"] = json!(299);
        assert!(validate_m4_next_bar_bench(&few_paints).is_err());

        let mut visible = ok_sample.clone();
        visible["visible_windows"] = json!(1);
        assert!(validate_m4_next_bar_bench(&visible).is_err());

        let mut focus = ok_sample.clone();
        focus["focus_stolen"] = json!(true);
        assert!(validate_m4_next_bar_bench(&focus).is_err());

        let mut tampered_pct = ok_sample.clone();
        tampered_pct["p95_ms"] = json!(12.345);
        assert!(validate_m4_next_bar_bench(&tampered_pct).is_err());

        let mut slow = ok_sample;
        let slow_us = vec![105_000_u64; 300]; // 105 ms > 100 ms budget
        let slow_ms = vec![105.0_f64; 300];
        slow["step_times_us"] = json!(slow_us);
        slow["step_times_ms"] = json!(slow_ms);
        slow["p50_ms"] = json!(105.0);
        slow["p95_ms"] = json!(105.0);
        slow["p99_ms"] = json!(105.0);
        slow["max_ms"] = json!(105.0);
        assert!(validate_m4_next_bar_bench(&slow).is_err());
    }
}
