mod installer;
mod packaging;
mod training_cmd;
mod verify;
mod verify_m2;
mod verify_m3;
mod verify_m4;
use chrono::{NaiveDate, Utc};
use delta_app::{
    apply_process_quiet_guards, resolve_app_paths, sanitize_terminal_output, AppService,
    BuildChannel, CredentialStore, InstanceLock, LiveProbeResult, QuietJobObject,
};
use delta_core::parse_decimal_lossless;
use delta_data::{AccountBalanceSnapshotDto, StatementPreset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use verify::run_verify_stage_m1;
use verify_m2::{install_per_user_channel, run_verify_stage_m2, uninstall_per_user_channel};
use verify_m3::run_verify_stage_m3;
use verify_m4::run_verify_stage_m4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PocResultRecord {
    pub id: String,
    pub title: String,
    pub status: String,
    pub metrics: BTreeMap<String, serde_json::Value>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResultRecord {
    pub case_id: String,
    pub work_package: String,
    pub tier: String,
    pub status: String,
    pub elapsed_ms: u64,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStageReport {
    pub stage: String,
    pub plan_version: String,
    pub executed_at: String,
    pub live_mode: bool,
    pub source_fingerprint: String,
    pub visible_window_count: usize,
    pub window_details: Vec<delta_app::WindowEvidence>,
    pub temp_residual_dirs: usize,
    pub outside_temp_created_dirs: usize,
    pub path_observation_scope: String,
    pub release_data_dir_touched: bool,
    pub poc_results: Vec<PocResultRecord>,
    pub case_results: Vec<CaseResultRecord>,
    pub live_probes: Vec<LiveProbeResult>,
    pub overall_status: String,
    pub gate_jobs: usize,
    pub process_priority: String,
    pub test_results: BTreeMap<String, String>,
    pub log_files: BTreeMap<String, String>,
    pub errors: Vec<String>,
}

fn main() -> ExitCode {
    apply_process_quiet_guards();
    let _job = QuietJobObject::new().inspect(|j| {
        let _ = j.assign_current_process();
    });

    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::SUCCESS;
    }

    match dispatch_cli(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!(
                "{}",
                sanitize_terminal_output(&serde_json::to_string_pretty(&err).unwrap_or_else(
                    |_| { format!(r#"{{"code":"{}","message":"{}"}}"#, err.code, err.message) }
                ))
            );
            ExitCode::from(1)
        }
    }
}

fn print_usage() {
    println!(
        "delta-cli (M4 v1.0) - financial data, ledger, training, installer and update operations\n\
         Subcommands:\n\
         - init [--data-dir <dir>]\n\
         - register --venue <NASDAQ|NYSE|OKX|BINANCE> --symbol <symbol> [--etf] [--data-dir <dir>]\n\
         - import --account <id> --preset <preset> --file <path> [--commit] [--data-dir <dir>]\n\
         - resolve-import --row-id <id> --include|--exclude [--data-dir <dir>]\n\
         - sync [--symbol <id,...>] [--from <date>] [--until <exclusive-date>] [--fixtures <dir>] [--data-dir <dir>]\n\
         - explain [--as-of <YYYY-MM-DD>] [--version <n>] [--data-dir <dir>]\n\
         - reconcile --account <id> --asset <id> --observed <qty> [--data-dir <dir>]\n\
         - quality [--data-dir <dir>]\n\
         - backup --dir <path> [--data-dir <dir>]\n\
         - restore --from <path> --to <path>\n\
         - export --dir <path> [--data-dir <dir>]\n\
         - library delete [--yes] [--data-dir <dir>]\n\
         - training create|list|show|order|cancel|advance|note|finish|reveal|report|replay|retry|delete|redraw|start|pause|resume|fee-profiles|self-eval|evidence [--data-dir <dir>]\n\
         - credential set|get|delete --provider <p> --user <u> --stdin [--prefix <pfx>]\n\
         - package-build [--identity delta-dev|delta-test|delta-dev-test] [--version <v>] [--out-dir <dir>] (installer in dist/; older ones of that identity removed)\n\
         - install-dev [--package <setup.exe>] (default: newest delta-dev installer in dist/)\n\
         - release-build --version 0.1.N [--key-file <file>] [--out-dir <dir>] | release-build --test-identity --key-file <throw-away key> (test rehearsal)\n\
         - update-test-source --release-dir <test release dir> [--seconds <n>] (loopback source for H-04)\n\
         - release-keygen [--key-file <file>] (default %APPDATA%/delta-release/update-signing.key; back up that file)\n\
         - control focus|quit-for-update [--identity <id>] [--timeout-secs <n>]\n\
         - verify-stage --stage M1 --plan-version 2.6 [--live]\n\
         - verify-stage --stage M2 --plan-version 1.0 [--live] [--ui-smoke] [--ui-bench]\n\
         - verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]\n\
         - verify-stage --stage M4 --plan-version 1.0 [--ui-smoke] [--ui-bench]\n\
         Test and migration diagnostics only: package-portable-build, package-install, package-uninstall"
    );
}

fn parse_flag(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn dispatch_cli(args: &[String]) -> Result<ExitCode, delta_app::ErrorEnvelope> {
    let sub = args[0].as_str();
    let cli_data_dir = parse_flag(args, "--data-dir").map(PathBuf::from);
    let channel_override = parse_flag(args, "--channel").and_then(|s| BuildChannel::parse(&s));
    if BuildChannel::current() != BuildChannel::Release
        && channel_override == Some(BuildChannel::Release)
        && !matches!(sub, "package-install" | "package-uninstall")
    {
        return Err(delta_app::ErrorEnvelope::new(
            "channel_isolation",
            "source builds cannot select the release channel",
            false,
        ));
    }

    match sub {
        "--build-info" => {
            println!("{}", delta_app::build_info("delta-cli"));
            Ok(ExitCode::SUCCESS)
        }
        "--version" | "-V" => {
            println!(
                "{} {} ({})",
                delta_app::InstallIdentity::current()
                    .map(delta_app::InstallIdentity::cli_exe)
                    .unwrap_or("delta-cli"),
                delta_app::app_version(),
                BuildChannel::current().as_str()
            );
            Ok(ExitCode::SUCCESS)
        }
        "package-build" | "install-dev" | "release-build" | "release-keygen" | "control"
        | "update-test-source" | "__migrate-portable" => installer_command(sub, args),
        "package-portable-build" => {
            let root = env::current_dir()
                .map_err(|e| delta_app::ErrorEnvelope::new("cwd", e.to_string(), false))?;
            let output = parse_flag(args, "--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".local/reports/m2/packages"));
            let artifacts = packaging::build_packages(&root, &output)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&artifacts).map_err(|e| {
                    delta_app::ErrorEnvelope::new("package_json", e.to_string(), false)
                })?
            );
            Ok(ExitCode::SUCCESS)
        }
        "--help" | "-h" | "help" => {
            print_usage();
            Ok(ExitCode::SUCCESS)
        }
        "__internal-crash-test" => {
            // S-10 self-test: verify SetErrorMode suppresses WER dialog on child abort
            apply_process_quiet_guards();
            std::process::abort();
        }
        "__internal-hold-lock" => {
            let dir = cli_data_dir.ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--data-dir required", false)
            })?;
            let ms: u64 = parse_flag(args, "--ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1000);
            let _lock = InstanceLock::acquire(&dir).map_err(|e| {
                delta_app::ErrorEnvelope::new("instance_locked", e.to_string(), true)
            })?;
            println!("LOCKED");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::thread::sleep(Duration::from_millis(ms));
            Ok(ExitCode::SUCCESS)
        }
        "__internal-hold-lock-tree" => {
            let directory = cli_data_dir.ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--data-dir required", false)
            })?;
            let _lock = InstanceLock::acquire(&directory).map_err(|e| {
                delta_app::ErrorEnvelope::new("instance_locked", e.to_string(), false)
            })?;
            let mut descendant = delta_app::spawn_quiet_child(
                env::current_exe()
                    .map_err(|e| delta_app::ErrorEnvelope::new("exe", e.to_string(), false))?
                    .as_os_str(),
                [
                    "__internal-hold-lock",
                    "--data-dir",
                    &directory.join("descendant").display().to_string(),
                    "--ms",
                    "10000",
                ],
                &BTreeMap::new(),
            )
            .map_err(|e| delta_app::ErrorEnvelope::new("spawn", e, false))?;
            std::thread::sleep(Duration::from_secs(10));
            descendant
                .wait()
                .map_err(|e| delta_app::ErrorEnvelope::new("wait", e.to_string(), false))?;
            Ok(ExitCode::SUCCESS)
        }
        "__internal-credential-read" => {
            let prefix = parse_flag(args, "--prefix")
                .filter(|s| s.starts_with("delta-test-"))
                .ok_or_else(|| {
                    delta_app::ErrorEnvelope::new(
                        "test_namespace_required",
                        "internal credential probe requires a delta-test- namespace",
                        false,
                    )
                })?;
            let provider = parse_flag(args, "--provider").unwrap_or_else(|| "tiingo".to_string());
            let user = parse_flag(args, "--user").unwrap_or_else(|| "default".to_string());
            let dir = cli_data_dir.as_deref().ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--data-dir required", false)
            })?;
            let store = CredentialStore::new(dir, prefix);
            let val = store.get_secret(&provider, &user).map_err(|e| {
                delta_app::ErrorEnvelope::new("credential_error", e.to_string(), false)
            })?;
            println!("{}", sanitize_terminal_output(&val));
            Ok(ExitCode::SUCCESS)
        }
        "init" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            println!(
                "{}",
                serde_json::to_string_pretty(svc.paths()).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "register" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let venue = parse_flag(args, "--venue").ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--venue required", false)
            })?;
            let symbol = parse_flag(args, "--symbol").ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--symbol required", false)
            })?;
            let id = svc.register_market_instrument(&venue, &symbol, has_flag(args, "--etf"))?;
            println!("{}", serde_json::json!({"instrument_id":id}));
            Ok(ExitCode::SUCCESS)
        }
        "import" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let account =
                parse_flag(args, "--account").unwrap_or_else(|| "ACC-DEFAULT".to_string());
            let preset_str = parse_flag(args, "--preset").unwrap_or_else(|| "generic".to_string());
            let preset = StatementPreset::parse(&preset_str).ok_or_else(|| {
                delta_app::ErrorEnvelope::new(
                    "invalid_preset",
                    format!("unknown statement preset '{preset_str}'"),
                    false,
                )
            })?;
            let file_path = parse_flag(args, "--file").ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--file <path> required", false)
            })?;
            let commit = has_flag(args, "--commit");
            let (preview, receipt) =
                svc.import_statement(&account, preset, Path::new(&file_path), commit)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "preview": preview,
                    "receipt": receipt,
                }))
                .unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "resolve-import" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let row = parse_flag(args, "--row-id").ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--row-id required", false)
            })?;
            if has_flag(args, "--include") == has_flag(args, "--exclude") {
                return Err(delta_app::ErrorEnvelope::new(
                    "invalid_arg",
                    "choose --include or --exclude",
                    false,
                ));
            }
            let revision = svc.resolve_import_row(&row, has_flag(args, "--include"))?;
            println!("{}", serde_json::json!({"ledger_revision":revision}));
            Ok(ExitCode::SUCCESS)
        }
        "sync" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let summary = if let Some(fixtures) = parse_flag(args, "--fixtures") {
                svc.sync_from_fixtures(Path::new(&fixtures))?
            } else {
                let parse_date = |flag: &str,
                                  default: NaiveDate|
                 -> Result<NaiveDate, delta_app::ErrorEnvelope> {
                    parse_flag(args, flag)
                        .map(|d| {
                            NaiveDate::parse_from_str(&d, "%Y-%m-%d").map_err(|e| {
                                delta_app::ErrorEnvelope::new("invalid_date", e.to_string(), false)
                            })
                        })
                        .unwrap_or(Ok(default))
                };
                let end = parse_date("--until", Utc::now().date_naive())?;
                let events = svc.load_all_ledger_events()?;
                let start_default = events
                    .iter()
                    .map(|e| e.occurred_at.date_naive())
                    .min()
                    .unwrap_or(end - chrono::Duration::days(30));
                let start = parse_date("--from", start_default)?;
                let mut instruments = Vec::new();
                for symbol in parse_flag(args, "--symbol")
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                {
                    instruments.push(svc.resolve_market_symbol(symbol)?);
                }
                svc.sync_market_data(&delta_app::SyncRequest {
                    start,
                    end,
                    instruments,
                    fx_pairs: vec![],
                    use_configured_credentials: true,
                    base_urls: BTreeMap::new(),
                })?
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "explain" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let as_of_str =
                parse_flag(args, "--as-of").unwrap_or_else(|| Utc::now().date_naive().to_string());
            let as_of = NaiveDate::parse_from_str(&as_of_str, "%Y-%m-%d")
                .map_err(|e| delta_app::ErrorEnvelope::new("invalid_date", e.to_string(), false))?;
            let version: i64 = parse_flag(args, "--version")
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| svc.latest_dataset_version());
            let summary = svc.evaluate_and_explain(as_of, version, None)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "reconcile" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let mut svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let account =
                parse_flag(args, "--account").unwrap_or_else(|| "ACC-DEFAULT".to_string());
            let asset = parse_flag(args, "--asset").unwrap_or_else(|| "USD".to_string());
            let obs_str = parse_flag(args, "--observed").unwrap_or_else(|| "0".to_string());
            let obs = parse_decimal_lossless(&obs_str).map_err(|e| {
                delta_app::ErrorEnvelope::new("invalid_decimal", e.to_string(), false)
            })?;
            let issues = svc.reconcile_account(
                &account,
                &[AccountBalanceSnapshotDto {
                    asset_id: asset,
                    quantity: obs,
                    as_of: Utc::now(),
                }],
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&issues).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "quality" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let issues = svc.list_quality_issues()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&issues).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "backup" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let svc = AppService::open(paths.clone(), "USD", "USD", "UTC", true)?;
            let dir = parse_flag(args, "--dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.default_backup_dir.join("manual"));
            let manifest = svc.create_backup(&dir)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&manifest).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "restore" => {
            let from = parse_flag(args, "--from").ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--from <dir> required", false)
            })?;
            let to = parse_flag(args, "--to").ok_or_else(|| {
                delta_app::ErrorEnvelope::new("missing_arg", "--to <dir> required", false)
            })?;
            let manifest = AppService::restore_backup(Path::new(&from), Path::new(&to))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&manifest).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "export" => {
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let svc = AppService::open(paths.clone(), "USD", "USD", "UTC", true)?;
            let dir = parse_flag(args, "--dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.default_export_dir.clone());
            let manifest = svc.export_library(&dir)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&manifest).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "library" => {
            if args.get(1).map(|s| s.as_str()) != Some("delete") {
                return Err(delta_app::ErrorEnvelope::new(
                    "invalid_subcommand",
                    "expected 'delta library delete [--yes]'",
                    false,
                ));
            }
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let yes = has_flag(args, "--yes");
            let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            let res = AppService::delete_library(&paths.data_dir, yes, is_tty)?;
            println!("{}", serde_json::to_string_pretty(&res).unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }
        "credential" => {
            let action = args.get(1).map(|s| s.as_str()).unwrap_or("");
            let provider = parse_flag(args, "--provider").unwrap_or_else(|| "tiingo".to_string());
            let user = parse_flag(args, "--user").unwrap_or_else(|| "default".to_string());
            // D-32: credentials are DPAPI-encrypted files in the library's data directory.
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| delta_app::ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let store = if let Some(pfx) = parse_flag(args, "--prefix") {
                if !pfx.starts_with("delta-test-") {
                    return Err(delta_app::ErrorEnvelope::new(
                        "credential_namespace",
                        "custom credential prefixes are restricted to delta-test-*",
                        false,
                    ));
                }
                CredentialStore::new(&paths.data_dir, pfx)
            } else {
                CredentialStore::for_paths(&paths)
            };
            match action {
                "set" => {
                    if has_flag(args, "--secret") || !has_flag(args, "--stdin") {
                        return Err(delta_app::ErrorEnvelope::new(
                            "credential_input",
                            "use --stdin to supply the secret; command-line secrets are rejected",
                            false,
                        ));
                    }
                    let mut secret = String::new();
                    std::io::stdin()
                        .take(16385)
                        .read_to_string(&mut secret)
                        .map_err(|_| {
                            delta_app::ErrorEnvelope::new(
                                "credential_input",
                                "cannot read secret from stdin",
                                false,
                            )
                        })?;
                    if secret.trim().is_empty() || secret.len() > 16384 {
                        return Err(delta_app::ErrorEnvelope::new(
                            "credential_input",
                            "empty or oversized secret",
                            false,
                        ));
                    }
                    let secret = secret.trim_end_matches(['\r', '\n']);
                    let r = store.set_secret(&provider, &user, secret).map_err(|e| {
                        delta_app::ErrorEnvelope::new("credential_error", e.to_string(), false)
                    })?;
                    println!("{}", serde_json::json!({"status":"set","reference":r}));
                    Ok(ExitCode::SUCCESS)
                }
                "get" => {
                    let _ = store.get_secret(&provider, &user).map_err(|e| {
                        delta_app::ErrorEnvelope::new("credential_error", e.to_string(), false)
                    })?;
                    println!(
                        "{}",
                        serde_json::json!({"status":"found","reference":store.opaque_reference(&provider)})
                    );
                    Ok(ExitCode::SUCCESS)
                }
                "delete" => {
                    store.delete_secret(&provider, &user).map_err(|e| {
                        delta_app::ErrorEnvelope::new("credential_error", e.to_string(), false)
                    })?;
                    println!(r#"{{"status":"deleted"}}"#);
                    Ok(ExitCode::SUCCESS)
                }
                _ => Err(delta_app::ErrorEnvelope::new(
                    "invalid_subcommand",
                    "expected 'credential set|get|delete'",
                    false,
                )),
            }
        }
        "package-install" => {
            let root = parse_flag(args, "--package-dir")
                .map(PathBuf::from)
                .map(Ok)
                .unwrap_or_else(env::current_dir)
                .map_err(|e| delta_app::ErrorEnvelope::new("cwd", e.to_string(), false))?;
            let local_app_data = parse_flag(args, "--local-app-data")
                .map(PathBuf::from)
                .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
                .ok_or_else(|| {
                    delta_app::ErrorEnvelope::new(
                        "missing_localappdata",
                        "LOCALAPPDATA is not set",
                        false,
                    )
                })?;
            let ch = parse_flag(args, "--channel").unwrap_or_else(|| "dev".to_string());
            let out_dir = parse_flag(args, "--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".local/reports/m2/packages"));
            let receipt = install_per_user_channel(&root, &local_app_data, &ch, &out_dir)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&receipt).unwrap_or_default()
            );
            Ok(ExitCode::SUCCESS)
        }
        "package-uninstall" => {
            let local_app_data = parse_flag(args, "--local-app-data")
                .map(PathBuf::from)
                .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
                .ok_or_else(|| {
                    delta_app::ErrorEnvelope::new(
                        "missing_localappdata",
                        "LOCALAPPDATA is not set",
                        false,
                    )
                })?;
            let ch = parse_flag(args, "--channel").unwrap_or_else(|| "dev".to_string());
            uninstall_per_user_channel(&local_app_data, &ch)?;
            println!(
                "{}",
                serde_json::json!({"status":"uninstalled","channel":ch,"data_preserved":true})
            );
            Ok(ExitCode::SUCCESS)
        }
        "training" => {
            training_cmd::run_training_command(args, channel_override, cli_data_dir.as_deref())
        }
        "verify-stage" => {
            let stage = parse_flag(args, "--stage").unwrap_or_default();
            let plan_ver = parse_flag(args, "--plan-version").unwrap_or_default();
            let live_mode = has_flag(args, "--live");
            // D-30: stage labels are uppercase in documents; the CLI accepts either case.
            if stage.eq_ignore_ascii_case("M1") && plan_ver == "2.6" {
                run_verify_stage_m1(live_mode)
            } else if stage.eq_ignore_ascii_case("M2") && plan_ver == "1.0" {
                let ui_smoke = has_flag(args, "--ui-smoke");
                let ui_bench = has_flag(args, "--ui-bench");
                run_verify_stage_m2(live_mode, ui_smoke, ui_bench)
            } else if stage.eq_ignore_ascii_case("M3") && plan_ver == "1.0" {
                run_verify_stage_m3(has_flag(args, "--installer"), has_flag(args, "--ui-smoke"))
            } else if stage.eq_ignore_ascii_case("M4") && plan_ver == "1.0" {
                run_verify_stage_m4(has_flag(args, "--ui-smoke"), has_flag(args, "--ui-bench"))
            } else {
                Err(delta_app::ErrorEnvelope::new(
                    "invalid_verify_args",
                    format!(
                        "expected '--stage M1 --plan-version 2.6', '--stage M2 --plan-version 1.0', '--stage M3 --plan-version 1.0' or '--stage M4 --plan-version 1.0', got stage='{stage}' plan_version='{plan_ver}'"
                    ),
                    false,
                ))
            }
        }
        other => Err(delta_app::ErrorEnvelope::new(
            "unknown_subcommand",
            format!("unknown subcommand '{other}'"),
            false,
        )),
    }
}

fn print_json(value: &impl Serialize) {
    println!(
        "{}",
        sanitize_terminal_output(&serde_json::to_string_pretty(value).unwrap_or_default())
    );
}

fn require_identity(text: &str) -> Result<delta_app::InstallIdentity, delta_app::ErrorEnvelope> {
    delta_app::InstallIdentity::parse(text).ok_or_else(|| {
        delta_app::ErrorEnvelope::new(
            "invalid_identity",
            "expected delta, delta-dev, delta-test or delta-dev-test",
            false,
        )
    })
}

/// M3 installer, update and lifecycle commands (`S-25`..`S-34`).
fn installer_command(sub: &str, args: &[String]) -> Result<ExitCode, delta_app::ErrorEnvelope> {
    use delta_app::InstallIdentity;
    let root = || {
        env::current_dir().map_err(|e| delta_app::ErrorEnvelope::new("cwd", e.to_string(), false))
    };
    match sub {
        "package-build" => {
            let root = root()?;
            let identity = require_identity(
                &parse_flag(args, "--identity").unwrap_or_else(|| "delta-dev".into()),
            )?;
            if identity == InstallIdentity::Release {
                return Err(delta_app::ErrorEnvelope::new(
                    "PACKAGE_IDENTITY",
                    "release installers are produced only by release-build (signed, S-32)",
                    false,
                ));
            }
            let version = match parse_flag(args, "--version") {
                Some(version) => version,
                None => installer::default_dev_version(&root, Utc::now())?,
            };
            // Installers go to `dist/` (only the newest per identity is kept); build logs,
            // metadata and scratch files stay under `.local/reports/m3/packages`.
            let dist = parse_flag(args, "--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(installer::DIST_DIR));
            let context = installer::PackagingContext::prepare(
                &root,
                &root.join(".local/reports/m3/packages"),
            )?;
            let artifact = context.build(identity, &version, &dist)?;
            let pruned = installer::prune_older_packages(&dist, identity, &artifact.setup_path)?;
            print_json(&serde_json::json!({
                "installer": artifact.setup_path,
                "artifact": artifact,
                "removed_older_installers": pruned,
            }));
            Ok(ExitCode::SUCCESS)
        }
        "install-dev" => {
            let expected = if has_flag(args, "--test-identity") {
                InstallIdentity::DevTest
            } else {
                InstallIdentity::Dev
            };
            let root = root()?;
            let package = match parse_flag(args, "--package") {
                Some(package) => PathBuf::from(package),
                None => installer::latest_package(&root.join(installer::DIST_DIR), expected)?,
            };
            // Keep `dist/` limited to installers: logs go to the workspace report directory.
            let log_dir = if root.join("crates/delta-cli/installer/delta.iss").is_file() {
                root.join(".local/reports/m3/install-dev")
            } else {
                package
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(env::temp_dir)
            };
            let receipt = installer::install_dev(&package, expected, &log_dir)?;
            print_json(&receipt);
            Ok(ExitCode::SUCCESS)
        }
        "release-build" => {
            let root = root()?;
            // `--test-identity` is the gate/H-04 rehearsal: a test-channel release signed with
            // a separate throw-away key file (never the release key, D-32).
            let identity = if has_flag(args, "--test-identity") {
                InstallIdentity::Test
            } else {
                InstallIdentity::Release
            };
            let key_file = match (parse_flag(args, "--key-file"), identity) {
                (Some(path), _) => PathBuf::from(path),
                (None, InstallIdentity::Release) => installer::default_release_key_file()?,
                (None, _) => {
                    return Err(delta_app::ErrorEnvelope::new(
                        "missing_arg",
                        "--test-identity requires --key-file <throw-away key>",
                        false,
                    ))
                }
            };
            let version = match (parse_flag(args, "--version"), identity) {
                (Some(version), _) => version,
                (None, InstallIdentity::Test) => installer::default_dev_version(&root, Utc::now())?,
                (None, _) => {
                    return Err(delta_app::ErrorEnvelope::new(
                        "missing_arg",
                        "--version 0.1.N required",
                        false,
                    ))
                }
            };
            let output = parse_flag(args, "--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".local/reports/m3/release"));
            let artifacts =
                installer::release_build(&root, &output, identity, &version, &key_file)?;
            print_json(&artifacts);
            Ok(ExitCode::SUCCESS)
        }
        "release-keygen" => {
            let key_file = match parse_flag(args, "--key-file") {
                Some(path) => PathBuf::from(path),
                None => installer::default_release_key_file()?,
            };
            print_json(&installer::release_keygen(&key_file, &root()?)?);
            Ok(ExitCode::SUCCESS)
        }
        "control" => {
            let command = args
                .get(1)
                .and_then(|c| delta_app::ControlCommand::parse(c))
                .ok_or_else(|| {
                    delta_app::ErrorEnvelope::new(
                        "invalid_subcommand",
                        "expected 'control focus|quit-for-update'",
                        false,
                    )
                })?;
            let own = InstallIdentity::current();
            let identity = match parse_flag(args, "--identity") {
                Some(text) => require_identity(&text)?,
                None => own.ok_or_else(|| {
                    delta_app::ErrorEnvelope::new("missing_arg", "--identity required", false)
                })?,
            };
            // Installed CLIs only control their own identity; source builds only test identities.
            if own.map_or(!identity.is_test(), |own| own != identity) {
                return Err(delta_app::ErrorEnvelope::new(
                    "CONTROL_IDENTITY",
                    format!("this CLI may not control {}", identity.name()),
                    false,
                ));
            }
            let timeout = parse_flag(args, "--timeout-secs")
                .and_then(|t| t.parse().ok())
                .unwrap_or(30u64)
                .clamp(1, 120);
            let (code, detail) =
                installer::control(identity, command, Duration::from_secs(timeout));
            print_json(&detail);
            Ok(ExitCode::from(code))
        }
        "update-test-source" => {
            let directory = parse_flag(args, "--release-dir")
                .map(PathBuf::from)
                .ok_or_else(|| {
                    delta_app::ErrorEnvelope::new("missing_arg", "--release-dir required", false)
                })?;
            let seconds = parse_flag(args, "--seconds")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1800u64)
                .clamp(10, 4 * 3600);
            installer::serve_test_release(&directory, Duration::from_secs(seconds))?;
            Ok(ExitCode::SUCCESS)
        }
        "__migrate-portable" => {
            let identity = require_identity(&parse_flag(args, "--identity").unwrap_or_default())?;
            let own = InstallIdentity::current();
            if own.is_some_and(|own| own != identity) || (own.is_none() && !identity.is_test()) {
                return Err(delta_app::ErrorEnvelope::new(
                    "MIGRATE_IDENTITY",
                    "the migration helper only handles its own identity",
                    false,
                ));
            }
            let directory = parse_flag(args, "--install-dir")
                .map(PathBuf::from)
                .ok_or_else(|| {
                    delta_app::ErrorEnvelope::new("missing_arg", "--install-dir required", false)
                })?;
            let report = installer::migrate_portable(&directory, identity)?;
            // Printed to stdout so Inno's ExecAndLogOutput copies it into the setup log.
            print_json(&report);
            Ok(ExitCode::SUCCESS)
        }
        _ => unreachable!("dispatch only routes installer commands here"),
    }
}
