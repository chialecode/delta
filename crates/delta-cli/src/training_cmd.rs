use crate::cli_args::{
    ensure_no_extra_args, extract_global_flags, has_standalone_help_flag, parse_usize_arg,
    FlagSpec, ParsedFlags,
};
use delta_app::{
    parse_indicator_config, resolve_app_paths, sanitize_terminal_output, AppService, BuildChannel,
    CreateTrainingRunRequest, ErrorEnvelope, FeeChargingTier, FeeCurrencyMode, FeeMarketCategory,
    FeeProfileDraftDto, FeeRoundingMode, KlineViewport, OrderSide, OrderSubmitKind, SimOrderType,
    SimRunStatus, SlippageSpec, TimeInForce, TrainingMarketCategory, TrainingOrderRequestDto,
    TrainingReplayMode, UpsertTrainingNoteRequest, BUILTIN_FEE_CRYPTO_RATE_ID,
    BUILTIN_FEE_US_EQUITY_PER_SHARE_ID, BUILTIN_FEE_ZERO_COST_ID,
};
use delta_core::{AttachedBracket, FillRule};
use serde::Serialize;
use serde_json::json;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[path = "training_m5_cmd.rs"]
mod m5;

/// Value flags recognized by `delta-cli training ...` so `--rationale -h` or `--summary --help`
/// is never mistaken for a standalone help switch.
const TRAINING_VALUE_FLAGS: &[&str] = &[
    "--run-id",
    "--market",
    "--instrument",
    "--from",
    "--start-date",
    "--to",
    "--end-date",
    "--warmup-bars",
    "--history-bars",
    "--training-bars",
    "--reveal-bars",
    "--seed",
    "--dataset-version",
    "--version",
    "--initial-cash",
    "--amount",
    "--timeframe",
    "--fee-profile",
    "--bars-limit",
    "--visible-start",
    "--visible-end",
    "--max-columns",
    "--indicators",
    "--type",
    "--kind",
    "--side",
    "--tif",
    "--bracket-tif",
    "--qty",
    "--quantity",
    "--limit-price",
    "--price",
    "--stop-price",
    "--take-profit",
    "--tp",
    "--stop-loss",
    "--sl",
    "--priority",
    "--fill-rule",
    "--order-id",
    "--steps",
    "--note-id",
    "--rationale",
    "--emotion",
    "--emotion-tag",
    "--linked-order-id",
    "--mode",
    "--new-run-id",
    "--id",
    "--json",
    "--profile-key",
    "--name",
    "--venue",
    "--notional-rate",
    "--per-unit-fee",
    "--fixed-fee",
    "--min-fee",
    "--max-fee",
    "--fee-currency",
    "--rounding",
    "--rounding-scale",
    "--charging-tier",
    "--slippage-bps",
    "--slippage-ticks",
    "--half-spread-bps",
    "--note",
    "--decision-score",
    "--discipline-score",
    "--summary",
    "--evidence-id",
    "--base-url",
    "--currency",
    "--deposit",
    "--withdraw",
    "--anchor",
    "--color",
    "--style",
    "--drawing-id",
];

pub fn print_training_usage() {
    let text = "\
delta-cli training - K-line replay training & simulation commands (JSON output)
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
    println!(
        "{}",
        sanitize_terminal_output(&format!("{text}{}", m5::M5_USAGE))
    );
}

pub fn to_sanitized_pretty_json(value: &impl Serialize) -> Result<String, ErrorEnvelope> {
    crate::cli_args::to_sanitized_pretty_json(value)
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
    channel_override: Option<BuildChannel>,
    cli_data_dir: Option<&Path>,
    is_interactive_tty: bool,
) -> Result<TrainingCliOutcome, ErrorEnvelope> {
    let slice = if args.first().map(String::as_str) == Some("training") {
        &args[1..]
    } else {
        args
    };

    let (globals, cleaned) = extract_global_flags(slice, false)?;
    let owned_data_dir: Option<PathBuf> = globals
        .data_dir
        .or_else(|| cli_data_dir.map(Path::to_path_buf));
    let effective_channel = globals.channel_override.or(channel_override);

    if BuildChannel::current() != BuildChannel::Release
        && effective_channel == Some(BuildChannel::Release)
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
            "expected 'delta-cli training <create|list|show|order|cancel|advance|note|finish|reveal|report|replay|retry|delete|redraw|start|pause|resume|fee-profiles|self-eval|evidence|prepare-data|account|history|modify|undo|abandon|restart|view|drawing>'",
            false,
        ));
    };

    if matches!(action, "--help" | "-h" | "help") {
        print_training_usage();
        return Ok(TrainingCliOutcome::HelpPrinted);
    }

    let sub_args = &cleaned[1..];
    if has_standalone_help_flag(sub_args, TRAINING_VALUE_FLAGS) {
        print_training_usage();
        return Ok(TrainingCliOutcome::HelpPrinted);
    }

    let open_svc = || -> Result<AppService, ErrorEnvelope> {
        let paths = resolve_app_paths(effective_channel, owned_data_dir.as_deref())
            .map_err(|e| ErrorEnvelope::new("path_error", e.to_string(), false))?;
        AppService::open(paths, "USD", "USD", "UTC", true)
    };

    let json_out = match action {
        "create" => cmd_create(&open_svc, sub_args)?,
        "list" => {
            ensure_no_extra_args("training list", sub_args)?;
            let svc = open_svc()?;
            to_sanitized_pretty_json(&svc.list_training_runs()?)?
        }
        "show" => cmd_show(&open_svc, sub_args)?,
        "order" => cmd_order(&open_svc, sub_args)?,
        "cancel" => cmd_cancel(&open_svc, sub_args)?,
        "advance" => cmd_advance(&open_svc, sub_args)?,
        "note" => cmd_note(&open_svc, sub_args)?,
        "finish" => cmd_finish(&open_svc, sub_args)?,
        "reveal" => {
            let run_id = parse_only_run_id("reveal", sub_args)?;
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.reveal_training_run(&run_id)?)?
        }
        "report" => {
            let run_id = parse_only_run_id("report", sub_args)?;
            let svc = open_svc()?;
            to_sanitized_pretty_json(&svc.get_training_report(&run_id)?)?
        }
        "replay" => cmd_replay(&open_svc, sub_args)?,
        "retry" => cmd_retry(&open_svc, sub_args)?,
        "delete" => cmd_delete(&open_svc, sub_args, is_interactive_tty)?,
        "redraw" => {
            let run_id = parse_only_run_id("redraw", sub_args)?;
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.redraw_training_run(&run_id)?)?
        }
        "start" => {
            let run_id = parse_only_run_id("start", sub_args)?;
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.start_training_run(&run_id)?)?
        }
        "pause" => {
            let run_id = parse_only_run_id("pause", sub_args)?;
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.pause_training_run(&run_id)?)?
        }
        "resume" => {
            let run_id = parse_only_run_id("resume", sub_args)?;
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.resume_training_run(&run_id)?)?
        }
        "fee-profiles" | "fee_profiles" => {
            cmd_fee_profiles(&open_svc, sub_args, is_interactive_tty)?
        }
        "self-eval" | "self_eval" => cmd_self_eval(&open_svc, sub_args)?,
        "evidence" => cmd_evidence(&open_svc, sub_args)?,
        other => match m5::dispatch_m5(other, &open_svc, sub_args) {
            Some(result) => result?,
            None => {
                return Err(ErrorEnvelope::new(
                    "invalid_subcommand",
                    format!("unknown 'training' subcommand '{other}'"),
                    false,
                ))
            }
        },
    };

    Ok(TrainingCliOutcome::Json(json_out))
}

// ============================================================================
// Parsing Helpers
// ============================================================================

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

fn parse_only_run_id(action: &str, args: &[String]) -> Result<String, ErrorEnvelope> {
    let label = format!("training {action}");
    let flags = ParsedFlags::parse(
        &label,
        args,
        FlagSpec {
            value_flags: &["--run-id"],
            ..Default::default()
        },
    )?;
    flags.required(&label, "--run-id")
}

fn resolve_fee_profile_id(svc: &AppService, raw: &str) -> Result<String, ErrorEnvelope> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(BUILTIN_FEE_ZERO_COST_ID.to_string());
    }
    if svc.get_fee_profile_version(trimmed).is_ok() {
        return Ok(trimmed.to_string());
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "zero_cost" | "zero-cost" => return Ok(BUILTIN_FEE_ZERO_COST_ID.to_string()),
        "us_equity_per_share_example" | "us-equity-per-share-example" => {
            return Ok(BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string())
        }
        "crypto_rate_example" | "crypto-rate-example" => {
            return Ok(BUILTIN_FEE_CRYPTO_RATE_ID.to_string())
        }
        _ => {}
    }
    let all = svc.list_fee_profile_versions()?;
    if let Some(found) = all
        .into_iter()
        .filter(|fp| fp.record.profile_key.eq_ignore_ascii_case(trimmed))
        .max_by_key(|fp| fp.record.version_num)
    {
        return Ok(found.record.id);
    }
    Ok(trimmed.to_string())
}

// ============================================================================
// Subcommand Implementations (parse & validate BEFORE opening AppService)
// ============================================================================

fn cmd_create(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training create",
        args,
        FlagSpec {
            value_flags: &[
                "--run-id",
                "--market",
                "--instrument",
                "--from",
                "--to",
                "--warmup-bars",
                "--history-bars",
                "--training-bars",
                "--reveal-bars",
                "--seed",
                "--dataset-version",
                "--initial-cash",
                "--amount",
                "--timeframe",
                "--fee-profile",
            ],
            switches: &[
                "--independent",
                "--hidden",
                "--no-hidden",
                "--start",
                "--prepare-only",
            ],
            optional_value_flags: &[],
            aliases: &[
                ("--start-date", "--from"),
                ("--end-date", "--to"),
                ("--version", "--dataset-version"),
                ("--no-start", "--prepare-only"),
            ],
        },
    )?;

    if flags.has("--hidden") && flags.has("--no-hidden") {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "cannot pass both --hidden and --no-hidden",
            false,
        ));
    }
    if flags.has("--start") && flags.has("--prepare-only") {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "cannot pass both --start and --prepare-only",
            false,
        ));
    }

    let mut req = CreateTrainingRunRequest {
        create_and_start: !flags.has("--prepare-only"),
        ..CreateTrainingRunRequest::default()
    };
    if let Some(id) = flags.one("--run-id") {
        req.run_id = Some(id.to_string());
    }
    if let Some(market) = flags.one("--market") {
        req.market_category = TrainingMarketCategory::parse_str(market)?;
    }
    req.start_date = flags.opt_date("--from")?;
    req.end_date = flags.opt_date("--to")?;
    if let Some(v) = flags.one("--warmup-bars") {
        req.warmup_bars = parse_usize_arg("--warmup-bars", v)?;
    }
    if let Some(v) = flags.one("--history-bars") {
        req.history_bars = parse_usize_arg("--history-bars", v)?;
    }
    if let Some(v) = flags.one("--training-bars") {
        req.training_bars = parse_usize_arg("--training-bars", v)?;
    }
    if let Some(v) = flags.one("--reveal-bars") {
        req.reveal_bars = parse_usize_arg("--reveal-bars", v)?;
    }
    req.seed = flags.opt_parsed::<u64>("--seed", "invalid_seed")?;
    req.dataset_version =
        flags.opt_parsed::<i64>("--dataset-version", "invalid_dataset_version")?;

    let mut flag_independent = flags.has("--independent");
    if let Some(cash) = flags.opt_decimal("--initial-cash")? {
        req.initial_cash = cash;
        flag_independent = true;
    }
    req.allocation = flags.opt_decimal("--amount")?;
    if let Some(tf) = flags.one("--timeframe") {
        req.timeframe = m5::parse_timeframe_arg(tf)?;
    }
    if flags.has("--no-hidden") {
        req.hidden_mode = false;
    }
    // `S-52`: CLI runs are funded from the practice account unless independent capital is asked.
    if flag_independent && req.allocation.is_some() {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "--amount funds the run from the practice account; it cannot be combined with --initial-cash/--independent",
            false,
        ));
    }
    req.use_practice_account = !flag_independent;

    let mut svc = open_svc()?;
    if let Some(inst) = flags.one("--instrument") {
        let trimmed = inst.trim();
        if !trimmed.is_empty() {
            let resolved = svc
                .resolve_market_symbol(trimmed)
                .unwrap_or_else(|_| trimmed.to_string());
            req.instrument_id = Some(resolved);
        }
    }
    if let Some(fp) = flags.one("--fee-profile") {
        req.fee_profile_version_id = resolve_fee_profile_id(&svc, fp)?;
    }

    let summary = svc.create_training_run(req)?;
    to_sanitized_pretty_json(&summary)
}

fn cmd_show(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training show",
        args,
        FlagSpec {
            value_flags: &[
                "--run-id",
                "--bars-limit",
                "--visible-start",
                "--visible-end",
                "--max-columns",
                "--indicators",
            ],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training show", "--run-id")?;
    let bars_limit = flags
        .one("--bars-limit")
        .map(|v| parse_usize_arg("--bars-limit", v))
        .transpose()?;
    if bars_limit == Some(0) {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "--bars-limit must be >= 1",
            false,
        ));
    }
    let visible_start = flags
        .one("--visible-start")
        .map(|v| parse_usize_arg("--visible-start", v))
        .transpose()?;
    let visible_end = flags
        .one("--visible-end")
        .map(|v| parse_usize_arg("--visible-end", v))
        .transpose()?;
    if visible_end == Some(0) {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "--visible-end must be >= 1",
            false,
        ));
    }
    if let (Some(s), Some(e)) = (visible_start, visible_end) {
        if s >= e {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "--visible-start must be strictly less than --visible-end",
                false,
            ));
        }
    }
    let max_columns = flags
        .one("--max-columns")
        .map(|v| parse_usize_arg("--max-columns", v))
        .transpose()?;
    if max_columns == Some(0) {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "--max-columns must be >= 1",
            false,
        ));
    }
    let parsed_indicators = flags
        .one("--indicators")
        .map(parse_indicator_config)
        .transpose()?;

    let svc = open_svc()?;
    let has_vp_override = bars_limit.is_some()
        || visible_start.is_some()
        || visible_end.is_some()
        || parsed_indicators.is_some();

    let viewport_override = if has_vp_override {
        let summary = svc.get_training_run_summary(&run_id)?;
        let total = summary.visible_bar_count.max(1);
        let mut vp = KlineViewport::new(total, bars_limit.unwrap_or(160));
        let (start, end) = match (visible_start, visible_end, bars_limit) {
            (Some(s), Some(e), _) => {
                let clamped_end = e.min(total).max(1);
                let clamped_start = s.min(clamped_end.saturating_sub(1));
                (clamped_start, clamped_end)
            }
            (Some(s), None, Some(lim)) => {
                let clamped_start = s.min(total.saturating_sub(1));
                let clamped_end = clamped_start.saturating_add(lim).min(total).max(1);
                (clamped_start, clamped_end)
            }
            (Some(s), None, None) => (s.min(total.saturating_sub(1)), total),
            (None, Some(e), Some(lim)) => {
                let clamped_end = e.min(total).max(1);
                (clamped_end.saturating_sub(lim), clamped_end)
            }
            (None, Some(e), None) => {
                let clamped_end = e.min(total).max(1);
                (
                    vp.visible_start.min(clamped_end.saturating_sub(1)),
                    clamped_end,
                )
            }
            (None, None, Some(lim)) => (total.saturating_sub(lim), total),
            (None, None, None) => (vp.visible_start.min(total.saturating_sub(1)), total),
        };
        vp.visible_start = start;
        vp.visible_end = end.max(1);
        if let Some(cfg) = parsed_indicators {
            vp.indicator_config = cfg;
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

fn cmd_order(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let (positional_kind, rest) = match args.split_first() {
        Some((first, tail)) if !first.starts_with('-') => (Some(first.clone()), tail),
        _ => (None, args),
    };
    let flags = ParsedFlags::parse(
        "training order",
        rest,
        FlagSpec {
            value_flags: &[
                "--run-id",
                "--type",
                "--side",
                "--tif",
                "--bracket-tif",
                "--qty",
                "--limit-price",
                "--stop-price",
                "--take-profit",
                "--stop-loss",
                "--priority",
                "--fill-rule",
            ],
            switches: &["--preview", "--no-trade-plan"],
            optional_value_flags: &[],
            aliases: &[
                ("--kind", "--type"),
                ("--quantity", "--qty"),
                ("--price", "--limit-price"),
                ("--tp", "--take-profit"),
                ("--sl", "--stop-loss"),
            ],
        },
    )?;

    let run_id = flags.required("training order", "--run-id")?;
    let quantity = flags.opt_decimal("--qty")?.ok_or_else(|| {
        ErrorEnvelope::new(
            "missing_arg",
            "'training order' requires --qty <decimal>",
            false,
        )
    })?;
    let side = flags.one("--side").map(parse_side_arg).transpose()?;
    let tif = flags
        .one("--tif")
        .map(|v| parse_tif_arg("--tif", v))
        .transpose()?;
    let bracket_tif = flags
        .one("--bracket-tif")
        .map(|v| parse_tif_arg("--bracket-tif", v))
        .transpose()?;
    let limit_price = flags.opt_decimal("--limit-price")?;
    let stop_price = flags.opt_decimal("--stop-price")?;
    let take_profit = flags.opt_decimal("--take-profit")?;
    let stop_loss = flags.opt_decimal("--stop-loss")?;
    let priority = flags.opt_parsed::<i32>("--priority", "invalid_integer")?;
    let fill_rule = flags
        .one("--fill-rule")
        .map(
            |v| match v.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "visible_close" | "quick" => Ok(FillRule::VisibleClose),
                "next_open_path" | "next_open" => Ok(FillRule::NextOpenPath),
                other => Err(ErrorEnvelope::new(
                    "invalid_arg",
                    format!(
                        "unknown --fill-rule '{other}' (expected visible-close or next-open-path)"
                    ),
                    false,
                )),
            },
        )
        .transpose()?;
    let preview = flags.has("--preview");
    let no_trade_plan = flags.has("--no-trade-plan");

    let normalize_kind = |raw: &str| raw.trim().to_ascii_lowercase().replace('-', "_");
    let flag_kind = flags.one("--type").map(str::to_string);
    if let (Some(ref pk), Some(ref fk)) = (&positional_kind, &flag_kind) {
        if normalize_kind(pk) != normalize_kind(fk) {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                format!("conflicting order types '{pk}' and '--type {fk}'"),
                false,
            ));
        }
    }

    let raw_kind = flag_kind.or(positional_kind).ok_or_else(|| {
        ErrorEnvelope::new(
            "missing_arg",
            "'training order' requires an order type (market, limit, stop-market, or oco via positional arg or --type)",
            false,
        )
    })?;
    let norm_kind = normalize_kind(&raw_kind);

    let req = if norm_kind == "oco" {
        if side == Some(OrderSide::Buy) {
            return Err(ErrorEnvelope::new(
                "INVALID_ORDER_PARAMS",
                "spot long OCO exit orders only support --side sell",
                false,
            ));
        }
        if fill_rule.is_some() || bracket_tif.is_some() {
            return Err(ErrorEnvelope::new(
                "INVALID_ORDER_PARAMS",
                "OCO orders do not accept --fill-rule or --bracket-tif",
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
            fill_rule: None,
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
            fill_rule,
        }
    };

    let mut svc = open_svc()?;
    if preview {
        to_sanitized_pretty_json(&svc.preview_training_order(&run_id, req)?)
    } else {
        to_sanitized_pretty_json(&svc.submit_training_order(&run_id, req)?)
    }
}

fn cmd_cancel(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training cancel",
        args,
        FlagSpec {
            value_flags: &["--run-id", "--order-id"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training cancel", "--run-id")?;
    let order_id = flags.required("training cancel", "--order-id")?;
    let mut svc = open_svc()?;
    to_sanitized_pretty_json(&svc.cancel_training_order(&run_id, &order_id)?)
}

fn cmd_advance(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training advance",
        args,
        FlagSpec {
            value_flags: &["--run-id", "--steps"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training advance", "--run-id")?;
    let steps = flags
        .one("--steps")
        .map(|v| parse_usize_arg("--steps", v))
        .transpose()?
        .unwrap_or(1);
    if steps == 0 {
        return Err(ErrorEnvelope::new(
            "INVALID_STEPS",
            "--steps must be >= 1",
            false,
        ));
    }

    let mut svc = open_svc()?;
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

fn cmd_note(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training note",
        args,
        FlagSpec {
            value_flags: &[
                "--run-id",
                "--note-id",
                "--rationale",
                "--emotion",
                "--linked-order-id",
            ],
            switches: &["--list", "--archive-all"],
            optional_value_flags: &["--archive"],
            aliases: &[("--emotion-tag", "--emotion")],
        },
    )?;
    let run_id = flags.required("training note", "--run-id")?;
    let list_only = flags.has("--list");
    let archive_requested = flags.has("--archive") || flags.has("--archive-all");
    let has_upsert_fields = flags.has("--rationale")
        || flags.has("--note-id")
        || flags.has("--emotion")
        || flags.has("--linked-order-id");

    if list_only {
        if has_upsert_fields || archive_requested {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "cannot combine --list with upsert flags or --archive",
                false,
            ));
        }
        let svc = open_svc()?;
        return to_sanitized_pretty_json(&svc.list_training_notes_dto(&run_id)?);
    }

    if archive_requested {
        if has_upsert_fields {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "cannot combine --archive with note upsert flags (--rationale, --emotion, --note-id, --linked-order-id)",
                false,
            ));
        }
        let mut archive_ids = Vec::new();
        for csv in flags.all("--archive") {
            for part in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if !part.eq_ignore_ascii_case("all") && part != "*" {
                    archive_ids.push(part.to_string());
                }
            }
        }
        let mut svc = open_svc()?;
        return to_sanitized_pretty_json(
            &svc.archive_training_notes_to_journal(&run_id, &archive_ids)?,
        );
    }

    let rationale = flags
        .one("--rationale")
        .map(str::to_string)
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                "'training note' requires --rationale <text> (or --archive / --list)",
                false,
            )
        })?;
    let mut svc = open_svc()?;
    let saved = svc.upsert_training_note(
        &run_id,
        UpsertTrainingNoteRequest {
            note_id: flags.one("--note-id").map(str::to_string),
            rationale,
            emotion_tag: flags.one("--emotion").map(str::to_string),
            linked_order_id: flags.one("--linked-order-id").map(str::to_string),
        },
    )?;
    to_sanitized_pretty_json(&saved)
}

fn cmd_finish(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training finish",
        args,
        FlagSpec {
            value_flags: &["--run-id"],
            switches: &["--reveal"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training finish", "--run-id")?;
    let mut svc = open_svc()?;
    let fin_receipt = svc.finish_training_run(&run_id)?;
    if flags.has("--reveal") {
        to_sanitized_pretty_json(&svc.reveal_training_run(&run_id)?)
    } else {
        to_sanitized_pretty_json(&fin_receipt)
    }
}

fn cmd_replay(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training replay",
        args,
        FlagSpec {
            value_flags: &["--run-id", "--mode"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training replay", "--run-id")?;
    let mode = match flags.one("--mode") {
        None => TrainingReplayMode::Scratch,
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "scratch" => TrainingReplayMode::Scratch,
            "checkpoint" | "from_checkpoint" | "from-checkpoint" => {
                TrainingReplayMode::FromCheckpoint
            }
            other => {
                return Err(ErrorEnvelope::new(
                    "INVALID_REPLAY_MODE",
                    format!("unsupported replay mode '{other}' (expected scratch or checkpoint)"),
                    false,
                ))
            }
        },
    };
    let svc = open_svc()?;
    to_sanitized_pretty_json(&svc.replay_training_run(&run_id, mode)?)
}

fn cmd_retry(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training retry",
        args,
        FlagSpec {
            value_flags: &["--run-id", "--new-run-id"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training retry", "--run-id")?;
    let new_run_id = flags.one("--new-run-id").map(str::to_string);
    let mut svc = open_svc()?;
    to_sanitized_pretty_json(&svc.retry_training_run(&run_id, new_run_id)?)
}

fn cmd_delete(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
    is_interactive_tty: bool,
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training delete",
        args,
        FlagSpec {
            value_flags: &["--run-id"],
            switches: &["--preview", "--yes"],
            ..Default::default()
        },
    )?;
    let preview = flags.has("--preview");
    let confirm_yes = flags.has("--yes");
    if preview && confirm_yes {
        return Err(ErrorEnvelope::new(
            "invalid_arg",
            "cannot pass both --preview and --yes on 'training delete'",
            false,
        ));
    }
    let run_id = flags.required("training delete", "--run-id")?;

    let mut svc = open_svc()?;
    if preview {
        return to_sanitized_pretty_json(&svc.preview_delete_training_run(&run_id)?);
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

    to_sanitized_pretty_json(&svc.delete_training_run(&run_id)?)
}

fn cmd_fee_profiles(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
    is_interactive_tty: bool,
) -> Result<String, ErrorEnvelope> {
    let (sub, rest) = match args.first().map(String::as_str) {
        Some(s) if !s.starts_with('-') => (s, &args[1..]),
        _ => ("list", args),
    };

    match sub {
        "list" => {
            let flags = ParsedFlags::parse(
                "training fee-profiles",
                rest,
                FlagSpec {
                    value_flags: &["--id"],
                    ..Default::default()
                },
            )?;
            let svc = open_svc()?;
            if let Some(id) = flags.one("--id") {
                to_sanitized_pretty_json(&svc.get_fee_profile_version(id.trim())?)
            } else {
                to_sanitized_pretty_json(&svc.list_fee_profile_versions()?)
            }
        }
        "get" => {
            let flags = ParsedFlags::parse(
                "training fee-profiles get",
                rest,
                FlagSpec {
                    value_flags: &["--id"],
                    ..Default::default()
                },
            )?;
            let id = flags.required("training fee-profiles get", "--id")?;
            let svc = open_svc()?;
            to_sanitized_pretty_json(&svc.get_fee_profile_version(&id)?)
        }
        "save" => {
            const FIELD_FLAGS: &[&str] = &[
                "--profile-key",
                "--name",
                "--market",
                "--venue",
                "--notional-rate",
                "--per-unit-fee",
                "--fixed-fee",
                "--min-fee",
                "--max-fee",
                "--fee-currency",
                "--rounding",
                "--rounding-scale",
                "--charging-tier",
                "--slippage-bps",
                "--slippage-ticks",
                "--half-spread-bps",
                "--note",
            ];
            let flags = ParsedFlags::parse(
                "training fee-profiles save",
                rest,
                FlagSpec {
                    value_flags: &[
                        "--json",
                        "--profile-key",
                        "--name",
                        "--market",
                        "--venue",
                        "--notional-rate",
                        "--per-unit-fee",
                        "--fixed-fee",
                        "--min-fee",
                        "--max-fee",
                        "--fee-currency",
                        "--rounding",
                        "--rounding-scale",
                        "--charging-tier",
                        "--slippage-bps",
                        "--slippage-ticks",
                        "--half-spread-bps",
                        "--note",
                    ],
                    ..Default::default()
                },
            )?;
            let draft = if let Some(raw_json) = flags.one("--json") {
                if FIELD_FLAGS.iter().any(|f| flags.has(f)) {
                    return Err(ErrorEnvelope::new(
                        "invalid_arg",
                        "cannot combine --json with individual fee-profile field flags",
                        false,
                    ));
                }
                serde_json::from_str::<FeeProfileDraftDto>(raw_json).map_err(|e| {
                    ErrorEnvelope::new(
                        "INVALID_FEE_PROFILE",
                        format!("invalid FeeProfileDraftDto JSON: {e}"),
                        false,
                    )
                })?
            } else {
                let mut draft = FeeProfileDraftDto::default();
                if let Some(v) = flags.one("--profile-key") {
                    draft.profile_key = v.to_string();
                }
                if let Some(v) = flags.one("--name") {
                    draft.name = v.to_string();
                }
                if let Some(v) = flags.one("--market") {
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
                if let Some(v) = flags.one("--venue") {
                    draft.venue = Some(v.to_string());
                }
                if let Some(v) = flags.opt_decimal("--notional-rate")? {
                    draft.notional_rate = v;
                }
                if let Some(v) = flags.opt_decimal("--per-unit-fee")? {
                    draft.per_unit_fee = v;
                }
                if let Some(v) = flags.opt_decimal("--fixed-fee")? {
                    draft.per_order_fixed_fee = v;
                }
                if let Some(v) = flags.opt_decimal("--min-fee")? {
                    draft.min_fee_per_order = Some(v);
                }
                if let Some(v) = flags.opt_decimal("--max-fee")? {
                    draft.max_fee_per_order = Some(v);
                }
                if let Some(v) = flags.one("--fee-currency") {
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
                if let Some(v) = flags.one("--rounding") {
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
                if let Some(v) = flags.opt_parsed::<u32>("--rounding-scale", "invalid_integer")? {
                    draft.rounding_scale = v;
                }
                if let Some(v) = flags.one("--charging-tier") {
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
                if let Some(v) = flags.opt_decimal("--slippage-bps")? {
                    draft.slippage = SlippageSpec::Bps(v);
                }
                if let Some(v) = flags.opt_parsed::<u32>("--slippage-ticks", "invalid_integer")? {
                    draft.slippage = SlippageSpec::Ticks(v);
                }
                if let Some(v) = flags.opt_decimal("--half-spread-bps")? {
                    draft.half_spread_bps = v;
                }
                if let Some(v) = flags.one("--note") {
                    draft.assumption_note = v.to_string();
                }
                draft
            };
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.save_fee_profile_version(draft)?)
        }
        "delete" => {
            let flags = ParsedFlags::parse(
                "training fee-profiles delete",
                rest,
                FlagSpec {
                    value_flags: &["--id"],
                    switches: &["--yes"],
                    ..Default::default()
                },
            )?;
            let id = flags.required("training fee-profiles delete", "--id")?;
            if !flags.has("--yes") {
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
            let mut svc = open_svc()?;
            svc.delete_unused_fee_profile_version(&id)?;
            to_sanitized_pretty_json(&json!({
                "deleted": true,
                "id": id,
            }))
        }
        other => Err(ErrorEnvelope::new(
            "invalid_subcommand",
            format!(
                "unknown 'training fee-profiles' action '{other}' (expected list, get, save, or delete)"
            ),
            false,
        )),
    }
}

fn cmd_self_eval(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training self-eval",
        args,
        FlagSpec {
            value_flags: &[
                "--run-id",
                "--decision-score",
                "--discipline-score",
                "--summary",
            ],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training self-eval", "--run-id")?;
    let decision_score = flags.opt_parsed::<u8>("--decision-score", "INVALID_SELF_SCORE")?;
    let discipline_score = flags.opt_parsed::<u8>("--discipline-score", "INVALID_SELF_SCORE")?;
    let summary = flags.one("--summary");

    if let Some(score) = decision_score {
        let mut svc = open_svc()?;
        to_sanitized_pretty_json(&svc.save_training_self_evaluation(
            &run_id,
            score,
            discipline_score,
            summary.unwrap_or(""),
        )?)
    } else if discipline_score.is_some() || summary.is_some() {
        Err(ErrorEnvelope::new(
            "missing_arg",
            "'training self-eval' requires --decision-score <1..5> when saving --discipline-score or --summary",
            false,
        ))
    } else {
        let svc = open_svc()?;
        to_sanitized_pretty_json(&svc.get_training_self_evaluation(&run_id)?)
    }
}

fn cmd_evidence(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training evidence",
        args,
        FlagSpec {
            value_flags: &["--run-id", "--evidence-id"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training evidence", "--run-id")?;
    let evidence_id = flags.required("training evidence", "--evidence-id")?;
    let svc = open_svc()?;
    to_sanitized_pretty_json(&svc.open_training_evidence(&run_id, &evidence_id)?)
}

#[cfg(test)]
#[path = "training_cmd_tests.rs"]
mod tests;
