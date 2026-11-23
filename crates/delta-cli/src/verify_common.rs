//! Shared stage verification helpers across `verify.rs` (M1) and `verify_m2..m5.rs`.

use super::{
    cli_args::to_sanitized_pretty_json,
    verify::{failure, record_tests, source_files},
    CaseResultRecord, VerifyStageReport,
};
use chrono::Utc;
use delta_app::{current_process_priority, run_quiet_command, ErrorEnvelope, QuietCommandResult};
use serde_json::Value;
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime},
};

pub(crate) fn ensure(
    code: &str,
    condition: bool,
    message: impl std::fmt::Display,
) -> Result<(), ErrorEnvelope> {
    if condition {
        Ok(())
    } else {
        Err(failure(code, message))
    }
}

pub(crate) fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

pub(crate) fn default_gate_jobs() -> usize {
    env::var("DELTA_GATE_JOBS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(1)
        })
}

pub(crate) fn build_gate_env(temp: &Path, jobs: usize) -> BTreeMap<String, String> {
    BTreeMap::from([
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
    ])
}

pub(crate) fn make_initial_report(
    stage: &str,
    plan_version: &str,
    live_mode: bool,
    source_fingerprint: String,
    jobs: usize,
    path_observation_scope: &str,
) -> VerifyStageReport {
    VerifyStageReport {
        stage: stage.into(),
        plan_version: plan_version.into(),
        executed_at: Utc::now().to_rfc3339(),
        live_mode,
        source_fingerprint,
        visible_window_count: 0,
        window_details: Vec::new(),
        temp_residual_dirs: 0,
        outside_temp_created_dirs: 0,
        path_observation_scope: path_observation_scope.into(),
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
    }
}

pub(crate) fn record_execution_outcome(
    report: &mut VerifyStageReport,
    execution: std::thread::Result<Result<(), ErrorEnvelope>>,
) {
    match execution {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report
            .errors
            .push(format!("{}: {}", error.code, error.message)),
        Err(payload) => report
            .errors
            .push(format!("gate panic: {}", extract_panic_message(&*payload))),
    }
}

fn extract_panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic")
}

pub(crate) fn stamp_path(path: Option<PathBuf>) -> Option<(bool, Option<SystemTime>)> {
    path.map(|p| (p.exists(), p.metadata().and_then(|m| m.modified()).ok()))
}

/// Nearest-rank percentile in milliseconds over microsecond samples.
pub(crate) fn percentile_ms(samples_us: &[u64], percent: usize) -> f64 {
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    sorted
        .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
        .map_or(0.0, |us| *us as f64 / 1000.0)
}

pub(crate) fn run_logged_command(
    root: &Path,
    logs: &Path,
    report: &mut VerifyStageReport,
    key: &str,
    program: &OsStr,
    args: Vec<String>,
    env: &BTreeMap<String, String>,
) -> Result<QuietCommandResult, ErrorEnvelope> {
    let res = run_quiet_command(program, args, Some(root), env).map_err(|e| failure(key, e))?;
    let path = logs.join(format!("{key}.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&res).map_err(|e| failure(key, e))?,
    )
    .map_err(|e| failure(key, e))?;
    report.log_files.insert(key.into(), rel_path(root, &path));
    Ok(res)
}

pub(crate) struct MechanicalGateConfig<'a> {
    pub stage_prefix: &'a str,
    pub work_package: &'a str,
    pub clippy_locked: bool,
    pub test_locked: bool,
}

pub(crate) fn run_mechanical_gates(
    root: &Path,
    logs: &Path,
    report: &mut VerifyStageReport,
    gate_env: &BTreeMap<String, String>,
    cfg: MechanicalGateConfig<'_>,
) -> Result<(), ErrorEnvelope> {
    let mut clippy_args = vec!["clippy", "--workspace", "--all-targets"];
    if cfg.clippy_locked {
        clippy_args.push("--locked");
    }
    clippy_args.extend(["--", "-D", "warnings"]);

    let mut test_args = vec!["test", "--workspace"];
    if cfg.test_locked {
        test_args.push("--locked");
    }

    let g01 = format!("{}-G-01", cfg.stage_prefix);
    let g02 = format!("{}-G-02", cfg.stage_prefix);
    let g03 = format!("{}-G-03", cfg.stage_prefix);
    let g04 = format!("{}-G-04", cfg.stage_prefix);
    let g05 = format!("{}-G-05", cfg.stage_prefix);

    let steps: [(&str, &str, Vec<&str>); 5] = [
        (&g01, "python", vec!["scripts/check_docs.py"]),
        (&g02, "git", vec!["diff", "--check"]),
        (&g03, "cargo", vec!["fmt", "--all", "--check"]),
        (&g04, "cargo", clippy_args),
        (&g05, "cargo", test_args),
    ];

    for (id, program, args) in steps {
        let started = Instant::now();
        let res = run_logged_command(
            root,
            logs,
            report,
            id,
            OsStr::new(program),
            args.iter().map(|s| (*s).to_string()).collect(),
            gate_env,
        )?;
        if id == g05 {
            record_tests(&res.stdout, &mut report.test_results);
        }
        let passed = res.status_code == Some(0) && !res.timed_out;
        let rel_log = report.log_files.get(id).cloned().unwrap_or_default();
        report.case_results.push(CaseResultRecord {
            case_id: id.into(),
            work_package: cfg.work_package.into(),
            tier: "required".into(),
            status: if passed { "passed" } else { "failed" }.into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            evidence: format!(
                "{program} {}; exit={:?}; log {rel_log}",
                args.join(" "),
                res.status_code,
            ),
        });
        if !passed {
            return Err(failure("gate_failed", format!("{id}: see {rel_log}")));
        }
    }
    Ok(())
}

pub(crate) fn build_release_desktop_exe(
    root: &Path,
    logs: &Path,
    report: &mut VerifyStageReport,
    gate_env: &BTreeMap<String, String>,
    code: &str,
) -> Result<PathBuf, ErrorEnvelope> {
    let started = Instant::now();
    let res = run_logged_command(
        root,
        logs,
        report,
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
        gate_env,
    )?;
    ensure(
        code,
        res.status_code == Some(0) && !res.timed_out,
        format!(
            "cargo build --release -p delta-desktop failed ({} ms): {}",
            started.elapsed().as_millis(),
            res.stderr
        ),
    )?;
    let exe = root.join("target/release/delta.exe");
    ensure(
        code,
        exe.is_file(),
        format!("expected release binary at {}", exe.display()),
    )?;
    Ok(exe)
}

pub(crate) fn record_mapped_case(
    report: &mut VerifyStageReport,
    g05_id: &str,
    id: &str,
    package: &str,
    tier: &str,
    names: &[&str],
    extra: Option<&str>,
) {
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| report.test_results.get(*n).is_none_or(|s| s != "passed"))
        .collect();
    let mut evidence = format!("{g05_id} captured test results: {}", names.join(", "));
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
        tier: tier.into(),
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

// ---------------------------------------------------------------------------
// Static source & dependency boundary checks
// ---------------------------------------------------------------------------

pub(crate) fn check_delta_core_purity(
    root: &Path,
    code: &str,
    scan_subdir: &str,
    forbidden_tokens: &[&str],
    strict_decimal_files: &[(&str, &[&str])],
) -> Result<(), ErrorEnvelope> {
    let mut files = Vec::new();
    source_files(&root.join(scan_subdir), &mut files).map_err(|e| failure(code, e))?;
    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| failure(code, e))?;
        for forbidden in forbidden_tokens {
            ensure(
                code,
                !text.contains(forbidden),
                format!(
                    "delta-core {:?} contains forbidden '{forbidden}'",
                    file.file_name()
                ),
            )?;
        }
        let fname = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        for (target_file, banned_tokens) in strict_decimal_files {
            if fname == *target_file {
                for (lineno, line) in text.lines().enumerate() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") {
                        continue;
                    }
                    for tok in *banned_tokens {
                        ensure(
                            code,
                            !trimmed.contains(tok),
                            format!("delta-core {fname}:{} contains '{tok}'", lineno + 1),
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check_delta_app_boundary(
    root: &Path,
    code: &str,
    forbid_from_bar_except: Option<&[&str]>,
) -> Result<(), ErrorEnvelope> {
    let mut files = Vec::new();
    source_files(&root.join("crates/delta-app/src"), &mut files).map_err(|e| failure(code, e))?;
    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| failure(code, e))?;
        ensure(
            code,
            !text.contains("pub async fn"),
            format!("delta-app {:?} leaks a public async API", file.file_name()),
        )?;
        if let Some(allowed) = forbid_from_bar_except {
            let fname = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !allowed.contains(&fname) {
                let prod = text
                    .split_once("#[cfg(test)]")
                    .map_or(text.as_str(), |(p, _)| p);
                ensure(
                    code,
                    !prod.to_ascii_lowercase().contains("from bar"),
                    format!(
                        "delta-app {fname} queries `FROM bar` directly outside SqliteStore port"
                    ),
                )?;
            }
        }
    }
    Ok(())
}

pub(crate) fn check_desktop_metadata_deps(
    root: &Path,
    gate_env: &BTreeMap<String, String>,
    code: &str,
    save_metadata_to: Option<&Path>,
) -> Result<(Value, Vec<String>), ErrorEnvelope> {
    let meta_res = run_quiet_command(
        OsStr::new("cargo"),
        ["metadata", "--locked", "--format-version", "1"],
        Some(root),
        gate_env,
    )
    .map_err(|e| failure(code, e))?;
    ensure(
        code,
        meta_res.status_code == Some(0) && !meta_res.timed_out,
        "cargo metadata --locked failed",
    )?;
    if let Some(path) = save_metadata_to {
        fs::write(path, &meta_res.stdout).map_err(|e| failure(code, e))?;
    }
    let meta_json: Value = serde_json::from_str(&meta_res.stdout).map_err(|e| failure(code, e))?;
    let packages = meta_json["packages"]
        .as_array()
        .ok_or_else(|| failure(code, "missing packages array"))?;
    let desktop_pkg = packages
        .iter()
        .find(|p| p["name"].as_str() == Some("delta-desktop"))
        .ok_or_else(|| failure(code, "delta-desktop missing from metadata"))?;
    let desktop_deps: Vec<String> = desktop_pkg["dependencies"]
        .as_array()
        .map(|deps| {
            deps.iter()
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ensure(
        code,
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
            code,
            !desktop_deps.iter().any(|d| d == forbidden_dep),
            format!("delta-desktop directly depends on forbidden crate '{forbidden_dep}'"),
        )?;
    }
    Ok((meta_json, desktop_deps))
}

pub(crate) fn check_desktop_source_boundary(
    root: &Path,
    code: &str,
    forbidden_patterns: &[&str],
    case_insensitive: bool,
) -> Result<usize, ErrorEnvelope> {
    let mut files = Vec::new();
    source_files(&root.join("crates/delta-desktop/src"), &mut files)
        .map_err(|e| failure(code, e))?;
    for file in &files {
        let text = fs::read_to_string(file).map_err(|e| failure(code, e))?;
        let hay = if case_insensitive {
            text.to_ascii_lowercase()
        } else {
            text
        };
        for pattern in forbidden_patterns {
            let needle = if case_insensitive {
                pattern.to_ascii_lowercase()
            } else {
                (*pattern).to_string()
            };
            ensure(
                code,
                !hay.contains(&needle),
                format!(
                    "delta-desktop {:?} references '{pattern}'",
                    file.file_name()
                ),
            )?;
        }
    }
    Ok(files.len())
}

pub(crate) fn check_workspace_source_hygiene(
    root: &Path,
    code: &str,
    check_command_new: bool,
) -> Result<usize, ErrorEnvelope> {
    let forbidden_bind = ["0.0", ".0.0"].concat();
    let forbidden_cmd = ["Command", "::new"].concat();
    let mut files = Vec::new();
    source_files(&root.join("crates"), &mut files).map_err(|e| failure(code, e))?;
    let mut rs_scanned = 0usize;
    for file in &files {
        if !file.extension().is_some_and(|e| e == "rs") {
            continue;
        }
        rs_scanned += 1;
        let is_process_rs = file
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("crates/delta-app/src/process.rs");
        let text = fs::read_to_string(file).map_err(|e| failure(code, e))?;
        for (lineno, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            ensure(
                code,
                !trimmed.contains(&forbidden_bind),
                format!(
                    "{:?}:{} non-loopback test bind address found",
                    file.file_name(),
                    lineno + 1
                ),
            )?;
            if check_command_new && !is_process_rs {
                ensure(
                    code,
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
    Ok(rs_scanned)
}

// ---------------------------------------------------------------------------
// Same-fingerprint prior-stage regression report verification
// ---------------------------------------------------------------------------

pub(crate) fn verify_prior_stage_regressions(
    root: &Path,
    expected_fingerprint: &str,
    stages: &[(&str, &str, &str)],
    check_extended_cases: bool,
) -> Result<String, ErrorEnvelope> {
    let mut summaries = Vec::new();
    for &(dir, stage, plan) in stages {
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
            "REGRESSION",
            rep["stage"].as_str() == Some(stage)
                && rep["plan_version"].as_str() == Some(plan)
                && rep["overall_status"].as_str() == Some("passed")
                && rep["source_fingerprint"].as_str() == Some(expected_fingerprint),
            format!(
                "{stage} regression mismatch: stage={} plan={} status={} fp={} (expected {expected_fingerprint})",
                rep["stage"], rep["plan_version"], rep["overall_status"], rep["source_fingerprint"]
            ),
        )?;
        if check_extended_cases {
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
                "REGRESSION",
                required.iter().all(|id| case_passed(id)),
                format!(
                    "{stage} regression must include passed {required:?} (UI smoke/bench/installer flags)"
                ),
            )?;
            if stage == "M2" {
                ensure(
                    "REGRESSION",
                    rep["poc_results"].as_array().is_some_and(|a| {
                        a.iter()
                            .any(|p| p["id"] == "M2-K06" && p["status"] == "passed")
                    }),
                    "M2 regression must include passed --ui-smoke (M2-K06)",
                )?;
            }
            if stage == "M3" {
                ensure(
                    "REGRESSION",
                    rep["case_results"].as_array().is_some_and(|a| {
                        a.iter().any(|c| {
                            c["case_id"] == "M3-A-03"
                                && c["status"] == "passed"
                                && c["evidence"]
                                    .as_str()
                                    .is_some_and(|e| e.contains("installed --ui-smoke passed"))
                        })
                    }),
                    "M3 regression must include --installer and --ui-smoke (M3-A-03)",
                )?;
            }
        }
        summaries.push(format!(
            "{stage} v{plan} passed @ {}",
            rep["executed_at"].as_str().unwrap_or("?")
        ));
    }
    Ok(summaries.join("; "))
}

// ---------------------------------------------------------------------------
// Unified D-25 quietness monitor (`XCR-10`, `XCR-U04`, `CLI-B14`)
// ---------------------------------------------------------------------------

type ProcessMatcher = Arc<dyn Fn(u32, &str) -> bool + Send + Sync + 'static>;

fn normalize_target_prefix(path: &Path) -> String {
    let mut norm = path
        .display()
        .to_string()
        .replace('/', "\\")
        .trim_start_matches("\\\\?\\")
        .to_ascii_lowercase();
    if !norm.is_empty() && !norm.ends_with('\\') {
        norm.push('\\');
    }
    norm
}

fn target_monitor_prefixes(root: &Path) -> Vec<String> {
    let canon = crate::installer::plain_canonical(root).unwrap_or_else(|_| root.to_path_buf());
    let mut prefixes = Vec::new();
    for candidate in [canon.join("target"), canon.join(".local/target-packages")] {
        let p = normalize_target_prefix(&candidate);
        if !p.is_empty() && !prefixes.contains(&p) {
            prefixes.push(p);
        }
    }
    if let Some(custom_target) = env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(custom_target);
        let resolved = crate::installer::plain_canonical(&path).unwrap_or(path);
        let p = normalize_target_prefix(&resolved);
        if !p.is_empty() && !prefixes.contains(&p) {
            prefixes.push(p);
        }
    }
    prefixes
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
        if ok == 0 {
            return None;
        }
        let len = (size as usize).min(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }
}

#[cfg(windows)]
fn sample_monitored_windows(
    matcher: &ProcessMatcher,
    include_pid_in_label: bool,
    foreground_label: &str,
    findings: &Mutex<BTreeSet<String>>,
) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };

    struct VisitCtx<'a> {
        matcher: &'a ProcessMatcher,
        include_pid_in_label: bool,
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
            if !(ctx.matcher)(pid, &image) {
                return None;
            }
            let mut title = [0u16; 256];
            let raw_len = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            let len = raw_len.clamp(0, title.len() as i32) as usize;
            let base = image.rsplit(['\\', '/']).next().unwrap_or(&image);
            let text = String::from_utf16_lossy(&title[..len]);
            if ctx.include_pid_in_label {
                Some(format!("pid={pid} {base} \"{text}\""))
            } else {
                Some(format!("{base} \"{text}\""))
            }
        }
    }

    // Never panic across the Win32 `extern "system"` callback boundary (`XCR-U04`).
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
        if lparam == 0 {
            return 1;
        }
        let ctx = &*(lparam as *const VisitCtx<'_>);
        if delta_app::window_is_perceivable(hwnd as isize) {
            if let Some(text) = describe_owned(hwnd, ctx) {
                ctx.findings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(format!("visible window: {text}"));
            }
        }
        1
    }

    let ctx = VisitCtx {
        matcher,
        include_pid_in_label,
        findings,
    };
    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.is_null() {
            if let Some(text) = describe_owned(foreground, &ctx) {
                findings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(format!("{foreground_label}: {text}"));
            }
        }
        EnumWindows(Some(visit), &ctx as *const VisitCtx<'_> as LPARAM);
    }
}

#[cfg(not(windows))]
fn sample_monitored_windows(
    _matcher: &ProcessMatcher,
    _include_pid_in_label: bool,
    _foreground_label: &str,
    _findings: &Mutex<BTreeSet<String>>,
) {
}

pub(crate) struct QuietnessMonitor {
    stop: Arc<AtomicBool>,
    findings: Arc<Mutex<BTreeSet<String>>>,
    samples: Arc<Mutex<u64>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl QuietnessMonitor {
    pub(crate) fn for_predicate(
        interval: Duration,
        include_pid_in_label: bool,
        foreground_label: &'static str,
        matcher: impl Fn(u32, &str) -> bool + Send + Sync + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let findings = Arc::new(Mutex::new(BTreeSet::new()));
        let samples = Arc::new(Mutex::new(0u64));
        let matcher: ProcessMatcher = Arc::new(matcher);
        let thread = {
            let (stop, findings, samples) = (
                Arc::clone(&stop),
                Arc::clone(&findings),
                Arc::clone(&samples),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    sample_monitored_windows(
                        &matcher,
                        include_pid_in_label,
                        foreground_label,
                        &findings,
                    );
                    *samples.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    std::thread::sleep(interval);
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

    pub(crate) fn for_target_prefixes(root: &Path) -> Self {
        let self_pid = std::process::id();
        let prefixes = target_monitor_prefixes(root);
        Self::for_predicate(
            Duration::from_millis(150),
            true,
            "foreground stolen",
            move |pid, image| {
                if pid == self_pid {
                    return true;
                }
                if image.is_empty() {
                    return false;
                }
                let norm = image.replace('/', "\\").to_ascii_lowercase();
                prefixes.iter().any(|p| norm.starts_with(p))
            },
        )
    }

    pub(crate) fn finish(mut self) -> (u64, Vec<String>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let samples = *self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let findings = self
            .findings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        (samples, findings)
    }
}

impl Drop for QuietnessMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Report status validation & sanitized finalization
// ---------------------------------------------------------------------------

pub(crate) fn validate_report_statuses(report: &mut VerifyStageReport) {
    let invalid_statuses: Vec<String> = report
        .case_results
        .iter()
        .map(|result| result.status.as_str())
        .chain(
            report
                .poc_results
                .iter()
                .map(|result| result.status.as_str()),
        )
        .chain(
            report
                .live_probes
                .iter()
                .map(|result| result.status.as_str()),
        )
        .chain(report.test_results.values().map(String::as_str))
        .filter(|status| !matches!(*status, "passed" | "failed" | "blocked" | "not-run"))
        .map(str::to_owned)
        .collect();
    for status in invalid_statuses {
        report
            .errors
            .push(format!("invalid verification status: {status}"));
    }
}

pub(crate) fn finalize_and_write_report(
    report: &mut VerifyStageReport,
    report_path: &Path,
    failed: bool,
    required_not_run: Option<bool>,
) -> Result<ExitCode, ErrorEnvelope> {
    validate_report_statuses(report);
    let failed = failed || !report.errors.is_empty();
    report.overall_status = if failed {
        "failed"
    } else if required_not_run == Some(true) {
        "not-run"
    } else {
        "passed"
    }
    .into();
    let json = to_sanitized_pretty_json(report).map_err(|e| failure("report", e.message))?;
    fs::write(report_path, &json).map_err(|e| failure("report", e))?;
    println!("{json}");
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_ms_uses_nearest_rank() {
        let samples: Vec<u64> = (1..=100).map(|i| i * 1_000).collect();
        assert_eq!(percentile_ms(&samples, 50), 50.0);
        assert_eq!(percentile_ms(&samples, 95), 95.0);
        assert_eq!(percentile_ms(&samples, 99), 99.0);
        assert_eq!(percentile_ms(&[], 95), 0.0);
    }

    #[test]
    fn target_monitor_prefixes_include_target_and_target_packages() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let prefixes = target_monitor_prefixes(&root);
        assert!(prefixes.iter().any(|p| p.ends_with("\\target\\")));
        assert!(prefixes
            .iter()
            .any(|p| p.ends_with("\\.local\\target-packages\\")));
        assert!(prefixes.iter().all(|p| !p.starts_with("\\\\?\\")));
    }

    #[test]
    fn validate_report_statuses_flags_unknown_values() {
        let mut report = make_initial_report("M5", "1.0", false, "fp".into(), 1, "scope");
        report.case_results.push(CaseResultRecord {
            case_id: "X-1".into(),
            work_package: "W1".into(),
            tier: "required".into(),
            status: "bogus".into(),
            elapsed_ms: 0,
            evidence: "ev".into(),
        });
        validate_report_statuses(&mut report);
        assert!(report
            .errors
            .iter()
            .any(|e| e.contains("invalid verification status: bogus")));
    }
}
