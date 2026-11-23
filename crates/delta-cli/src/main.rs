mod cli_args;
mod installer;
mod packaging;
mod training_cmd;
mod verify;
mod verify_common;
mod verify_m2;
mod verify_m3;
mod verify_m4;
mod verify_m5;

use chrono::{NaiveDate, Utc};
use cli_args::{
    ensure_no_extra_args, extract_global_flags, print_error, print_json, FlagSpec, ParsedFlags,
};
use delta_app::{
    apply_process_quiet_guards, resolve_app_paths, sanitize_terminal_output, AppService,
    BuildChannel, CredentialStore, ErrorEnvelope, InstanceLock, LiveProbeResult, QuietJobObject,
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
use verify_m5::run_verify_stage_m5;

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
            print_error(&err);
            ExitCode::from(1)
        }
    }
}

fn print_usage() {
    println!(
        "delta-cli (M5 v1.0) - financial data, ledger, training, installer and update operations\n\
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
         - training create|list|show|order|cancel|advance|note|finish|reveal|report|replay|retry|delete|redraw|start|pause|resume|fee-profiles|self-eval|evidence|prepare-data|account|history|modify|undo|abandon|restart|view|drawing [--data-dir <dir>]\n\
         - credential set|get|delete --provider <p> --user <u> --stdin [--prefix <pfx>]\n\
         - package-build [--identity delta-dev|delta-test|delta-dev-test] [--version <v>] [--out-dir <dir>] (installer in dist/; older ones of that identity removed)\n\
         - install-dev [--package <setup.exe>] (default: newest delta-dev installer in dist/; always starts the app again with --background)\n\
         - release-build --version 0.1.N [--key-file <file>] [--out-dir <dir>] | release-build --test-identity --key-file <throw-away key> (test rehearsal)\n\
         - update-test-source --release-dir <test release dir> [--seconds <n>] (loopback source for H-04)\n\
         - release-keygen [--key-file <file>] (default %APPDATA%/delta-release/update-signing.key; back up that file)\n\
         - control focus|quit-for-update [--identity <id>] [--timeout-secs <n>]\n\
         - verify-stage --stage M1 --plan-version 2.6 [--live]\n\
         - verify-stage --stage M2 --plan-version 1.0 [--live] [--ui-smoke] [--ui-bench]\n\
         - verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]\n\
         - verify-stage --stage M4 --plan-version 1.0 [--ui-smoke] [--ui-bench]\n\
         - verify-stage --stage M5 --plan-version 1.0 [--ui-smoke] [--ui-bench] [--live-data]\n\
         Test and migration diagnostics only: package-portable-build, package-install, package-uninstall"
    );
}

fn dispatch_cli(args: &[String]) -> Result<ExitCode, ErrorEnvelope> {
    let sub = args[0].as_str();
    let tail = &args[1..];

    if sub == "training" {
        return training_cmd::run_training_command(args, None, None);
    }

    let allow_raw_channel = matches!(sub, "package-install" | "package-uninstall");
    let (globals, sub_args) = extract_global_flags(tail, allow_raw_channel)?;
    let cli_data_dir = globals.data_dir;
    let channel_override = globals.channel_override;

    if BuildChannel::current() != BuildChannel::Release
        && channel_override == Some(BuildChannel::Release)
        && !allow_raw_channel
    {
        return Err(ErrorEnvelope::new(
            "channel_isolation",
            "source builds cannot select the release channel",
            false,
        ));
    }

    let open_service = || -> Result<AppService, ErrorEnvelope> {
        let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
            .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
        AppService::open(paths, "USD", "USD", "UTC", true)
    };

    match sub {
        "--build-info" => {
            ensure_no_extra_args("--build-info", &sub_args)?;
            println!("{}", delta_app::build_info("delta-cli"));
            Ok(ExitCode::SUCCESS)
        }
        "--version" | "-V" => {
            ensure_no_extra_args("--version", &sub_args)?;
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
        | "update-test-source" | "__migrate-portable" => installer_command(sub, &sub_args),
        "package-portable-build" => {
            let flags = ParsedFlags::parse(
                "package-portable-build",
                &sub_args,
                FlagSpec {
                    value_flags: &["--out-dir"],
                    ..Default::default()
                },
            )?;
            let root =
                env::current_dir().map_err(|e| ErrorEnvelope::new("cwd", e.to_string(), false))?;
            let output = flags
                .one("--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".local/reports/m2/packages"));
            let artifacts = packaging::build_packages(&root, &output)?;
            print_json(&artifacts);
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
            let flags = ParsedFlags::parse(
                "__internal-hold-lock",
                &sub_args,
                FlagSpec {
                    value_flags: &["--ms"],
                    ..Default::default()
                },
            )?;
            let dir = cli_data_dir
                .ok_or_else(|| ErrorEnvelope::new("missing_arg", "--data-dir required", false))?;
            let ms: u64 = flags.opt_parsed("--ms", "invalid_integer")?.unwrap_or(1000);
            let _lock = InstanceLock::acquire(&dir)
                .map_err(|e| ErrorEnvelope::new("instance_locked", e.to_string(), true))?;
            println!("LOCKED");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::thread::sleep(Duration::from_millis(ms));
            Ok(ExitCode::SUCCESS)
        }
        "__internal-hold-lock-tree" => {
            ensure_no_extra_args("__internal-hold-lock-tree", &sub_args)?;
            let directory = cli_data_dir
                .ok_or_else(|| ErrorEnvelope::new("missing_arg", "--data-dir required", false))?;
            let _lock = InstanceLock::acquire(&directory)
                .map_err(|e| ErrorEnvelope::new("instance_locked", e.to_string(), false))?;
            let mut descendant = delta_app::spawn_quiet_child(
                env::current_exe()
                    .map_err(|e| ErrorEnvelope::new("exe", e.to_string(), false))?
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
            .map_err(|e| ErrorEnvelope::new("spawn", e, false))?;
            std::thread::sleep(Duration::from_secs(10));
            descendant
                .wait()
                .map_err(|e| ErrorEnvelope::new("wait", e.to_string(), false))?;
            Ok(ExitCode::SUCCESS)
        }
        "__internal-credential-read" => {
            let flags = ParsedFlags::parse(
                "__internal-credential-read",
                &sub_args,
                FlagSpec {
                    value_flags: &["--prefix", "--provider", "--user"],
                    ..Default::default()
                },
            )?;
            let prefix = flags
                .one("--prefix")
                .filter(|s| s.starts_with("delta-test-"))
                .map(str::to_string)
                .ok_or_else(|| {
                    ErrorEnvelope::new(
                        "test_namespace_required",
                        "internal credential probe requires a delta-test- namespace",
                        false,
                    )
                })?;
            let provider = flags.one("--provider").unwrap_or("tiingo");
            let user = flags.one("--user").unwrap_or("default");
            let dir = cli_data_dir
                .as_deref()
                .ok_or_else(|| ErrorEnvelope::new("missing_arg", "--data-dir required", false))?;
            let store = CredentialStore::new(dir, prefix);
            let val = store
                .get_secret(provider, user)
                .map_err(|e| ErrorEnvelope::new("credential_error", e.to_string(), false))?;
            println!("{}", sanitize_terminal_output(&val));
            Ok(ExitCode::SUCCESS)
        }
        "init" => {
            ensure_no_extra_args("init", &sub_args)?;
            let svc = open_service()?;
            print_json(svc.paths());
            Ok(ExitCode::SUCCESS)
        }
        "register" => {
            let flags = ParsedFlags::parse(
                "register",
                &sub_args,
                FlagSpec {
                    value_flags: &["--venue", "--symbol"],
                    switches: &["--etf"],
                    ..Default::default()
                },
            )?;
            let venue = flags.required("register", "--venue")?;
            let symbol = flags.required("register", "--symbol")?;
            let mut svc = open_service()?;
            let id = svc.register_market_instrument(&venue, &symbol, flags.has("--etf"))?;
            print_json(&serde_json::json!({"instrument_id": id}));
            Ok(ExitCode::SUCCESS)
        }
        "import" => {
            let flags = ParsedFlags::parse(
                "import",
                &sub_args,
                FlagSpec {
                    value_flags: &["--account", "--preset", "--file"],
                    switches: &["--commit"],
                    ..Default::default()
                },
            )?;
            let account = flags.one("--account").unwrap_or("ACC-DEFAULT");
            let preset_str = flags.one("--preset").unwrap_or("generic");
            let preset = StatementPreset::parse(preset_str).ok_or_else(|| {
                ErrorEnvelope::new(
                    "invalid_preset",
                    format!("unknown statement preset '{preset_str}'"),
                    false,
                )
            })?;
            let file_path = flags.required("import", "--file")?;
            let mut svc = open_service()?;
            let (preview, receipt) = svc.import_statement(
                account,
                preset,
                Path::new(&file_path),
                flags.has("--commit"),
            )?;
            print_json(&serde_json::json!({
                "preview": preview,
                "receipt": receipt,
            }));
            Ok(ExitCode::SUCCESS)
        }
        "resolve-import" => {
            let flags = ParsedFlags::parse(
                "resolve-import",
                &sub_args,
                FlagSpec {
                    value_flags: &["--row-id"],
                    switches: &["--include", "--exclude"],
                    ..Default::default()
                },
            )?;
            let row = flags.required("resolve-import", "--row-id")?;
            if flags.has("--include") == flags.has("--exclude") {
                return Err(ErrorEnvelope::new(
                    "invalid_arg",
                    "choose --include or --exclude",
                    false,
                ));
            }
            let mut svc = open_service()?;
            let revision = svc.resolve_import_row(&row, flags.has("--include"))?;
            print_json(&serde_json::json!({"ledger_revision": revision}));
            Ok(ExitCode::SUCCESS)
        }
        "sync" => {
            let flags = ParsedFlags::parse(
                "sync",
                &sub_args,
                FlagSpec {
                    value_flags: &["--fixtures", "--until", "--from", "--symbol"],
                    ..Default::default()
                },
            )?;
            let fixtures = flags.one("--fixtures").map(PathBuf::from);
            let until_opt = flags.opt_date("--until")?;
            let from_opt = flags.opt_date("--from")?;
            let raw_symbols = flags.one("--symbol").unwrap_or("").to_string();
            let mut svc = open_service()?;
            let summary = if let Some(fixtures_dir) = fixtures {
                svc.sync_from_fixtures(&fixtures_dir)?
            } else {
                let end = until_opt.unwrap_or_else(|| Utc::now().date_naive());
                let events = svc.load_all_ledger_events()?;
                let start_default = events
                    .iter()
                    .map(|e| e.occurred_at.date_naive())
                    .min()
                    .unwrap_or(end - chrono::Duration::days(30));
                let start = from_opt.unwrap_or(start_default);
                let mut instruments = Vec::new();
                for symbol in raw_symbols.split(',').filter(|s| !s.is_empty()) {
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
            print_json(&summary);
            Ok(ExitCode::SUCCESS)
        }
        "explain" => {
            let flags = ParsedFlags::parse(
                "explain",
                &sub_args,
                FlagSpec {
                    value_flags: &["--as-of", "--version"],
                    ..Default::default()
                },
            )?;
            let as_of: NaiveDate = flags
                .opt_date("--as-of")?
                .unwrap_or_else(|| Utc::now().date_naive());
            let version_opt: Option<i64> = flags.opt_parsed("--version", "invalid_integer")?;
            let mut svc = open_service()?;
            let version = version_opt.unwrap_or_else(|| svc.latest_dataset_version());
            let summary = svc.evaluate_and_explain(as_of, version, None)?;
            print_json(&summary);
            Ok(ExitCode::SUCCESS)
        }
        "reconcile" => {
            let flags = ParsedFlags::parse(
                "reconcile",
                &sub_args,
                FlagSpec {
                    value_flags: &["--account", "--asset", "--observed"],
                    ..Default::default()
                },
            )?;
            let account = flags.one("--account").unwrap_or("ACC-DEFAULT");
            let asset = flags.one("--asset").unwrap_or("USD").to_string();
            let obs_str = flags.one("--observed").unwrap_or("0");
            let obs = parse_decimal_lossless(obs_str)
                .map_err(|e| ErrorEnvelope::new("invalid_decimal", e.to_string(), false))?;
            let mut svc = open_service()?;
            let issues = svc.reconcile_account(
                account,
                &[AccountBalanceSnapshotDto {
                    asset_id: asset,
                    quantity: obs,
                    as_of: Utc::now(),
                }],
            )?;
            print_json(&issues);
            Ok(ExitCode::SUCCESS)
        }
        "quality" => {
            ensure_no_extra_args("quality", &sub_args)?;
            let svc = open_service()?;
            let issues = svc.list_quality_issues()?;
            print_json(&issues);
            Ok(ExitCode::SUCCESS)
        }
        "backup" => {
            let flags = ParsedFlags::parse(
                "backup",
                &sub_args,
                FlagSpec {
                    value_flags: &["--dir"],
                    ..Default::default()
                },
            )?;
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let dir = flags
                .one("--dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.default_backup_dir.join("manual"));
            let svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let manifest = svc.create_backup(&dir)?;
            print_json(&manifest);
            Ok(ExitCode::SUCCESS)
        }
        "restore" => {
            let flags = ParsedFlags::parse(
                "restore",
                &sub_args,
                FlagSpec {
                    value_flags: &["--from", "--to"],
                    ..Default::default()
                },
            )?;
            let from = flags.required("restore", "--from")?;
            let to = flags.required("restore", "--to")?;
            let manifest = AppService::restore_backup(Path::new(&from), Path::new(&to))?;
            print_json(&manifest);
            Ok(ExitCode::SUCCESS)
        }
        "export" => {
            let flags = ParsedFlags::parse(
                "export",
                &sub_args,
                FlagSpec {
                    value_flags: &["--dir"],
                    ..Default::default()
                },
            )?;
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let dir = flags
                .one("--dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.default_export_dir.clone());
            let svc = AppService::open(paths, "USD", "USD", "UTC", true)?;
            let manifest = svc.export_library(&dir)?;
            print_json(&manifest);
            Ok(ExitCode::SUCCESS)
        }
        "library" => {
            if sub_args.first().map(String::as_str) != Some("delete") {
                return Err(ErrorEnvelope::new(
                    "invalid_subcommand",
                    "expected 'delta library delete [--yes]'",
                    false,
                ));
            }
            let flags = ParsedFlags::parse(
                "library delete",
                &sub_args[1..],
                FlagSpec {
                    switches: &["--yes"],
                    ..Default::default()
                },
            )?;
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let yes = flags.has("--yes");
            let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            let res = AppService::delete_library(&paths.data_dir, yes, is_tty)?;
            print_json(&res);
            Ok(ExitCode::SUCCESS)
        }
        "credential" => {
            let action = sub_args.first().map(String::as_str).unwrap_or("");
            if !matches!(action, "set" | "get" | "delete") {
                return Err(ErrorEnvelope::new(
                    "invalid_subcommand",
                    "expected 'credential set|get|delete'",
                    false,
                ));
            }
            let flags = ParsedFlags::parse(
                &format!("credential {action}"),
                &sub_args[1..],
                FlagSpec {
                    value_flags: &["--provider", "--user", "--prefix"],
                    switches: if action == "set" { &["--stdin"] } else { &[] },
                    optional_value_flags: if action == "set" { &["--secret"] } else { &[] },
                    aliases: &[],
                },
            )?;
            let provider = flags.one("--provider").unwrap_or("tiingo");
            let user = flags.one("--user").unwrap_or("default");
            // D-32: credentials are DPAPI-encrypted files in the library's data directory.
            let paths = resolve_app_paths(channel_override, cli_data_dir.as_deref())
                .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
            let store = if let Some(pfx) = flags.one("--prefix") {
                if !pfx.starts_with("delta-test-") {
                    return Err(ErrorEnvelope::new(
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
                    if flags.has("--secret") || !flags.has("--stdin") {
                        return Err(ErrorEnvelope::new(
                            "credential_input",
                            "use --stdin to supply the secret; command-line secrets are rejected",
                            false,
                        ));
                    }
                    let mut raw_secret = String::new();
                    std::io::stdin()
                        .take(16384 + 2 + 1)
                        .read_to_string(&mut raw_secret)
                        .map_err(|_| {
                            ErrorEnvelope::new(
                                "credential_input",
                                "cannot read secret from stdin",
                                false,
                            )
                        })?;
                    let secret = raw_secret.trim_end_matches(['\r', '\n']);
                    if secret.trim().is_empty() || secret.len() > 16384 {
                        return Err(ErrorEnvelope::new(
                            "credential_input",
                            "empty or oversized secret",
                            false,
                        ));
                    }
                    let r = store.set_secret(provider, user, secret).map_err(|e| {
                        ErrorEnvelope::new("credential_error", e.to_string(), false)
                    })?;
                    print_json(&serde_json::json!({"status": "set", "reference": r}));
                    Ok(ExitCode::SUCCESS)
                }
                "get" => {
                    let _ = store.get_secret(provider, user).map_err(|e| {
                        ErrorEnvelope::new("credential_error", e.to_string(), false)
                    })?;
                    print_json(&serde_json::json!({
                        "status": "found",
                        "reference": store.opaque_reference(provider),
                    }));
                    Ok(ExitCode::SUCCESS)
                }
                "delete" => {
                    store.delete_secret(provider, user).map_err(|e| {
                        ErrorEnvelope::new("credential_error", e.to_string(), false)
                    })?;
                    print_json(&serde_json::json!({"status": "deleted"}));
                    Ok(ExitCode::SUCCESS)
                }
                _ => unreachable!("action validated above"),
            }
        }
        "package-install" => {
            let flags = ParsedFlags::parse(
                "package-install",
                &sub_args,
                FlagSpec {
                    value_flags: &["--package-dir", "--local-app-data", "--out-dir"],
                    ..Default::default()
                },
            )?;
            let root = flags
                .one("--package-dir")
                .map(PathBuf::from)
                .map(Ok)
                .unwrap_or_else(env::current_dir)
                .map_err(|e| ErrorEnvelope::new("cwd", e.to_string(), false))?;
            let local_app_data = flags
                .one("--local-app-data")
                .map(PathBuf::from)
                .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
                .ok_or_else(|| {
                    ErrorEnvelope::new("missing_localappdata", "LOCALAPPDATA is not set", false)
                })?;
            let ch = globals.channel_raw.as_deref().unwrap_or("dev");
            let out_dir = flags
                .one("--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".local/reports/m2/packages"));
            let receipt = install_per_user_channel(&root, &local_app_data, ch, &out_dir)?;
            print_json(&receipt);
            Ok(ExitCode::SUCCESS)
        }
        "package-uninstall" => {
            let flags = ParsedFlags::parse(
                "package-uninstall",
                &sub_args,
                FlagSpec {
                    value_flags: &["--local-app-data"],
                    ..Default::default()
                },
            )?;
            let local_app_data = flags
                .one("--local-app-data")
                .map(PathBuf::from)
                .or_else(|| env::var_os("LOCALAPPDATA").map(PathBuf::from))
                .ok_or_else(|| {
                    ErrorEnvelope::new("missing_localappdata", "LOCALAPPDATA is not set", false)
                })?;
            let ch = globals.channel_raw.as_deref().unwrap_or("dev");
            uninstall_per_user_channel(&local_app_data, ch)?;
            print_json(&serde_json::json!({
                "status": "uninstalled",
                "channel": ch,
                "data_preserved": true,
            }));
            Ok(ExitCode::SUCCESS)
        }
        "verify-stage" => {
            let flags = ParsedFlags::parse(
                "verify-stage",
                &sub_args,
                FlagSpec {
                    value_flags: &["--stage", "--plan-version"],
                    switches: &[
                        "--live",
                        "--ui-smoke",
                        "--ui-bench",
                        "--installer",
                        "--live-data",
                    ],
                    ..Default::default()
                },
            )?;
            let stage = flags.one("--stage").unwrap_or_default();
            let plan_ver = flags.one("--plan-version").unwrap_or_default();
            let live_mode = flags.has("--live");
            // D-30: stage labels are uppercase in documents; the CLI accepts either case.
            if stage.eq_ignore_ascii_case("M1") && plan_ver == "2.6" {
                run_verify_stage_m1(live_mode)
            } else if stage.eq_ignore_ascii_case("M2") && plan_ver == "1.0" {
                run_verify_stage_m2(live_mode, flags.has("--ui-smoke"), flags.has("--ui-bench"))
            } else if stage.eq_ignore_ascii_case("M3") && plan_ver == "1.0" {
                run_verify_stage_m3(flags.has("--installer"), flags.has("--ui-smoke"))
            } else if stage.eq_ignore_ascii_case("M4") && plan_ver == "1.0" {
                run_verify_stage_m4(flags.has("--ui-smoke"), flags.has("--ui-bench"))
            } else if stage.eq_ignore_ascii_case("M5") && plan_ver == "1.0" {
                run_verify_stage_m5(
                    flags.has("--ui-smoke"),
                    flags.has("--ui-bench"),
                    flags.has("--live-data"),
                )
            } else {
                Err(ErrorEnvelope::new(
                    "invalid_verify_args",
                    format!(
                        "expected '--stage M1 --plan-version 2.6', '--stage M2 --plan-version 1.0', '--stage M3 --plan-version 1.0', '--stage M4 --plan-version 1.0' or '--stage M5 --plan-version 1.0', got stage='{stage}' plan_version='{plan_ver}'"
                    ),
                    false,
                ))
            }
        }
        other => Err(ErrorEnvelope::new(
            "unknown_subcommand",
            format!("unknown subcommand '{other}'"),
            false,
        )),
    }
}

fn require_identity(text: &str) -> Result<delta_app::InstallIdentity, ErrorEnvelope> {
    delta_app::InstallIdentity::parse(text).ok_or_else(|| {
        ErrorEnvelope::new(
            "invalid_identity",
            "expected delta, delta-dev, delta-test or delta-dev-test",
            false,
        )
    })
}

/// M3 installer, update and lifecycle commands (`S-25`..`S-34`).
fn installer_command(sub: &str, sub_args: &[String]) -> Result<ExitCode, ErrorEnvelope> {
    use delta_app::InstallIdentity;
    let root = || env::current_dir().map_err(|e| ErrorEnvelope::new("cwd", e.to_string(), false));
    match sub {
        "package-build" => {
            let flags = ParsedFlags::parse(
                "package-build",
                sub_args,
                FlagSpec {
                    value_flags: &["--identity", "--version", "--out-dir"],
                    ..Default::default()
                },
            )?;
            let root = root()?;
            let identity = require_identity(flags.one("--identity").unwrap_or("delta-dev"))?;
            if identity == InstallIdentity::Release {
                return Err(ErrorEnvelope::new(
                    "PACKAGE_IDENTITY",
                    "release installers are produced only by release-build (signed, S-32)",
                    false,
                ));
            }
            let version = match flags.one("--version") {
                Some(version) => version.to_string(),
                None => installer::default_dev_version(&root, Utc::now())?,
            };
            // Installers go to `dist/` (only the newest per identity is kept); build logs,
            // metadata and scratch files stay under `.local/reports/m3/packages`.
            let dist = flags
                .one("--out-dir")
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
            let flags = ParsedFlags::parse(
                "install-dev",
                sub_args,
                FlagSpec {
                    value_flags: &["--package"],
                    switches: &["--test-identity"],
                    ..Default::default()
                },
            )?;
            let expected = if flags.has("--test-identity") {
                InstallIdentity::DevTest
            } else {
                InstallIdentity::Dev
            };
            let root = root()?;
            let package = match flags.one("--package") {
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
            if receipt.in_job {
                eprintln!(
                    "install-dev: {} {} was started inside this command's job object and may end with the command; open it once from the Start menu",
                    receipt.identity, receipt.installed_version
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "release-build" => {
            let flags = ParsedFlags::parse(
                "release-build",
                sub_args,
                FlagSpec {
                    value_flags: &["--key-file", "--version", "--out-dir"],
                    switches: &["--test-identity"],
                    ..Default::default()
                },
            )?;
            let root = root()?;
            // `--test-identity` is the gate/H-04 rehearsal: a test-channel release signed with
            // a separate throw-away key file (never the release key, D-32).
            let identity = if flags.has("--test-identity") {
                InstallIdentity::Test
            } else {
                InstallIdentity::Release
            };
            let key_file = match (flags.one("--key-file"), identity) {
                (Some(path), _) => PathBuf::from(path),
                (None, InstallIdentity::Release) => installer::default_release_key_file()?,
                (None, _) => {
                    return Err(ErrorEnvelope::new(
                        "missing_arg",
                        "--test-identity requires --key-file <throw-away key>",
                        false,
                    ))
                }
            };
            let version = match (flags.one("--version"), identity) {
                (Some(version), _) => version.to_string(),
                (None, InstallIdentity::Test) => installer::default_dev_version(&root, Utc::now())?,
                (None, _) => {
                    return Err(ErrorEnvelope::new(
                        "missing_arg",
                        "--version 0.1.N required",
                        false,
                    ))
                }
            };
            let output = flags
                .one("--out-dir")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(".local/reports/m3/release"));
            let artifacts =
                installer::release_build(&root, &output, identity, &version, &key_file)?;
            print_json(&artifacts);
            Ok(ExitCode::SUCCESS)
        }
        "release-keygen" => {
            let flags = ParsedFlags::parse(
                "release-keygen",
                sub_args,
                FlagSpec {
                    value_flags: &["--key-file"],
                    ..Default::default()
                },
            )?;
            let key_file = match flags.one("--key-file") {
                Some(path) => PathBuf::from(path),
                None => installer::default_release_key_file()?,
            };
            print_json(&installer::release_keygen(&key_file, &root()?)?);
            Ok(ExitCode::SUCCESS)
        }
        "control" => {
            let command = sub_args
                .first()
                .and_then(|c| delta_app::ControlCommand::parse(c))
                .ok_or_else(|| {
                    ErrorEnvelope::new(
                        "invalid_subcommand",
                        "expected 'control focus|quit-for-update'",
                        false,
                    )
                })?;
            let flags = ParsedFlags::parse(
                "control",
                &sub_args[1..],
                FlagSpec {
                    value_flags: &["--identity", "--timeout-secs"],
                    ..Default::default()
                },
            )?;
            let own = InstallIdentity::current();
            let identity = match flags.one("--identity") {
                Some(text) => require_identity(text)?,
                None => own.ok_or_else(|| {
                    ErrorEnvelope::new("missing_arg", "--identity required", false)
                })?,
            };
            // Installed CLIs only control their own identity; source builds only test identities.
            if own.map_or(!identity.is_test(), |own| own != identity) {
                return Err(ErrorEnvelope::new(
                    "CONTROL_IDENTITY",
                    format!("this CLI may not control {}", identity.name()),
                    false,
                ));
            }
            let timeout = flags
                .opt_parsed::<u64>("--timeout-secs", "invalid_integer")?
                .unwrap_or(30)
                .clamp(1, 120);
            let (code, detail) =
                installer::control(identity, command, Duration::from_secs(timeout));
            print_json(&detail);
            Ok(ExitCode::from(code))
        }
        "update-test-source" => {
            let flags = ParsedFlags::parse(
                "update-test-source",
                sub_args,
                FlagSpec {
                    value_flags: &["--release-dir", "--seconds"],
                    ..Default::default()
                },
            )?;
            let directory = PathBuf::from(flags.required("update-test-source", "--release-dir")?);
            let seconds = flags
                .opt_parsed::<u64>("--seconds", "invalid_integer")?
                .unwrap_or(1800)
                .clamp(10, 4 * 3600);
            installer::serve_test_release(&directory, Duration::from_secs(seconds))?;
            Ok(ExitCode::SUCCESS)
        }
        "__migrate-portable" => {
            let flags = ParsedFlags::parse(
                "__migrate-portable",
                sub_args,
                FlagSpec {
                    value_flags: &["--identity", "--install-dir"],
                    ..Default::default()
                },
            )?;
            let identity = require_identity(flags.one("--identity").unwrap_or_default())?;
            let own = InstallIdentity::current();
            if own.is_some_and(|own| own != identity) || (own.is_none() && !identity.is_test()) {
                return Err(ErrorEnvelope::new(
                    "MIGRATE_IDENTITY",
                    "the migration helper only handles its own identity",
                    false,
                ));
            }
            let directory = PathBuf::from(flags.required("__migrate-portable", "--install-dir")?);
            let report = installer::migrate_portable(&directory, identity)?;
            // Printed to stdout so Inno's ExecAndLogOutput copies it into the setup log.
            print_json(&report);
            Ok(ExitCode::SUCCESS)
        }
        _ => unreachable!("dispatch only routes installer commands here"),
    }
}
