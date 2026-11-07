mod verify;
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
        "delta (M1 v2.6) - local-first financial data & ledger foundation\n\
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
         - credential set|get|delete --provider <p> --user <u> --stdin [--prefix <pfx>]\n\
         - verify-stage --stage M1 --plan-version 2.6 [--live]"
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
    {
        return Err(delta_app::ErrorEnvelope::new(
            "channel_isolation",
            "source builds cannot select the release channel",
            false,
        ));
    }

    match sub {
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
        "__internal-keyring-read" => {
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
            let store = CredentialStore::with_custom_prefix(prefix);
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
                let events = delta_data::load_all_ledger_events(svc.store()).map_err(|e| {
                    delta_app::ErrorEnvelope::new("load_events", e.to_string(), false)
                })?;
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
                .unwrap_or_else(|| {
                    svc.store()
                        .conn()
                        .query_row(
                            "SELECT COALESCE(MAX(version_num),0) FROM dataset_version",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap_or(0)
                });
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
            let issues = svc.store().list_quality_issues().map_err(|e| {
                delta_app::ErrorEnvelope::new("quality_query_failed", e.to_string(), false)
            })?;
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
            let store = if let Some(pfx) = parse_flag(args, "--prefix") {
                if !pfx.starts_with("delta-test-") {
                    return Err(delta_app::ErrorEnvelope::new(
                        "credential_namespace",
                        "custom credential prefixes are restricted to delta-test-*",
                        false,
                    ));
                }
                CredentialStore::with_custom_prefix(pfx)
            } else {
                CredentialStore::for_channel(channel_override.unwrap_or_else(BuildChannel::current))
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
        "verify-stage" => {
            let stage = parse_flag(args, "--stage").unwrap_or_default();
            let plan_ver = parse_flag(args, "--plan-version").unwrap_or_default();
            if stage != "M1" || plan_ver != "2.6" {
                return Err(delta_app::ErrorEnvelope::new(
                    "invalid_verify_args",
                    format!(
                        "expected '--stage M1 --plan-version 2.6', got stage='{stage}' plan_version='{plan_ver}'"
                    ),
                    false,
                ));
            }
            let live_mode = has_flag(args, "--live");
            run_verify_stage_m1(live_mode)
        }
        other => Err(delta_app::ErrorEnvelope::new(
            "unknown_subcommand",
            format!("unknown subcommand '{other}'"),
            false,
        )),
    }
}
