use super::{
    verify::{
        case_from_tests, directory_snapshot, failure, owned_test_directories, record_tests,
        source_files, source_fingerprint,
    },
    CaseResultRecord, PocResultRecord, VerifyStageReport,
};
use chrono::Utc;
use delta_app::{
    count_visible_windows_for_current_process, current_process_priority, observed_window_details,
    run_m2_k02_viewport_bench, run_quiet_command, sanitize_terminal_output, AppService,
    BuildChannel, ErrorEnvelope, QuietJobObject, ResolvedAppPaths,
};
use delta_data::run_m2_k05_fts5_trigram_spike;
use delta_model::run_m2_k03_and_a13_verification;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerUserPackageReceipt {
    pub channel: String,
    pub install_dir: String,
    pub zip_path: String,
    pub zip_sha256: String,
    pub files_installed: Vec<String>,
    pub data_preserved_on_uninstall: bool,
}

/// Install DELTA per-user (`S-23`, `M2-K04`) into `<local_app_data>/Programs/delta` (`release`)
/// or `<local_app_data>/Programs/delta-dev` (`dev`) without administrator privileges.
pub fn install_per_user_channel(
    root: &Path,
    local_app_data: &Path,
    channel: &str,
    portable_out_dir: &Path,
) -> Result<PerUserPackageReceipt, ErrorEnvelope> {
    crate::packaging::install(root, local_app_data, channel, portable_out_dir)
}

pub fn uninstall_per_user_channel(
    local_app_data: &Path,
    channel: &str,
) -> Result<(), ErrorEnvelope> {
    crate::packaging::uninstall(local_app_data, channel)
}

fn package_cli_command(
    artifact: &crate::packaging::PackageArtifact,
    operation: &str,
    local_app_data: &Path,
    output: &Path,
    log: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<Value, ErrorEnvelope> {
    let cli = artifact.directory.join(if artifact.channel == "dev" {
        "delta-dev-cli.exe"
    } else {
        "delta-cli.exe"
    });
    let mut args = vec![
        operation.to_string(),
        "--channel".into(),
        artifact.channel.clone(),
        "--local-app-data".into(),
        local_app_data.display().to_string(),
    ];
    if operation == "package-install" {
        args.extend([
            "--package-dir".into(),
            artifact.directory.display().to_string(),
            "--out-dir".into(),
            output.display().to_string(),
        ]);
    }
    let result = run_quiet_command(
        cli.as_os_str(),
        args,
        Some(&artifact.directory),
        environment,
    )
    .map_err(|e| failure("package_cli", e))?;
    fs::write(
        log,
        serde_json::to_vec_pretty(&result).map_err(|e| failure("package_log", e))?,
    )
    .map_err(|e| failure("package_log", e))?;
    if result.status_code != Some(0) || result.timed_out {
        return Err(failure(
            "package_cli",
            format!(
                "{} {operation} failed; inspect {}",
                artifact.channel,
                log.display()
            ),
        ));
    }
    let receipt: Value =
        serde_json::from_str(&result.stdout).map_err(|e| failure("package_cli", e))?;
    if receipt["channel"] != artifact.channel
        || (operation == "package-uninstall"
            && (receipt["status"] != "uninstalled" || receipt["data_preserved"] != true))
    {
        return Err(failure(
            "package_cli",
            "package CLI returned an invalid receipt",
        ));
    }
    Ok(receipt)
}

pub(super) fn run_verify_stage_m2(
    live_mode: bool,
    ui_smoke: bool,
    ui_bench: bool,
) -> Result<ExitCode, ErrorEnvelope> {
    let root = env::current_dir().map_err(|e| failure("cwd", e))?;
    let fingerprint = source_fingerprint(&root)?;
    let run_id = format!("{}-{}", std::process::id(), Utc::now().timestamp_millis());
    let temp = env::temp_dir().join(format!("delta-m2-verify-{run_id}"));
    fs::create_dir(&temp).map_err(|e| failure("temp", e))?;
    let reports = root.join(".local/reports/m2");
    let logs = reports.join(format!("logs-{run_id}"));
    fs::create_dir_all(&logs).map_err(|e| failure("logs", e))?;

    let appdata = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let watched: Vec<_> = appdata
        .iter()
        .flat_map(|p| [p.join("delta"), p.join("delta-dev")])
        .collect();
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
        stage: "M2".into(),
        plan_version: "1.0".into(),
        executed_at: Utc::now().to_rfc3339(),
        live_mode,
        source_fingerprint: fingerprint,
        visible_window_count: 0,
        window_details: Vec::new(),
        temp_residual_dirs: 0,
        outside_temp_created_dirs: 0,
        path_observation_scope: "directory metadata under real LOCALAPPDATA/delta and delta-dev; test roots are isolated in TEMP; no system-wide write tracing".into(),
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

    let execution = catch_unwind(AssertUnwindSafe(|| -> Result<(), ErrorEnvelope> {
        let job = QuietJobObject::new().map_err(|e| failure("job_object", e))?;
        job.assign_current_process()
            .map_err(|e| failure("job_object", e))?;

        // 1. Static source boundary & loopback bind scans (`S-12`, `S-22`)
        let mut core_files = Vec::new();
        source_files(&root.join("crates/delta-core"), &mut core_files)
            .map_err(|e| failure("boundary_scan", e))?;
        for file in core_files {
            let text = fs::read_to_string(&file).map_err(|e| failure("boundary_scan", e))?;
            if text.contains("tokio") || text.contains("reqwest") || text.contains("gpui") {
                return Err(failure(
                    "boundary_scan",
                    "delta-core contains IO or UI dependencies",
                ));
            }
        }

        let mut app_files = Vec::new();
        source_files(&root.join("crates/delta-app/src"), &mut app_files)
            .map_err(|e| failure("boundary_scan", e))?;
        for file in app_files {
            if fs::read_to_string(&file)
                .map_err(|e| failure("boundary_scan", e))?
                .contains("pub async fn")
            {
                return Err(failure(
                    "boundary_scan",
                    "delta-app leaks a public async API",
                ));
            }
        }

        let forbidden_bind = ["0.0", ".0.0"].concat();
        let mut all_crate_files = Vec::new();
        source_files(&root.join("crates"), &mut all_crate_files)
            .map_err(|e| failure("bind_scan", e))?;
        for file in all_crate_files {
            if file.extension().is_some_and(|e| e == "rs")
                && fs::read_to_string(&file)
                    .map_err(|e| failure("bind_scan", e))?
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//"))
                    .any(|line| line.contains(&forbidden_bind))
            {
                return Err(failure("bind_scan", "non-loopback test bind address found"));
            }
        }

        // 2. Run M2 Mechanical Gates (`M2-G-01`..`M2-G-05`)
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
            ("CARGO_BUILD_JOBS".into(), jobs.to_string()),
            ("CARGO_INCREMENTAL".into(), "0".into()),
            ("PYTHONUTF8".into(), "1".into()),
            ("RUST_TEST_THREADS".into(), jobs.to_string()),
            (
                "DELTA_TEST_ROOT".into(),
                temp.join("tests").display().to_string(),
            ),
        ]);

        for (id, program, args) in [
            ("M2-G-01", "python", vec!["scripts/check_docs.py"]),
            ("M2-G-02", "git", vec!["diff", "--check"]),
            ("M2-G-03", "cargo", vec!["fmt", "--all", "--check"]),
            (
                "M2-G-04",
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
            ("M2-G-05", "cargo", vec!["test", "--workspace", "--locked"]),
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
            report.log_files.insert(
                id.into(),
                path.strip_prefix(&root).unwrap().display().to_string(),
            );
            if id == "M2-G-05" {
                record_tests(&result.stdout, &mut report.test_results);
            }
            report.case_results.push(CaseResultRecord {
                case_id: id.into(),
                work_package: "M2-W08".into(),
                tier: "required".into(),
                status: if result.status_code == Some(0) && !result.timed_out {
                    "passed"
                } else {
                    "failed"
                }
                .into(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                evidence: format!(
                    "{program} {}; exit={:?}; captured log {}",
                    args.join(" "),
                    result.status_code,
                    path.display()
                ),
            });
            if result.status_code != Some(0) || result.timed_out {
                return Err(failure(
                    "gate_failed",
                    format!("{id}: see {}", path.display()),
                ));
            }
        }

        // 3. `M2-G-06`: `cargo metadata --locked` dependency boundary (`S-12`) & license inventory (`M2-A-19`, `OSS-11`)
        let g06_start = Instant::now();
        let meta_res = run_quiet_command(
            OsStr::new("cargo"),
            ["metadata", "--locked", "--format-version", "1"],
            Some(&root),
            &gate_env,
        )
        .map_err(|e| failure("cargo_metadata", e))?;
        if meta_res.status_code != Some(0) {
            return Err(failure("cargo_metadata", "cargo metadata --locked failed"));
        }
        let meta_path = reports.join("cargo-metadata-locked.json");
        fs::write(&meta_path, &meta_res.stdout).map_err(|e| failure("write_metadata", e))?;
        let meta_json: Value =
            serde_json::from_str(&meta_res.stdout).map_err(|e| failure("parse_metadata", e))?;
        let packages = meta_json["packages"]
            .as_array()
            .ok_or_else(|| failure("metadata_packages", "missing packages array"))?;
        let ws_members: BTreeSet<String> = meta_json["workspace_members"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let desktop_pkg = packages
            .iter()
            .find(|p| p["name"].as_str() == Some("delta-desktop"))
            .ok_or_else(|| failure("metadata_desktop", "delta-desktop not found in metadata"))?;
        let desktop_deps: Vec<String> = desktop_pkg["dependencies"]
            .as_array()
            .map(|deps| {
                deps.iter()
                    .filter_map(|d| d["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for forbidden_dep in [
            "delta-data",
            "delta-core",
            "delta-model",
            "rusqlite",
            "tokio",
            "reqwest",
        ] {
            if desktop_deps.iter().any(|d| d == forbidden_dep) {
                return Err(failure(
                    "desktop_boundary_violation",
                    format!("delta-desktop directly depends on forbidden crate '{forbidden_dep}'"),
                ));
            }
        }

        let mut third_party_count = 0usize;
        let mut mpl_packages = Vec::new();
        for pkg in packages {
            let id = pkg["id"].as_str().unwrap_or_default();
            if ws_members.contains(id) {
                continue;
            }
            third_party_count += 1;
            let name = pkg["name"].as_str().unwrap_or_default();
            let ver = pkg["version"].as_str().unwrap_or_default();
            let lic = pkg["license"]
                .as_str()
                .or_else(|| pkg["license_file"].as_str())
                .unwrap_or("");
            if lic.trim().is_empty() {
                return Err(failure(
                    "missing_license",
                    format!("third-party package {name} {ver} has no declared license"),
                ));
            }
            if lic.contains("MPL-2.0") && !lic.contains("MIT") && !lic.contains("Apache-2.0") {
                mpl_packages.push(format!("{name}@{ver}"));
            }
        }
        let notices_text = fs::read_to_string(root.join("THIRD-PARTY-NOTICES.txt"))
            .map_err(|e| failure("notices_missing", e))?;
        for required_notice in ["dwrote", "option-ext", "cbindgen", "MPL-2.0"] {
            if !notices_text.contains(required_notice) {
                return Err(failure(
                    "notices_incomplete",
                    format!("THIRD-PARTY-NOTICES.txt missing '{required_notice}'"),
                ));
            }
        }
        for (id, arguments) in [
            (
                "license-regressions",
                vec![
                    "-m".into(),
                    "unittest".into(),
                    "discover".into(),
                    "-s".into(),
                    "scripts".into(),
                    "-p".into(),
                    "test_collect_third_party_licenses.py".into(),
                ],
            ),
            (
                "license-collection",
                vec![
                    "scripts/collect_third_party_licenses.py".into(),
                    "--metadata".into(),
                    meta_path.display().to_string(),
                    "--output".into(),
                    logs.join("licenses").display().to_string(),
                ],
            ),
        ] {
            let result = run_quiet_command(OsStr::new("python"), arguments, Some(&root), &gate_env)
                .map_err(|e| failure("license_command", e))?;
            let path = logs.join(format!("{id}.json"));
            fs::write(
                &path,
                serde_json::to_vec_pretty(&result).map_err(|e| failure("license_log", e))?,
            )
            .map_err(|e| failure("license_log", e))?;
            report.log_files.insert(
                id.into(),
                path.strip_prefix(&root).unwrap().display().to_string(),
            );
            if result.status_code != Some(0) || result.timed_out {
                return Err(failure(
                    "license_collection",
                    format!("{id}: see {}", path.display()),
                ));
            }
        }
        let licenses: Value = serde_json::from_slice(
            &fs::read(logs.join("licenses/manifest.json"))
                .map_err(|e| failure("license_manifest", e))?,
        )
        .map_err(|e| failure("license_manifest", e))?;
        let collected = licenses["packages"]
            .as_array()
            .ok_or_else(|| failure("license_manifest", "missing package inventory"))?;
        let expected_ids: BTreeSet<_> = packages
            .iter()
            .filter(|package| !ws_members.contains(package["id"].as_str().unwrap_or_default()))
            .filter_map(|package| package["id"].as_str())
            .collect();
        let collected_ids: BTreeSet<_> = collected
            .iter()
            .filter_map(|package| package["source_binding"]["package_id"].as_str())
            .collect();
        if licenses["schema_version"] != 2
            || expected_ids != collected_ids
            || collected.len() != third_party_count
            || licenses["errors"]
                .as_array()
                .is_none_or(|errors| !errors.is_empty())
        {
            return Err(failure(
                "license_manifest",
                "locked package inventory or mandatory license coverage is incomplete",
            ));
        }
        report.case_results.push(CaseResultRecord {
            case_id: "M2-G-06".into(),
            work_package: "M2-W08".into(),
            tier: "required".into(),
            status: "passed".into(),
            elapsed_ms: g06_start.elapsed().as_millis() as u64,
            evidence: format!(
                "delta-desktop direct deps={desktop_deps:?}; {third_party_count} locked packages collected with exact SPDX OR selection, all AND obligations and original notices; MPL-2.0={mpl_packages:?}; {}",
                logs.join("licenses/manifest.json").display()
            ),
        });

        // 4. Execute & Record Technical Spikes (`M2-K01`..`M2-K06`)
        // M2-K01: AppHost + gpui-kit headless overview + dependency boundary
        let k01_passed = report
            .test_results
            .get("workbench::tests::test_m2_k01_and_a03_thin_slice_ac02_overview_and_dependency_boundary")
            .is_some_and(|s| s == "passed");
        let mut k01_metrics = BTreeMap::new();
        k01_metrics.insert("desktop_direct_deps".into(), json!(desktop_deps));
        k01_metrics.insert("cash_value".into(), json!("1438"));
        k01_metrics.insert("net_worth".into(), json!("2068"));
        k01_metrics.insert("total_pnl".into(), json!("68"));
        report.poc_results.push(PocResultRecord {
            id: "M2-K01".into(),
            title: "AppHost thin-slice + gpui-kit 0.9.0 headless MetricCard (AC-02) + dependency boundary".into(),
            status: if k01_passed { "passed" } else { "failed" }.into(),
            metrics: k01_metrics,
            detail: "Headless VisualTestAppContext rendered Cash 1438, NAV 2068, Total P&L 68 with 0 visible windows; delta-desktop direct dependency boundary verified.".into(),
        });

        // M2-K02: 100,000-bar KlineViewport 300-frame zoom/pan benchmark
        let (k02_p50_us, k02_p95_us, k02_total_bars) = run_m2_k02_viewport_bench();
        let mut k02_metrics = BTreeMap::new();
        k02_metrics.insert("total_bars".into(), json!(k02_total_bars));
        k02_metrics.insert("visible_bars".into(), json!(1_000));
        k02_metrics.insert("frames_executed".into(), json!(300));
        k02_metrics.insert("p50_us".into(), json!(k02_p50_us));
        k02_metrics.insert("p95_us".into(), json!(k02_p95_us));
        k02_metrics.insert("measurement".into(), json!("viewport slice microbenchmark only; excludes GPUI layout, painting and presentation"));
        report.poc_results.push(PocResultRecord {
            id: "M2-K02".into(),
            title: "100,000-bar native GPUI K-line benchmark (release); separate viewport microbenchmark".into(),
            status: "not-run".into(),
            metrics: k02_metrics,
            detail: format!(
                "Viewport slicing p50={k02_p50_us} us, p95={k02_p95_us} us; actual frame acceptance requires explicit --ui-bench."
            ),
        });

        // M2-K03: SSE framing & dual-protocol loopback verification
        let k03_rep = run_m2_k03_and_a13_verification().map_err(|e| failure("m2_k03_failed", e))?;
        let k03_passed = k03_rep["status"].as_str() == Some("passed");
        let k03_metrics: BTreeMap<String, Value> =
            serde_json::from_value(k03_rep).unwrap_or_default();
        report.poc_results.push(PocResultRecord {
            id: "M2-K03".into(),
            title: "eventsource-stream 0.2.3 SSE framing & OpenAI Responses / Chat Completions loopback verification".into(),
            status: if k03_passed {
                "passed"
            } else {
                "failed"
            }
            .into(),
            metrics: k03_metrics,
            detail: "Both protocols verified across loopback cases (cross-byte UTF-8, split tool arguments, [DONE]/response.completed, truncated stream no-replay, 401/403 no-retry, 429 Retry-After, cancellation, unknown protocol rejection)".into(),
        });

        // M2-K04: Per-user packaging & side-by-side coexistence (`S-23`)
        let k04_start = Instant::now();
        let pkg_localappdata = temp.join("k04-localappdata");
        let pkg_out = reports.join("packages");
        // Seed user data directories for both `delta` and `delta-dev` to verify uninstall preserves them
        fs::create_dir_all(pkg_localappdata.join("delta")).map_err(|e| failure("k04_seed", e))?;
        fs::write(
            pkg_localappdata.join("delta/user_marker.txt"),
            b"preserve_release_data",
        )
        .map_err(|e| failure("k04_seed", e))?;
        fs::create_dir_all(pkg_localappdata.join("delta-dev"))
            .map_err(|e| failure("k04_seed", e))?;
        fs::write(
            pkg_localappdata.join("delta-dev/user_marker.txt"),
            b"preserve_dev_data",
        )
        .map_err(|e| failure("k04_seed", e))?;

        let artifacts = crate::packaging::build_packages(&root, &pkg_out)?;
        let release_artifact = artifacts
            .iter()
            .find(|artifact| artifact.channel == "release")
            .ok_or_else(|| failure("package_artifact", "release package missing"))?;
        let dev_artifact = artifacts
            .iter()
            .find(|artifact| artifact.channel == "dev")
            .ok_or_else(|| failure("package_artifact", "dev package missing"))?;
        let rel_pkg: PerUserPackageReceipt = serde_json::from_value(package_cli_command(
            release_artifact,
            "package-install",
            &pkg_localappdata,
            &pkg_out,
            &logs.join("package-install-release.json"),
            &gate_env,
        )?)
        .map_err(|e| failure("package_receipt", e))?;
        let dev_pkg: PerUserPackageReceipt = serde_json::from_value(package_cli_command(
            dev_artifact,
            "package-install",
            &pkg_localappdata,
            &pkg_out,
            &logs.join("package-install-dev.json"),
            &gate_env,
        )?)
        .map_err(|e| failure("package_receipt", e))?;
        // Verify both coexist side-by-side under Programs/delta and Programs/delta-dev
        let both_coexist = Path::new(&rel_pkg.install_dir).join("delta.exe").is_file()
            && Path::new(&dev_pkg.install_dir)
                .join("delta-dev.exe")
                .is_file();
        let self_uninstall = run_quiet_command(
            Path::new(&dev_pkg.install_dir)
                .join("delta-dev-cli.exe")
                .as_os_str(),
            [
                "package-uninstall".into(),
                "--channel".into(),
                "dev".into(),
                "--local-app-data".into(),
                pkg_localappdata.display().to_string(),
            ],
            None,
            &gate_env,
        )
        .map_err(|e| failure("self_uninstall", e))?;
        fs::write(
            logs.join("installed-self-uninstall.json"),
            serde_json::to_vec_pretty(&self_uninstall).map_err(|e| failure("package_log", e))?,
        )
        .map_err(|e| failure("package_log", e))?;
        if self_uninstall.status_code == Some(0)
            || self_uninstall.timed_out
            || !Path::new(&dev_pkg.install_dir)
                .join("delta-dev.exe")
                .is_file()
            || !Path::new(&dev_pkg.install_dir)
                .join("install-manifest.json")
                .is_file()
        {
            return Err(failure(
                "self_uninstall",
                "running installed CLI must refuse before removing files",
            ));
        }
        // Uninstall `dev` channel and verify `delta-dev/user_marker.txt` and `Programs/delta` remain intact
        package_cli_command(
            dev_artifact,
            "package-uninstall",
            &pkg_localappdata,
            &pkg_out,
            &logs.join("package-external-uninstall-dev.json"),
            &gate_env,
        )?;
        let data_preserved = pkg_localappdata.join("delta-dev/user_marker.txt").is_file()
            && pkg_localappdata.join("delta/user_marker.txt").is_file()
            && Path::new(&rel_pkg.install_dir).join("delta.exe").is_file()
            && !Path::new(&dev_pkg.install_dir).exists();
        // Re-install `dev` so both packages remain available for inspection
        let dev_pkg: PerUserPackageReceipt = serde_json::from_value(package_cli_command(
            dev_artifact,
            "package-install",
            &pkg_localappdata,
            &pkg_out,
            &logs.join("package-reinstall-dev.json"),
            &gate_env,
        )?)
        .map_err(|e| failure("package_receipt", e))?;
        let unknown_file = Path::new(&dev_pkg.install_dir).join("user-unowned-marker.txt");
        fs::write(&unknown_file, b"preserve unknown file")
            .map_err(|e| failure("package_marker", e))?;
        package_cli_command(
            dev_artifact,
            "package-uninstall",
            &pkg_localappdata,
            &pkg_out,
            &logs.join("package-external-uninstall-unknown-files.json"),
            &gate_env,
        )?;
        let unknown_preserved = unknown_file.is_file()
            && !Path::new(&dev_pkg.install_dir)
                .join("delta-dev.exe")
                .exists();
        package_cli_command(
            release_artifact,
            "package-uninstall",
            &pkg_localappdata,
            &pkg_out,
            &logs.join("package-external-uninstall-release.json"),
            &gate_env,
        )?;
        let release_uninstalled_preserving_data = !Path::new(&rel_pkg.install_dir).exists()
            && pkg_localappdata.join("delta/user_marker.txt").is_file()
            && pkg_localappdata.join("delta-dev/user_marker.txt").is_file()
            && unknown_file.is_file();
        if !both_coexist
            || !data_preserved
            || !unknown_preserved
            || !release_uninstalled_preserving_data
        {
            return Err(failure(
                "m2_k04_packaging",
                "per-user side-by-side coexistence or uninstall data preservation failed",
            ));
        }
        let mut k04_metrics = BTreeMap::new();
        k04_metrics.insert("release_zip_sha256".into(), json!(rel_pkg.zip_sha256));
        k04_metrics.insert("dev_zip_sha256".into(), json!(dev_pkg.zip_sha256));
        k04_metrics.insert("coexistence_verified".into(), json!(both_coexist));
        k04_metrics.insert("uninstall_preserves_data".into(), json!(data_preserved));
        k04_metrics.insert(
            "release_uninstall_preserves_both_data_roots".into(),
            json!(release_uninstalled_preserving_data),
        );
        k04_metrics.insert(
            "uninstall_preserves_unknown_files".into(),
            json!(unknown_preserved),
        );
        k04_metrics.insert(
            "installed_self_uninstall_refused_before_mutation".into(),
            json!(true),
        );
        k04_metrics.insert("artifacts".into(), json!(artifacts));
        k04_metrics.insert(
            "packaged_cli_install_and_external_uninstall".into(),
            json!(true),
        );
        k04_metrics.insert("binary_imports".into(), json!({
            "release": serde_json::from_slice::<Value>(&fs::read(release_artifact.directory.join("binary-imports.json"))
                .map_err(|e| failure("package_imports", e))?).map_err(|e| failure("package_imports", e))?,
            "dev": serde_json::from_slice::<Value>(&fs::read(dev_artifact.directory.join("binary-imports.json"))
                .map_err(|e| failure("package_imports", e))?).map_err(|e| failure("package_imports", e))?,
        }));
        k04_metrics.insert(
            "elapsed_ms".into(),
            json!(k04_start.elapsed().as_millis() as u64),
        );
        report.poc_results.push(PocResultRecord {
            id: "M2-K04".into(),
            title: "Per-user Windows package builder, channel coexistence (delta / delta-dev), and data-preserving uninstall".into(),
            status: "passed".into(),
            metrics: k04_metrics,
            detail: "S-23 portable zip + Rust installer fallback: real x86_64 MSVC release builds with static CRT and separate release/dev channels; normal/delay PE imports, build-info and file hashes checked. Packaged CLIs install side-by-side and perform external uninstall preserving data and unknown files; installed self-uninstall refuses without mutation.".into(),
        });

        // M2-K05: Bundled SQLite FTS5 + trigram tokenizer spike
        let k05_val = run_m2_k05_fts5_trigram_spike(&temp.join("k05_fts5"))
            .map_err(|e| failure("m2_k05_fts5", e))?;
        let k05_metrics: BTreeMap<String, Value> =
            serde_json::from_value(k05_val).unwrap_or_default();
        report.poc_results.push(PocResultRecord {
            id: "M2-K05".into(),
            title: "Bundled SQLite FTS5 trigram tokenizer + CJK 1/2/3+ char & English search verification".into(),
            status: "passed".into(),
            metrics: k05_metrics,
            detail: "rusqlite 0.41.1 bundled SQLite supports FTS5 tokenize='trigram'; 3+ char Chinese & English queries use FTS5 MATCH, 1-2 char queries use bounded LIKE fallback.".into(),
        });

        // M2-K06: Main-thread 8 MiB stack linker config & optional `--ui-smoke` real window check
        let build_rs_text =
            fs::read_to_string(root.join("crates/delta-desktop/build.rs")).unwrap_or_default();
        let has_8mb_stack = build_rs_text.contains("8388608");
        let mut k06_metrics = BTreeMap::new();
        k06_metrics.insert("msvc_stack_bytes".into(), json!(8_388_608));
        k06_metrics.insert("ui_smoke_executed".into(), json!(ui_smoke));
        if ui_smoke {
            let smoke_res = run_quiet_command(
                OsStr::new("cargo"),
                [
                    "run",
                    "-p",
                    "delta-desktop",
                    "--bin",
                    "delta",
                    "--locked",
                    "--",
                    "--ui-smoke",
                    "--data-dir",
                    &temp.join("ui-smoke-data").display().to_string(),
                ],
                Some(&root),
                &gate_env,
            )
            .map_err(|e| failure("ui_smoke", e))?;
            let path = logs.join("native-ui-smoke.json");
            fs::write(
                &path,
                serde_json::to_vec_pretty(&smoke_res).map_err(|e| failure("smoke_log", e))?,
            )
            .map_err(|e| failure("smoke_log", e))?;
            report.log_files.insert(
                "native-ui-smoke".into(),
                path.strip_prefix(&root).unwrap().display().to_string(),
            );
            let smoke: Value =
                serde_json::from_str(&smoke_res.stdout).map_err(|e| failure("smoke_report", e))?;
            if smoke_res.status_code != Some(0)
                || smoke_res.timed_out
                || smoke["status"] != "passed"
                || smoke["native_window_created"] != true
                || smoke["visible_windows"] != 0
                || smoke["focus_stolen"] != false
            {
                return Err(failure(
                    "ui_smoke_failed",
                    format!("delta --ui-smoke failed: {}", smoke_res.stderr),
                ));
            }
            k06_metrics.insert("ui_smoke_stdout".into(), json!(smoke_res.stdout.trim()));
        }
        report.poc_results.push(PocResultRecord {
            id: "M2-K06".into(),
            title: "Windows main-thread 8 MiB stack configuration (/STACK:8388608) & quiet window startup".into(),
            status: if !has_8mb_stack { "failed" } else if ui_smoke { "passed" } else { "not-run" }.into(),
            metrics: k06_metrics,
            detail: format!(
                "MSVC /STACK:8388608 configured; real hidden-window startup ui_smoke={ui_smoke}; configuration alone does not count as a window test"
            ),
        });

        // 5. Map all M2 Stage Cases (`M2-A-01`..`M2-A-20`, `M2-M-01`..`M2-M-03`, `M2-L-01`..`M2-L-02`)
        for (id, package, tier, names) in [
            (
                "M2-A-01",
                "M2-W01",
                "required",
                vec!["host::tests::test_m2_a01_scoped_query_consistency_and_stale_generation_rejection",
                    "regression::frozen_empty_scope_and_zero_market_version_never_expand_to_new_data"],
            ),
            (
                "M2-A-02",
                "M2-W01",
                "required",
                vec!["m2_schema::tests::test_m2_a02_schema_4_migration_backup_fault_injection_and_idempotency"],
            ),
            (
                "M2-A-03",
                "M2-W01,M2-W03,M2-W04",
                "required",
                vec!["workbench::tests::test_m2_k01_and_a03_thin_slice_ac02_overview_and_dependency_boundary"],
            ),
            (
                "M2-A-04",
                "M2-W02",
                "required",
                vec![
                    "host::tests::test_m2_a04_accounts_portfolios_and_scoped_period_pnl_breakdown",
                    "m2_schema::tests::test_m2_a04_portfolio_deduplication_and_accounts_crud",
                ],
            ),
            (
                "M2-A-05",
                "M2-W02",
                "required",
                vec!["host::tests::test_m2_a05_missing_price_fx_cost_manual_valuations_and_stale_reports"],
            ),
            (
                "M2-A-06",
                "M2-W02",
                "required",
                vec!["tests::m2_a06_ledger01_cash_exchange_usd_usdt_btc"],
            ),
            (
                "M2-A-07",
                "M2-W03",
                "required",
                vec!["workbench::tests::test_m2_a07_workbench_navigation_scope_bar_layout_recovery_and_import_preview",
                    "workbench::tests::actual_controls_preserve_input_save_before_scope_switch_and_recover_failure",
                    "workbench::tests::actual_first_run_import_preview_commit_and_ai_failure_controls_reach_the_host"],
            ),
            (
                "M2-A-08",
                "M2-W03",
                "required",
                vec!["workbench::tests::test_m2_a08_data_center_sync_progress_cancel_and_failure_states",
                    "regression::in_flight_sync_cancellation_releases_resources_and_delete_restore_keep_identity",
                    "regression::real_sync_job_reports_actual_prepared_and_published_records"],
            ),
            (
                "M2-A-09",
                "M2-W04",
                "required",
                vec!["workbench::tests::test_m2_a09_assets_holdings_fifo_lots_flows_reconciliation_and_drilldown"],
            ),
            (
                "M2-A-10",
                "M2-W05",
                "required",
                vec!["kline::tests::test_m2_a10_kline_viewport_100k_bars_window_invariance_and_future_sentinel",
                    "workbench::tests::chart_mouse_wheel_crosshair_escape_and_marker_click_reach_scoped_host"],
            ),
            (
                "M2-A-11",
                "M2-W05,M2-W06",
                "required",
                vec![
                    "m2_schema::tests::test_m2_a11_trade_plan_fill_allocation_bounds_and_chart_context_roundtrip",
                    "workbench::tests::test_m2_a11_and_a12_markers_chart_context_roundtrip_and_journal_autosave",
                    "workbench::tests::chart_mouse_wheel_crosshair_escape_and_marker_click_reach_scoped_host",
                    "regression::journal_scope_enforces_accounts_review_period_cutoff_and_permission_changes",
                ],
            ),
            (
                "M2-A-12",
                "M2-W06",
                "required",
                vec![
                    "m2_schema::tests::test_m2_a12_notes_autosave_honesty_templates_fts5_search_and_attachments",
                    "workbench::tests::test_m2_a11_and_a12_markers_chart_context_roundtrip_and_journal_autosave",
                ],
            ),
            (
                "M2-A-13",
                "M2-W07",
                "required",
                vec!["tests::test_m2_k03_and_a13_full_loopback_verification"],
            ),
            (
                "M2-A-14",
                "M2-W07",
                "required",
                vec!["ai::tests::test_m2_a14_ai_review_ac02_qa_evidence_number_verification_and_injection_defense",
                    "ai::tests::report_requires_explicit_evidence_and_rejects_chinese_signed_or_forged_values",
                    "ai::tests::detail_projection_has_its_own_result_id_and_excludes_omitted_evidence"],
            ),
            (
                "M2-A-15",
                "M2-W07",
                "required",
                vec!["ai::tests::test_m2_a15_sessions_atomic_compaction_recovery_cancel_and_late_events",
                    "ai::tests::model_credentials_are_library_scoped_and_blank_edits_restart_and_rollback_preserve_them"],
            ),
            (
                "M2-A-16",
                "M2-W07",
                "required",
                vec![
                    "ai::tests::test_m2_a16_local_features_and_saved_reports_work_without_model",
                    "workbench::tests::test_m2_a16_local_workbench_without_model_and_unreachable_endpoint",
                    "workbench::tests::actual_first_run_import_preview_commit_and_ai_failure_controls_reach_the_host",
                ],
            ),
            (
                "M2-A-17",
                "M2-W08",
                "required",
                vec!["service::export::tests::test_m2_a17_backup_restore_export_delete_and_log_redaction",
                    "regression::in_flight_sync_cancellation_releases_resources_and_delete_restore_keep_identity",
                    "packaging::tests::uninstall_validates_all_hashes_before_mutation_and_retains_unknown_files",
                    "packaging::tests::installer_rejects_wrong_identity_before_creating_target",
                    "packaging::tests::windows_pe_runtime_imports_include_delay_load_and_reject_truncation",
                    "packaging::tests::windows_pe_inspection_reads_the_actual_executable",
                    "verify_m2::benchmark_tests::viewport_or_dev_samples_cannot_pass_native_frame_acceptance"],
            ),
        ] {
            case_from_tests(&mut report, id, package, tier, &names);
        }

        // M2-A-18: M1 regression through the M2 workspace
        let m1_regression_tests = [
            "tests::test_poc_d02_xnys_calendar_1990_to_2027_day_by_day",
            "tests::test_s08_yahoo_restore_raw_and_cross_validation_over_loopback",
            "regression::repeated_company_action_versions_and_sources_do_not_multiply_adjustment",
            "regression::foreign_report_currency_uses_historical_flows_and_explains_translation",
            "regression::missing_book_fx_is_synced_before_import_commits",
            "regression::non_usd_book_currency_triangulates_and_freezes_event_fx",
            "tests::test_binance_archive_ms_and_us_timestamps_and_checksum",
            "regression::monthly_archive_backfill_verifies_checksum_before_publishing",
            "regression::multi_page_sync_retries_429_and_persists_the_subscription_id",
            "tests::poc_d05_ac02_minimal_ledger_and_fifo",
            "regression::ac02_application_import_sync_report_and_export_match_hand_calculation",
            "regression::asset_fee_disposes_one_fifo_slice_and_preserves_unknown_cost_flag",
            "regression::third_asset_fee_reduces_one_lot_and_corrections_preserve_journals",
            "regression::opening_cash_requires_its_own_currency_and_known_openings_need_unit_cost",
            "regression::stablecoin_quote_leg_is_settled_once_from_cash",
            "tests::ac04_internal_transfer_and_ac05_split_and_ac07_twr_xirr",
            "regression::report_cash_flows_produce_zero_returns_for_unchanged_market",
            "regression::external_destination_is_excluded_and_registered_empty_account_keeps_internal_transit",
            "regression::crypto_funding_keeps_unknown_history_after_full_disposal_until_cost_correction",
            "tests::test_statement_importers_and_header_mismatch",
            "regression::independent_funding_presets_commit_cash_income_fees_and_withdrawals",
            "regression::strict_headers_missing_ids_and_multi_account_flex_are_handled_explicitly",
            "regression::source_business_ids_deduplicate_across_files_and_revisions_require_correction",
            "regression::same_preview_replay_is_idempotent_and_modified_preview_is_rejected",
            "regression::event_book_fx_does_not_change_when_historical_market_fx_is_revised",
            "tests::test_s09_storage_version_intervals_and_backup_restore",
            "regression::recursive_backup_includes_settings_and_detects_tampering",
            "regression::past_balance_snapshot_ignores_later_deposit",
            "regression::recorded_account_providers_normalize_fills_balances_and_completed_funding",
            "regression::all_indicators_match_independent_hand_values_and_ignore_future_sentinel",
            "regression::historical_indicators_ignore_future_sentinel_and_published_versions_are_immutable",
            "tests::test_s02_background_job_panic_timeout_and_cancel",
            "regression::cancelled_sync_publishes_no_version",
            "tests::test_s11_channel_resolution_and_non_writable_no_fallback",
            "regression::public_sessions_require_lock_and_cannot_impersonate_release_channel",
            "regression::source_channel_cannot_target_release_user_file_root",
        ];
        case_from_tests(
            &mut report,
            "M2-A-18",
            "M2-W08",
            "required",
            &m1_regression_tests,
        );

        // M2-A-19: Dependency lock and license inventory for distribution
        report.case_results.push(CaseResultRecord {
            case_id: "M2-A-19".into(),
            work_package: "M2-W08".into(),
            tier: "required".into(),
            status: "passed".into(),
            elapsed_ms: 0,
            evidence: format!(
                "M2-G-06 verified {third_party_count} third-party packages from cargo metadata --locked; MPL-2.0 source notices bundled in THIRD-PARTY-NOTICES.txt and per-user packages"
            ),
        });

        // M2-A-20: Fixed-scale performance benchmarks (`--ui-bench`)
        if ui_bench {
            let b_start = Instant::now();
            let bench_res = run_quiet_command(
                release_artifact.directory.join("delta.exe").as_os_str(),
                [
                    "--ui-bench",
                    "--data-dir",
                    &temp.join("ui-bench-data").display().to_string(),
                ],
                Some(&root),
                &gate_env,
            )
            .map_err(|e| failure("ui_bench", e))?;
            let path = logs.join("native-ui-bench.json");
            fs::write(
                &path,
                serde_json::to_vec_pretty(&bench_res).map_err(|e| failure("bench_log", e))?,
            )
            .map_err(|e| failure("bench_log", e))?;
            report.log_files.insert(
                "native-ui-bench".into(),
                path.strip_prefix(&root).unwrap().display().to_string(),
            );
            let bench_path = reports.join("ui-bench-report.json");
            fs::write(&bench_path, &bench_res.stdout).map_err(|e| failure("write_bench", e))?;
            let bench: Value =
                serde_json::from_str(&bench_res.stdout).map_err(|e| failure("bench_report", e))?;
            let status = if bench_res.status_code == Some(0)
                && !bench_res.timed_out
                && native_benchmark_passed(&bench)
            {
                "passed"
            } else {
                "failed"
            };
            if let Some(k02) = report.poc_results.iter_mut().find(|poc| poc.id == "M2-K02") {
                k02.status = status.into();
                k02.metrics
                    .insert("native_release_frames".into(), bench.clone());
                k02.detail = format!("Real release package ran a hidden Windows HWND, 100k bars/rows and 300 measured frames per component; p95 chart={} us, table={} us; raw samples saved in {}",
                    bench["kline_p95_us"], bench["table_scroll_p95_us"], bench_path.display());
            }
            report.case_results.push(CaseResultRecord {
                case_id: "M2-A-20".into(),
                work_package: "M2-W04,M2-W05,M2-W08".into(),
                tier: "required".into(),
                status: status.into(),
                elapsed_ms: b_start.elapsed().as_millis() as u64,
                evidence: format!(
                    "Explicit native release --ui-bench; 5 AC-02 first screens p95={} ms, 300 navigation actions p95={} us; 100k transactions/bars, 300 measured frames each; chart p50/p95/p99={}/{}/{} us, table={}/{}/{} us; controlled cancellation dispatch={} ms / actual worker release={} ms; raw report {}",
                    bench["first_screen_p95_ms"], bench["interaction_p95_us"],
                    bench["kline_p50_us"], bench["kline_p95_us"], bench["kline_p99_us"], bench["table_scroll_p50_us"], bench["table_scroll_p95_us"], bench["table_scroll_p99_us"],
                    bench["cancel_ui_latency_ms"], bench["cancel_backend_latency_ms"], bench_path.display()
                ),
            });
        } else {
            report.case_results.push(CaseResultRecord {
                case_id: "M2-A-20".into(),
                work_package: "M2-W04,M2-W05,M2-W08".into(),
                tier: "required".into(),
                status: "not-run".into(),
                elapsed_ms: 0,
                evidence: format!(
                    "Actual UI frame performance was not measured; explicit --ui-bench is required for AC-26. Viewport-only microbenchmark p50={k02_p50_us} us, p95={k02_p95_us} us is separate evidence."
                ),
            });
        }

        // Manual cases (`M2-M-01`..`M2-M-03`): tracked in USER-ACTIONS H-01..H-03, never fabricated as passed
        for (mid, pkg, desc) in [
            (
                "M2-M-01",
                "M2-W03,M2-W06",
                "Manual check USER-ACTIONS H-01 (Chinese IME composition in notes, search, and import fields)",
            ),
            (
                "M2-M-02",
                "M2-W03,M2-W04,M2-W05",
                "Manual check USER-ACTIONS H-02 (Keyboard navigation, focus, Esc, 125%/150% scaling, and color convention)",
            ),
            (
                "M2-M-03",
                "M2-W08",
                "Manual check USER-ACTIONS H-03 (Clean-machine Windows Sandbox per-user install, coexistence, and uninstall)",
            ),
        ] {
            report.case_results.push(CaseResultRecord {
                case_id: mid.into(),
                work_package: pkg.into(),
                tier: "manual".into(),
                status: "not-run".into(),
                elapsed_ms: 0,
                evidence: desc.into(),
            });
        }

        // Live cases (`M2-L-01`, `M2-L-02`)
        if live_mode {
            let docs = temp.join("live-documents");
            let service = AppService::open(
                ResolvedAppPaths {
                    channel: BuildChannel::Source,
                    data_dir: temp.join("live-data"),
                    documents_dir: docs.clone(),
                    default_export_dir: docs.join("exports"),
                    default_backup_dir: docs.join("backups"),
                    credential_prefix: "delta-dev".into(),
                },
                "USD",
                "USD",
                "UTC",
                true,
            )?;
            report.live_probes = service.run_live_network_probes();
            let public_providers = [
                "yahoo",
                "eastmoney",
                "yahoo-nvda",
                "yahoo-spy",
                "okx",
                "binance-vision",
                "binance-archive",
                "coinbase",
                "bitstamp",
                "bitstamp-usdt",
                "kraken",
                "frankfurter",
            ];
            let pub_probes: Vec<_> = report
                .live_probes
                .iter()
                .filter(|p| public_providers.contains(&p.provider.as_str()))
                .collect();
            let l01_status = if pub_probes.is_empty() {
                "not-run"
            } else if pub_probes
                .iter()
                .any(|p| p.status == "fail" || p.status == "failed")
            {
                "failed"
            } else {
                "passed"
            };
            report.case_results.push(CaseResultRecord {
                case_id: "M2-L-01".into(),
                work_package: "M2-W08".into(),
                tier: "live".into(),
                status: l01_status.into(),
                elapsed_ms: 0,
                evidence: pub_probes
                    .iter()
                    .map(|p| format!("{}:{}({:?})", p.provider, p.status, p.http_status))
                    .collect::<Vec<_>>()
                    .join(" | "),
            });
        } else {
            report.case_results.push(CaseResultRecord {
                case_id: "M2-L-01".into(),
                work_package: "M2-W08".into(),
                tier: "live".into(),
                status: "not-run".into(),
                elapsed_ms: 0,
                evidence: "Offline run; pass --live to execute public market data probes".into(),
            });
        }

        // M2-L-02: Real model service for user-configured protocol (ACT-01)
        report.case_results.push(CaseResultRecord {
            case_id: "M2-L-02".into(),
            work_package: "M2-W07".into(),
            tier: "live".into(),
            status: "not-run".into(),
            elapsed_ms: 0,
            evidence: "USER-ACTIONS ACT-01 real model endpoint/credential not configured; dual-protocol loopback verified in M2-A-13 / M2-K03".into(),
        });

        if report.case_results.iter().any(|c| c.status == "failed") {
            return Err(failure(
                "test_mapping",
                "one or more required M2 cases failed",
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
    {
        report
            .errors
            .push("quietness or data isolation check failed".into());
    }
    if source_fingerprint(&root)? != report.source_fingerprint {
        report
            .errors
            .push("source changed during verification".into());
    }
    if cfg!(windows) && report.process_priority != "below_normal" {
        report
            .errors
            .push("process priority is not below normal".into());
    }

    let failed = !report.errors.is_empty()
        || report.case_results.iter().any(|c| c.status == "failed")
        || report.poc_results.iter().any(|poc| poc.status == "failed");
    let required_not_run = report
        .case_results
        .iter()
        .any(|case| case.tier == "required" && case.status == "not-run")
        || report.poc_results.iter().any(|poc| poc.status == "not-run");
    report.overall_status = if failed {
        "failed"
    } else if required_not_run {
        "not-run"
    } else {
        "passed"
    }
    .into();
    let json = serde_json::to_string_pretty(&report).map_err(|e| failure("report", e))?;
    let name = if live_mode {
        "verify-stage-live-report.json"
    } else {
        "verify-stage-report.json"
    };
    fs::write(reports.join(name), &json).map_err(|e| failure("report", e))?;
    println!("{}", sanitize_terminal_output(&json));
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn native_benchmark_passed(report: &Value) -> bool {
    if report["build_profile"] != "release"
        || report["native_window_created"] != true
        || report["focus_stolen"] != false
        || report["visible_windows"] != 0
        || report["transaction_rows"] != 100_000
        || report["kline_total_bars"] != 100_000
        || report["kline_visible_bars"] != 1_000
        || report["native_chart_paints"]
            .as_u64()
            .is_none_or(|count| count < 300)
        || report["native_table_cells"]
            .as_u64()
            .is_none_or(|count| count < 2400)
        || report["cancel_ui_latency_ms"]
            .as_u64()
            .is_none_or(|time| time > 1000)
        || report["cancel_backend_latency_ms"]
            .as_u64()
            .is_none_or(|time| time > 5000)
    {
        return false;
    }
    for (samples, prefix, count, limit) in [
        ("first_screen_times_ms", "first_screen", 5, 3000),
        ("interaction_times_us", "interaction", 300, 100_000),
    ] {
        let Some(mut times) = report[samples].as_array().and_then(|samples| {
            samples
                .iter()
                .map(Value::as_u64)
                .collect::<Option<Vec<_>>>()
        }) else {
            return false;
        };
        if times.len() != count || times.contains(&0) {
            return false;
        }
        times.sort_unstable();
        let unit = if prefix == "first_screen" { "ms" } else { "us" };
        for percent in [50, 95, 99] {
            let value = times[(times.len() * percent).div_ceil(100) - 1];
            if report[format!("{prefix}_p{percent}_{unit}")].as_u64() != Some(value)
                || (percent == 95 && value > limit)
            {
                return false;
            }
        }
    }
    for (samples, frames, prefix) in [
        ("chart_frame_times_us", "kline_frames", "kline"),
        (
            "table_frame_times_us",
            "table_scroll_frames",
            "table_scroll",
        ),
    ] {
        let Some(samples) = report[samples].as_array() else {
            return false;
        };
        let Some(mut times) = samples
            .iter()
            .map(Value::as_u64)
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        if times.len() != 300 || report[frames] != 300 || times.contains(&0) {
            return false;
        }
        times.sort_unstable();
        for percent in [50, 95, 99] {
            let measured = times[(times.len() * percent).div_ceil(100) - 1];
            if report[format!("{prefix}_p{percent}_us")].as_u64() != Some(measured)
                || (percent == 95 && measured > 20_000)
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod benchmark_tests {
    use super::*;

    #[test]
    fn viewport_or_dev_samples_cannot_pass_native_frame_acceptance() {
        assert!(!native_benchmark_passed(
            &json!({"kline_p95_us": 1, "kline_total_bars": 100_000})
        ));
        let mut report = json!({"build_profile":"release", "native_window_created":true,"visible_windows":0,"focus_stolen":false,
            "transaction_rows":100_000,"kline_total_bars":100_000,"kline_visible_bars":1000,"native_chart_paints":320,"native_table_cells":49600,
            "cancel_ui_latency_ms":0,"cancel_backend_latency_ms":5,"kline_frames":300,"table_scroll_frames":300,
            "first_screen_times_ms":vec![500_u64;5],"first_screen_p50_ms":500,"first_screen_p95_ms":500,"first_screen_p99_ms":500,
            "interaction_times_us":vec![25000_u64;300],"interaction_p50_us":25000,"interaction_p95_us":25000,"interaction_p99_us":25000,
            "chart_frame_times_us":vec![1000_u64;300],"table_frame_times_us":vec![1000_u64;300],
            "kline_p50_us":1000,"kline_p95_us":1000,"kline_p99_us":1000,"table_scroll_p50_us":1000,"table_scroll_p95_us":1000,"table_scroll_p99_us":1000});
        assert!(native_benchmark_passed(&report));
        report["build_profile"] = json!("dev");
        assert!(!native_benchmark_passed(&report));
        report["build_profile"] = json!("release");
        report["kline_p99_us"] = json!(999);
        assert!(!native_benchmark_passed(&report));
        report["kline_p99_us"] = json!(1000);
        report["native_table_cells"] = json!(640);
        assert!(!native_benchmark_passed(&report));
    }
}
