//! M5 training commands (`S-55`): data preparation, practice accounts, modify / undo /
//! abandon / restart, drawings, history and higher-timeframe views. Same service as the desktop.

use super::*;
use crate::cli_args::{ensure_no_extra_args, parse_date_arg, parse_dec_arg, FlagSpec, ParsedFlags};
use delta_app::{
    ModifyTrainingOrderRequest, PracticeTransferKind, PrepareTrainingDataRequest, Timeframe,
    TrainingDrawingDto, DEFAULT_TRAINING_INSTRUMENT,
};

pub(super) const M5_USAGE: &str = "
  delta-cli training prepare-data [--instrument <id>]... [--timeframe 1d|4h|1h]... [--from <YYYY-MM-DD>] [--status] [--base-url <provider>=<url>]... [--data-dir <dir>]
  delta-cli training account show|history|analysis [--currency USD|USDT] [--data-dir <dir>]
  delta-cli training account transfer --currency USD|USDT (--deposit <dec> | --withdraw <dec>) [--data-dir <dir>]
  delta-cli training history [--data-dir <dir>]
  delta-cli training modify --run-id <id> --order-id <id> [--qty <dec>] [--limit-price <dec>] [--stop-price <dec>] [--data-dir <dir>]
  delta-cli training undo|abandon|restart --run-id <id> [--data-dir <dir>]
  delta-cli training view --run-id <id> --timeframe 4h|1d [--data-dir <dir>]
  delta-cli training drawing list --run-id <id> | add --run-id <id> --kind ray|horizontal|segment --anchor <bar>:<price>... [--color <c>] [--style solid|dashed|dotted] | delete --run-id <id> --drawing-id <id> [--data-dir <dir>]
  (create also accepts --timeframe 1d|4h|1h, --amount <dec> to fund from the practice account, --independent for independent capital;
   order accepts --fill-rule visible-close for a quick fill at the visible close)";

pub(super) fn parse_timeframe_arg(raw: &str) -> Result<Timeframe, ErrorEnvelope> {
    Timeframe::parse(raw.trim()).ok_or_else(|| {
        ErrorEnvelope::new(
            "invalid_timeframe",
            format!("unknown timeframe '{raw}' (expected 1d, 4h or 1h)"),
            false,
        )
    })
}

fn currency(flags: &ParsedFlags) -> String {
    flags
        .one("--currency")
        .map(|c| c.trim().to_ascii_uppercase())
        .unwrap_or_else(|| "USDT".to_string())
}

pub(super) fn dispatch_m5(
    action: &str,
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Option<Result<String, ErrorEnvelope>> {
    let result = match action {
        "prepare-data" | "prepare_data" => cmd_prepare_data(open_svc, args),
        "account" => cmd_account(open_svc, args),
        "history" => ensure_no_extra_args("training history", args)
            .and_then(|_| open_svc())
            .and_then(|svc| svc.training_history())
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "modify" => cmd_modify(open_svc, args),
        "undo" => parse_only_run_id("undo", args)
            .and_then(|id| open_svc()?.undo_training_last(&id))
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "abandon" => parse_only_run_id("abandon", args)
            .and_then(|id| open_svc()?.abandon_training_run(&id))
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "restart" => parse_only_run_id("restart", args)
            .and_then(|id| open_svc()?.restart_training_run(&id))
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "view" => cmd_view(open_svc, args),
        "drawing" => cmd_drawing(open_svc, args),
        _ => return None,
    };
    Some(result)
}

fn cmd_prepare_data(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training prepare-data",
        args,
        FlagSpec {
            value_flags: &["--instrument", "--timeframe", "--from", "--base-url"],
            switches: &["--status"],
            ..Default::default()
        },
    )?;
    if flags.has("--status") {
        if flags.has("--timeframe")
            || flags.has("--from")
            || flags.has("--base-url")
            || flags.all("--instrument").len() > 1
        {
            return Err(ErrorEnvelope::new(
                "invalid_arg",
                "'training prepare-data --status' accepts at most one --instrument and cannot combine with --timeframe, --from or --base-url",
                false,
            ));
        }
        let id = flags
            .one("--instrument")
            .unwrap_or(DEFAULT_TRAINING_INSTRUMENT);
        let svc = open_svc()?;
        return to_sanitized_pretty_json(&svc.training_data_status(id, true)?);
    }
    let mut req = PrepareTrainingDataRequest::default();
    let instruments = flags.all("--instrument");
    if !instruments.is_empty() {
        req.instruments = instruments.iter().map(|s| s.trim().to_string()).collect();
    }
    let timeframes = flags.all("--timeframe");
    if !timeframes.is_empty() {
        req.timeframes = timeframes
            .iter()
            .map(|t| parse_timeframe_arg(t))
            .collect::<Result<_, _>>()?;
    }
    if let Some(from) = flags.one("--from") {
        req.from = Some(parse_date_arg("--from", from)?);
    }
    // Provider overrides (`binance=…`, `binance_archive=…`, `okx=…`) for recorded fixtures.
    for raw in flags.all("--base-url") {
        let (key, url) = raw.split_once('=').ok_or_else(|| {
            ErrorEnvelope::new(
                "invalid_arg",
                format!("--base-url '{raw}' must be <provider>=<url>"),
                false,
            )
        })?;
        req.base_urls
            .insert(key.trim().to_string(), url.trim().to_string());
    }
    let mut svc = open_svc()?;
    to_sanitized_pretty_json(&svc.prepare_training_data(&req)?)
}

fn cmd_account(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let (sub, rest) = match args.split_first() {
        Some((s, rest)) if !s.starts_with("--") => (s.as_str(), rest),
        _ => ("show", args),
    };
    match sub {
        "show" => {
            let flags = ParsedFlags::parse(
                "training account show",
                rest,
                FlagSpec {
                    value_flags: &["--currency"],
                    ..Default::default()
                },
            )?;
            let ccy = currency(&flags);
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.show_practice_account(&ccy)?)
        }
        "history" => {
            let flags = ParsedFlags::parse(
                "training account history",
                rest,
                FlagSpec {
                    value_flags: &["--currency"],
                    ..Default::default()
                },
            )?;
            let ccy = currency(&flags);
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.practice_account_history(&ccy)?)
        }
        "analysis" => {
            let flags = ParsedFlags::parse(
                "training account analysis",
                rest,
                FlagSpec {
                    value_flags: &["--currency"],
                    ..Default::default()
                },
            )?;
            let ccy = currency(&flags);
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.practice_account_analysis(&ccy)?)
        }
        "transfer" => {
            let flags = ParsedFlags::parse(
                "training account transfer",
                rest,
                FlagSpec {
                    value_flags: &["--currency", "--deposit", "--withdraw"],
                    ..Default::default()
                },
            )?;
            let ccy = currency(&flags);
            let (kind, flag_name, raw) =
                match (flags.one("--deposit"), flags.one("--withdraw")) {
                    (Some(d), None) => (PracticeTransferKind::Deposit, "--deposit", d),
                    (None, Some(w)) => (PracticeTransferKind::Withdrawal, "--withdraw", w),
                    _ => return Err(ErrorEnvelope::new(
                        "invalid_arg",
                        "'training account transfer' needs exactly one of --deposit or --withdraw",
                        false,
                    )),
                };
            let amount = parse_dec_arg(flag_name, raw)?;
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.transfer_practice_funds(&ccy, kind, amount)?)
        }
        other => Err(ErrorEnvelope::new(
            "invalid_subcommand",
            format!("unknown 'training account' subcommand '{other}'"),
            false,
        )),
    }
}

fn cmd_modify(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training modify",
        args,
        FlagSpec {
            value_flags: &[
                "--run-id",
                "--order-id",
                "--qty",
                "--limit-price",
                "--stop-price",
            ],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training modify", "--run-id")?;
    let req = ModifyTrainingOrderRequest {
        order_id: flags.required("training modify", "--order-id")?,
        quantity: flags.opt_decimal("--qty")?,
        limit_price: flags.opt_decimal("--limit-price")?,
        stop_price: flags.opt_decimal("--stop-price")?,
    };
    let mut svc = open_svc()?;
    to_sanitized_pretty_json(&svc.modify_training_order(&run_id, req)?)
}

fn cmd_view(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let flags = ParsedFlags::parse(
        "training view",
        args,
        FlagSpec {
            value_flags: &["--run-id", "--timeframe"],
            ..Default::default()
        },
    )?;
    let run_id = flags.required("training view", "--run-id")?;
    let tf = parse_timeframe_arg(&flags.required("training view", "--timeframe")?)?;
    let svc = open_svc()?;
    to_sanitized_pretty_json(&svc.get_training_aggregated_view(&run_id, tf)?)
}

fn cmd_drawing(
    open_svc: &dyn Fn() -> Result<AppService, ErrorEnvelope>,
    args: &[String],
) -> Result<String, ErrorEnvelope> {
    let (sub, rest) = match args.split_first() {
        Some((s, rest)) if !s.starts_with("--") => (s.as_str(), rest),
        _ => ("list", args),
    };
    match sub {
        "list" => {
            let flags = ParsedFlags::parse(
                "training drawing list",
                rest,
                FlagSpec {
                    value_flags: &["--run-id"],
                    ..Default::default()
                },
            )?;
            let run_id = flags.required("training drawing list", "--run-id")?;
            let svc = open_svc()?;
            to_sanitized_pretty_json(&svc.list_training_drawings(&run_id)?)
        }
        "add" => {
            let flags = ParsedFlags::parse(
                "training drawing add",
                rest,
                FlagSpec {
                    value_flags: &[
                        "--run-id",
                        "--kind",
                        "--anchor",
                        "--color",
                        "--style",
                        "--drawing-id",
                    ],
                    ..Default::default()
                },
            )?;
            let run_id = flags.required("training drawing add", "--run-id")?;
            let anchors = flags
                .all("--anchor")
                .iter()
                .map(|a| {
                    let (bar, price) = a.split_once(':').ok_or_else(|| {
                        ErrorEnvelope::new(
                            "invalid_arg",
                            format!("anchor '{a}' must be <bar>:<price>"),
                            false,
                        )
                    })?;
                    let bar = bar.trim().parse::<i64>().map_err(|e| {
                        ErrorEnvelope::new("invalid_arg", format!("anchor bar '{bar}': {e}"), false)
                    })?;
                    Ok((bar, parse_dec_arg("--anchor", price)?))
                })
                .collect::<Result<Vec<_>, ErrorEnvelope>>()?;
            let dto = TrainingDrawingDto {
                drawing_id: flags.one("--drawing-id").unwrap_or_default().to_string(),
                kind: flags.required("training drawing add", "--kind")?,
                anchors,
                color: flags.one("--color").unwrap_or("#f0b90b").to_string(),
                line_style: flags.one("--style").unwrap_or("solid").to_string(),
            };
            let mut svc = open_svc()?;
            to_sanitized_pretty_json(&svc.save_training_drawing(&run_id, dto)?)
        }
        "delete" => {
            let flags = ParsedFlags::parse(
                "training drawing delete",
                rest,
                FlagSpec {
                    value_flags: &["--run-id", "--drawing-id"],
                    ..Default::default()
                },
            )?;
            let run_id = flags.required("training drawing delete", "--run-id")?;
            let id = flags.required("training drawing delete", "--drawing-id")?;
            let mut svc = open_svc()?;
            svc.delete_training_drawing(&run_id, &id)?;
            to_sanitized_pretty_json(&serde_json::json!({ "deleted": id }))
        }
        other => Err(ErrorEnvelope::new(
            "invalid_subcommand",
            format!("unknown 'training drawing' subcommand '{other}'"),
            false,
        )),
    }
}
