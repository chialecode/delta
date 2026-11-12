use chrono::NaiveDate;
use delta_app::{
    parse_indicator_config, resolve_app_paths, sanitize_terminal_output, AppService, BuildChannel,
    CreateTrainingRunRequest, ErrorEnvelope, FeeChargingTier, FeeCurrencyMode, FeeMarketCategory,
    FeeProfileDraftDto, FeeRoundingMode, KlineViewport, OrderSide, OrderSubmitKind, SimOrderType,
    SimRunStatus, SlippageSpec, TimeInForce, TrainingMarketCategory, TrainingOrderRequestDto,
    TrainingReplayMode, UpsertTrainingNoteRequest, BUILTIN_FEE_CRYPTO_RATE_ID,
    BUILTIN_FEE_US_EQUITY_PER_SHARE_ID, BUILTIN_FEE_ZERO_COST_ID,
};
use delta_core::{parse_decimal_lossless, AttachedBracket};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub fn print_training_usage() {
    let text = "\
delta-cli training - M4 K-line replay training & simulation commands (JSON output)
Usage:
  delta-cli training create [--market any|us_equity_etf|crypto_spot] [--instrument <id>]
                            [--from <YYYY-MM-DD>] [--to <YYYY-MM-DD>]
                            [--warmup-bars <n>] [--history-bars <n>] [--training-bars <n>] [--reveal-bars <n>]
                            [--seed <u64>] [--dataset-version <n>] [--initial-cash <dec>]
                            [--fee-profile <id_or_key>] [--hidden|--no-hidden]
                            [--start|--prepare-only] [--run-id <id>] [--data-dir <dir>]
  delta-cli training list [--data-dir <dir>]
  delta-cli training show --run-id <id> [--bars-limit <n>] [--visible-start <n>] [--visible-end <n>]
                          [--max-columns <n>] [--indicators <json>] [--data-dir <dir>]
  delta-cli training order [market|limit|stop-market|oco] --run-id <id> [--type <kind>] [--preview]
                           [--side buy|sell] [--tif day|gtc] --qty <dec>
                           [--limit-price <dec>] [--stop-price <dec>]
                           [--take-profit <dec>] [--stop-loss <dec>] [--bracket-tif day|gtc] [--data-dir <dir>]
  delta-cli training cancel --run-id <id> --order-id <id> [--data-dir <dir>]
  delta-cli training advance --run-id <id> [--steps <n>] [--data-dir <dir>]
  delta-cli training note --run-id <id> (--rationale <text> [--emotion <tag>] [--linked-order-id <id>] [--note-id <id>]
                          | --archive [<note-id,...>] | --list) [--data-dir <dir>]
  delta-cli training finish --run-id <id> [--reveal] [--data-dir <dir>]
  delta-cli training reveal --run-id <id> [--data-dir <dir>]
  delta-cli training report --run-id <id> [--data-dir <dir>]
  delta-cli training replay --run-id <id> [--mode scratch|checkpoint] [--data-dir <dir>]
  delta-cli training retry --run-id <id> [--new-run-id <id>] [--data-dir <dir>]
  delta-cli training delete --run-id <id> (--preview | --yes) [--data-dir <dir>]
  delta-cli training redraw|start|pause|resume --run-id <id> [--data-dir <dir>]
  delta-cli training fee-profiles [list | get --id <id> | save --json <draft> | delete --id <id> --yes] [--data-dir <dir>]
  delta-cli training self-eval --run-id <id> [--decision-score <1..5> [--discipline-score <1..5>] [--summary <text>]] [--data-dir <dir>]
  delta-cli training evidence --run-id <id> --evidence-id <id> [--data-dir <dir>]";
    println!("{}", sanitize_terminal_output(text));
}

/// Serialize `value` as pretty JSON while ensuring every ASCII/Unicode control character
/// (`\x07` BEL, `\x1b` ESC, `\x7f` DEL, C1 controls) is safely escaped so output is both
/// terminal-safe (`D-25` / `S-10`) and lossless RFC 8259 JSON.
pub fn to_sanitized_pretty_json(value: &impl Serialize) -> Result<String, ErrorEnvelope> {
    let raw = serde_json::to_string_pretty(value)
        .map_err(|e| ErrorEnvelope::new("JSON_SERIALIZE_FAILED", e.to_string(), false))?;
    let mut json_safe = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\n' | '\r' | '\t' => json_safe.push(ch),
            c if c.is_control() => {
                let code = c as u32;
                if code <= 0xFFFF {
                    json_safe.push_str(&format!("\\u{code:04x}"));
                } else {
                    json_safe.push_str(&format!("\\x{code:02x}"));
                }
            }
            c => json_safe.push(c),
        }
    }
    Ok(sanitize_terminal_output(&json_safe))
}

pub fn run_training_command(
    args: &[String],
    channel_override: Option<BuildChannel>,
    cli_data_dir: Option<&Path>,
) -> Result<ExitCode, ErrorEnvelope> {
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match execute_training_cli(args, channel_override, cli_data_dir, is_tty)? {
        TrainingCliOutcome::HelpPrinted => Ok(ExitCode::SUCCESS),
        TrainingCliOutcome::Json(text) => {
            println!("{}", sanitize_terminal_output(&text));
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrainingCliOutcome {
    HelpPrinted,
    Json(String),
}

pub fn execute_training_cli(
    args: &[String],
    mut channel_override: Option<BuildChannel>,
    cli_data_dir: Option<&Path>,
    is_interactive_tty: bool,
) -> Result<TrainingCliOutcome, ErrorEnvelope> {
    let slice = if args.first().map(String::as_str) == Some("training") {
        &args[1..]
    } else {
        args
    };

    let mut owned_data_dir: Option<PathBuf> = cli_data_dir.map(Path::to_path_buf);
    let mut cleaned = Vec::with_capacity(slice.len());
    let mut idx = 0;
    while idx < slice.len() {
        let tok = slice[idx].as_str();
        if tok == "--data-dir" {
            let val = slice.get(idx + 1).ok_or_else(|| {
                ErrorEnvelope::new("missing_arg", "--data-dir requires a path value", false)
            })?;
            owned_data_dir = Some(PathBuf::from(val));
            idx += 2;
            continue;
        }
        if let Some(val) = tok.strip_prefix("--data-dir=") {
            if val.trim().is_empty() {
                return Err(ErrorEnvelope::new(
                    "missing_arg",
                    "--data-dir requires a non-empty path value",
                    false,
                ));
            }
            owned_data_dir = Some(PathBuf::from(val));
            idx += 1;
            continue;
        }
        if tok == "--channel" {
            let val = slice.get(idx + 1).ok_or_else(|| {
                ErrorEnvelope::new("missing_arg", "--channel requires a channel name", false)
            })?;
            channel_override = Some(BuildChannel::parse(val).ok_or_else(|| {
                ErrorEnvelope::new(
                    "invalid_channel",
                    format!("unsupported channel '{val}'"),
                    false,
                )
            })?);
            idx += 2;
            continue;
        }
        if let Some(val) = tok.strip_prefix("--channel=") {
            channel_override = Some(BuildChannel::parse(val).ok_or_else(|| {
                ErrorEnvelope::new(
                    "invalid_channel",
                    format!("unsupported channel '{val}'"),
                    false,
                )
            })?);
            idx += 1;
            continue;
        }
        cleaned.push(slice[idx].clone());
        idx += 1;
    }

    if BuildChannel::current() != BuildChannel::Release
        && channel_override == Some(BuildChannel::Release)
    {
        return Err(ErrorEnvelope::new(
            "channel_isolation",
            "source builds cannot select the release channel",
            false,
        ));
    }

    let Some(action) = cleaned.first().map(String::as_str) else {
        return Err(ErrorEnvelope::new(
            "invalid_subcommand",
            "expected 'delta-cli training <create|list|show|order|cancel|advance|note|finish|reveal|report|replay|retry|delete|redraw|start|pause|resume|fee-profiles|self-eval|evidence>'",
            false,
        ));
    };

    if matches!(action, "--help" | "-h" | "help") {
        print_training_usage();
        return Ok(TrainingCliOutcome::HelpPrinted);
    }

    let sub_args = &cleaned[1..];
    if sub_args
        .iter()
        .any(|a| matches!(a.as_str(), "--help" | "-h"))
    {
        print_training_usage();
        return Ok(TrainingCliOutcome::HelpPrinted);
    }

    let open_svc = || -> Result<AppService, ErrorEnvelope> {
        let paths = resolve_app_paths(channel_override, owned_data_dir.as_deref())
            .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
        AppService::open(paths, "USD", "USD", "UTC", true)
    };

    let json_out = match action {
        "create" => {
            let mut svc = open_svc()?;
            cmd_create(&mut svc, sub_args)?
        }
        "list" => {
            ensure_no_extra_args("list", sub_args)?;
            let svc = open_svc()?;
            let runs = svc.list_training_runs()?;
            to_sanitized_pretty_json(&runs)?
        }
        "show" => {
            let svc = open_svc()?;
            cmd_show(&svc, sub_args)?
        }
        "order" => {
            let mut svc = open_svc()?;
            cmd_order(&mut svc, sub_args)?
        }
        "cancel" => {
            let mut svc = open_svc()?;
            cmd_cancel(&mut svc, sub_args)?
        }
        "advance" => {
            let mut svc = open_svc()?;
            cmd_advance(&mut svc, sub_args)?
        }
        "note" => {
            let mut svc = open_svc()?;
            cmd_note(&mut svc, sub_args)?
        }
        "finish" => {
            let mut svc = open_svc()?;
            cmd_finish(&mut svc, sub_args)?
        }
        "reveal" => {
            let run_id = parse_only_run_id("reveal", sub_args)?;
            let mut svc = open_svc()?;
            let dto = svc.reveal_training_run(&run_id)?;
            to_sanitized_pretty_json(&dto)?
        }
        "report" => {
            let run_id = parse_only_run_id("report", sub_args)?;
            let svc = open_svc()?;
            let dto = svc.get_training_report(&run_id)?;
            to_sanitized_pretty_json(&dto)?
        }
        "replay" => {
            let svc = open_svc()?;
            cmd_replay(&svc, sub_args)?
        }
        "retry" => {
            let mut svc = open_svc()?;
            cmd_retry(&mut svc, sub_args)?
        }
        "delete" => {
            let mut svc = open_svc()?;
            cmd_delete(&mut svc, sub_args, is_interactive_tty)?
        }
        "redraw" => {
            let run_id = parse_only_run_id("redraw", sub_args)?;
            let mut svc = open_svc()?;
            let dto = svc.redraw_training_run(&run_id)?;
            to_sanitized_pretty_json(&dto)?
        }
        "start" => {
            let run_id = parse_only_run_id("start", sub_args)?;
            let mut svc = open_svc()?;
            let dto = svc.start_training_run(&run_id)?;
            to_sanitized_pretty_json(&dto)?
        }
        "pause" => {
            let run_id = parse_only_run_id("pause", sub_args)?;
            let mut svc = open_svc()?;
            let dto = svc.pause_training_run(&run_id)?;
            to_sanitized_pretty_json(&dto)?
        }
        "resume" => {
            let run_id = parse_only_run_id("resume", sub_args)?;
            let mut svc = open_svc()?;
            let dto = svc.resume_training_run(&run_id)?;
            to_sanitized_pretty_json(&dto)?
        }
        "fee-profiles" | "fee_profiles" => {
            let mut svc = open_svc()?;
            cmd_fee_profiles(&mut svc, sub_args, is_interactive_tty)?
        }
        "self-eval" | "self_eval" => {
            let mut svc = open_svc()?;
            cmd_self_eval(&mut svc, sub_args)?
        }
        "evidence" => {
            let svc = open_svc()?;
            cmd_evidence(&svc, sub_args)?
        }
        other => {
            return Err(ErrorEnvelope::new(
                "invalid_subcommand",
                format!("unknown 'training' subcommand '{other}'"),
                false,
            ))
        }
    };

    Ok(TrainingCliOutcome::Json(json_out))
}

// ============================================================================
// Argument Parsing Helpers
// ============================================================================

fn next_flag_value<'a>(
    action: &str,
    flag: &str,
    inline_val: Option<&'a str>,
    args: &'a [String],
    idx: &mut usize,
) -> Result<String, ErrorEnvelope> {
    if let Some(v) = inline_val {
        return Ok(v.to_string());
    }
    let val = args.get(*idx + 1).ok_or_else(|| {
        ErrorEnvelope::new(
            "missing_arg",
            format!("flag '{flag}' on 'training {action}' requires a value"),
            false,
        )
    })?;
    *idx += 1;
    Ok(val.clone())
}

fn split_flag(raw: &str) -> (&str, Option<&str>) {
    if raw.starts_with("--") {
        if let Some((k, v)) = raw.split_once('=') {
            return (k, Some(v));
        }
    }
    (raw, None)
}

fn reject_inline_bool_val(
    action: &str,
    flag: &str,
    inline: Option<&str>,
) -> Result<(), ErrorEnvelope> {
    if inline.is_some() {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            format!("boolean flag '{flag}' on 'training {action}' does not take '=value'"),
            false,
        ));
    }
    Ok(())
}

fn unknown_flag_err(action: &str, tok: &str) -> ErrorEnvelope {
    ErrorEnvelope::new(
        "unknown_flag",
        format!("unexpected argument '{tok}' for 'training {action}'"),
        false,
    )
}

fn parse_dec_arg(flag: &str, raw: &str) -> Result<Decimal, ErrorEnvelope> {
    parse_decimal_lossless(raw.trim()).map_err(|e| {
        ErrorEnvelope::new(
            "invalid_decimal",
            format!("invalid decimal for {flag} ('{raw}'): {e}"),
            false,
        )
    })
}

fn parse_usize_arg(flag: &str, raw: &str) -> Result<usize, ErrorEnvelope> {
    raw.trim().parse::<usize>().map_err(|e| {
        ErrorEnvelope::new(
            "invalid_integer",
            format!("invalid non-negative integer for {flag} ('{raw}'): {e}"),
            false,
        )
    })
}

fn parse_date_arg(flag: &str, raw: &str) -> Result<NaiveDate, ErrorEnvelope> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|e| {
        ErrorEnvelope::new(
            "invalid_date",
            format!("invalid date for {flag} ('{raw}', expected YYYY-MM-DD): {e}"),
            false,
        )
    })
}

fn parse_tif_arg(flag: &str, raw: &str) -> Result<TimeInForce, ErrorEnvelope> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "day" => Ok(TimeInForce::Day),
        "gtc" => Ok(TimeInForce::Gtc),
        other => Err(ErrorEnvelope::new(
            "INVALID_TIF",
            format!("unsupported time-in-force '{other}' for {flag} (expected day or gtc)"),
            false,
        )),
    }
}

fn parse_side_arg(raw: &str) -> Result<OrderSide, ErrorEnvelope> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        other => Err(ErrorEnvelope::new(
            "INVALID_SIDE",
            format!("unsupported order side '{other}' (expected buy or sell)"),
            false,
        )),
    }
}

fn ensure_no_extra_args(action: &str, args: &[String]) -> Result<(), ErrorEnvelope> {
    if let Some(first) = args.first() {
        return Err(unknown_flag_err(action, first));
    }
    Ok(())
}

fn parse_only_run_id(action: &str, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value(action, flag, inline, args, &mut i)?);
            }
            _ => return Err(unknown_flag_err(action, &args[i])),
        }
        i += 1;
    }
    run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                format!("'training {action}' requires --run-id <id>"),
                false,
            )
        })
}

fn resolve_fee_profile_id(svc: &AppService, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return BUILTIN_FEE_ZERO_COST_ID.to_string();
    }
    if svc.get_fee_profile_version(trimmed).is_ok() {
        return trimmed.to_string();
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "zero_cost" | "zero-cost" => return BUILTIN_FEE_ZERO_COST_ID.to_string(),
        "us_equity_per_share_example" | "us-equity-per-share-example" => {
            return BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string()
        }
        "crypto_rate_example" | "crypto-rate-example" => {
            return BUILTIN_FEE_CRYPTO_RATE_ID.to_string()
        }
        _ => {}
    }
    if let Ok(all) = svc.list_fee_profile_versions() {
        if let Some(found) = all
            .into_iter()
            .filter(|fp| fp.record.profile_key.eq_ignore_ascii_case(trimmed))
            .max_by_key(|fp| fp.record.version_num)
        {
            return found.record.id;
        }
    }
    trimmed.to_string()
}

// ============================================================================
// Subcommand Implementations
// ============================================================================

fn cmd_create(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut req = CreateTrainingRunRequest {
        create_and_start: true,
        ..CreateTrainingRunRequest::default()
    };
    let mut raw_instrument: Option<String> = None;
    let mut raw_fee_profile: Option<String> = None;
    let mut flag_hidden = false;
    let mut flag_no_hidden = false;
    let mut flag_start = false;
    let mut flag_prepare_only = false;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                req.run_id = Some(next_flag_value("create", flag, inline, args, &mut i)?);
            }
            "--market" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.market_category = TrainingMarketCategory::parse_str(&v)?;
            }
            "--instrument" => {
                raw_instrument = Some(next_flag_value("create", flag, inline, args, &mut i)?);
            }
            "--from" | "--start-date" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.start_date = Some(parse_date_arg(flag, &v)?);
            }
            "--to" | "--end-date" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.end_date = Some(parse_date_arg(flag, &v)?);
            }
            "--warmup-bars" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.warmup_bars = parse_usize_arg(flag, &v)?;
            }
            "--history-bars" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.history_bars = parse_usize_arg(flag, &v)?;
            }
            "--training-bars" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.training_bars = parse_usize_arg(flag, &v)?;
            }
            "--reveal-bars" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.reveal_bars = parse_usize_arg(flag, &v)?;
            }
            "--seed" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.seed = Some(v.trim().parse::<u64>().map_err(|e| {
                    ErrorEnvelope::new(
                        "invalid_seed",
                        format!("invalid u64 seed '{v}': {e}"),
                        false,
                    )
                })?);
            }
            "--dataset-version" | "--version" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.dataset_version = Some(v.trim().parse::<i64>().map_err(|e| {
                    ErrorEnvelope::new(
                        "invalid_dataset_version",
                        format!("invalid i64 dataset_version '{v}': {e}"),
                        false,
                    )
                })?);
            }
            "--initial-cash" => {
                let v = next_flag_value("create", flag, inline, args, &mut i)?;
                req.initial_cash = parse_dec_arg(flag, &v)?;
            }
            "--fee-profile" => {
                raw_fee_profile = Some(next_flag_value("create", flag, inline, args, &mut i)?);
            }
            "--hidden" => {
                reject_inline_bool_val("create", flag, inline)?;
                flag_hidden = true;
            }
            "--no-hidden" => {
                reject_inline_bool_val("create", flag, inline)?;
                flag_no_hidden = true;
            }
            "--start" => {
                reject_inline_bool_val("create", flag, inline)?;
                flag_start = true;
            }
            "--prepare-only" | "--no-start" => {
                reject_inline_bool_val("create", flag, inline)?;
                flag_prepare_only = true;
            }
            _ => return Err(unknown_flag_err("create", &args[i])),
        }
        i += 1;
    }

    if flag_hidden && flag_no_hidden {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "cannot pass both --hidden and --no-hidden",
            false,
        ));
    }
    if flag_start && flag_prepare_only {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "cannot pass both --start and --prepare-only",
            false,
        ));
    }
    if flag_no_hidden {
        req.hidden_mode = false;
    }
    if flag_prepare_only {
        req.create_and_start = false;
    }
    if let Some(inst) = raw_instrument {
        let trimmed = inst.trim();
        if !trimmed.is_empty() {
            let resolved = svc
                .resolve_market_symbol(trimmed)
                .unwrap_or_else(|_| trimmed.to_string());
            req.instrument_id = Some(resolved);
        }
    }
    if let Some(fp) = raw_fee_profile {
        req.fee_profile_version_id = resolve_fee_profile_id(svc, &fp);
    }

    let summary = svc.create_training_run(req)?;
    to_sanitized_pretty_json(&summary)
}

fn cmd_show(svc: &AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut bars_limit: Option<usize> = None;
    let mut visible_start: Option<usize> = None;
    let mut visible_end: Option<usize> = None;
    let mut max_columns: Option<usize> = None;
    let mut indicators_json: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("show", flag, inline, args, &mut i)?);
            }
            "--bars-limit" => {
                let v = next_flag_value("show", flag, inline, args, &mut i)?;
                let lim = parse_usize_arg(flag, &v)?;
                if lim == 0 {
                    return Err(ErrorEnvelope::new(
                        "invalid_arg",
                        "--bars-limit must be >= 1",
                        false,
                    ));
                }
                bars_limit = Some(lim);
            }
            "--visible-start" => {
                let v = next_flag_value("show", flag, inline, args, &mut i)?;
                visible_start = Some(parse_usize_arg(flag, &v)?);
            }
            "--visible-end" => {
                let v = next_flag_value("show", flag, inline, args, &mut i)?;
                visible_end = Some(parse_usize_arg(flag, &v)?);
            }
            "--max-columns" => {
                let v = next_flag_value("show", flag, inline, args, &mut i)?;
                let cols = parse_usize_arg(flag, &v)?;
                if cols == 0 {
                    return Err(ErrorEnvelope::new(
                        "invalid_arg",
                        "--max-columns must be >= 1",
                        false,
                    ));
                }
                max_columns = Some(cols);
            }
            "--indicators" => {
                indicators_json = Some(next_flag_value("show", flag, inline, args, &mut i)?);
            }
            _ => return Err(unknown_flag_err("show", &args[i])),
        }
        i += 1;
    }

    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training show' requires --run-id <id>",
                false,
            )
        })?;

    let has_vp_override = bars_limit.is_some()
        || visible_start.is_some()
        || visible_end.is_some()
        || indicators_json.is_some();

    let viewport_override = if has_vp_override {
        let summary = svc.get_training_run_summary(&run_id)?;
        let total = summary.visible_bar_count.max(1);
        let mut vp = KlineViewport::new(total, bars_limit.unwrap_or(160));
        let end = visible_end.unwrap_or(total).min(total);
        let start = match (visible_start, bars_limit) {
            (Some(s), _) => s.min(end),
            (None, Some(lim)) => end.saturating_sub(lim),
            (None, None) => vp.visible_start.min(end),
        };
        if visible_end == Some(0) {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "--visible-end must be >= 1",
                false,
            ));
        }
        vp.visible_start = start;
        vp.visible_end = end.max(1);
        if let Some(ind_raw) = indicators_json {
            vp.indicator_config = parse_indicator_config(&ind_raw)?;
        }
        Some(vp)
    } else {
        None
    };

    let snap = svc.get_training_workspace_snapshot(
        &run_id,
        viewport_override,
        max_columns.unwrap_or(320),
    )?;
    to_sanitized_pretty_json(&snap)
}

fn cmd_order(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut positional_kind: Option<String> = None;
    let mut flag_kind: Option<String> = None;
    let mut preview = false;
    let mut side: Option<OrderSide> = None;
    let mut tif: Option<TimeInForce> = None;
    let mut bracket_tif: Option<TimeInForce> = None;
    let mut qty: Option<Decimal> = None;
    let mut limit_price: Option<Decimal> = None;
    let mut stop_price: Option<Decimal> = None;
    let mut take_profit: Option<Decimal> = None;
    let mut stop_loss: Option<Decimal> = None;
    let mut priority: Option<i32> = None;
    let mut no_trade_plan = false;

    let mut i = 0;
    if let Some(first) = args.first() {
        if !first.starts_with('-') {
            positional_kind = Some(first.clone());
            i = 1;
        }
    }

    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("order", flag, inline, args, &mut i)?);
            }
            "--type" | "--kind" => {
                flag_kind = Some(next_flag_value("order", flag, inline, args, &mut i)?);
            }
            "--preview" => {
                reject_inline_bool_val("order", flag, inline)?;
                preview = true;
            }
            "--side" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                side = Some(parse_side_arg(&v)?);
            }
            "--tif" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                tif = Some(parse_tif_arg(flag, &v)?);
            }
            "--bracket-tif" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                bracket_tif = Some(parse_tif_arg(flag, &v)?);
            }
            "--qty" | "--quantity" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                qty = Some(parse_dec_arg(flag, &v)?);
            }
            "--limit-price" | "--price" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                limit_price = Some(parse_dec_arg(flag, &v)?);
            }
            "--stop-price" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                stop_price = Some(parse_dec_arg(flag, &v)?);
            }
            "--take-profit" | "--tp" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                take_profit = Some(parse_dec_arg(flag, &v)?);
            }
            "--stop-loss" | "--sl" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                stop_loss = Some(parse_dec_arg(flag, &v)?);
            }
            "--priority" => {
                let v = next_flag_value("order", flag, inline, args, &mut i)?;
                priority = Some(v.trim().parse::<i32>().map_err(|e| {
                    ErrorEnvelope::new(
                        "invalid_integer",
                        format!("invalid i32 --priority '{v}': {e}"),
                        false,
                    )
                })?);
            }
            "--no-trade-plan" => {
                reject_inline_bool_val("order", flag, inline)?;
                no_trade_plan = true;
            }
            _ => return Err(unknown_flag_err("order", &args[i])),
        }
        i += 1;
    }

    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training order' requires --run-id <id>",
                false,
            )
        })?;
    let quantity = qty.ok_or_else(|| {
        ErrorEnvelope::new(
            "missing_arg",
            "'training order' requires --qty <decimal>",
            false,
        )
    })?;

    if let (Some(ref pk), Some(ref fk)) = (&positional_kind, &flag_kind) {
        if !pk.eq_ignore_ascii_case(fk) {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                format!("conflicting order types '{pk}' and '--type {fk}'"),
                false,
            ));
        }
    }

    let raw_kind = flag_kind
        .or(positional_kind)
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training order' requires an order type (market, limit, stop-market, or oco via positional arg or --type)",
                false,
            )
        })?;
    let norm_kind = raw_kind.trim().to_ascii_lowercase().replace('-', "_");

    let req = if norm_kind == "oco" {
        if side == Some(OrderSide::Buy) {
            return Err(ErrorEnvelope::new(
                "INVALID_ORDER_PARAMS",
                "spot long OCO exit orders only support --side sell",
                false,
            ));
        }
        let tp = take_profit.or(limit_price).ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "OCO order requires --take-profit <price> (or --limit-price <price>)",
                false,
            )
        })?;
        let sl = stop_loss.or(stop_price).ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "OCO order requires --stop-loss <price> (or --stop-price <price>)",
                false,
            )
        })?;
        TrainingOrderRequestDto {
            kind: OrderSubmitKind::Oco {
                take_profit_limit: tp,
                stop_loss_stop: sl,
                is_trade_plan: !no_trade_plan,
            },
            quantity,
            tif: tif.unwrap_or(TimeInForce::Gtc),
            priority,
        }
    } else {
        let order_side = side.ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training order' requires --side buy|sell",
                false,
            )
        })?;
        let order_type = match norm_kind.as_str() {
            "market" => {
                if limit_price.is_some() || stop_price.is_some() {
                    return Err(ErrorEnvelope::new(
                        "INVALID_ORDER_PARAMS",
                        "market order must not specify --limit-price or --stop-price",
                        false,
                    ));
                }
                SimOrderType::Market
            }
            "limit" => {
                if stop_price.is_some() {
                    return Err(ErrorEnvelope::new(
                        "INVALID_ORDER_PARAMS",
                        "limit order must not specify --stop-price",
                        false,
                    ));
                }
                let lp = limit_price.ok_or_else(|| {
                    ErrorEnvelope::new(
                        "missing_arg",
                        "limit order requires --limit-price <decimal>",
                        false,
                    )
                })?;
                SimOrderType::Limit { limit_price: lp }
            }
            "stop_market" | "stop" => {
                if limit_price.is_some() {
                    return Err(ErrorEnvelope::new(
                        "INVALID_ORDER_PARAMS",
                        "stop-market order must not specify --limit-price",
                        false,
                    ));
                }
                let sp = stop_price.ok_or_else(|| {
                    ErrorEnvelope::new(
                        "missing_arg",
                        "stop-market order requires --stop-price <decimal>",
                        false,
                    )
                })?;
                SimOrderType::StopMarket { stop_price: sp }
            }
            "stop_limit" => {
                let sp = stop_price.ok_or_else(|| {
                    ErrorEnvelope::new(
                        "missing_arg",
                        "stop-limit order requires --stop-price <decimal>",
                        false,
                    )
                })?;
                let lp = limit_price.ok_or_else(|| {
                    ErrorEnvelope::new(
                        "missing_arg",
                        "stop-limit order requires --limit-price <decimal>",
                        false,
                    )
                })?;
                SimOrderType::StopLimit {
                    stop_price: sp,
                    limit_price: lp,
                }
            }
            other => {
                return Err(ErrorEnvelope::new(
                    "UNSUPPORTED_ORDER_TYPE",
                    format!(
                        "unsupported order type '{other}' (expected market, limit, stop-market, or oco)"
                    ),
                    false,
                ))
            }
        };

        let attached_bracket = if take_profit.is_some() || stop_loss.is_some() {
            Some(AttachedBracket {
                take_profit_limit: take_profit,
                stop_loss_stop: stop_loss,
                tif: bracket_tif.unwrap_or(TimeInForce::Gtc),
            })
        } else {
            None
        };

        TrainingOrderRequestDto {
            kind: OrderSubmitKind::Single {
                side: order_side,
                order_type,
                attached_bracket,
            },
            quantity,
            tif: tif.unwrap_or(TimeInForce::Day),
            priority,
        }
    };

    if preview {
        let dto = svc.preview_training_order(&run_id, req)?;
        to_sanitized_pretty_json(&dto)
    } else {
        let receipt = svc.submit_training_order(&run_id, req)?;
        to_sanitized_pretty_json(&receipt)
    }
}

fn cmd_cancel(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut order_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("cancel", flag, inline, args, &mut i)?);
            }
            "--order-id" => {
                order_id = Some(next_flag_value("cancel", flag, inline, args, &mut i)?);
            }
            _ => return Err(unknown_flag_err("cancel", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training cancel' requires --run-id <id>",
                false,
            )
        })?;
    let order_id = order_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training cancel' requires --order-id <id>",
                false,
            )
        })?;

    let receipt = svc.cancel_training_order(&run_id, &order_id)?;
    to_sanitized_pretty_json(&receipt)
}

fn cmd_advance(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut steps: usize = 1;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("advance", flag, inline, args, &mut i)?);
            }
            "--steps" => {
                let v = next_flag_value("advance", flag, inline, args, &mut i)?;
                steps = parse_usize_arg(flag, &v)?;
                if steps == 0 {
                    return Err(ErrorEnvelope::new(
                        "INVALID_STEPS",
                        "--steps must be >= 1",
                        false,
                    ));
                }
            }
            _ => return Err(unknown_flag_err("advance", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training advance' requires --run-id <id>",
                false,
            )
        })?;

    let mut first_receipt = svc.advance_training_bar(&run_id)?;
    for _ in 1..steps {
        if first_receipt.run.status == SimRunStatus::Finished {
            break;
        }
        let next = svc.advance_training_bar(&run_id)?;
        first_receipt.run = next.run;
        first_receipt.seq = next.seq;
        first_receipt.state_hash = next.state_hash;
        first_receipt.events.extend(next.events);
        first_receipt
            .split_cancelled_order_ids
            .extend(next.split_cancelled_order_ids);
        first_receipt
            .corporate_action_notices
            .extend(next.corporate_action_notices);
    }
    to_sanitized_pretty_json(&first_receipt)
}

fn cmd_note(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut note_id: Option<String> = None;
    let mut rationale: Option<String> = None;
    let mut emotion_tag: Option<String> = None;
    let mut linked_order_id: Option<String> = None;
    let mut list_only = false;
    let mut archive_requested = false;
    let mut archive_ids: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("note", flag, inline, args, &mut i)?);
            }
            "--note-id" => {
                note_id = Some(next_flag_value("note", flag, inline, args, &mut i)?);
            }
            "--rationale" => {
                rationale = Some(next_flag_value("note", flag, inline, args, &mut i)?);
            }
            "--emotion" | "--emotion-tag" => {
                emotion_tag = Some(next_flag_value("note", flag, inline, args, &mut i)?);
            }
            "--linked-order-id" => {
                linked_order_id = Some(next_flag_value("note", flag, inline, args, &mut i)?);
            }
            "--list" => {
                reject_inline_bool_val("note", flag, inline)?;
                list_only = true;
            }
            "--archive-all" => {
                reject_inline_bool_val("note", flag, inline)?;
                archive_requested = true;
            }
            "--archive" => {
                archive_requested = true;
                let raw_csv = if let Some(v) = inline {
                    Some(v.to_string())
                } else if let Some(peek) = args.get(i + 1) {
                    if !peek.starts_with("--") {
                        i += 1;
                        Some(peek.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(csv) = raw_csv {
                    for part in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        if !part.eq_ignore_ascii_case("all") && part != "*" {
                            archive_ids.push(part.to_string());
                        }
                    }
                }
            }
            _ => return Err(unknown_flag_err("note", &args[i])),
        }
        i += 1;
    }

    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training note' requires --run-id <id>",
                false,
            )
        })?;

    if list_only {
        if rationale.is_some() || archive_requested {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "cannot combine --list with --rationale or --archive",
                false,
            ));
        }
        let notes = svc.list_training_notes_dto(&run_id)?;
        return to_sanitized_pretty_json(&notes);
    }

    if archive_requested {
        if rationale.is_some() || note_id.is_some() || emotion_tag.is_some() {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "cannot combine --archive with note upsert flags (--rationale, --emotion, --note-id)",
                false,
            ));
        }
        let archived = svc.archive_training_notes_to_journal(&run_id, &archive_ids)?;
        return to_sanitized_pretty_json(&archived);
    }

    let rationale = rationale.ok_or_else(|| {
        ErrorEnvelope::new(
            "missing_arg",
            "'training note' requires --rationale <text> (or --archive / --list)",
            false,
        )
    })?;

    let saved = svc.upsert_training_note(
        &run_id,
        UpsertTrainingNoteRequest {
            note_id,
            rationale,
            emotion_tag,
            linked_order_id,
        },
    )?;
    to_sanitized_pretty_json(&saved)
}

fn cmd_finish(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut reveal = false;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("finish", flag, inline, args, &mut i)?);
            }
            "--reveal" => {
                reject_inline_bool_val("finish", flag, inline)?;
                reveal = true;
            }
            _ => return Err(unknown_flag_err("finish", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training finish' requires --run-id <id>",
                false,
            )
        })?;

    let fin_receipt = svc.finish_training_run(&run_id)?;
    if reveal {
        let revealed = svc.reveal_training_run(&run_id)?;
        to_sanitized_pretty_json(&revealed)
    } else {
        to_sanitized_pretty_json(&fin_receipt)
    }
}

fn cmd_replay(svc: &AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut mode = TrainingReplayMode::Scratch;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("replay", flag, inline, args, &mut i)?);
            }
            "--mode" => {
                let raw = next_flag_value("replay", flag, inline, args, &mut i)?;
                mode = match raw.trim().to_ascii_lowercase().as_str() {
                    "scratch" => TrainingReplayMode::Scratch,
                    "checkpoint" | "from_checkpoint" | "from-checkpoint" => {
                        TrainingReplayMode::FromCheckpoint
                    }
                    other => {
                        return Err(ErrorEnvelope::new(
                            "INVALID_REPLAY_MODE",
                            format!(
                                "unsupported replay mode '{other}' (expected scratch or checkpoint)"
                            ),
                            false,
                        ))
                    }
                };
            }
            _ => return Err(unknown_flag_err("replay", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training replay' requires --run-id <id>",
                false,
            )
        })?;

    let replay = svc.replay_training_run(&run_id, mode)?;
    to_sanitized_pretty_json(&replay)
}

fn cmd_retry(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut new_run_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("retry", flag, inline, args, &mut i)?);
            }
            "--new-run-id" => {
                new_run_id = Some(next_flag_value("retry", flag, inline, args, &mut i)?);
            }
            _ => return Err(unknown_flag_err("retry", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training retry' requires --run-id <id>",
                false,
            )
        })?;

    let child = svc.retry_training_run(&run_id, new_run_id)?;
    to_sanitized_pretty_json(&child)
}

fn cmd_delete(
    svc: &mut AppService,
    args: &[String],
    is_interactive_tty: bool,
) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut preview = false;
    let mut confirm_yes = false;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("delete", flag, inline, args, &mut i)?);
            }
            "--preview" => {
                reject_inline_bool_val("delete", flag, inline)?;
                preview = true;
            }
            "--yes" => {
                reject_inline_bool_val("delete", flag, inline)?;
                confirm_yes = true;
            }
            _ => return Err(unknown_flag_err("delete", &args[i])),
        }
        i += 1;
    }
    if preview && confirm_yes {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "cannot pass both --preview and --yes on 'training delete'",
            false,
        ));
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training delete' requires --run-id <id>",
                false,
            )
        })?;

    if preview {
        let dto = svc.preview_delete_training_run(&run_id)?;
        return to_sanitized_pretty_json(&dto);
    }

    if !confirm_yes {
        let scope_preview = svc.preview_delete_training_run(&run_id)?;
        let code = if is_interactive_tty {
            "confirmation_required"
        } else {
            "confirmation_required_non_tty"
        };
        return Err(ErrorEnvelope::new(
            code,
            format!(
                "destructive operation 'training delete' on run '{}' requires explicit --yes (or --preview to inspect scope: {} orders, {} fills, {} notes, {} events)",
                scope_preview.run_id,
                scope_preview.order_count,
                scope_preview.fill_count,
                scope_preview.note_count,
                scope_preview.event_count,
            ),
            false,
        ));
    }

    let deleted = svc.delete_training_run(&run_id)?;
    to_sanitized_pretty_json(&deleted)
}

fn cmd_fee_profiles(
    svc: &mut AppService,
    args: &[String],
    is_interactive_tty: bool,
) -> Result<String, ErrorEnvelope> {
    let (sub, rest) = match args.first().map(String::as_str) {
        Some(s) if !s.starts_with('-') => (s, &args[1..]),
        _ => ("list", args),
    };

    match sub {
        "list" => {
            if rest.is_empty() {
                let profiles = svc.list_fee_profile_versions()?;
                return to_sanitized_pretty_json(&profiles);
            }
            // Allow `training fee-profiles --id <id>` as shorthand for `get --id <id>`
            let mut id: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                let (flag, inline) = split_flag(&rest[i]);
                match flag {
                    "--id" => {
                        id = Some(next_flag_value("fee-profiles", flag, inline, rest, &mut i)?);
                    }
                    _ => return Err(unknown_flag_err("fee-profiles", &rest[i])),
                }
                i += 1;
            }
            let id = id.ok_or_else(|| {
                ErrorEnvelope::new("missing_arg", "expected --id <fee_profile_id>", false)
            })?;
            let profile = svc.get_fee_profile_version(id.trim())?;
            to_sanitized_pretty_json(&profile)
        }
        "get" => {
            let mut id: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                let (flag, inline) = split_flag(&rest[i]);
                match flag {
                    "--id" => {
                        id = Some(next_flag_value("fee-profiles", flag, inline, rest, &mut i)?);
                    }
                    _ => return Err(unknown_flag_err("fee-profiles", &rest[i])),
                }
                i += 1;
            }
            let id = id
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ErrorEnvelope::new(
                        "missing_arg",
                        "'training fee-profiles get' requires --id <id>",
                        false,
                    )
                })?;
            let profile = svc.get_fee_profile_version(&id)?;
            to_sanitized_pretty_json(&profile)
        }
        "save" => {
            let mut draft = FeeProfileDraftDto::default();
            let mut draft_json: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                let (flag, inline) = split_flag(&rest[i]);
                match flag {
                    "--json" => {
                        draft_json =
                            Some(next_flag_value("fee-profiles", flag, inline, rest, &mut i)?);
                    }
                    "--profile-key" => {
                        draft.profile_key =
                            next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                    }
                    "--name" => {
                        draft.name = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                    }
                    "--market" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.market_category = match v.trim().to_ascii_lowercase().as_str() {
                            "any" | "all" => FeeMarketCategory::Any,
                            "us_equity" | "us_equity_etf" => FeeMarketCategory::UsEquity,
                            "crypto_spot" | "crypto" => FeeMarketCategory::CryptoSpot,
                            other => {
                                return Err(ErrorEnvelope::new(
                                    "INVALID_MARKET_CATEGORY",
                                    format!("unsupported fee profile market '{other}'"),
                                    false,
                                ))
                            }
                        };
                    }
                    "--venue" => {
                        draft.venue =
                            Some(next_flag_value("fee-profiles", flag, inline, rest, &mut i)?);
                    }
                    "--notional-rate" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.notional_rate = parse_dec_arg(flag, &v)?;
                    }
                    "--per-unit-fee" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.per_unit_fee = parse_dec_arg(flag, &v)?;
                    }
                    "--fixed-fee" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.per_order_fixed_fee = parse_dec_arg(flag, &v)?;
                    }
                    "--min-fee" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.min_fee_per_order = Some(parse_dec_arg(flag, &v)?);
                    }
                    "--max-fee" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.max_fee_per_order = Some(parse_dec_arg(flag, &v)?);
                    }
                    "--fee-currency" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.fee_currency_mode = match v.trim().to_ascii_lowercase().as_str() {
                            "quote" => FeeCurrencyMode::Quote,
                            "received_asset" | "received-asset" => FeeCurrencyMode::ReceivedAsset,
                            other => {
                                return Err(ErrorEnvelope::new(
                                    "INVALID_FEE_PROFILE",
                                    format!("unsupported fee currency mode '{other}'"),
                                    false,
                                ))
                            }
                        };
                    }
                    "--rounding" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.rounding_mode = match v.trim().to_ascii_lowercase().as_str() {
                            "half_up" | "half-up" => FeeRoundingMode::HalfUp,
                            "half_even" | "half-even" => FeeRoundingMode::HalfEven,
                            "up" => FeeRoundingMode::Up,
                            "down" => FeeRoundingMode::Down,
                            other => {
                                return Err(ErrorEnvelope::new(
                                    "INVALID_FEE_PROFILE",
                                    format!("unsupported fee rounding mode '{other}'"),
                                    false,
                                ))
                            }
                        };
                    }
                    "--rounding-scale" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.rounding_scale = v.trim().parse::<u32>().map_err(|e| {
                            ErrorEnvelope::new("invalid_integer", e.to_string(), false)
                        })?;
                    }
                    "--charging-tier" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.charging_tier = match v.trim().to_ascii_lowercase().as_str() {
                            "per_order" | "per-order" => FeeChargingTier::PerOrder,
                            "per_fill" | "per-fill" => FeeChargingTier::PerFill,
                            other => {
                                return Err(ErrorEnvelope::new(
                                    "INVALID_FEE_PROFILE",
                                    format!("unsupported charging tier '{other}'"),
                                    false,
                                ))
                            }
                        };
                    }
                    "--slippage-bps" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.slippage = SlippageSpec::Bps(parse_dec_arg(flag, &v)?);
                    }
                    "--slippage-ticks" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        let n = v.trim().parse::<u32>().map_err(|e| {
                            ErrorEnvelope::new("invalid_integer", e.to_string(), false)
                        })?;
                        draft.slippage = SlippageSpec::Ticks(n);
                    }
                    "--half-spread-bps" => {
                        let v = next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                        draft.half_spread_bps = parse_dec_arg(flag, &v)?;
                    }
                    "--note" => {
                        draft.assumption_note =
                            next_flag_value("fee-profiles", flag, inline, rest, &mut i)?;
                    }
                    _ => return Err(unknown_flag_err("fee-profiles", &rest[i])),
                }
                i += 1;
            }
            if let Some(raw_json) = draft_json {
                draft = serde_json::from_str(&raw_json).map_err(|e| {
                    ErrorEnvelope::new(
                        "INVALID_FEE_PROFILE",
                        format!("invalid FeeProfileDraftDto JSON: {e}"),
                        false,
                    )
                })?;
            }
            let saved = svc.save_fee_profile_version(draft)?;
            to_sanitized_pretty_json(&saved)
        }
        "delete" => {
            let mut id: Option<String> = None;
            let mut yes = false;
            let mut i = 0;
            while i < rest.len() {
                let (flag, inline) = split_flag(&rest[i]);
                match flag {
                    "--id" => {
                        id = Some(next_flag_value("fee-profiles", flag, inline, rest, &mut i)?);
                    }
                    "--yes" => {
                        reject_inline_bool_val("fee-profiles", flag, inline)?;
                        yes = true;
                    }
                    _ => return Err(unknown_flag_err("fee-profiles", &rest[i])),
                }
                i += 1;
            }
            let id = id
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ErrorEnvelope::new(
                        "missing_arg",
                        "'training fee-profiles delete' requires --id <id>",
                        false,
                    )
                })?;
            if !yes {
                let code = if is_interactive_tty {
                    "confirmation_required"
                } else {
                    "confirmation_required_non_tty"
                };
                return Err(ErrorEnvelope::new(
                    code,
                    format!(
                        "destructive operation 'training fee-profiles delete' on '{id}' requires --yes"
                    ),
                    false,
                ));
            }
            svc.delete_unused_fee_profile_version(&id)?;
            to_sanitized_pretty_json(&json!({
                "deleted": true,
                "id": id,
            }))
        }
        other => Err(ErrorEnvelope::new(
            "invalid_subcommand",
            format!("unknown 'training fee-profiles' action '{other}' (expected list, get, save, or delete)"),
            false,
        )),
    }
}

fn cmd_self_eval(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut decision_score: Option<u8> = None;
    let mut discipline_score: Option<u8> = None;
    let mut summary: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("self-eval", flag, inline, args, &mut i)?);
            }
            "--decision-score" => {
                let v = next_flag_value("self-eval", flag, inline, args, &mut i)?;
                decision_score =
                    Some(v.trim().parse::<u8>().map_err(|e| {
                        ErrorEnvelope::new("INVALID_SELF_SCORE", e.to_string(), false)
                    })?);
            }
            "--discipline-score" => {
                let v = next_flag_value("self-eval", flag, inline, args, &mut i)?;
                discipline_score =
                    Some(v.trim().parse::<u8>().map_err(|e| {
                        ErrorEnvelope::new("INVALID_SELF_SCORE", e.to_string(), false)
                    })?);
            }
            "--summary" => {
                summary = Some(next_flag_value("self-eval", flag, inline, args, &mut i)?);
            }
            _ => return Err(unknown_flag_err("self-eval", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training self-eval' requires --run-id <id>",
                false,
            )
        })?;
    if let Some(score) = decision_score {
        let saved = svc.save_training_self_evaluation(
            &run_id,
            score,
            discipline_score,
            summary.as_deref().unwrap_or(""),
        )?;
        to_sanitized_pretty_json(&saved)
    } else {
        let existing = svc.get_training_self_evaluation(&run_id)?;
        to_sanitized_pretty_json(&existing)
    }
}

fn cmd_evidence(svc: &AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let mut run_id: Option<String> = None;
    let mut evidence_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        match flag {
            "--run-id" => {
                run_id = Some(next_flag_value("evidence", flag, inline, args, &mut i)?);
            }
            "--evidence-id" => {
                evidence_id = Some(next_flag_value("evidence", flag, inline, args, &mut i)?);
            }
            _ => return Err(unknown_flag_err("evidence", &args[i])),
        }
        i += 1;
    }
    let run_id = run_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training evidence' requires --run-id <id>",
                false,
            )
        })?;
    let evidence_id = evidence_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training evidence' requires --evidence-id <id>",
                false,
            )
        })?;
    let val = svc.open_training_evidence(&run_id, &evidence_id)?;
    to_sanitized_pretty_json(&val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Days, Utc};
    use delta_app::{
        apply_process_quiet_guards, run_quiet_command_with_timeout, seed_ac02_fixture_in_service,
        AdjustmentKind, AppHost, ArchiveTrainingNotesResultDto, BarRecord, CorporateActionType,
        DeleteTrainingRunPreview, FeeProfileDto, OrderStatus, PlanExecutionStatus,
        QuietCommandResult, ResolvedAppPaths, SimRejectCode, TrainingNoteDto,
        TrainingOrderPreviewDto, TrainingReplayResultDto, TrainingReportDto, TrainingRevealDto,
        TrainingRunSummaryDto, TrainingStepReceiptDto, TrainingWorkspaceSnapshotDto,
        UserSelfEvaluationDto, HIDDEN_INSTRUMENT_ID, SIM_ENGINE_VERSION,
    };
    use delta_core::{to_canonical_text, CalendarSessionKind};
    use delta_data::{query_calendar_session, SqliteStore};
    use rusqlite::params;
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
            let credential_prefix =
                format!("delta-test-m4-cli-{label}-{}-{id}", std::process::id());
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

            let mut store =
                SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").ok();
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
                    _ => {
                        make_equity_bar(&aapl, d, &(145 + (idx % 9) as i64).to_string(), seeded_dsv)
                    }
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
        let end_to =
            trading_days_from("XNAS", NaiveDate::from_ymd_opt(2024, 4, 1).unwrap(), 60)[52];
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
        assert_eq!(created.engine_version, SIM_ENGINE_VERSION);
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
        let adv1_out =
            sandbox.run_cli_ok(["advance", "--run-id", "TRN-A15-PARITY", "--steps", "1"]);
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
            .find(|o| {
                o.side == OrderSide::Buy && o.order_type.limit_price() == Some(Decimal::from(90))
            })
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
            serde_json::from_str(&sandbox.run_cli_ok(["pause", "--run-id", "TRN-A15-PARITY"]))
                .unwrap();
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

        let store_check =
            SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
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
                .map(|(i, &d)| {
                    make_equity_bar(&nvda_id, d, &(220 + (i % 13) as i64).to_string(), dsv)
                })
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
            let store =
                SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
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
            let store =
                SqliteStore::open_or_init(&sandbox.data_dir(), "USD", "USD", "UTC").unwrap();
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
            serde_json::from_str(&sandbox.run_cli_ok(["show", "--run-id", "TRN-CLI-HIDDEN"]))
                .unwrap();
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
            serde_json::from_str(&sandbox.run_cli_ok(["show", "--run-id", "TRN-DEL-TARGET"]))
                .unwrap();
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
            serde_json::from_str(&sandbox.run_cli_ok(["redraw", "--run-id", "TRN-CLI-LIFE"]))
                .unwrap();
        assert_eq!(redrawn.redraw_count, 1);

        let started: TrainingRunSummaryDto =
            serde_json::from_str(&sandbox.run_cli_ok(["start", "--run-id", "TRN-CLI-LIFE"]))
                .unwrap();
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

        let snap_limited: TrainingWorkspaceSnapshotDto =
            serde_json::from_str(&sandbox.run_cli_ok([
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
}
